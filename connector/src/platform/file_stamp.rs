//! File identity for invalidating caches, including replacements that preserve size and mtime.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[cfg(unix)]
#[path = "file_stamp/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "file_stamp/windows.rs"]
mod imp;
pub use imp::Os as FileIdentity;

/// The identity/change fields the host adds to a file's portable metadata.
pub trait FileIdentities {
    /// Returns host identity or change-time fields to supplement size and mtime.
    fn change(metadata: &fs::Metadata) -> [u64; 4];
}

#[derive(Clone, PartialEq, Eq)]
pub struct Stamp {
    path: PathBuf,
    len: u64,
    modified: SystemTime,
    change: [u64; 4],
}

impl Stamp {
    /// Canonical path and metadata of the current target. An unreadable file cannot be cached.
    pub fn read(path: &Path) -> io::Result<Self> {
        let path = fs::canonicalize(path)?;
        let metadata = fs::metadata(&path)?;
        Ok(Self {
            path,
            len: metadata.len(),
            modified: metadata.modified()?,
            change: FileIdentity::change(&metadata),
        })
    }
}
