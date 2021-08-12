use serde::{Deserialize, Serialize};

use proxmox::api::api;
use proxmox::api::schema::{BooleanSchema, IntegerSchema, Schema, StringSchema};

use super::{SINGLE_LINE_COMMENT_FORMAT, SINGLE_LINE_COMMENT_SCHEMA};
use super::userid::{Authid, Userid, PROXMOX_TOKEN_ID_SCHEMA};

pub const ENABLE_USER_SCHEMA: Schema = BooleanSchema::new(
    "Enable the account (default). You can set this to '0' to disable the account.")
    .default(true)
    .schema();

pub const EXPIRE_USER_SCHEMA: Schema = IntegerSchema::new(
    "Account expiration date (seconds since epoch). '0' means no expiration date.")
    .default(0)
    .minimum(0)
    .schema();

pub const FIRST_NAME_SCHEMA: Schema = StringSchema::new("First name.")
    .format(&SINGLE_LINE_COMMENT_FORMAT)
    .min_length(2)
    .max_length(64)
    .schema();

pub const LAST_NAME_SCHEMA: Schema = StringSchema::new("Last name.")
    .format(&SINGLE_LINE_COMMENT_FORMAT)
    .min_length(2)
    .max_length(64)
    .schema();

pub const EMAIL_SCHEMA: Schema = StringSchema::new("E-Mail Address.")
    .format(&SINGLE_LINE_COMMENT_FORMAT)
    .min_length(2)
    .max_length(64)
    .schema();

#[api(
    properties: {
        userid: {
            type: Userid,
        },
        comment: {
            optional: true,
            schema: SINGLE_LINE_COMMENT_SCHEMA,
        },
        enable: {
            optional: true,
            schema: ENABLE_USER_SCHEMA,
        },
        expire: {
            optional: true,
            schema: EXPIRE_USER_SCHEMA,
        },
        firstname: {
            optional: true,
            schema: FIRST_NAME_SCHEMA,
        },
        lastname: {
            schema: LAST_NAME_SCHEMA,
            optional: true,
         },
        email: {
            schema: EMAIL_SCHEMA,
            optional: true,
        },
        tokens: {
            type: Array,
            optional: true,
            description: "List of user's API tokens.",
            items: {
                type: ApiToken
            },
        },
    }
)]
#[derive(Serialize,Deserialize)]
/// User properties with added list of ApiTokens
pub struct UserWithTokens {
    pub userid: Userid,
    #[serde(skip_serializing_if="Option::is_none")]
    pub comment: Option<String>,
    #[serde(skip_serializing_if="Option::is_none")]
    pub enable: Option<bool>,
    #[serde(skip_serializing_if="Option::is_none")]
    pub expire: Option<i64>,
    #[serde(skip_serializing_if="Option::is_none")]
    pub firstname: Option<String>,
    #[serde(skip_serializing_if="Option::is_none")]
    pub lastname: Option<String>,
    #[serde(skip_serializing_if="Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if="Vec::is_empty", default)]
    pub tokens: Vec<ApiToken>,
}

#[api(
    properties: {
        tokenid: {
            schema: PROXMOX_TOKEN_ID_SCHEMA,
        },
        comment: {
            optional: true,
            schema: SINGLE_LINE_COMMENT_SCHEMA,
        },
        enable: {
            optional: true,
            schema: ENABLE_USER_SCHEMA,
        },
        expire: {
            optional: true,
            schema: EXPIRE_USER_SCHEMA,
        },
    }
)]
#[derive(Serialize,Deserialize)]
/// ApiToken properties.
pub struct ApiToken {
    pub tokenid: Authid,
    #[serde(skip_serializing_if="Option::is_none")]
    pub comment: Option<String>,
    #[serde(skip_serializing_if="Option::is_none")]
    pub enable: Option<bool>,
    #[serde(skip_serializing_if="Option::is_none")]
    pub expire: Option<i64>,
}

impl ApiToken {
    pub fn is_active(&self) -> bool {
        if !self.enable.unwrap_or(true) {
            return false;
        }
        if let Some(expire) = self.expire {
            let now =  proxmox::tools::time::epoch_i64();
            if expire > 0 && expire <= now {
                return false;
            }
        }
        true
    }
}

#[api(
    properties: {
        userid: {
            type: Userid,
        },
        comment: {
            optional: true,
            schema: SINGLE_LINE_COMMENT_SCHEMA,
        },
        enable: {
            optional: true,
            schema: ENABLE_USER_SCHEMA,
        },
        expire: {
            optional: true,
            schema: EXPIRE_USER_SCHEMA,
        },
        firstname: {
            optional: true,
            schema: FIRST_NAME_SCHEMA,
        },
        lastname: {
            schema: LAST_NAME_SCHEMA,
            optional: true,
         },
        email: {
            schema: EMAIL_SCHEMA,
            optional: true,
        },
    }
)]
#[derive(Serialize,Deserialize)]
/// User properties.
pub struct User {
    pub userid: Userid,
    #[serde(skip_serializing_if="Option::is_none")]
    pub comment: Option<String>,
    #[serde(skip_serializing_if="Option::is_none")]
    pub enable: Option<bool>,
    #[serde(skip_serializing_if="Option::is_none")]
    pub expire: Option<i64>,
    #[serde(skip_serializing_if="Option::is_none")]
    pub firstname: Option<String>,
    #[serde(skip_serializing_if="Option::is_none")]
    pub lastname: Option<String>,
    #[serde(skip_serializing_if="Option::is_none")]
    pub email: Option<String>,
}

impl User {
    pub fn is_active(&self) -> bool {
        if !self.enable.unwrap_or(true) {
            return false;
        }
        if let Some(expire) = self.expire {
            let now =  proxmox::tools::time::epoch_i64();
            if expire > 0 && expire <= now {
                return false;
            }
        }
        true
    }
}
