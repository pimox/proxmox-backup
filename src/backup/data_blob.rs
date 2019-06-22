use failure::*;
use std::convert::TryInto;

use crate::tools::write::*;
use super::*;

/// Data blob binary storage format
///
/// Data blobs store arbitrary binary data (< 16MB), and can be
/// compressed and encrypted. A simply binary format is used to store
/// them on disk or transfer them over the network. Please use index
/// files to store large data files (".fidx" of ".didx").
///
/// The format start with a 8 byte magic number to identify the type,
/// followed by a 4 byte CRC. This CRC is used on the server side to
/// detect file corruption (computed when upload data), so there is
/// usually no need to compute it on the client side.
///
/// Encrypted blobs contain a 16 byte IV, followed by a 16 byte AD
/// tag, followed by the encrypted data:
///
/// (MAGIC || CRC32 || IV || TAG || EncryptedData).
///
/// Unencrypted blobs simply contain the CRC, followed by the
/// (compressed) data.
///
/// (MAGIC || CRC32 || Data)
///
/// This is basically the same format we use for ``DataChunk``, but
/// with other magic numbers so that we can distinguish them.

pub struct DataBlob {
    raw_data: Vec<u8>, // tagged, compressed, encryped data
}

impl DataBlob {

    /// accessor to raw_data field
    pub fn raw_data(&self) -> &[u8]  {
        &self.raw_data
    }

    /// accessor to chunk type (magic number)
    pub fn magic(&self) -> &[u8; 8] {
        self.raw_data[0..8].try_into().unwrap()
    }

    /// accessor to crc32 checksum
    pub fn crc(&self) -> u32 {
        let crc_o = proxmox::tools::offsetof!(DataBlobHeader, crc);
        u32::from_le_bytes(self.raw_data[crc_o..crc_o+4].try_into().unwrap())
    }

    // set the CRC checksum field
    pub fn set_crc(&mut self, crc: u32) {
        let crc_o = proxmox::tools::offsetof!(DataBlobHeader, crc);
        self.raw_data[crc_o..crc_o+4].copy_from_slice(&crc.to_le_bytes());
    }

    /// compute the CRC32 checksum
    pub fn compute_crc(&mut self) -> u32 {
        let mut hasher = crc32fast::Hasher::new();
        let start = std::mem::size_of::<DataBlobHeader>(); // start after HEAD
        hasher.update(&self.raw_data[start..]);
        hasher.finalize()
    }

    pub fn encode(
        data: &[u8],
        config: Option<&CryptConfig>,
        compress: bool,
    ) -> Result<Self, Error> {

        if data.len() > 16*1024*1024 {
            bail!("data blob too large ({} bytes).", data.len());
        }

        if let Some(config) = config {

            let enc_data = config.encode_chunk(
                data,
                compress,
                &ENCRYPTED_BLOB_MAGIC_1_0,
                &ENCR_COMPR_BLOB_MAGIC_1_0,
            )?;
            return Ok(DataBlob { raw_data: enc_data });
        } else {

            let max_data_len = data.len() + std::mem::size_of::<DataBlobHeader>();
            if compress {
                let mut comp_data = Vec::with_capacity(max_data_len);

                let head =  DataBlobHeader {
                    magic: COMPRESSED_BLOB_MAGIC_1_0,
                    crc: [0; 4],
                };
                comp_data.write_value(&head)?;

                zstd::stream::copy_encode(data, &mut comp_data, 1)?;

                if comp_data.len() < max_data_len {
                    return Ok(DataBlob { raw_data: comp_data });
                }
            }

            let mut raw_data = Vec::with_capacity(max_data_len);

            let head =  DataBlobHeader {
                magic: UNCOMPRESSED_BLOB_MAGIC_1_0,
                crc: [0; 4],
            };
            raw_data.write_value(&head)?;
            raw_data.extend_from_slice(data);

            return Ok(DataBlob { raw_data });
        }
    }

    /// Decode blob data
    pub fn decode(self, config: Option<&CryptConfig>) -> Result<Vec<u8>, Error> {

        let magic = self.magic();

        if magic == &UNCOMPRESSED_BLOB_MAGIC_1_0 {
            let data_start = std::mem::size_of::<DataBlobHeader>();
            return Ok(self.raw_data[data_start..].to_vec());
        } else if magic == &COMPRESSED_BLOB_MAGIC_1_0 {
            let data_start = std::mem::size_of::<DataBlobHeader>();
            let data = zstd::block::decompress(&self.raw_data[data_start..], 16*1024*1024)?;
            return Ok(data);
        } else if magic == &ENCR_COMPR_BLOB_MAGIC_1_0 || magic == &ENCRYPTED_BLOB_MAGIC_1_0 {
            if let Some(config) = config  {
                let data = if magic == &ENCR_COMPR_BLOB_MAGIC_1_0 {
                    config.decode_compressed_chunk(&self.raw_data)?
                } else {
                    config.decode_uncompressed_chunk(&self.raw_data)?
                };
                return Ok(data);
            } else {
                bail!("unable to decrypt blob - missing CryptConfig");
            }
        } else {
            bail!("Invalid blob magic number.");
        }
    }
}
