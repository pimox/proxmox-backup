use anyhow::Error;
use serde_json::Value;

use proxmox::api::{api, cli::*, RpcEnvironment, ApiHandler};

use proxmox_backup::config;
use proxmox_backup::tools;
use proxmox_backup::api2;
use proxmox_backup::api2::types::Userid;

#[api(
    input: {
        properties: {
            "output-format": {
                schema: OUTPUT_FORMAT,
                optional: true,
            },
        }
    }
)]
/// List configured users.
fn list_users(param: Value, rpcenv: &mut dyn RpcEnvironment) -> Result<Value, Error> {

    let output_format = get_output_format(&param);

    let info = &api2::access::user::API_METHOD_LIST_USERS;
    let mut data = match info.handler {
        ApiHandler::Sync(handler) => (handler)(param, info, rpcenv)?,
        _ => unreachable!(),
    };

    let options = default_table_format_options()
        .column(ColumnConfig::new("userid"))
        .column(
            ColumnConfig::new("enable")
                .renderer(tools::format::render_bool_with_default_true)
        )
        .column(
            ColumnConfig::new("expire")
                .renderer(tools::format::render_epoch)
        )
        .column(ColumnConfig::new("firstname"))
        .column(ColumnConfig::new("lastname"))
        .column(ColumnConfig::new("email"))
        .column(ColumnConfig::new("comment"));

    format_and_print_result_full(&mut data, info.returns, &output_format, &options);

    Ok(Value::Null)
}

#[api(
    input: {
        properties: {
            "output-format": {
                schema: OUTPUT_FORMAT,
                optional: true,
            },
            userid: {
                type: Userid,
            }
        }
    }
)]
/// List tokens associated with user.
fn list_tokens(param: Value, rpcenv: &mut dyn RpcEnvironment) -> Result<Value, Error> {

    let output_format = get_output_format(&param);

    let info = &api2::access::user::API_METHOD_LIST_TOKENS;
    let mut data = match info.handler {
        ApiHandler::Sync(handler) => (handler)(param, info, rpcenv)?,
        _ => unreachable!(),
    };

    let options = default_table_format_options()
        .column(ColumnConfig::new("tokenid"))
        .column(
            ColumnConfig::new("enable")
                .renderer(tools::format::render_bool_with_default_true)
        )
        .column(
            ColumnConfig::new("expire")
                .renderer(tools::format::render_epoch)
        )
        .column(ColumnConfig::new("comment"));

    format_and_print_result_full(&mut data, info.returns, &output_format, &options);

    Ok(Value::Null)
}


pub fn user_commands() -> CommandLineInterface {

    let cmd_def = CliCommandMap::new()
        .insert("list", CliCommand::new(&&API_METHOD_LIST_USERS))
        .insert(
            "create",
            // fixme: howto handle password parameter?
            CliCommand::new(&api2::access::user::API_METHOD_CREATE_USER)
                .arg_param(&["userid"])
        )
        .insert(
            "update",
            CliCommand::new(&api2::access::user::API_METHOD_UPDATE_USER)
                .arg_param(&["userid"])
                .completion_cb("userid", config::user::complete_userid)
        )
        .insert(
            "remove",
            CliCommand::new(&api2::access::user::API_METHOD_DELETE_USER)
                .arg_param(&["userid"])
                .completion_cb("userid", config::user::complete_userid)
        )
        .insert(
            "list-tokens",
            CliCommand::new(&&API_METHOD_LIST_TOKENS)
                .arg_param(&["userid"])
                .completion_cb("userid", config::user::complete_userid)
        )
        .insert(
            "generate-token",
            CliCommand::new(&api2::access::user::API_METHOD_GENERATE_TOKEN)
                .arg_param(&["userid", "tokenname"])
                .completion_cb("userid", config::user::complete_userid)
        )
        .insert(
            "delete-token",
            CliCommand::new(&api2::access::user::API_METHOD_DELETE_TOKEN)
                .arg_param(&["userid", "tokenname"])
                .completion_cb("userid", config::user::complete_userid)
                .completion_cb("tokenname", config::user::complete_token_name)
        );

    cmd_def.into()
}
