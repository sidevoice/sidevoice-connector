//! Records the connector keeps, host-agnostic: a value written whole as private JSON, and the digest of a file.

use crate::platform::fs::write_private;
use anyhow::Result;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// [`write_private`] of a value as JSON.
pub fn atomic_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    write_private(path, &serde_json::to_vec(value)?)
}

pub fn digest_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut block = [0u8; 65536];
    loop {
        let read = file.read(&mut block)?;
        if read == 0 {
            break;
        }
        hasher.update(&block[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}
