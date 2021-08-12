//! Shared tools useful for common CLI clients.
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::io::FromRawFd;
use std::env::VarError::{NotUnicode, NotPresent};
use std::io::{BufReader, BufRead};
use std::process::Command;

use anyhow::{bail, format_err, Context, Error};
use serde_json::{json, Value};
use xdg::BaseDirectories;

use proxmox::{
    api::schema::*,
    api::cli::shellword_split,
    tools::fs::file_get_json,
};

use pbs_api_types::{BACKUP_REPO_URL, Authid, UserWithTokens};
use pbs_datastore::BackupDir;
use pbs_tools::json::json_object_to_query;

use crate::{BackupRepository, HttpClient, HttpClientOptions};

pub mod key_source;

const ENV_VAR_PBS_FINGERPRINT: &str = "PBS_FINGERPRINT";
const ENV_VAR_PBS_PASSWORD: &str = "PBS_PASSWORD";

pub const REPO_URL_SCHEMA: Schema = StringSchema::new("Repository URL.")
    .format(&BACKUP_REPO_URL)
    .max_length(256)
    .schema();

pub const CHUNK_SIZE_SCHEMA: Schema = IntegerSchema::new("Chunk size in KB. Must be a power of 2.")
    .minimum(64)
    .maximum(4096)
    .default(4096)
    .schema();

/// Helper to read a secret through a environment variable (ENV).
///
/// Tries the following variable names in order and returns the value
/// it will resolve for the first defined one:
///
/// BASE_NAME => use value from ENV(BASE_NAME) directly as secret
/// BASE_NAME_FD => read the secret from the specified file descriptor
/// BASE_NAME_FILE => read the secret from the specified file name
/// BASE_NAME_CMD => read the secret from specified command first line of output on stdout
///
/// Only return the first line of data (without CRLF).
pub fn get_secret_from_env(base_name: &str) -> Result<Option<String>, Error> {

    let firstline = |data: String| -> String {
        match data.lines().next() {
            Some(line) => line.to_string(),
            None => String::new(),
        }
    };

    let firstline_file = |file: &mut File| -> Result<String, Error> {
        let reader = BufReader::new(file);
        match reader.lines().next() {
            Some(Ok(line)) => Ok(line),
            Some(Err(err)) => Err(err.into()),
            None => Ok(String::new()),
        }
    };

    match std::env::var(base_name) {
        Ok(p) => return Ok(Some(firstline(p))),
        Err(NotUnicode(_)) => bail!(format!("{} contains bad characters", base_name)),
        Err(NotPresent) => {},
    };

    let env_name = format!("{}_FD", base_name);
    match std::env::var(&env_name) {
        Ok(fd_str) => {
            let fd: i32 = fd_str.parse()
                .map_err(|err| format_err!("unable to parse file descriptor in ENV({}): {}", env_name, err))?;
            let mut file = unsafe { File::from_raw_fd(fd) };
            return Ok(Some(firstline_file(&mut file)?));
        }
        Err(NotUnicode(_)) => bail!(format!("{} contains bad characters", env_name)),
        Err(NotPresent) => {},
    }

    let env_name = format!("{}_FILE", base_name);
    match std::env::var(&env_name) {
        Ok(filename) => {
            let mut file = std::fs::File::open(filename)
                .map_err(|err| format_err!("unable to open file in ENV({}): {}", env_name, err))?;
            return Ok(Some(firstline_file(&mut file)?));
        }
        Err(NotUnicode(_)) => bail!(format!("{} contains bad characters", env_name)),
        Err(NotPresent) => {},
    }

    let env_name = format!("{}_CMD", base_name);
    match std::env::var(&env_name) {
        Ok(ref command) => {
            let args = shellword_split(command)?;
            let mut command = Command::new(&args[0]);
            command.args(&args[1..]);
            let output = pbs_tools::run_command(command, None)?;
            return Ok(Some(firstline(output)));
        }
        Err(NotUnicode(_)) => bail!(format!("{} contains bad characters", env_name)),
        Err(NotPresent) => {},
    }

    Ok(None)
}

pub fn get_default_repository() -> Option<String> {
    std::env::var("PBS_REPOSITORY").ok()
}

pub fn extract_repository_from_value(param: &Value) -> Result<BackupRepository, Error> {
    let repo_url = param["repository"]
        .as_str()
        .map(String::from)
        .or_else(get_default_repository)
        .ok_or_else(|| format_err!("unable to get (default) repository"))?;

    let repo: BackupRepository = repo_url.parse()?;

    Ok(repo)
}

pub fn extract_repository_from_map(param: &HashMap<String, String>) -> Option<BackupRepository> {
    param
        .get("repository")
        .map(String::from)
        .or_else(get_default_repository)
        .and_then(|repo_url| repo_url.parse::<BackupRepository>().ok())
}

pub fn connect(repo: &BackupRepository) -> Result<HttpClient, Error> {
    connect_do(repo.host(), repo.port(), repo.auth_id())
        .map_err(|err| format_err!("error building client for repository {} - {}", repo, err))
}

fn connect_do(server: &str, port: u16, auth_id: &Authid) -> Result<HttpClient, Error> {
    let fingerprint = std::env::var(ENV_VAR_PBS_FINGERPRINT).ok();

    let password = get_secret_from_env(ENV_VAR_PBS_PASSWORD)?;
    let options = HttpClientOptions::new_interactive(password, fingerprint);

    HttpClient::new(server, port, auth_id, options)
}

/// like get, but simply ignore errors and return Null instead
pub async fn try_get(repo: &BackupRepository, url: &str) -> Value {

    let fingerprint = std::env::var(ENV_VAR_PBS_FINGERPRINT).ok();
    let password = get_secret_from_env(ENV_VAR_PBS_PASSWORD).unwrap_or(None);

    // ticket cache, but no questions asked
    let options = HttpClientOptions::new_interactive(password, fingerprint)
        .interactive(false);

    let client = match HttpClient::new(repo.host(), repo.port(), repo.auth_id(), options) {
        Ok(v) => v,
        _ => return Value::Null,
    };

    let mut resp = match client.get(url, None).await {
        Ok(v) => v,
        _ => return Value::Null,
    };

    if let Some(map) = resp.as_object_mut() {
        if let Some(data) = map.remove("data") {
            return data;
        }
    }
    Value::Null
}

pub fn complete_backup_group(_arg: &str, param: &HashMap<String, String>) -> Vec<String> {
    pbs_runtime::main(async { complete_backup_group_do(param).await })
}

pub async fn complete_backup_group_do(param: &HashMap<String, String>) -> Vec<String> {

    let mut result = vec![];

    let repo = match extract_repository_from_map(param) {
        Some(v) => v,
        _ => return result,
    };

    let path = format!("api2/json/admin/datastore/{}/groups", repo.store());

    let data = try_get(&repo, &path).await;

    if let Some(list) = data.as_array() {
        for item in list {
            if let (Some(backup_id), Some(backup_type)) =
                (item["backup-id"].as_str(), item["backup-type"].as_str())
            {
                result.push(format!("{}/{}", backup_type, backup_id));
            }
        }
    }

    result
}

pub fn complete_group_or_snapshot(arg: &str, param: &HashMap<String, String>) -> Vec<String> {
    pbs_runtime::main(async { complete_group_or_snapshot_do(arg, param).await })
}

pub async fn complete_group_or_snapshot_do(arg: &str, param: &HashMap<String, String>) -> Vec<String> {

    if arg.matches('/').count() < 2 {
        let groups = complete_backup_group_do(param).await;
        let mut result = vec![];
        for group in groups {
            result.push(group.to_string());
            result.push(format!("{}/", group));
        }
        return result;
    }

    complete_backup_snapshot_do(param).await
}

pub fn complete_backup_snapshot(_arg: &str, param: &HashMap<String, String>) -> Vec<String> {
    pbs_runtime::main(async { complete_backup_snapshot_do(param).await })
}

pub async fn complete_backup_snapshot_do(param: &HashMap<String, String>) -> Vec<String> {

    let mut result = vec![];

    let repo = match extract_repository_from_map(param) {
        Some(v) => v,
        _ => return result,
    };

    let path = format!("api2/json/admin/datastore/{}/snapshots", repo.store());

    let data = try_get(&repo, &path).await;

    if let Some(list) = data.as_array() {
        for item in list {
            if let (Some(backup_id), Some(backup_type), Some(backup_time)) =
                (item["backup-id"].as_str(), item["backup-type"].as_str(), item["backup-time"].as_i64())
            {
                if let Ok(snapshot) = BackupDir::new(backup_type, backup_id, backup_time) {
                    result.push(snapshot.relative_path().to_str().unwrap().to_owned());
                }
            }
        }
    }

    result
}

pub fn complete_server_file_name(_arg: &str, param: &HashMap<String, String>) -> Vec<String> {
    pbs_runtime::main(async { complete_server_file_name_do(param).await })
}

pub async fn complete_server_file_name_do(param: &HashMap<String, String>) -> Vec<String> {

    let mut result = vec![];

    let repo = match extract_repository_from_map(param) {
        Some(v) => v,
        _ => return result,
    };

    let snapshot: BackupDir = match param.get("snapshot") {
        Some(path) => {
            match path.parse() {
                Ok(v) => v,
                _ => return result,
            }
        }
        _ => return result,
    };

    let query = json_object_to_query(json!({
        "backup-type": snapshot.group().backup_type(),
        "backup-id": snapshot.group().backup_id(),
        "backup-time": snapshot.backup_time(),
    })).unwrap();

    let path = format!("api2/json/admin/datastore/{}/files?{}", repo.store(), query);

    let data = try_get(&repo, &path).await;

    if let Some(list) = data.as_array() {
        for item in list {
            if let Some(filename) = item["filename"].as_str() {
                result.push(filename.to_owned());
            }
        }
    }

    result
}

pub fn complete_archive_name(arg: &str, param: &HashMap<String, String>) -> Vec<String> {
    complete_server_file_name(arg, param)
        .iter()
        .map(|v| pbs_tools::format::strip_server_file_extension(&v))
        .collect()
}

pub fn complete_pxar_archive_name(arg: &str, param: &HashMap<String, String>) -> Vec<String> {
    complete_server_file_name(arg, param)
        .iter()
        .filter_map(|name| {
            if name.ends_with(".pxar.didx") {
                Some(pbs_tools::format::strip_server_file_extension(name))
            } else {
                None
            }
        })
        .collect()
}

pub fn complete_img_archive_name(arg: &str, param: &HashMap<String, String>) -> Vec<String> {
    complete_server_file_name(arg, param)
        .iter()
        .filter_map(|name| {
            if name.ends_with(".img.fidx") {
                Some(pbs_tools::format::strip_server_file_extension(name))
            } else {
                None
            }
        })
        .collect()
}

pub fn complete_chunk_size(_arg: &str, _param: &HashMap<String, String>) -> Vec<String> {

    let mut result = vec![];

    let mut size = 64;
    loop {
        result.push(size.to_string());
        size *= 2;
        if size > 4096 { break; }
    }

    result
}

pub fn complete_auth_id(_arg: &str, param: &HashMap<String, String>) -> Vec<String> {
    pbs_runtime::main(async { complete_auth_id_do(param).await })
}

pub async fn complete_auth_id_do(param: &HashMap<String, String>) -> Vec<String> {

    let mut result = vec![];

    let repo = match extract_repository_from_map(param) {
        Some(v) => v,
        _ => return result,
    };

    let data = try_get(&repo, "api2/json/access/users?include_tokens=true").await;

    if let Ok(parsed) = serde_json::from_value::<Vec<UserWithTokens>>(data) {
        for user in parsed {
            result.push(user.userid.to_string());
            for token in user.tokens {
                result.push(token.tokenid.to_string());
            }
        }
    };

    result
}

pub fn complete_repository(_arg: &str, _param: &HashMap<String, String>) -> Vec<String> {
    let mut result = vec![];

    let base = match BaseDirectories::with_prefix("proxmox-backup") {
        Ok(v) => v,
        _ => return result,
    };

    // usually $HOME/.cache/proxmox-backup/repo-list
    let path = match base.place_cache_file("repo-list") {
        Ok(v) => v,
        _ => return result,
    };

    let data = file_get_json(&path, None).unwrap_or_else(|_| json!({}));

    if let Some(map) = data.as_object() {
        for (repo, _count) in map {
            result.push(repo.to_owned());
        }
    }

    result
}

pub fn complete_backup_source(arg: &str, param: &HashMap<String, String>) -> Vec<String> {
    let mut result = vec![];

    let data: Vec<&str> = arg.splitn(2, ':').collect();

    if data.len() != 2 {
        result.push(String::from("root.pxar:/"));
        result.push(String::from("etc.pxar:/etc"));
        return result;
    }

    let files = pbs_tools::fs::complete_file_name(data[1], param);

    for file in files {
        result.push(format!("{}:{}", data[0], file));
    }

    result
}

pub fn base_directories() -> Result<xdg::BaseDirectories, Error> {
    xdg::BaseDirectories::with_prefix("proxmox-backup").map_err(Error::from)
}

/// Convenience helper for better error messages:
pub fn find_xdg_file(
    file_name: impl AsRef<std::path::Path>,
    description: &'static str,
) -> Result<Option<std::path::PathBuf>, Error> {
    let file_name = file_name.as_ref();
    base_directories()
        .map(|base| base.find_config_file(file_name))
        .with_context(|| format!("error searching for {}", description))
}

pub fn place_xdg_file(
    file_name: impl AsRef<std::path::Path>,
    description: &'static str,
) -> Result<std::path::PathBuf, Error> {
    let file_name = file_name.as_ref();
    base_directories()
        .and_then(|base| base.place_config_file(file_name).map_err(Error::from))
        .with_context(|| format!("failed to place {} in xdg home", description))
}
