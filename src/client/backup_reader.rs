use failure::*;
use std::io::Write;
use std::sync::Arc;
use std::os::unix::fs::OpenOptionsExt;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use proxmox::tools::digest_to_hex;

use crate::tools::futures::Canceller;
use crate::backup::*;

use super::{HttpClient, H2Client};

/// Backup Reader
pub struct BackupReader {
    h2: H2Client,
    canceller: Canceller,
    crypt_config: Option<Arc<CryptConfig>>,
}

impl Drop for BackupReader {

    fn drop(&mut self) {
        self.canceller.cancel();
    }
}

impl BackupReader {

    fn new(h2: H2Client, canceller: Canceller, crypt_config: Option<Arc<CryptConfig>>) -> Arc<Self> {
        Arc::new(Self { h2, canceller, crypt_config})
    }

    /// Create a new instance by upgrading the connection at '/api2/json/reader'
    pub async fn start(
        client: HttpClient,
        crypt_config: Option<Arc<CryptConfig>>,
        datastore: &str,
        backup_type: &str,
        backup_id: &str,
        backup_time: DateTime<Utc>,
        debug: bool,
    ) -> Result<Arc<BackupReader>, Error> {

        let param = json!({
            "backup-type": backup_type,
            "backup-id": backup_id,
            "backup-time": backup_time.timestamp(),
            "store": datastore,
            "debug": debug,
        });
        let req = HttpClient::request_builder(client.server(), "GET", "/api2/json/reader", Some(param)).unwrap();

        let (h2, canceller) = client.start_h2_connection(req, String::from(PROXMOX_BACKUP_READER_PROTOCOL_ID_V1!())).await?;

        Ok(BackupReader::new(h2, canceller, crypt_config))
    }

    /// Execute a GET request
    pub async fn get(
        &self,
        path: &str,
        param: Option<Value>,
    ) -> Result<Value, Error> {
        self.h2.get(path, param).await
    }

    /// Execute a PUT request
    pub async fn put(
        &self,
        path: &str,
        param: Option<Value>,
    ) -> Result<Value, Error> {
        self.h2.put(path, param).await
    }

    /// Execute a POST request
    pub async fn post(
        &self,
        path: &str,
        param: Option<Value>,
    ) -> Result<Value, Error> {
        self.h2.post(path, param).await
    }

    /// Execute a GET request and send output to a writer
    pub async fn download<W: Write + Send>(
        &self,
        file_name: &str,
        output: W,
    ) -> Result<W, Error> {
        let path = "download";
        let param = json!({ "file-name": file_name });
        self.h2.download(path, Some(param), output).await
    }

    /// Execute a special GET request and send output to a writer
    ///
    /// This writes random data, and is only useful to test download speed.
    pub async fn speedtest<W: Write + Send>(
        &self,
        output: W,
    ) -> Result<W, Error> {
        self.h2.download("speedtest", None, output).await
    }

    /// Download a specific chunk
    pub async fn download_chunk<W: Write + Send>(
        &self,
        digest: &[u8; 32],
        output: W,
    ) -> Result<W, Error> {
        let path = "chunk";
        let param = json!({ "digest": digest_to_hex(digest) });
        self.h2.download(path, Some(param), output).await
    }

    pub fn force_close(self) {
        self.canceller.cancel();
    }

    /// Download backup manifest (index.json)
    pub async fn download_manifest(&self) -> Result<BackupManifest, Error> {

        use std::convert::TryFrom;

        let raw_data = self.download(MANIFEST_BLOB_NAME, Vec::with_capacity(64*1024)).await?;
        let blob = DataBlob::from_raw(raw_data)?;
        blob.verify_crc()?;
        let data = blob.decode(self.crypt_config.as_ref().map(Arc::as_ref))?;
        let json: Value = serde_json::from_slice(&data[..])?;

        BackupManifest::try_from(json)
    }

    /// Download dynamic index file
    ///
    /// This creates a temorary file in /tmp (using O_TMPFILE). The index is verified using
    /// the provided manifest.
    pub async fn download_dynamic_index(
        &self,
        manifest: &BackupManifest,
        name: &str,
    ) -> Result<DynamicIndexReader, Error> {

        let tmpfile = std::fs::OpenOptions::new()
            .write(true)
            .read(true)
            .custom_flags(libc::O_TMPFILE)
            .open("/tmp")?;

        let tmpfile = self.download(name, tmpfile).await?;

        let index = DynamicIndexReader::new(tmpfile)
            .map_err(|err| format_err!("unable to read dynamic index '{}' - {}", name, err))?;

        // Note: do not use values stored in index (not trusted) - instead, computed them again
        let (csum, size) = index.compute_csum();
        manifest.verify_file(name, &csum, size)?;

        Ok(index)
    }
}
