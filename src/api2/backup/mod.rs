//! Backup protocol (HTTP2 upgrade)

use anyhow::{bail, format_err, Error};
use futures::*;
use hyper::header::{HeaderValue, UPGRADE};
use hyper::http::request::Parts;
use hyper::{Body, Response, Request, StatusCode};
use serde_json::{json, Value};

use proxmox::{sortable, identity, list_subdirs_api_method};
use proxmox::api::{ApiResponseFuture, ApiHandler, ApiMethod, Router, RpcEnvironment, Permission};
use proxmox::api::router::SubdirMap;
use proxmox::api::schema::*;

use pbs_tools::fs::lock_dir_noblock_shared;
use pbs_tools::json::{required_array_param, required_integer_param, required_string_param};
use pbs_datastore::PROXMOX_BACKUP_PROTOCOL_ID_V1;

use crate::server::{WorkerTask, H2Service};
use crate::backup::*;
use crate::api2::types::*;
use crate::config::acl::PRIV_DATASTORE_BACKUP;
use crate::config::cached_user_info::CachedUserInfo;

mod environment;
use environment::*;

mod upload_chunk;
use upload_chunk::*;

pub const ROUTER: Router = Router::new()
    .upgrade(&API_METHOD_UPGRADE_BACKUP);

#[sortable]
pub const API_METHOD_UPGRADE_BACKUP: ApiMethod = ApiMethod::new(
    &ApiHandler::AsyncHttp(&upgrade_to_backup_protocol),
    &ObjectSchema::new(
        concat!("Upgraded to backup protocol ('", PROXMOX_BACKUP_PROTOCOL_ID_V1!(), "')."),
        &sorted!([
            ("store", false, &DATASTORE_SCHEMA),
            ("backup-type", false, &BACKUP_TYPE_SCHEMA),
            ("backup-id", false, &BACKUP_ID_SCHEMA),
            ("backup-time", false, &BACKUP_TIME_SCHEMA),
            ("debug", true, &BooleanSchema::new("Enable verbose debug logging.").schema()),
            ("benchmark", true, &BooleanSchema::new("Job is a benchmark (do not keep data).").schema()),
        ]),
    )
).access(
    // Note: parameter 'store' is no uri parameter, so we need to test inside function body
    Some("The user needs Datastore.Backup privilege on /datastore/{store} and needs to own the backup group."),
    &Permission::Anybody
);

fn upgrade_to_backup_protocol(
    parts: Parts,
    req_body: Body,
    param: Value,
    _info: &ApiMethod,
    rpcenv: Box<dyn RpcEnvironment>,
) -> ApiResponseFuture {

async move {
    let debug = param["debug"].as_bool().unwrap_or(false);
    let benchmark = param["benchmark"].as_bool().unwrap_or(false);

    let auth_id: Authid = rpcenv.get_auth_id().unwrap().parse()?;

    let store = required_string_param(&param, "store")?.to_owned();

    let user_info = CachedUserInfo::new()?;
    user_info.check_privs(&auth_id, &["datastore", &store], PRIV_DATASTORE_BACKUP, false)?;

    let datastore = DataStore::lookup_datastore(&store)?;

    let backup_type = required_string_param(&param, "backup-type")?;
    let backup_id = required_string_param(&param, "backup-id")?;
    let backup_time = required_integer_param(&param, "backup-time")?;

    let protocols = parts
        .headers
        .get("UPGRADE")
        .ok_or_else(|| format_err!("missing Upgrade header"))?
        .to_str()?;

    if protocols != PROXMOX_BACKUP_PROTOCOL_ID_V1!() {
        bail!("invalid protocol name");
    }

    if parts.version >=  http::version::Version::HTTP_2 {
        bail!("unexpected http version '{:?}' (expected version < 2)", parts.version);
    }

    let worker_id = format!("{}:{}/{}", store, backup_type, backup_id);

    let env_type = rpcenv.env_type();

    let backup_group = BackupGroup::new(backup_type, backup_id);

    let worker_type = if backup_type == "host" && backup_id == "benchmark" {
        if !benchmark {
            bail!("unable to run benchmark without --benchmark flags");
        }
        "benchmark"
    } else {
        if benchmark {
            bail!("benchmark flags is only allowed on 'host/benchmark'");
        }
        "backup"
    };

    // lock backup group to only allow one backup per group at a time
    let (owner, _group_guard) = datastore.create_locked_backup_group(&backup_group, &auth_id)?;

    // permission check
    let correct_owner = owner == auth_id
        || (owner.is_token()
            && Authid::from(owner.user().clone()) == auth_id);
    if !correct_owner && worker_type != "benchmark" {
        // only the owner is allowed to create additional snapshots
        bail!("backup owner check failed ({} != {})", auth_id, owner);
    }

    let last_backup = {
        let info = BackupInfo::last_backup(&datastore.base_path(), &backup_group, true).unwrap_or(None);
        if let Some(info) = info {
            let (manifest, _) = datastore.load_manifest(&info.backup_dir)?;
            let verify = manifest.unprotected["verify_state"].clone();
            match serde_json::from_value::<SnapshotVerifyState>(verify) {
                Ok(verify) => {
                    match verify.state {
                        VerifyState::Ok => Some(info),
                        VerifyState::Failed => None,
                    }
                },
                Err(_) => {
                    // no verify state found, treat as valid
                    Some(info)
                }
            }
        } else {
            None
        }
    };

    let backup_dir = BackupDir::with_group(backup_group, backup_time)?;

    let _last_guard = if let Some(last) = &last_backup {
        if backup_dir.backup_time() <= last.backup_dir.backup_time() {
            bail!("backup timestamp is older than last backup.");
        }

        // lock last snapshot to prevent forgetting/pruning it during backup
        let full_path = datastore.snapshot_path(&last.backup_dir);
        Some(lock_dir_noblock_shared(&full_path, "snapshot", "base snapshot is already locked by another operation")?)
    } else {
        None
    };

    let (path, is_new, snap_guard) = datastore.create_locked_backup_dir(&backup_dir)?;
    if !is_new { bail!("backup directory already exists."); }


    WorkerTask::spawn(worker_type, Some(worker_id), auth_id.clone(), true, move |worker| {
        let mut env = BackupEnvironment::new(
            env_type, auth_id, worker.clone(), datastore, backup_dir);

        env.debug = debug;
        env.last_backup = last_backup;

        env.log(format!("starting new {} on datastore '{}': {:?}", worker_type, store, path));

        let service = H2Service::new(env.clone(), worker.clone(), &BACKUP_API_ROUTER, debug);

        let abort_future = worker.abort_future();

        let env2 = env.clone();

        let mut req_fut = hyper::upgrade::on(Request::from_parts(parts, req_body))
            .map_err(Error::from)
            .and_then(move |conn| {
                env2.debug("protocol upgrade done");

                let mut http = hyper::server::conn::Http::new();
                http.http2_only(true);
                // increase window size: todo - find optiomal size
                let window_size = 32*1024*1024; // max = (1 << 31) - 2
                http.http2_initial_stream_window_size(window_size);
                http.http2_initial_connection_window_size(window_size);
                http.http2_max_frame_size(4*1024*1024);

                let env3 = env2.clone();
                http.serve_connection(conn, service)
                    .map(move |result| {
                        match result {
                            Err(err) => {
                                // Avoid  Transport endpoint is not connected (os error 107)
                                // fixme: find a better way to test for that error
                                if err.to_string().starts_with("connection error") && env3.finished() {
                                    Ok(())
                                } else {
                                    Err(Error::from(err))
                                }
                            }
                            Ok(()) => Ok(()),
                        }
                    })
            });
        let mut abort_future = abort_future
            .map(|_| Err(format_err!("task aborted")));

        async move {
            // keep flock until task ends
            let _group_guard = _group_guard;
            let snap_guard = snap_guard;
            let _last_guard = _last_guard;

            let res = select!{
                req = req_fut => req,
                abrt = abort_future => abrt,
            };
            if benchmark {
                env.log("benchmark finished successfully");
                pbs_runtime::block_in_place(|| env.remove_backup())?;
                return Ok(());
            }

            let verify = |env: BackupEnvironment| {
                if let Err(err) = env.verify_after_complete(snap_guard) {
                    env.log(format!(
                        "backup finished, but starting the requested verify task failed: {}",
                        err
                    ));
                }
            };

            match (res, env.ensure_finished()) {
                (Ok(_), Ok(())) => {
                    env.log("backup finished successfully");
                    verify(env);
                    Ok(())
                },
                (Err(err), Ok(())) => {
                    // ignore errors after finish
                    env.log(format!("backup had errors but finished: {}", err));
                    verify(env);
                    Ok(())
                },
                (Ok(_), Err(err)) => {
                    env.log(format!("backup ended and finish failed: {}", err));
                    env.log("removing unfinished backup");
                    pbs_runtime::block_in_place(|| env.remove_backup())?;
                    Err(err)
                },
                (Err(err), Err(_)) => {
                    env.log(format!("backup failed: {}", err));
                    env.log("removing failed backup");
                    pbs_runtime::block_in_place(|| env.remove_backup())?;
                    Err(err)
                },
            }
        }
    })?;

    let response = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(UPGRADE, HeaderValue::from_static(PROXMOX_BACKUP_PROTOCOL_ID_V1!()))
        .body(Body::empty())?;

    Ok(response)
    }.boxed()
}

const BACKUP_API_SUBDIRS: SubdirMap = &[
    (
        "blob", &Router::new()
            .upload(&API_METHOD_UPLOAD_BLOB)
    ),
    (
        "dynamic_chunk", &Router::new()
            .upload(&API_METHOD_UPLOAD_DYNAMIC_CHUNK)
    ),
    (
        "dynamic_close", &Router::new()
            .post(&API_METHOD_CLOSE_DYNAMIC_INDEX)
    ),
    (
        "dynamic_index", &Router::new()
            .post(&API_METHOD_CREATE_DYNAMIC_INDEX)
            .put(&API_METHOD_DYNAMIC_APPEND)
    ),
    (
        "finish", &Router::new()
            .post(
                &ApiMethod::new(
                    &ApiHandler::Sync(&finish_backup),
                    &ObjectSchema::new("Mark backup as finished.", &[])
                )
            )
    ),
    (
        "fixed_chunk", &Router::new()
            .upload(&API_METHOD_UPLOAD_FIXED_CHUNK)
    ),
    (
        "fixed_close", &Router::new()
            .post(&API_METHOD_CLOSE_FIXED_INDEX)
    ),
    (
        "fixed_index", &Router::new()
            .post(&API_METHOD_CREATE_FIXED_INDEX)
            .put(&API_METHOD_FIXED_APPEND)
    ),
    (
        "previous", &Router::new()
            .download(&API_METHOD_DOWNLOAD_PREVIOUS)
    ),
    (
        "previous_backup_time", &Router::new()
            .get(&API_METHOD_GET_PREVIOUS_BACKUP_TIME)
    ),
    (
        "speedtest", &Router::new()
            .upload(&API_METHOD_UPLOAD_SPEEDTEST)
    ),
];

pub const BACKUP_API_ROUTER: Router = Router::new()
    .get(&list_subdirs_api_method!(BACKUP_API_SUBDIRS))
    .subdirs(BACKUP_API_SUBDIRS);

#[sortable]
pub const API_METHOD_CREATE_DYNAMIC_INDEX: ApiMethod = ApiMethod::new(
    &ApiHandler::Sync(&create_dynamic_index),
    &ObjectSchema::new(
        "Create dynamic chunk index file.",
        &sorted!([
            ("archive-name", false, &crate::api2::types::BACKUP_ARCHIVE_NAME_SCHEMA),
        ]),
    )
);

fn create_dynamic_index(
    param: Value,
    _info: &ApiMethod,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<Value, Error> {

    let env: &BackupEnvironment = rpcenv.as_ref();

    let name = required_string_param(&param, "archive-name")?.to_owned();

    let archive_name = name.clone();
    if !archive_name.ends_with(".didx") {
        bail!("wrong archive extension: '{}'", archive_name);
    }

    let mut path = env.backup_dir.relative_path();
    path.push(archive_name);

    let index = env.datastore.create_dynamic_writer(&path)?;
    let wid = env.register_dynamic_writer(index, name)?;

    env.log(format!("created new dynamic index {} ({:?})", wid, path));

    Ok(json!(wid))
}

#[sortable]
pub const API_METHOD_CREATE_FIXED_INDEX: ApiMethod = ApiMethod::new(
    &ApiHandler::Sync(&create_fixed_index),
    &ObjectSchema::new(
        "Create fixed chunk index file.",
        &sorted!([
            ("archive-name", false, &crate::api2::types::BACKUP_ARCHIVE_NAME_SCHEMA),
            ("size", false, &IntegerSchema::new("File size.")
             .minimum(1)
             .schema()
            ),
            ("reuse-csum", true, &StringSchema::new("If set, compare last backup's \
                csum and reuse index for incremental backup if it matches.").schema()),
        ]),
    )
);

fn create_fixed_index(
    param: Value,
    _info: &ApiMethod,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<Value, Error> {

    let env: &BackupEnvironment = rpcenv.as_ref();

    let name = required_string_param(&param, "archive-name")?.to_owned();
    let size = required_integer_param(&param, "size")? as usize;
    let reuse_csum = param["reuse-csum"].as_str();

    let archive_name = name.clone();
    if !archive_name.ends_with(".fidx") {
        bail!("wrong archive extension: '{}'", archive_name);
    }

    let mut path = env.backup_dir.relative_path();
    path.push(&archive_name);

    let chunk_size = 4096*1024; // todo: ??

    // do incremental backup if csum is set
    let mut reader = None;
    let mut incremental = false;
    if let Some(csum) = reuse_csum {
        incremental = true;
        let last_backup = match &env.last_backup {
            Some(info) => info,
            None => {
                bail!("cannot reuse index - no valid previous backup exists");
            }
        };

        let mut last_path = last_backup.backup_dir.relative_path();
        last_path.push(&archive_name);

        let index = match env.datastore.open_fixed_reader(last_path) {
            Ok(index) => index,
            Err(_) => {
                bail!("cannot reuse index - no previous backup exists for archive");
            }
        };

        let (old_csum, _) = index.compute_csum();
        let old_csum = proxmox::tools::digest_to_hex(&old_csum);
        if old_csum != csum {
            bail!("expected csum ({}) doesn't match last backup's ({}), cannot do incremental backup",
                csum, old_csum);
        }

        reader = Some(index);
    }

    let mut writer = env.datastore.create_fixed_writer(&path, size, chunk_size)?;

    if let Some(reader) = reader {
        writer.clone_data_from(&reader)?;
    }

    let wid = env.register_fixed_writer(writer, name, size, chunk_size as u32, incremental)?;

    env.log(format!("created new fixed index {} ({:?})", wid, path));

    Ok(json!(wid))
}

#[sortable]
pub const API_METHOD_DYNAMIC_APPEND: ApiMethod = ApiMethod::new(
    &ApiHandler::Sync(&dynamic_append),
    &ObjectSchema::new(
        "Append chunk to dynamic index writer.",
        &sorted!([
            (
                "wid",
                false,
                &IntegerSchema::new("Dynamic writer ID.")
                    .minimum(1)
                    .maximum(256)
                    .schema()
            ),
            (
                "digest-list",
                false,
                &ArraySchema::new("Chunk digest list.", &CHUNK_DIGEST_SCHEMA).schema()
            ),
            (
                "offset-list",
                false,
                &ArraySchema::new(
                    "Chunk offset list.",
                    &IntegerSchema::new("Corresponding chunk offsets.")
                        .minimum(0)
                        .schema()
                ).schema()
            ),
        ]),
    )
);

fn dynamic_append (
    param: Value,
    _info: &ApiMethod,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<Value, Error> {

    let wid = required_integer_param(&param, "wid")? as usize;
    let digest_list = required_array_param(&param, "digest-list")?;
    let offset_list = required_array_param(&param, "offset-list")?;

    if offset_list.len() != digest_list.len() {
        bail!("offset list has wrong length ({} != {})", offset_list.len(), digest_list.len());
    }

    let env: &BackupEnvironment = rpcenv.as_ref();

    env.debug(format!("dynamic_append {} chunks", digest_list.len()));

    for (i, item) in digest_list.iter().enumerate() {
        let digest_str = item.as_str().unwrap();
        let digest = proxmox::tools::hex_to_digest(digest_str)?;
        let offset = offset_list[i].as_u64().unwrap();
        let size = env.lookup_chunk(&digest).ok_or_else(|| format_err!("no such chunk {}", digest_str))?;

        env.dynamic_writer_append_chunk(wid, offset, size, &digest)?;

        env.debug(format!("successfully added chunk {} to dynamic index {} (offset {}, size {})", digest_str, wid, offset, size));
    }

    Ok(Value::Null)
}

#[sortable]
pub const API_METHOD_FIXED_APPEND: ApiMethod = ApiMethod::new(
    &ApiHandler::Sync(&fixed_append),
    &ObjectSchema::new(
        "Append chunk to fixed index writer.",
        &sorted!([
            (
                "wid",
                false,
                &IntegerSchema::new("Fixed writer ID.")
                    .minimum(1)
                    .maximum(256)
                    .schema()
            ),
            (
                "digest-list",
                false,
                &ArraySchema::new("Chunk digest list.", &CHUNK_DIGEST_SCHEMA).schema()
            ),
            (
                "offset-list",
                false,
                &ArraySchema::new(
                    "Chunk offset list.",
                    &IntegerSchema::new("Corresponding chunk offsets.")
                        .minimum(0)
                        .schema()
                ).schema()
            )
        ]),
    )
);

fn fixed_append (
    param: Value,
    _info: &ApiMethod,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<Value, Error> {

    let wid = required_integer_param(&param, "wid")? as usize;
    let digest_list = required_array_param(&param, "digest-list")?;
    let offset_list = required_array_param(&param, "offset-list")?;

    if offset_list.len() != digest_list.len() {
        bail!("offset list has wrong length ({} != {})", offset_list.len(), digest_list.len());
    }

    let env: &BackupEnvironment = rpcenv.as_ref();

    env.debug(format!("fixed_append {} chunks", digest_list.len()));

    for (i, item) in digest_list.iter().enumerate() {
        let digest_str = item.as_str().unwrap();
        let digest = proxmox::tools::hex_to_digest(digest_str)?;
        let offset = offset_list[i].as_u64().unwrap();
        let size = env.lookup_chunk(&digest).ok_or_else(|| format_err!("no such chunk {}", digest_str))?;

        env.fixed_writer_append_chunk(wid, offset, size, &digest)?;

        env.debug(format!("successfully added chunk {} to fixed index {} (offset {}, size {})", digest_str, wid, offset, size));
    }

    Ok(Value::Null)
}

#[sortable]
pub const API_METHOD_CLOSE_DYNAMIC_INDEX: ApiMethod = ApiMethod::new(
    &ApiHandler::Sync(&close_dynamic_index),
    &ObjectSchema::new(
        "Close dynamic index writer.",
        &sorted!([
            (
                "wid",
                false,
                &IntegerSchema::new("Dynamic writer ID.")
                    .minimum(1)
                    .maximum(256)
                    .schema()
            ),
            (
                "chunk-count",
                false,
                &IntegerSchema::new("Chunk count. This is used to verify that the server got all chunks.")
                    .minimum(1)
                    .schema()
            ),
            (
                "size",
                false,
                &IntegerSchema::new("File size. This is used to verify that the server got all data.")
                    .minimum(1)
                    .schema()
            ),
            ("csum", false, &StringSchema::new("Digest list checksum.").schema()),
        ]),
    )
);

fn close_dynamic_index (
    param: Value,
    _info: &ApiMethod,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<Value, Error> {

    let wid = required_integer_param(&param, "wid")? as usize;
    let chunk_count = required_integer_param(&param, "chunk-count")? as u64;
    let size = required_integer_param(&param, "size")? as u64;
    let csum_str = required_string_param(&param, "csum")?;
    let csum = proxmox::tools::hex_to_digest(csum_str)?;

    let env: &BackupEnvironment = rpcenv.as_ref();

    env.dynamic_writer_close(wid, chunk_count, size, csum)?;

    env.log(format!("successfully closed dynamic index {}", wid));

    Ok(Value::Null)
}

#[sortable]
pub const API_METHOD_CLOSE_FIXED_INDEX: ApiMethod = ApiMethod::new(
    &ApiHandler::Sync(&close_fixed_index),
    &ObjectSchema::new(
        "Close fixed index writer.",
        &sorted!([
            (
                "wid",
                false,
                &IntegerSchema::new("Fixed writer ID.")
                    .minimum(1)
                    .maximum(256)
                    .schema()
            ),
            (
                "chunk-count",
                false,
                &IntegerSchema::new("Chunk count. This is used to verify that the server got all chunks. Ignored for incremental backups.")
                    .minimum(0)
                    .schema()
            ),
            (
                "size",
                false,
                &IntegerSchema::new("File size. This is used to verify that the server got all data. Ignored for incremental backups.")
                    .minimum(0)
                    .schema()
            ),
            ("csum", false, &StringSchema::new("Digest list checksum.").schema()),
        ]),
    )
);

fn close_fixed_index (
    param: Value,
    _info: &ApiMethod,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<Value, Error> {

    let wid = required_integer_param(&param, "wid")? as usize;
    let chunk_count = required_integer_param(&param, "chunk-count")? as u64;
    let size = required_integer_param(&param, "size")? as u64;
    let csum_str = required_string_param(&param, "csum")?;
    let csum = proxmox::tools::hex_to_digest(csum_str)?;

    let env: &BackupEnvironment = rpcenv.as_ref();

    env.fixed_writer_close(wid, chunk_count, size, csum)?;

    env.log(format!("successfully closed fixed index {}", wid));

    Ok(Value::Null)
}

fn finish_backup (
    _param: Value,
    _info: &ApiMethod,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<Value, Error> {

    let env: &BackupEnvironment = rpcenv.as_ref();

    env.finish_backup()?;
    env.log("successfully finished backup");

    Ok(Value::Null)
}

#[sortable]
pub const API_METHOD_GET_PREVIOUS_BACKUP_TIME: ApiMethod = ApiMethod::new(
    &ApiHandler::Sync(&get_previous_backup_time),
    &ObjectSchema::new(
        "Get previous backup time.",
        &[],
    )
);

fn get_previous_backup_time(
    _param: Value,
    _info: &ApiMethod,
    rpcenv: &mut dyn RpcEnvironment,
) -> Result<Value, Error> {

    let env: &BackupEnvironment = rpcenv.as_ref();

    let backup_time = env.last_backup.as_ref().map(|info| info.backup_dir.backup_time());

    Ok(json!(backup_time))
}

#[sortable]
pub const API_METHOD_DOWNLOAD_PREVIOUS: ApiMethod = ApiMethod::new(
    &ApiHandler::AsyncHttp(&download_previous),
    &ObjectSchema::new(
        "Download archive from previous backup.",
        &sorted!([
            ("archive-name", false, &crate::api2::types::BACKUP_ARCHIVE_NAME_SCHEMA)
        ]),
    )
);

fn download_previous(
    _parts: Parts,
    _req_body: Body,
    param: Value,
    _info: &ApiMethod,
    rpcenv: Box<dyn RpcEnvironment>,
) -> ApiResponseFuture {

    async move {
        let env: &BackupEnvironment = rpcenv.as_ref();

        let archive_name = required_string_param(&param, "archive-name")?.to_owned();

        let last_backup = match &env.last_backup {
            Some(info) => info,
            None => bail!("no valid previous backup"),
        };

        let mut path = env.datastore.snapshot_path(&last_backup.backup_dir);
        path.push(&archive_name);

        {
            let index: Option<Box<dyn IndexFile>> = match archive_type(&archive_name)? {
                ArchiveType::FixedIndex => {
                    let index = env.datastore.open_fixed_reader(&path)?;
                    Some(Box::new(index))
                }
                ArchiveType::DynamicIndex => {
                    let index = env.datastore.open_dynamic_reader(&path)?;
                    Some(Box::new(index))
                }
                _ => { None }
            };
            if let Some(index) = index {
                env.log(format!("register chunks in '{}' from previous backup.", archive_name));

                for pos in 0..index.index_count() {
                    let info = index.chunk_info(pos).unwrap();
                    let size = info.range.end - info.range.start;
                    env.register_chunk(info.digest, size as u32)?;
                }
            }
        }

        env.log(format!("download '{}' from previous backup.", archive_name));
        crate::api2::helpers::create_download_response(path).await
    }.boxed()
}
