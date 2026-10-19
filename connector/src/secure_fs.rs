//! The file-system half of the trust boundary: the OS user. What keeps another user out is that every directory on
//! the way to our files is this user's (or root's) and nobody else can replace what is in it, and that our own
//! directories, records and sockets are open to nobody else. Anything that fails a check is refused, never repaired
//! behind the person's back and never used.
//!
//! Records are written whole: a fresh private temporary file (0600, exclusive, no link followed), synced, renamed
//! over the old one, and the directory synced, so a reader never sees half of one.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub fn uid() -> u32 {
    unsafe { libc::geteuid() }
}

/// A directory of ours: a real directory (not a link), this user's, open to nobody else.
pub fn private_dir(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)
        .with_context(|| format!("missing private directory {}", path.display()))?;
    if !m.is_dir() || m.file_type().is_symlink() || m.uid() != uid() || m.mode() & 0o077 != 0 {
        bail!("unsafe private directory {}", path.display());
    }
    Ok(())
}

/// A record of ours: a regular file (not a link), this user's, open to nobody else.
pub fn private_file(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_file() || m.file_type().is_symlink() || m.uid() != uid() || m.mode() & 0o077 != 0 {
        bail!("unsafe private file {}", path.display());
    }
    Ok(())
}

/// A directory of the person's that we read or write in (a home, an agent's configuration): a real directory, this
/// user's, writable by nobody else. Returns its canonical path.
pub fn validate_user_directory(path: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("missing user directory {}", path.display()))?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != uid()
        || metadata.mode() & 0o022 != 0
    {
        bail!("unsafe user directory {}", path.display());
    }
    path.canonicalize().map_err(Into::into)
}

/// A socket before anything talks to it: in a private directory, a socket, this user's, open to nobody else.
pub fn verify_socket(path: &Path) -> Result<()> {
    private_dir(path.parent().context("socket parent")?)?;
    let m = fs::symlink_metadata(path)?;
    if !m.file_type().is_socket() || m.uid() != uid() || m.mode() & 0o077 != 0 {
        bail!("unsafe socket {}", path.display());
    }
    Ok(())
}

/// Every ancestor of `path`: this user's or root's, and if others can write into it, sticky (as `/tmp`), so
/// nobody else can replace what is in it.
pub fn verify_ancestors(path: &Path) -> Result<()> {
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir.as_os_str().is_empty() {
            break;
        }
        let m = fs::symlink_metadata(dir).with_context(|| format!("missing {}", dir.display()))?;
        let owner = m.uid() == uid() || m.uid() == 0;
        let replaceable = m.mode() & 0o022 != 0 && m.mode() & 0o1000 == 0;
        if !owner || replaceable {
            bail!(
                "unsafe directory {}: {}",
                dir.display(),
                if owner {
                    "others can replace what is in it"
                } else {
                    "owned by another user"
                }
            );
        }
        current = dir.parent();
    }
    Ok(())
}

/// A private directory, made 0700 (with any missing parents) when it does not exist, then checked with its
/// ancestors. An existing directory with the wrong owner or mode is refused, not repaired.
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("{} is not an absolute path", path.display());
    }
    match fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
    {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    private_dir(path)?;
    verify_ancestors(path)
}

/// The bytes of a record we trust, judged on the descriptor that was opened (no link followed), so the file judged
/// is the file read: a regular file, this user's, writable by nobody else, at most `limit` bytes. `None` when it
/// does not exist; refused otherwise.
pub fn read_trusted(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            bail!("unsafe file {}: a symbolic link", path.display())
        }
        Err(error) => return Err(error.into()),
    };
    let m = file.metadata()?;
    if !m.is_file() || m.uid() != uid() || m.mode() & 0o022 != 0 {
        bail!("unsafe file {}", path.display());
    }
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

/// Writes `bytes` as `path`, 0600, whole or not at all, in a directory that must already be private.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("file parent")?;
    private_dir(parent)?;
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().context("file name")?.to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

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

/// A program we are about to run or vouch for: a regular file (not a link), this user's, writable by nobody else,
/// executable.
pub fn safe_executable(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != uid()
        || metadata.mode() & 0o022 != 0
        || metadata.mode() & 0o111 == 0
    {
        bail!("unsafe executable {}", path.display());
    }
    Ok(metadata)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    /// A private scratch directory under `/tmp`, removed when dropped.
    pub(crate) struct Scratch(pub PathBuf);

    impl Scratch {
        pub(crate) fn new(label: &str) -> Self {
            let path = Path::new("/tmp").canonicalize().unwrap().join(format!(
                "sv-{label}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn chmod(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn a_private_directory_is_refused_when_others_can_enter_or_it_is_a_link() {
        let scratch = Scratch::new("dir");
        let dir = scratch.0.join("data");
        ensure_private_dir(&dir).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        // Created again: already there and still private.
        ensure_private_dir(&dir).unwrap();
        chmod(&dir, 0o750);
        assert!(private_dir(&dir).is_err());
        assert!(
            ensure_private_dir(&dir).is_err(),
            "an open directory is refused, not repaired"
        );
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o750);
        chmod(&dir, 0o700);
        let link = scratch.0.join("link");
        symlink(&dir, &link).unwrap();
        assert!(private_dir(&link).is_err());
        assert!(ensure_private_dir(Path::new("relative/dir")).is_err());
    }

    #[test]
    fn an_ancestor_others_can_replace_entries_in_is_refused_unless_sticky() {
        let scratch = Scratch::new("anc");
        let open = scratch.0.join("open");
        fs::create_dir(&open).unwrap();
        chmod(&open, 0o777);
        let inside = open.join("data");
        assert!(ensure_private_dir(&inside).is_err());
        chmod(&open, 0o1777);
        ensure_private_dir(&inside).unwrap();
        verify_ancestors(&inside).unwrap();
    }

    #[test]
    fn a_trusted_record_is_read_only_when_it_is_this_users_regular_file() {
        let scratch = Scratch::new("read");
        let file = scratch.0.join("record.json");
        assert!(read_trusted(&file, 1024).unwrap().is_none());
        atomic_json(&file, &serde_json::json!({"a": 1})).unwrap();
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
        assert_eq!(read_trusted(&file, 1024).unwrap().unwrap(), br#"{"a":1}"#);
        assert!(read_trusted(&file, 3).is_err(), "oversized");
        chmod(&file, 0o622);
        assert!(read_trusted(&file, 1024).is_err(), "writable by others");
        chmod(&file, 0o600);
        let link = scratch.0.join("link.json");
        symlink(&file, &link).unwrap();
        assert!(read_trusted(&link, 1024).is_err(), "a link is not followed");
        assert!(
            read_trusted(&scratch.0, 1024).is_err(),
            "not a regular file"
        );
    }

    #[test]
    fn a_private_write_replaces_the_record_whole_and_leaves_no_temporary_file() {
        let scratch = Scratch::new("write");
        let file = scratch.0.join("state.json");
        write_private(&file, b"first").unwrap();
        write_private(&file, b"second").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"second");
        private_file(&file).unwrap();
        let names: Vec<_> = fs::read_dir(&scratch.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["state.json"]);
        // Never written into a directory others can enter.
        chmod(&scratch.0, 0o755);
        assert!(write_private(&scratch.0.join("other.json"), b"x").is_err());
        chmod(&scratch.0, 0o700);
    }

    #[test]
    fn a_socket_is_trusted_only_in_a_private_directory_and_open_to_nobody_else() {
        let scratch = Scratch::new("sock");
        let path = scratch.0.join("s.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        chmod(&path, 0o600);
        verify_socket(&path).unwrap();
        chmod(&path, 0o666);
        assert!(verify_socket(&path).is_err());
        chmod(&path, 0o600);
        chmod(&scratch.0, 0o755);
        assert!(verify_socket(&path).is_err());
        chmod(&scratch.0, 0o700);
        let file = scratch.0.join("plain");
        write_private(&file, b"").unwrap();
        assert!(verify_socket(&file).is_err());
    }

    #[test]
    fn an_executable_must_be_a_regular_file_nobody_else_can_change() {
        let scratch = Scratch::new("exe");
        let program = scratch.0.join("program");
        fs::write(&program, b"#!/bin/sh\n").unwrap();
        chmod(&program, 0o700);
        safe_executable(&program).unwrap();
        chmod(&program, 0o600);
        assert!(safe_executable(&program).is_err(), "not executable");
        chmod(&program, 0o722);
        assert!(safe_executable(&program).is_err(), "writable by others");
        chmod(&program, 0o700);
        let link = scratch.0.join("link");
        symlink(&program, &link).unwrap();
        assert!(safe_executable(&link).is_err());
    }
}
