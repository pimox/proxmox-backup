pub use pbs_api_types::upid::UPID;

pub trait UPIDExt: private::Sealed {
    /// Returns the absolute path to the task log file
    fn log_path(&self) -> std::path::PathBuf;
}

mod private {
    pub trait Sealed {}
    impl Sealed for super::UPID {}
}

impl UPIDExt for UPID {
    fn log_path(&self) -> std::path::PathBuf {
        let mut path = std::path::PathBuf::from(super::PROXMOX_BACKUP_TASK_DIR);
        path.push(format!("{:02X}", self.pstart % 256));
        path.push(self.to_string());
        path
    }
}
