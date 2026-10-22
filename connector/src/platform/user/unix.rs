//! Unix: the effective uid, from `rustix` (no `unsafe` of ours), compared with the file's owner.

// The raw calls the rest of the crate is not allowed to make (`clippy.toml`) are made here.
#![allow(clippy::disallowed_methods)]

use std::fs::{File, Metadata};
use std::io;
use std::path::Path;

/// The user this process runs as. Opaque: callers ask it questions and never see what it is made of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserId(u32);

/// The effective user of this process.
pub fn current() -> UserId {
    UserId(rustix::process::geteuid().as_raw())
}

impl UserId {
    #[allow(dead_code)] // its callers arrive with `platform::fs`, which replaces the `owns(&Metadata)` ones
    /// Whether this user owns what `path` names. A symlink is judged itself, not what it points to.
    pub fn owns_path(&self, path: &Path) -> io::Result<bool> {
        Ok(self.owns(&std::fs::symlink_metadata(path)?))
    }

    #[allow(dead_code)] // as `owns_path`
    /// Whether this user owns the file behind an open handle.
    pub fn owns_file(&self, file: &File) -> io::Result<bool> {
        Ok(self.owns(&file.metadata()?))
    }

    /// Whether this user owns the file `metadata` describes. Unix-only: `Metadata` carries no owner on Windows, so
    /// host-agnostic callers use [`UserId::owns_path`] or [`UserId::owns_file`].
    pub fn owns(&self, metadata: &Metadata) -> bool {
        use std::os::unix::fs::MetadataExt;
        metadata.uid() == self.0
    }

    /// The numeric uid, for the places that must put it in a Unix-shaped value: `ps`'s uid column, a `stat` owner,
    /// launchd's `gui/<uid>` domain. Nothing host-agnostic should need it.
    pub(crate) fn raw(&self) -> u32 {
        self.0
    }
}

/// This user's login name, for what must name the user to the system (`loginctl enable-linger`). `None` when the
/// system has no name for it.
pub fn name() -> Option<String> {
    let entry = unsafe { libc::getpwuid(libc::geteuid()) };
    if entry.is_null() {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr((*entry).pw_name) };
    name.to_str().ok().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_is_the_effective_uid() {
        assert_eq!(current().raw(), unsafe { libc::geteuid() });
    }

    #[test]
    fn owns_metadata_of_what_it_creates() {
        let dir =
            std::env::temp_dir().join(format!("sv-platform-user-unix-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(current().owns(&std::fs::metadata(&dir).unwrap()));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
