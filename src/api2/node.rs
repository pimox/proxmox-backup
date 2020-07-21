use proxmox::api::router::{Router, SubdirMap};
use proxmox::list_subdirs_api_method;

pub mod tasks;
mod time;
pub mod network;
pub mod dns;
mod syslog;
mod journal;
mod services;
mod status;
mod subscription;
pub(crate) mod rrd;
pub mod disks;
mod apt;

pub const SUBDIRS: SubdirMap = &[
    ("apt", &apt::ROUTER),
    ("disks", &disks::ROUTER),
    ("dns", &dns::ROUTER),
    ("journal", &journal::ROUTER),
    ("network", &network::ROUTER),
    ("rrd", &rrd::ROUTER),
    ("services", &services::ROUTER),
    ("status", &status::ROUTER),
    ("subscription", &subscription::ROUTER),
    ("syslog", &syslog::ROUTER),
    ("tasks", &tasks::ROUTER),
    ("time", &time::ROUTER),
];

pub const ROUTER: Router = Router::new()
    .get(&list_subdirs_api_method!(SUBDIRS))
    .subdirs(SUBDIRS);
