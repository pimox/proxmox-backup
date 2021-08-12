use anyhow::{Error, bail, format_err};
use serde_json::{json, Value};
use std::collections::HashMap;

use proxmox::list_subdirs_api_method;
use proxmox::api::{api, RpcEnvironment, RpcEnvironmentType, Permission};
use proxmox::api::router::{Router, SubdirMap};
use proxmox::tools::fs::{replace_file, CreateOptions};

use proxmox_apt::repositories::{
    APTRepositoryFile, APTRepositoryFileError, APTRepositoryHandle, APTRepositoryInfo,
    APTStandardRepository,
};
use proxmox_http::ProxyConfig;

use crate::config::node;
use crate::server::WorkerTask;
use crate::tools::{
    apt,
    pbs_simple_http,
    subscription,
};
use crate::config::acl::{PRIV_SYS_AUDIT, PRIV_SYS_MODIFY};
use crate::api2::types::{Authid, APTUpdateInfo, NODE_SCHEMA, PROXMOX_CONFIG_DIGEST_SCHEMA, UPID_SCHEMA};

#[api(
    input: {
        properties: {
            node: {
                schema: NODE_SCHEMA,
            },
        },
    },
    returns: {
        description: "A list of packages with available updates.",
        type: Array,
        items: {
            type: APTUpdateInfo
        },
    },
    protected: true,
    access: {
        permission: &Permission::Privilege(&[], PRIV_SYS_AUDIT, false),
    },
)]
/// List available APT updates
fn apt_update_available(_param: Value) -> Result<Value, Error> {

    if let Ok(false) = apt::pkg_cache_expired() {
        if let Ok(Some(cache)) = apt::read_pkg_state() {
            return Ok(json!(cache.package_status));
        }
    }

    let cache = apt::update_cache()?;

    Ok(json!(cache.package_status))
}

pub fn update_apt_proxy_config(proxy_config: Option<&ProxyConfig>) -> Result<(), Error> {

    const PROXY_CFG_FN: &str = "/etc/apt/apt.conf.d/76pveproxy"; // use same file as PVE

    if let Some(proxy_config) = proxy_config {
        let proxy = proxy_config.to_proxy_string()?;
        let data = format!("Acquire::http::Proxy \"{}\";\n", proxy);
        replace_file(PROXY_CFG_FN, data.as_bytes(), CreateOptions::new())
    } else {
        match std::fs::remove_file(PROXY_CFG_FN) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => bail!("failed to remove proxy config '{}' - {}", PROXY_CFG_FN, err),
        }
    }
}

fn read_and_update_proxy_config() -> Result<Option<ProxyConfig>, Error> {
    let proxy_config = if let Ok((node_config, _digest)) = node::config() {
        node_config.http_proxy()
    } else {
        None
    };
    update_apt_proxy_config(proxy_config.as_ref())?;

    Ok(proxy_config)
}

fn do_apt_update(worker: &WorkerTask, quiet: bool) -> Result<(), Error> {
    if !quiet { worker.log("starting apt-get update") }

    read_and_update_proxy_config()?;

    let mut command = std::process::Command::new("apt-get");
    command.arg("update");

    // apt "errors" quite easily, and run_command is a bit rigid, so handle this inline for now.
    let output = command.output()
        .map_err(|err| format_err!("failed to execute {:?} - {}", command, err))?;

    if !quiet {
        worker.log(String::from_utf8(output.stdout)?);
    }

    // TODO: improve run_command to allow outputting both, stderr and stdout
    if !output.status.success() {
        if output.status.code().is_some() {
            let msg = String::from_utf8(output.stderr)
                .map(|m| if m.is_empty() { String::from("no error message") } else { m })
                .unwrap_or_else(|_| String::from("non utf8 error message (suppressed)"));
            worker.warn(msg);
        } else {
            bail!("terminated by signal");
        }
    }
    Ok(())
}

#[api(
    protected: true,
    input: {
        properties: {
            node: {
                schema: NODE_SCHEMA,
            },
            notify: {
                type: bool,
                description: r#"Send notification mail about new package updates available to the
                    email address configured for 'root@pam')."#,
                default: false,
                optional: true,
            },
            quiet: {
                description: "Only produces output suitable for logging, omitting progress indicators.",
                type: bool,
                default: false,
                optional: true,
            },
        },
    },
    returns: {
        schema: UPID_SCHEMA,
    },
    access: {
        permission: &Permission::Privilege(&[], PRIV_SYS_MODIFY, false),
    },
)]
/// Update the APT database
pub fn apt_update_database(
    notify: bool,
    quiet: bool,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<String, Error> {

    let auth_id: Authid = rpcenv.get_auth_id().unwrap().parse()?;
    let to_stdout = rpcenv.env_type() == RpcEnvironmentType::CLI;

    let upid_str = WorkerTask::new_thread("aptupdate", None, auth_id, to_stdout, move |worker| {
        do_apt_update(&worker, quiet)?;

        let mut cache = apt::update_cache()?;

        if notify {
            let mut notified = match cache.notified {
                Some(notified) => notified,
                None => std::collections::HashMap::new(),
            };
            let mut to_notify: Vec<&APTUpdateInfo> = Vec::new();

            for pkg in &cache.package_status {
                match notified.insert(pkg.package.to_owned(), pkg.version.to_owned()) {
                    Some(notified_version) => {
                        if notified_version != pkg.version {
                            to_notify.push(pkg);
                        }
                    },
                    None => to_notify.push(pkg),
                }
            }
            if !to_notify.is_empty() {
                to_notify.sort_unstable_by_key(|k| &k.package);
                crate::server::send_updates_available(&to_notify)?;
            }
            cache.notified = Some(notified);
            apt::write_pkg_cache(&cache)?;
        }

        Ok(())
    })?;

    Ok(upid_str)
}

#[api(
    protected: true,
    input: {
        properties: {
            node: {
                schema: NODE_SCHEMA,
            },
            name: {
                description: "Package name to get changelog of.",
                type: String,
            },
            version: {
                description: "Package version to get changelog of. Omit to use candidate version.",
                type: String,
                optional: true,
            },
        },
    },
    returns: {
        schema: UPID_SCHEMA,
    },
    access: {
        permission: &Permission::Privilege(&[], PRIV_SYS_MODIFY, false),
    },
)]
/// Retrieve the changelog of the specified package.
fn apt_get_changelog(
    param: Value,
) -> Result<Value, Error> {

    let name = pbs_tools::json::required_string_param(&param, "name")?.to_owned();
    let version = param["version"].as_str();

    let pkg_info = apt::list_installed_apt_packages(|data| {
        match version {
            Some(version) => version == data.active_version,
            None => data.active_version == data.candidate_version
        }
    }, Some(&name));

    if pkg_info.is_empty() {
        bail!("Package '{}' not found", name);
    }

    let proxy_config = read_and_update_proxy_config()?;
    let mut client = pbs_simple_http(proxy_config);

    let changelog_url = &pkg_info[0].change_log_url;
    // FIXME: use 'apt-get changelog' for proxmox packages as well, once repo supports it
    if changelog_url.starts_with("http://download.proxmox.com/") {
        let changelog = pbs_runtime::block_on(client.get_string(changelog_url, None))
            .map_err(|err| format_err!("Error downloading changelog from '{}': {}", changelog_url, err))?;
        Ok(json!(changelog))

    } else if changelog_url.starts_with("https://enterprise.proxmox.com/") {
        let sub = match subscription::read_subscription()? {
            Some(sub) => sub,
            None => bail!("cannot retrieve changelog from enterprise repo: no subscription info found")
        };
        let (key, id) = match sub.key {
            Some(key) => {
                match sub.serverid {
                    Some(id) => (key, id),
                    None =>
                        bail!("cannot retrieve changelog from enterprise repo: no server id found")
                }
            },
            None => bail!("cannot retrieve changelog from enterprise repo: no subscription key found")
        };

        let mut auth_header = HashMap::new();
        auth_header.insert("Authorization".to_owned(),
            format!("Basic {}", base64::encode(format!("{}:{}", key, id))));

        let changelog = pbs_runtime::block_on(client.get_string(changelog_url, Some(&auth_header)))
            .map_err(|err| format_err!("Error downloading changelog from '{}': {}", changelog_url, err))?;
        Ok(json!(changelog))

    } else {
        let mut command = std::process::Command::new("apt-get");
        command.arg("changelog");
        command.arg("-qq"); // don't display download progress
        command.arg(name);
        let output = crate::tools::run_command(command, None)?;
        Ok(json!(output))
    }
}

#[api(
    input: {
        properties: {
            node: {
                schema: NODE_SCHEMA,
            },
        },
    },
    returns: {
        description: "List of more relevant packages.",
        type: Array,
        items: {
            type: APTUpdateInfo,
        },
    },
    access: {
        permission: &Permission::Privilege(&[], PRIV_SYS_AUDIT, false),
    },
)]
/// Get package information for important Proxmox Backup Server packages.
pub fn get_versions() -> Result<Vec<APTUpdateInfo>, Error> {
    const PACKAGES: &[&str] = &[
        "ifupdown2",
        "libjs-extjs",
        "proxmox-backup",
        "proxmox-backup-docs",
        "proxmox-backup-client",
        "proxmox-backup-server",
        "proxmox-mini-journalreader",
        "proxmox-widget-toolkit",
        "pve-xtermjs",
        "smartmontools",
        "zfsutils-linux",
    ];

    fn unknown_package(package: String, extra_info: Option<String>) -> APTUpdateInfo {
        APTUpdateInfo {
            package,
            title: "unknown".into(),
            arch: "unknown".into(),
            description: "unknown".into(),
            version: "unknown".into(),
            old_version: "unknown".into(),
            origin: "unknown".into(),
            priority: "unknown".into(),
            section: "unknown".into(),
            change_log_url: "unknown".into(),
            extra_info,
        }
    }

    let is_kernel = |name: &str| name.starts_with("pve-kernel-");

    let mut packages: Vec<APTUpdateInfo> = Vec::new();
    let pbs_packages = apt::list_installed_apt_packages(
        |filter| {
            filter.installed_version == Some(filter.active_version)
                && (is_kernel(filter.package) || PACKAGES.contains(&filter.package))
        },
        None,
    );

    let running_kernel = format!(
        "running kernel: {}",
        nix::sys::utsname::uname().release().to_owned()
    );
    if let Some(proxmox_backup) = pbs_packages.iter().find(|pkg| pkg.package == "proxmox-backup") {
        let mut proxmox_backup = proxmox_backup.clone();
        proxmox_backup.extra_info = Some(running_kernel);
        packages.push(proxmox_backup);
    } else {
        packages.push(unknown_package("proxmox-backup".into(), Some(running_kernel)));
    }

    let version = pbs_buildcfg::PROXMOX_PKG_VERSION;
    let release = pbs_buildcfg::PROXMOX_PKG_RELEASE;
    let daemon_version_info = Some(format!("running version: {}.{}", version, release));
    if let Some(pkg) = pbs_packages.iter().find(|pkg| pkg.package == "proxmox-backup-server") {
        let mut pkg = pkg.clone();
        pkg.extra_info = daemon_version_info;
        packages.push(pkg);
    } else {
        packages.push(unknown_package("proxmox-backup".into(), daemon_version_info));
    }

    let mut kernel_pkgs: Vec<APTUpdateInfo> = pbs_packages
        .iter()
        .filter(|pkg| is_kernel(&pkg.package))
        .cloned()
        .collect();
    // make sure the cache mutex gets dropped before the next call to list_installed_apt_packages
    {
        let cache = apt_pkg_native::Cache::get_singleton();
        kernel_pkgs.sort_by(|left, right| {
            cache
                .compare_versions(&left.old_version, &right.old_version)
                .reverse()
        });
    }
    packages.append(&mut kernel_pkgs);

    // add entry for all packages we're interested in, even if not installed
    for pkg in PACKAGES.iter() {
        if pkg == &"proxmox-backup" || pkg == &"proxmox-backup-server" {
            continue;
        }
        match pbs_packages.iter().find(|item| &item.package == pkg) {
            Some(apt_pkg) => packages.push(apt_pkg.to_owned()),
            None => packages.push(unknown_package(pkg.to_string(), None)),
        }
    }

    Ok(packages)
}

#[api(
    input: {
        properties: {
            node: {
                schema: NODE_SCHEMA,
            },
        },
    },
    returns: {
        type: Object,
        description: "Result from parsing the APT repository files in /etc/apt/.",
        properties: {
            files: {
                description: "List of parsed repository files.",
                type: Array,
                items: {
                    type: APTRepositoryFile,
                },
            },
            errors: {
                description: "List of problematic files.",
                type: Array,
                items: {
                    type: APTRepositoryFileError,
                },
            },
            digest: {
                schema: PROXMOX_CONFIG_DIGEST_SCHEMA,
            },
            infos: {
                description: "List of additional information/warnings about the repositories.",
                items: {
                    type: APTRepositoryInfo,
                },
            },
            "standard-repos": {
                description: "List of standard repositories and their configuration status.",
                items: {
                    type: APTStandardRepository,
                },
            },
        },
    },
    access: {
        permission: &Permission::Privilege(&[], PRIV_SYS_AUDIT, false),
    },
)]
/// Get APT repository information.
pub fn get_repositories() -> Result<Value, Error> {
    let (files, errors, digest) = proxmox_apt::repositories::repositories()?;
    let digest = proxmox::tools::digest_to_hex(&digest);

    let suite = proxmox_apt::repositories::get_current_release_codename()?;

    let infos = proxmox_apt::repositories::check_repositories(&files, suite);
    let standard_repos = proxmox_apt::repositories::standard_repositories(&files, "pbs", suite);

    Ok(json!({
        "files": files,
        "errors": errors,
        "digest": digest,
        "infos": infos,
        "standard-repos": standard_repos,
    }))
}

#[api(
    input: {
        properties: {
            node: {
                schema: NODE_SCHEMA,
            },
            handle: {
                type: APTRepositoryHandle,
            },
            digest: {
                schema: PROXMOX_CONFIG_DIGEST_SCHEMA,
                optional: true,
            },
        },
    },
    protected: true,
    access: {
        permission: &Permission::Privilege(&[], PRIV_SYS_MODIFY, false),
    },
)]
/// Add the repository identified by the `handle`.
/// If the repository is already configured, it will be set to enabled.
///
/// The `digest` parameter asserts that the configuration has not been modified.
pub fn add_repository(handle: APTRepositoryHandle, digest: Option<String>) -> Result<(), Error> {
    let (mut files, errors, current_digest) = proxmox_apt::repositories::repositories()?;

    let suite = proxmox_apt::repositories::get_current_release_codename()?;

    if let Some(expected_digest) = digest {
        let current_digest = proxmox::tools::digest_to_hex(&current_digest);
        crate::tools::assert_if_modified(&expected_digest, &current_digest)?;
    }

    // check if it's already configured first
    for file in files.iter_mut() {
        for repo in file.repositories.iter_mut() {
            if repo.is_referenced_repository(handle, "pbs", &suite.to_string()) {
                if repo.enabled {
                    return Ok(());
                }

                repo.set_enabled(true);
                file.write()?;

                return Ok(());
            }
        }
    }

    let (repo, path) = proxmox_apt::repositories::get_standard_repository(handle, "pbs", suite);

    if let Some(error) = errors.iter().find(|error| error.path == path) {
        bail!(
            "unable to parse existing file {} - {}",
            error.path,
            error.error,
        );
    }

    if let Some(file) = files.iter_mut().find(|file| file.path == path) {
        file.repositories.push(repo);

        file.write()?;
    } else {
        let mut file = match APTRepositoryFile::new(&path)? {
            Some(file) => file,
            None => bail!("invalid path - {}", path),
        };

        file.repositories.push(repo);

        file.write()?;
    }

    Ok(())
}

#[api(
    input: {
        properties: {
            node: {
                schema: NODE_SCHEMA,
            },
            path: {
                description: "Path to the containing file.",
                type: String,
            },
            index: {
                description: "Index within the file (starting from 0).",
                type: usize,
            },
            enabled: {
                description: "Whether the repository should be enabled or not.",
                type: bool,
                optional: true,
            },
            digest: {
                schema: PROXMOX_CONFIG_DIGEST_SCHEMA,
                optional: true,
            },
        },
    },
    protected: true,
    access: {
        permission: &Permission::Privilege(&[], PRIV_SYS_MODIFY, false),
    },
)]
/// Change the properties of the specified repository.
///
/// The `digest` parameter asserts that the configuration has not been modified.
pub fn change_repository(
    path: String,
    index: usize,
    enabled: Option<bool>,
    digest: Option<String>,
) -> Result<(), Error> {
    let (mut files, errors, current_digest) = proxmox_apt::repositories::repositories()?;

    if let Some(expected_digest) = digest {
        let current_digest = proxmox::tools::digest_to_hex(&current_digest);
        crate::tools::assert_if_modified(&expected_digest, &current_digest)?;
    }

    if let Some(error) = errors.iter().find(|error| error.path == path) {
        bail!("unable to parse file {} - {}", error.path, error.error);
    }

    if let Some(file) = files.iter_mut().find(|file| file.path == path) {
        if let Some(repo) = file.repositories.get_mut(index) {
            if let Some(enabled) = enabled {
                repo.set_enabled(enabled);
            }

            file.write()?;
        } else {
            bail!("invalid index - {}", index);
        }
    } else {
        bail!("invalid path - {}", path);
    }

    Ok(())
}

const SUBDIRS: SubdirMap = &[
    ("changelog", &Router::new().get(&API_METHOD_APT_GET_CHANGELOG)),
    ("repositories", &Router::new()
        .get(&API_METHOD_GET_REPOSITORIES)
        .post(&API_METHOD_CHANGE_REPOSITORY)
        .put(&API_METHOD_ADD_REPOSITORY)
    ),
    ("update", &Router::new()
        .get(&API_METHOD_APT_UPDATE_AVAILABLE)
        .post(&API_METHOD_APT_UPDATE_DATABASE)
    ),
    ("versions", &Router::new().get(&API_METHOD_GET_VERSIONS)),
];

pub const ROUTER: Router = Router::new()
    .get(&list_subdirs_api_method!(SUBDIRS))
    .subdirs(SUBDIRS);
