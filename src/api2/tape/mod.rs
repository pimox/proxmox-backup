use proxmox::api::router::SubdirMap;
use proxmox::api::Router;
use proxmox::list_subdirs_api_method;

pub mod drive;
pub mod changer;

pub const SUBDIRS: SubdirMap = &[
    ("changer", &changer::ROUTER),
    ("drive", &drive::ROUTER),
];

pub const ROUTER: Router = Router::new()
    .get(&list_subdirs_api_method!(SUBDIRS))
    .subdirs(SUBDIRS);