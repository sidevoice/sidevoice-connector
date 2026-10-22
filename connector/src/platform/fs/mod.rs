//! Private storage: the file-system half of the trust boundary, the OS user. What keeps another user out is that
//! every directory on the way to our files is this user's (or root's) and nobody else can replace what is in it,
//! and that our own directories and records are open to nobody else. Anything that fails a check is refused, never
//! repaired behind the person's back and never used.
//!
//! Unix: owner and mode bits, `O_NOFOLLOW`, the descriptor judged as opened. Windows: the owner SID and the DACL,
//! reparse points refused, the handle judged as opened.
//!
//! Records are written whole: a fresh private temporary file (exclusive, no link followed), synced, renamed over
//! the old one, and (where the OS allows) the directory synced, so a reader never sees half of one.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as sys;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as sys;

#[cfg(unix)]
pub use sys::verify_socket;
pub use sys::{
    check_private_dir, check_private_file, create_private_dir, create_private_dir_all,
    create_private_executable_new, create_private_new, has_single_link, is_executable_file,
    is_owned_file, is_private_unshared_handle, is_user_directory, is_user_file, mode_of,
    open_no_follow, open_private_append, open_private_lock, open_private_truncate, owner_can_write,
    read_user_text, set_mode, trusted_executable, verify_ancestors, Mode,
};

use anyhow::{bail, Context, Result};
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

/// A directory of the person's that we read or write in (a home, an agent's configuration): a real directory,
/// this user's, writable by nobody else. Returns its canonical path.
pub fn check_user_directory(path: &Path) -> Result<PathBuf> {
    fs::symlink_metadata(path)
        .with_context(|| format!("missing user directory {}", path.display()))?;
    if !is_user_directory(path) {
        bail!("unsafe user directory {}", path.display());
    }
    path.canonicalize().map_err(Into::into)
}

/// A private directory, made (with any missing parents) when it does not exist, then checked with its
/// ancestors. An existing directory with the wrong owner or mode is refused, not repaired.
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("{} is not an absolute path", path.display());
    }
    match create_private_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    check_private_dir(path)?;
    verify_ancestors(path)
}

/// The bytes of a record we trust, judged on the descriptor that was opened (no link followed), so the file judged
/// is the file read: a regular file, this user's, writable by nobody else, at most `limit` bytes. `None` when it
/// does not exist; refused otherwise.
pub fn read_trusted(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    let file = match open_no_follow(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) if sys::is_link_error(&error) => {
            bail!("unsafe file {}: a symbolic link", path.display())
        }
        Err(error) => return Err(error.into()),
    };
    if !sys::is_trusted_handle(&file)? {
        bail!("unsafe file {}", path.display());
    }
    let m = file.metadata()?;
    if m.len() > limit {
        bail!("{} is larger than {limit} bytes", path.display());
    }
    let mut bytes = Vec::with_capacity(m.len() as usize);
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("{} is larger than {limit} bytes", path.display());
    }
    Ok(Some(bytes))
}

/// Writes `bytes` as `path`, private, whole or not at all, in a directory the caller has already vouched for: a
/// private temporary file beside it, synced, renamed over, and the directory synced.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidInput, "file has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidInput, "file has no name"))?;
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| -> std::io::Result<()> {
        let mut file = create_private_new(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        sys::sync_dir(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Writes `bytes` as `path`, private, whole or not at all, in a directory that must already be private.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    check_private_dir(path.parent().context("file parent")?)?;
    path.file_name().context("file name")?;
    write_atomic(path, bytes).map_err(Into::into)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A private scratch directory under the temporary directory, removed when dropped.
    pub(crate) struct Scratch(pub PathBuf);

    impl Scratch {
        pub(crate) fn new(label: &str) -> Self {
            let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "sv-{label}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            create_private_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_private_write_replaces_the_record_whole_and_leaves_no_temporary_file() {
        let scratch = Scratch::new("write");
        let file = scratch.0.join("state.json");
        write_private(&file, b"first").unwrap();
        write_private(&file, b"second").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"second");
        check_private_file(&file).unwrap();
        let names: Vec<_> = fs::read_dir(&scratch.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["state.json"]);
    }

    #[test]
    fn a_trusted_record_is_read_back_and_bounded() {
        let scratch = Scratch::new("read");
        let file = scratch.0.join("record.json");
        assert!(read_trusted(&file, 1024).unwrap().is_none());
        write_private(&file, br#"{"a":1}"#).unwrap();
        assert_eq!(read_trusted(&file, 1024).unwrap().unwrap(), br#"{"a":1}"#);
        assert!(read_trusted(&file, 3).is_err(), "oversized");
        assert!(
            read_trusted(&scratch.0, 1024).is_err(),
            "not a regular file"
        );
    }

    #[test]
    fn a_private_directory_is_ensured_once_and_a_relative_one_is_refused() {
        let scratch = Scratch::new("ensure");
        let dir = scratch.0.join("a").join("b");
        ensure_private_dir(&dir).unwrap();
        ensure_private_dir(&dir).unwrap();
        check_private_dir(&dir).unwrap();
        assert!(ensure_private_dir(Path::new("relative/dir")).is_err());
    }

    #[test]
    fn the_private_openers_create_private_files_and_keep_what_is_in_them() {
        let scratch = Scratch::new("open");
        let log = scratch.0.join("log");
        writeln!(open_private_append(&log).unwrap(), "one").unwrap();
        writeln!(open_private_append(&log).unwrap(), "two").unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "one\ntwo\n");
        check_private_file(&log).unwrap();
        open_private_truncate(&log)
            .unwrap()
            .write_all(b"x")
            .unwrap();
        assert_eq!(fs::read(&log).unwrap(), b"x");
        let lock = scratch.0.join("lock");
        open_private_lock(&lock)
            .unwrap()
            .write_all(b"held")
            .unwrap();
        open_private_lock(&lock).unwrap();
        assert_eq!(fs::read(&lock).unwrap(), b"held");
        let handle = open_private_lock(&lock).unwrap();
        assert!(is_private_unshared_handle(&handle).unwrap());
        assert!(has_single_link(&handle).unwrap());
        let new = scratch.0.join("new");
        create_private_new(&new).unwrap();
        assert!(create_private_new(&new).is_err(), "exclusive");
        check_private_file(&new).unwrap();
        create_private_executable_new(&scratch.0.join("run")).unwrap();
    }

    #[test]
    fn a_user_directory_and_its_text_are_read_when_they_are_the_users() {
        let scratch = Scratch::new("user");
        assert!(is_user_directory(&scratch.0));
        assert_eq!(
            check_user_directory(&scratch.0).unwrap(),
            scratch.0.canonicalize().unwrap()
        );
        let file = scratch.0.join("config.toml");
        fs::write(&file, "a = 1\n").unwrap();
        assert!(is_user_file(&file));
        assert_eq!(read_user_text(&file, 1024).unwrap(), "a = 1\n");
        assert!(read_user_text(&file, 2).is_err(), "oversized");
        assert!(!is_user_file(&scratch.0));
        assert!(!is_user_directory(&file));
    }
}

#[cfg(all(test, unix))]
mod unix_tests;
