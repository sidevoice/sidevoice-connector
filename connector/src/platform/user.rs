//! This process's user: who it is, whether a file is theirs, what the host is called.
//!
//! The interface is the [`User`] trait; [`UserId`] is the one OS implementation compiled in, chosen below. Unix:
//! the effective uid, from `rustix`, compared with the file's owner. Windows: the user's SID, read from the process
//! token, compared (`EqualSid`) with the owner of the file's security descriptor. Callers import both:
//! `use crate::platform::user::{User, UserId};` then `UserId::current().owns_path(..)`, `UserId::name()`.

#[cfg(unix)]
#[path = "user/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "user/windows.rs"]
mod imp;

pub use imp::Id as UserId;

use std::fs::File;
use std::io;
use std::path::Path;

/// What every OS implements ([`UserId`], in `user/unix.rs` and `user/windows.rs`).
#[allow(dead_code)] // `owns_path`/`owns_file`: their callers arrive with `platform::fs`
pub trait User: Sized {
    /// The user this process runs as (Unix: the effective uid; Windows: the SID in its token).
    fn current() -> Self;
    /// Whether this user owns what `path` names. A symlink (or Windows junction) is judged itself, not what it
    /// points to.
    fn owns_path(&self, path: &Path) -> io::Result<bool>;
    /// Whether this user owns the file behind an open handle.
    fn owns_file(&self, file: &File) -> io::Result<bool>;
    /// The login name, for what must name the user to the system (`loginctl enable-linger`); `None` when the
    /// system cannot tell. One body for every OS (`whoami`); an OS overrides it only if it must.
    fn name() -> Option<String> {
        whoami::username().ok()
    }
    /// The host name as the operating system reports it, trimmed; empty when it cannot be read. One body for every OS
    /// (`gethostname`).
    fn host_name() -> String {
        gethostname::gethostname()
            .to_string_lossy()
            .trim()
            .to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owns_what_it_creates() {
        let dir = std::env::temp_dir().join(format!("sv-platform-user-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(UserId::current().owns_path(&dir).unwrap());
        let file = std::fs::File::create(dir.join("f")).unwrap();
        assert!(UserId::current().owns_file(&file).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn owns_path_reports_a_missing_path() {
        let missing =
            std::env::temp_dir().join(format!("sv-platform-missing-{}", std::process::id()));
        assert!(UserId::current().owns_path(&missing).is_err());
    }

    #[test]
    fn name_is_known() {
        assert!(UserId::name().is_some_and(|name| !name.is_empty()));
    }

    #[test]
    fn host_name_is_trimmed() {
        let name = UserId::host_name();
        assert_eq!(name, name.trim());
    }
}
