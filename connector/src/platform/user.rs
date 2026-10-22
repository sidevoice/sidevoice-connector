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

// `UserId` is named by no caller yet: they only chain off `current()`.
#[allow(unused_imports)]
pub use imp::UserId;
pub use imp::{current, name};

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
    fn current_is_stable() {
        assert_eq!(current(), current());
    }

    #[test]
    fn host_name_is_trimmed() {
        let name = host_name();
        assert_eq!(name, name.trim());
    }
}
