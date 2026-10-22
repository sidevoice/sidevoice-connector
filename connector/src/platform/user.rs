//! This process's user: who it is, whether a file is theirs, what the host is called.
//!
//! Unix: the effective uid, from `rustix` (no `unsafe` of ours), compared with the file's owner. Windows will be
//! the user's SID, read from the process token, compared with the owner of the file's security descriptor.

// The raw calls the rest of the crate is not allowed to make (`clippy.toml`) are made here.
#![allow(clippy::disallowed_methods)]

use std::fs::Metadata;

/// The user this process runs as. Opaque: callers ask it questions and never see what it is made of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserId(u32);

/// The effective user of this process.
pub fn current() -> UserId {
    UserId(rustix::process::geteuid().as_raw())
}

impl UserId {
    /// Whether this user owns the file `metadata` describes.
    pub fn owns(self, metadata: &Metadata) -> bool {
        use std::os::unix::fs::MetadataExt;
        metadata.uid() == self.0
    }

    /// The numeric uid, for the places that must put it in a Unix-shaped value: `ps`'s uid column, a `stat` owner,
    /// launchd's `gui/<uid>` domain. Nothing host-agnostic should need it.
    pub(crate) fn raw(self) -> u32 {
        self.0
    }
}

/// The host name as the operating system reports it, trimmed; empty when it cannot be read.
pub fn host_name() -> String {
    gethostname::gethostname()
        .to_string_lossy()
        .trim()
        .to_owned()
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
    fn owns_what_it_creates() {
        let dir = std::env::temp_dir().join(format!("sv-platform-user-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(current().owns(&std::fs::metadata(&dir).unwrap()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn raw_is_the_effective_uid() {
        assert_eq!(current().raw(), unsafe { libc::geteuid() });
    }

    #[test]
    fn host_name_is_trimmed() {
        let name = host_name();
        assert_eq!(name, name.trim());
    }
}
