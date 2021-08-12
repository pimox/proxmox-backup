use anyhow::{bail, format_err, Error};
use serde_json::Value;
use ::serde::{Deserialize, Serialize};

use proxmox::api::{api, ApiMethod, Router, RpcEnvironment, Permission};
use proxmox::http_err;

use pbs_client::{HttpClient, HttpClientOptions};

use crate::api2::types::*;
use crate::config::cached_user_info::CachedUserInfo;
use crate::config::remote;
use crate::config::acl::{PRIV_REMOTE_AUDIT, PRIV_REMOTE_MODIFY};
use crate::backup::open_backup_lockfile;

#[api(
    input: {
        properties: {},
    },
    returns: {
        description: "The list of configured remotes (with config digest).",
        type: Array,
        items: { type: remote::Remote },
    },
    access: {
        description: "List configured remotes filtered by Remote.Audit privileges",
        permission: &Permission::Anybody,
    },
)]
/// List all remotes
pub fn list_remotes(
    _param: Value,
    _info: &ApiMethod,
    mut rpcenv: &mut dyn RpcEnvironment,
) -> Result<Vec<remote::Remote>, Error> {
    let auth_id: Authid = rpcenv.get_auth_id().unwrap().parse()?;
    let user_info = CachedUserInfo::new()?;

    let (config, digest) = remote::config()?;

    let mut list: Vec<remote::Remote> = config.convert_to_typed_array("remote")?;
    // don't return password in api
    for remote in &mut list {
        remote.password = "".to_string();
    }

    let list = list
        .into_iter()
        .filter(|remote| {
            let privs = user_info.lookup_privs(&auth_id, &["remote", &remote.name]);
            privs & PRIV_REMOTE_AUDIT != 0
        })
        .collect();

    rpcenv["digest"] = proxmox::tools::digest_to_hex(&digest).into();
    Ok(list)
}

#[api(
    protected: true,
    input: {
        properties: {
            name: {
                schema: REMOTE_ID_SCHEMA,
            },
            comment: {
                optional: true,
                schema: SINGLE_LINE_COMMENT_SCHEMA,
            },
            host: {
                schema: DNS_NAME_OR_IP_SCHEMA,
            },
            port: {
                description: "The (optional) port.",
                type: u16,
                optional: true,
                default: 8007,
            },
            "auth-id": {
                type: Authid,
            },
            password: {
                schema: remote::REMOTE_PASSWORD_SCHEMA,
            },
            fingerprint: {
                optional: true,
                schema: CERT_FINGERPRINT_SHA256_SCHEMA,
            },
        },
    },
    access: {
        permission: &Permission::Privilege(&["remote"], PRIV_REMOTE_MODIFY, false),
    },
)]
/// Create new remote.
pub fn create_remote(password: String, param: Value) -> Result<(), Error> {

    let _lock = open_backup_lockfile(remote::REMOTE_CFG_LOCKFILE, None, true)?;

    let mut data = param;
    data["password"] = Value::from(base64::encode(password.as_bytes()));
    let remote: remote::Remote = serde_json::from_value(data)?;

    let (mut config, _digest) = remote::config()?;

    if config.sections.get(&remote.name).is_some() {
        bail!("remote '{}' already exists.", remote.name);
    }

    config.set_data(&remote.name, "remote", &remote)?;

    remote::save_config(&config)?;

    Ok(())
}

#[api(
   input: {
        properties: {
            name: {
                schema: REMOTE_ID_SCHEMA,
            },
        },
    },
    returns: { type: remote::Remote },
    access: {
        permission: &Permission::Privilege(&["remote", "{name}"], PRIV_REMOTE_AUDIT, false),
    }
)]
/// Read remote configuration data.
pub fn read_remote(
    name: String,
    _info: &ApiMethod,
    mut rpcenv: &mut dyn RpcEnvironment,
) -> Result<remote::Remote, Error> {
    let (config, digest) = remote::config()?;
    let mut data: remote::Remote = config.lookup("remote", &name)?;
    data.password = "".to_string(); // do not return password in api
    rpcenv["digest"] = proxmox::tools::digest_to_hex(&digest).into();
    Ok(data)
}

#[api()]
#[derive(Serialize, Deserialize)]
#[allow(non_camel_case_types)]
/// Deletable property name
pub enum DeletableProperty {
    /// Delete the comment property.
    comment,
    /// Delete the fingerprint property.
    fingerprint,
    /// Delete the port property.
    port,
}

#[api(
    protected: true,
    input: {
        properties: {
            name: {
                schema: REMOTE_ID_SCHEMA,
            },
            comment: {
                optional: true,
                schema: SINGLE_LINE_COMMENT_SCHEMA,
            },
            host: {
                optional: true,
                schema: DNS_NAME_OR_IP_SCHEMA,
            },
            port: {
                description: "The (optional) port.",
                type: u16,
                optional: true,
            },
            "auth-id": {
                optional: true,
                type: Authid,
            },
            password: {
                optional: true,
                schema: remote::REMOTE_PASSWORD_SCHEMA,
            },
            fingerprint: {
                optional: true,
                schema: CERT_FINGERPRINT_SHA256_SCHEMA,
            },
            delete: {
                description: "List of properties to delete.",
                type: Array,
                optional: true,
                items: {
                    type: DeletableProperty,
                }
            },
            digest: {
                optional: true,
                schema: PROXMOX_CONFIG_DIGEST_SCHEMA,
            },
        },
    },
    access: {
        permission: &Permission::Privilege(&["remote", "{name}"], PRIV_REMOTE_MODIFY, false),
    },
)]
/// Update remote configuration.
#[allow(clippy::too_many_arguments)]
pub fn update_remote(
    name: String,
    comment: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    auth_id: Option<Authid>,
    password: Option<String>,
    fingerprint: Option<String>,
    delete: Option<Vec<DeletableProperty>>,
    digest: Option<String>,
) -> Result<(), Error> {

    let _lock = open_backup_lockfile(remote::REMOTE_CFG_LOCKFILE, None, true)?;

    let (mut config, expected_digest) = remote::config()?;

    if let Some(ref digest) = digest {
        let digest = proxmox::tools::hex_to_digest(digest)?;
        crate::tools::detect_modified_configuration_file(&digest, &expected_digest)?;
    }

    let mut data: remote::Remote = config.lookup("remote", &name)?;

    if let Some(delete) = delete {
        for delete_prop in delete {
            match delete_prop {
                DeletableProperty::comment => { data.comment = None; },
                DeletableProperty::fingerprint => { data.fingerprint = None; },
                DeletableProperty::port => { data.port = None; },
            }
        }
    }

    if let Some(comment) = comment {
        let comment = comment.trim().to_string();
        if comment.is_empty() {
            data.comment = None;
        } else {
            data.comment = Some(comment);
        }
    }
    if let Some(host) = host { data.host = host; }
    if port.is_some() { data.port = port; }
    if let Some(auth_id) = auth_id { data.auth_id = auth_id; }
    if let Some(password) = password { data.password = password; }

    if let Some(fingerprint) = fingerprint { data.fingerprint = Some(fingerprint); }

    config.set_data(&name, "remote", &data)?;

    remote::save_config(&config)?;

    Ok(())
}

#[api(
    protected: true,
    input: {
        properties: {
            name: {
                schema: REMOTE_ID_SCHEMA,
            },
            digest: {
                optional: true,
                schema: PROXMOX_CONFIG_DIGEST_SCHEMA,
            },
        },
    },
    access: {
        permission: &Permission::Privilege(&["remote", "{name}"], PRIV_REMOTE_MODIFY, false),
    },
)]
/// Remove a remote from the configuration file.
pub fn delete_remote(name: String, digest: Option<String>) -> Result<(), Error> {

    use crate::config::sync::{self, SyncJobConfig};

    let (sync_jobs, _) = sync::config()?;

    let job_list: Vec<SyncJobConfig>  = sync_jobs.convert_to_typed_array("sync")?;
    for job in job_list {
        if job.remote == name {
            bail!("remote '{}' is used by sync job '{}' (datastore '{}')", name, job.id, job.store);
        }
    }

    let _lock = open_backup_lockfile(remote::REMOTE_CFG_LOCKFILE, None, true)?;

    let (mut config, expected_digest) = remote::config()?;

    if let Some(ref digest) = digest {
        let digest = proxmox::tools::hex_to_digest(digest)?;
        crate::tools::detect_modified_configuration_file(&digest, &expected_digest)?;
    }

    match config.sections.get(&name) {
        Some(_) => { config.sections.remove(&name); },
        None => bail!("remote '{}' does not exist.", name),
    }

    remote::save_config(&config)?;

    Ok(())
}

/// Helper to get client for remote.cfg entry
pub async fn remote_client(remote: remote::Remote) -> Result<HttpClient, Error> {
    let options = HttpClientOptions::new_non_interactive(remote.password.clone(), remote.fingerprint.clone());

    let client = HttpClient::new(
        &remote.host,
        remote.port.unwrap_or(8007),
        &remote.auth_id,
        options)?;
    let _auth_info = client.login() // make sure we can auth
        .await
        .map_err(|err| format_err!("remote connection to '{}' failed - {}", remote.host, err))?;

    Ok(client)
}


#[api(
    input: {
        properties: {
            name: {
                schema: REMOTE_ID_SCHEMA,
            },
        },
    },
    access: {
        permission: &Permission::Privilege(&["remote", "{name}"], PRIV_REMOTE_AUDIT, false),
    },
    returns: {
        description: "List the accessible datastores.",
        type: Array,
        items: { type: DataStoreListItem },
    },
)]
/// List datastores of a remote.cfg entry
pub async fn scan_remote_datastores(name: String) -> Result<Vec<DataStoreListItem>, Error> {
    let (remote_config, _digest) = remote::config()?;
    let remote: remote::Remote = remote_config.lookup("remote", &name)?;

    let map_remote_err = |api_err| {
        http_err!(INTERNAL_SERVER_ERROR,
                  "failed to scan remote '{}' - {}",
                  &name,
                  api_err)
    };

    let client = remote_client(remote)
        .await
        .map_err(map_remote_err)?;
    let api_res = client
        .get("api2/json/admin/datastore", None)
        .await
        .map_err(map_remote_err)?;
    let parse_res = match api_res.get("data") {
        Some(data) => serde_json::from_value::<Vec<DataStoreListItem>>(data.to_owned()),
        None => bail!("remote {} did not return any datastore list data", &name),
    };

    match parse_res {
        Ok(parsed) => Ok(parsed),
        Err(_) => bail!("Failed to parse remote scan api result."),
    }
}

const SCAN_ROUTER: Router = Router::new()
    .get(&API_METHOD_SCAN_REMOTE_DATASTORES);

const ITEM_ROUTER: Router = Router::new()
    .get(&API_METHOD_READ_REMOTE)
    .put(&API_METHOD_UPDATE_REMOTE)
    .delete(&API_METHOD_DELETE_REMOTE)
    .subdirs(&[("scan", &SCAN_ROUTER)]);

pub const ROUTER: Router = Router::new()
    .get(&API_METHOD_LIST_REMOTES)
    .post(&API_METHOD_CREATE_REMOTE)
    .match_all("name", &ITEM_ROUTER);
