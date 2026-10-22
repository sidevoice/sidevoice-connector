//! Unix: the effective uid, from `rustix` (no `unsafe` of ours), compared with the file's owner.

// The raw calls the rest of the crate is not allowed to make (`clippy.toml`) are made here.
#![allow(clippy::disallowed_methods)]

use std::fs::{File, Metadata};
use std::io;
use std::path::Path;

/// Unix side of [`super::UserId`]: the effective uid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Id(u32);

/// The effective user of this process.
pub(super) fn current() -> Id {
    Id(rustix::process::geteuid().as_raw())
}

impl Id {
    // A symlink is judged itself (`symlink_metadata`).
    pub(super) fn owns_path(&self, path: &Path) -> io::Result<bool> {
        Ok(self.owns(&std::fs::symlink_metadata(path)?))
    }

    pub(super) fn owns_file(&self, file: &File) -> io::Result<bool> {
        Ok(self.owns(&file.metadata()?))
    }

    fn owns(&self, metadata: &Metadata) -> bool {
        use std::os::unix::fs::MetadataExt;
        metadata.uid() == self.0
    }
}

/// This user's login name, for what must name the user to the system (`loginctl enable-linger`). `None` when the
/// system has no name for it.
pub(super) fn name() -> Option<String> {
    let entry = unsafe { libc::getpwuid(libc::geteuid()) };
    if entry.is_null() {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr((*entry).pw_name) };
    name.to_str().ok().map(str::to_owned)
}
