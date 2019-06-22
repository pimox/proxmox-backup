// WARNING: PLEASE DO NOT MODIFY THOSE MAGIC VALUES

// openssl::sha::sha256(b"Proxmox Backup uncompressed chunk v1.0")[0..8]
pub static UNCOMPRESSED_CHUNK_MAGIC_1_0: [u8; 8] = [79, 127, 200, 4, 121, 74, 135, 239];

// openssl::sha::sha256(b"Proxmox Backup encrypted chunk v1.0")[0..8]
pub static ENCRYPTED_CHUNK_MAGIC_1_0: [u8; 8] = [8, 54, 114, 153, 70, 156, 26, 151];

// openssl::sha::sha256(b"Proxmox Backup zstd compressed chunk v1.0")[0..8]
pub static COMPRESSED_CHUNK_MAGIC_1_0: [u8; 8] = [191, 237, 46, 195, 108, 17, 228, 235];

// openssl::sha::sha256(b"Proxmox Backup zstd compressed encrypted chunk v1.0")[0..8]
pub static ENCR_COMPR_CHUNK_MAGIC_1_0: [u8; 8] = [9, 40, 53, 200, 37, 150, 90, 196];

// openssl::sha::sha256(b"Proxmox Backup uncompressed blob v1.0")[0..8]
pub static UNCOMPRESSED_BLOB_MAGIC_1_0: [u8; 8] = [66, 171, 56, 7, 190, 131, 112, 161];

//openssl::sha::sha256(b"Proxmox Backup zstd compressed blob v1.0")[0..8]
pub static COMPRESSED_BLOB_MAGIC_1_0: [u8; 8] = [49, 185, 88, 66, 111, 182, 163, 127];

// openssl::sha::sha256(b"Proxmox Backup encrypted blob v1.0")[0..8]
pub static ENCRYPTED_BLOB_MAGIC_1_0: [u8; 8] = [123, 103, 133, 190, 34, 45, 76, 240];

// openssl::sha::sha256(b"Proxmox Backup zstd compressed encrypted blob v1.0")[0..8]
pub static ENCR_COMPR_BLOB_MAGIC_1_0: [u8; 8] = [230, 89, 27, 191, 11, 191, 216, 11];

// openssl::sha::sha256(b"Proxmox Backup fixed sized chunk index v1.0")[0..8]
pub static FIXED_SIZED_CHUNK_INDEX_1_0: [u8; 8] = [47, 127, 65, 237, 145, 253, 15, 205];

// openssl::sha::sha256(b"Proxmox Backup dynamic sized chunk index v1.0")[0..8]
pub static DYNAMIC_SIZED_CHUNK_INDEX_1_0: [u8; 8] = [28, 145, 78, 165, 25, 186, 179, 205];

#[repr(C,packed)]
pub struct DataBlobHeader {
    pub magic: [u8; 8],
    pub crc: [u8; 4],
}

#[repr(C,packed)]
pub struct EncryptedDataBlobHeader {
    pub head: DataBlobHeader,
    pub iv: [u8; 16],
    pub tag: [u8; 16],
}

#[repr(C,packed)]
pub struct DataChunkHeader {
    pub magic: [u8; 8],
    pub crc: [u8; 4],
}

#[repr(C,packed)]
pub struct EncryptedDataChunkHeader {
    pub head: DataChunkHeader,
    pub iv: [u8; 16],
    pub tag: [u8; 16],
}
