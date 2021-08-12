//! Cached user info for fast ACL permission checks

use std::sync::{RwLock, Arc};

use anyhow::{Error, bail};

use proxmox::api::section_config::SectionConfigData;
use lazy_static::lazy_static;
use proxmox::api::UserInformation;
use proxmox::tools::time::epoch_i64;

use super::acl::{AclTree, ROLE_NAMES, ROLE_ADMIN};
use super::user::{ApiToken, User};
use crate::api2::types::{Authid, Userid};
use crate::tools::Memcom;

/// Cache User/Group/Token/Acl configuration data for fast permission tests
pub struct CachedUserInfo {
    user_cfg: Arc<SectionConfigData>,
    acl_tree: Arc<AclTree>,
}

struct ConfigCache {
    data: Option<Arc<CachedUserInfo>>,
    last_update: i64,
    last_user_cache_generation: usize,
}

lazy_static! {
    static ref CACHED_CONFIG: RwLock<ConfigCache> = RwLock::new(
        ConfigCache { data: None, last_update: 0, last_user_cache_generation: 0 }
    );
}

impl CachedUserInfo {

    /// Returns a cached instance (up to 5 seconds old).
    pub fn new() -> Result<Arc<Self>, Error> {
        let now = epoch_i64();

        let memcom = Memcom::new()?;
        let user_cache_generation = memcom.user_cache_generation();

        { // limit scope
            let cache = CACHED_CONFIG.read().unwrap();
            if (user_cache_generation == cache.last_user_cache_generation) &&
                ((now - cache.last_update) < 5)
            {
                if let Some(ref config) = cache.data {
                    return Ok(config.clone());
                }
            }
        }

        let config = Arc::new(CachedUserInfo {
            user_cfg: super::user::cached_config()?,
            acl_tree: super::acl::cached_config()?,
        });

        let mut cache = CACHED_CONFIG.write().unwrap();
        cache.last_update = now;
        cache.last_user_cache_generation = user_cache_generation;
        cache.data = Some(config.clone());

        Ok(config)
    }

    #[cfg(test)]
    pub(crate) fn test_new(user_cfg: SectionConfigData, acl_tree: AclTree) -> Self {
        Self {
            user_cfg: Arc::new(user_cfg),
            acl_tree: Arc::new(acl_tree),
        }
    }

    /// Test if a user_id is enabled and not expired
    pub fn is_active_user_id(&self, userid: &Userid) -> bool {
        if let Ok(info) = self.user_cfg.lookup::<User>("user", userid.as_str()) {
            info.is_active()
        } else {
            false
        }
    }

    /// Test if a authentication id is enabled and not expired
    pub fn is_active_auth_id(&self, auth_id: &Authid) -> bool {
        let userid = auth_id.user();

        if !self.is_active_user_id(userid) {
            return false;
        }

        if auth_id.is_token() {
            if let Ok(info) = self.user_cfg.lookup::<ApiToken>("token", &auth_id.to_string()) {
                return info.is_active();
            } else {
                return false;
            }
        }

        true
    }

    pub fn check_privs(
        &self,
        auth_id: &Authid,
        path: &[&str],
        required_privs: u64,
        partial: bool,
    ) -> Result<(), Error> {
        let privs = self.lookup_privs(&auth_id, path);
        let allowed = if partial {
            (privs & required_privs) != 0
        } else {
            (privs & required_privs) == required_privs
        };
        if !allowed {
            // printing the path doesn't leaks any information as long as we
            // always check privilege before resource existence
            bail!("no permissions on '/{}'", path.join("/"));
        }
        Ok(())
    }

    pub fn is_superuser(&self, auth_id: &Authid) -> bool {
        !auth_id.is_token() && auth_id.user() == "root@pam"
    }

    pub fn is_group_member(&self, _userid: &Userid, _group: &str) -> bool {
        false
    }

    pub fn lookup_privs(&self, auth_id: &Authid, path: &[&str]) -> u64 {
        let (privs, _) = self.lookup_privs_details(auth_id, path);
        privs
    }

    pub fn lookup_privs_details(&self, auth_id: &Authid, path: &[&str]) -> (u64, u64) {
        if self.is_superuser(auth_id) {
            return (ROLE_ADMIN, ROLE_ADMIN);
        }

        let roles = self.acl_tree.roles(auth_id, path);
        let mut privs: u64 = 0;
        let mut propagated_privs: u64 = 0;
        for (role, propagate) in roles {
            if let Some((role_privs, _)) = ROLE_NAMES.get(role.as_str()) {
                if propagate {
                    propagated_privs |= role_privs;
                }
                privs |= role_privs;
            }
        }

        if auth_id.is_token() {
            // limit privs to that of owning user
            let user_auth_id = Authid::from(auth_id.user().clone());
            privs &= self.lookup_privs(&user_auth_id, path);
            let (owner_privs, owner_propagated_privs) = self.lookup_privs_details(&user_auth_id, path);
            privs &= owner_privs;
            propagated_privs &= owner_propagated_privs;
        }

        (privs, propagated_privs)
    }

}

impl UserInformation for CachedUserInfo {
    fn is_superuser(&self, userid: &str) -> bool {
        userid == "root@pam"
    }

    fn is_group_member(&self, _userid: &str, _group: &str) -> bool {
        false
    }

    fn lookup_privs(&self, auth_id: &str, path: &[&str]) -> u64 {
        match auth_id.parse::<Authid>() {
            Ok(auth_id) => Self::lookup_privs(self, &auth_id, path),
            Err(_) => 0,
        }
    }
}
