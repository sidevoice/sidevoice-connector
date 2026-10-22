//! This process's user: who it is, whether a file is theirs, what the host is called.
//!
//! The host-agnostic face lives here; what differs per OS is one submodule, chosen once below. Unix: the effective
//! uid, from `rustix`, compared with the file's owner. Windows: the user's SID, read from the process token,
//! compared (`EqualSid`) with the owner of the file's security descriptor.

#[cfg(unix)]
#[path = "user/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "user/windows.rs"]
mod imp;

use std::fs::File;
use std::io;
use std::path::Path;

/// What every OS implements (`imp::Id` in `user/unix.rs` and `user/windows.rs`). The public functions below are
/// this, documented for callers; nothing else is OS-specific.
trait User: Sized {
    /// The user this process runs as (Unix: the effective uid; Windows: the SID in its token).
    fn current() -> Self;
    /// Whether this user owns what `path` names, the path itself and not what a link points to.
    fn owns_path(&self, path: &Path) -> io::Result<bool>;
    /// Whether this user owns the file behind an open handle.
    fn owns_file(&self, file: &File) -> io::Result<bool>;
}

/// The user this process runs as. Opaque: callers ask it questions and never see what it is made of.
#[derive(Debug)]
pub struct UserId(imp::Id);

/// The user this process runs as (Unix: the effective uid; Windows: the SID in its token).
pub fn current() -> UserId {
    UserId(<imp::Id as User>::current())
}

impl UserId {
    #[allow(dead_code)] // its callers arrive with `platform::fs`
    /// Whether this user owns what `path` names. A symlink (or Windows junction) is judged itself, not what it
    /// points to.
    pub fn owns_path(&self, path: &Path) -> io::Result<bool> {
        self.0.owns_path(path)
    }

    #[allow(dead_code)] // as `owns_path`
    /// Whether this user owns the file behind an open handle.
    pub fn owns_file(&self, file: &File) -> io::Result<bool> {
        self.0.owns_file(file)
    }
}

/// This user's login name, for what must name the user to the system (`loginctl enable-linger`). `None` when
/// the system cannot tell. Same code on every OS (`whoami`).
pub fn name() -> Option<String> {
    whoami::username().ok()
}

/// The host name as the operating system reports it, trimmed; empty when it cannot be read.
pub fn host_name() -> String {
    gethostname::gethostname()
        .to_string_lossy()
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owns_what_it_creates() {
        let dir = std::env::temp_dir().join(format!("sv-platform-user-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(current().owns_path(&dir).unwrap());
        let file = std::fs::File::create(dir.join("f")).unwrap();
        assert!(current().owns_file(&file).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn owns_path_reports_a_missing_path() {
        let missing =
            std::env::temp_dir().join(format!("sv-platform-missing-{}", std::process::id()));
        assert!(current().owns_path(&missing).is_err());
    }

    #[test]
    fn name_is_known() {
        assert!(name().is_some_and(|name| !name.is_empty()));
    }

    #[test]
    fn host_name_is_trimmed() {
        let name = host_name();
        assert_eq!(name, name.trim());
    }
}
