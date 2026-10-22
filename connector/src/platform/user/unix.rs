//! Unix: the effective uid, from `rustix` (no `unsafe` of ours), compared with the file's owner.

use std::fs::{File, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// The user as its effective uid.
#[derive(Debug)]
pub struct Id(u32);

impl super::User for Id {
    fn current() -> Id {
        Id(rustix::process::geteuid().as_raw())
    }

    // A symlink is judged itself (`symlink_metadata`).
    fn owns_path(&self, path: &Path) -> io::Result<bool> {
        Ok(self.owns(&std::fs::symlink_metadata(path)?))
    }

    fn owns_file(&self, file: &File) -> io::Result<bool> {
        Ok(self.owns(&file.metadata()?))
    }
}

impl Id {
    fn owns(&self, metadata: &Metadata) -> bool {
        metadata.uid() == self.0
    }
}
