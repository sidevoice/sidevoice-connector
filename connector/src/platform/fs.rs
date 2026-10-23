//! Private files and directories: the file-system half of the trust boundary, the OS user. What keeps another user
//! out is that every directory on the way to our files is this user's (or the system's) and nobody else can replace
//! what is in it, and that our own directories and records are open to nobody else. Anything that fails a check is
//! refused, never repaired behind the person's back and never used.
//!
//! The interface is the [`Fs`] trait; [`Files`] is the one OS implementation compiled in, chosen below. What differs
//! per OS is only how access is set and read: Unix mode bits and `O_NOFOLLOW`; on Windows a protected DACL set when a
//! file or directory is created, read back from the handle, and reparse points (links, junctions) refused. Who owns
//! something is [`platform::user`](super::user)'s ([`UserId::current`]`.owns_path/owns_file`). Everything else has
//! one body for every OS, the trait's default methods. Records are written whole by `atomic-write-file` (a temporary
//! file beside the record, synced, renamed over it). Callers import both:
//! `use crate::platform::fs::{Files, Fs};` then `Files::ensure_private_dir(..)`, `Files::read_trusted(..)`.

#[cfg(unix)]
#[path = "fs/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "fs/windows.rs"]
mod imp;

pub use imp::Os as Files;

// How a test widens or narrows the access to a path: the interface only makes private things and checks them.
#[cfg(test)]
pub(crate) use imp::{let_others_change, shut_others_out};

use super::user::{User, UserId};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

/// What somebody other than a file's owner (and the system: root, SYSTEM, Administrators) may do with it.
#[derive(Debug, PartialEq, Eq)]
pub enum Others {
    /// Nothing at all: the file is private.
    Nothing,
    /// Read it, never change it.
    Read,
    /// Change it, or (a directory) replace what is in it.
    Change,
}

/// How [`Fs::open_private`] opens a file of ours. Whatever it creates is private; a link in its place is refused,
/// never followed.
#[derive(Clone, Copy, Debug)]
pub enum Create {
    /// A new file for writing, never one already there.
    New,
    /// As `New`, for a program: executable where the OS marks that on the file (Unix).
    NewProgram,
    /// For appending, created when missing.
    Append,
    /// For reading and writing, created when missing, what is in it kept.
    ReadWrite,
}

/// Which file a handle is open on ([`Fs::identity`]): equal for two handles to one file, different for a file that
/// took its path (a rotation, a replacement). Compared only with another reading on this machine; holds nothing open.
#[derive(PartialEq)]
pub struct FileId {
    volume: u64,
    index: u128,
}

/// What every OS implements ([`Files`], in `fs/unix.rs` and `fs/windows.rs`): the required methods. The rest is one
/// body for every OS; an OS overrides it only if it must.
pub trait Fs {
    /// Makes one directory, private, failing when something is already there.
    fn create_private_dir(path: &Path) -> io::Result<()>;

    /// Opens a file of ours as `how` says.
    fn open_private(path: &Path, how: Create) -> io::Result<File>;

    /// Opens `path` (a file or a directory) for reading: a link there is refused, not followed, and nothing is
    /// waited on (a FIFO or a device put in a file's place).
    fn open_read(path: &Path) -> io::Result<File>;

    /// What others may do with the file or directory behind an open handle.
    fn others_of(file: &File) -> io::Result<Others>;

    /// Which file is behind an open handle (Unix: device and inode; Windows: volume serial number and the 128-bit file id, ReFS needing all of it; the 64-bit file index where the file system has no such id),
    /// read from the handle itself: the file being read, not whatever its path names a moment later.
    fn identity(file: &File) -> io::Result<FileId>;

    /// A private directory, made (with any missing parents, each private) when it does not exist, then checked with
    /// its ancestors: each this user's or the system's, nobody else able to replace what is in it. An existing
    /// directory with the wrong owner or access is refused, not repaired.
    fn ensure_private_dir(path: &Path) -> Result<()> {
        if !path.is_absolute() {
            bail!("{} is not an absolute path", path.display());
        }
        create_all::<Self>(path)?;
        let dir = Self::open_read(path)?;
        if !dir.metadata()?.is_dir() {
            bail!("{} is not a directory", path.display());
        }
        Self::check_private(&dir)
            .with_context(|| format!("unsafe private directory {}", path.display()))?;
        let mut current = path.parent();
        while let Some(ancestor) = current.filter(|dir| !dir.as_os_str().is_empty()) {
            imp::check_ancestor(ancestor)?;
            current = ancestor.parent();
        }
        Ok(())
    }

    /// That what is behind `file` is private: this user's and open to nobody else; a regular file also has no
    /// other name (a hard link somebody else made would see what is written to it). A directory or a regular file,
    /// judged from what is there; anything else is refused.
    fn check_private(file: &File) -> Result<()> {
        let metadata = file.metadata()?;
        if !UserId::current().owns_file(file)? {
            bail!("not this user's");
        }
        if Self::others_of(file)? != Others::Nothing {
            bail!("open to others");
        }
        if !metadata.is_dir() && !(metadata.is_file() && imp::link_count(file)? == 1) {
            bail!("not a directory or a regular file with one name");
        }
        Ok(())
    }

    /// A directory of the person's that we read or write in (a home, an agent's configuration): a real directory,
    /// this user's, that nobody else can change. Returns its canonical path.
    fn check_user_directory(path: &Path) -> Result<PathBuf> {
        let dir = Self::open_read(path)
            .with_context(|| format!("missing user directory {}", path.display()))?;
        if !dir.metadata()?.is_dir()
            || !UserId::current().owns_file(&dir)?
            || Self::others_of(&dir)? == Others::Change
        {
            bail!("unsafe user directory {}", path.display());
        }
        path.canonicalize().map_err(Into::into)
    }

    /// The bytes of a file we trust, judged on the handle that was opened (no link followed), so the file judged is
    /// the file read: a regular file, this user's, that nobody else can change, at most `limit` bytes. `None` when
    /// it does not exist; refused otherwise.
    fn read_trusted(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
        let file = match Self::open_read(path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(_) if fs::symlink_metadata(path).is_ok_and(|m| m.is_symlink()) => {
                bail!("unsafe file {}: a link", path.display())
            }
            Err(error) => return Err(error.into()),
        };
        if !file.metadata()?.is_file()
            || !UserId::current().owns_file(&file)?
            || Self::others_of(&file)? == Others::Change
        {
            bail!("unsafe file {}", path.display());
        }
        let mut bytes = Vec::new();
        file.take(limit + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            bail!("{} is larger than {limit} bytes", path.display());
        }
        Ok(Some(bytes))
    }

    /// Writes `bytes` as `path`, private, whole or not at all, in a directory that must already be private.
    fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
        let parent = path.parent().context("file parent")?;
        Self::check_private(&Self::open_read(parent)?)
            .with_context(|| format!("unsafe private directory {}", parent.display()))?;
        write_whole(imp::atomic(false), path, bytes).map_err(Into::into)
    }

    /// [`Fs::write_private`] of `value` as JSON.
    fn write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
        Self::write_private(path, &serde_json::to_vec(value)?)
    }

    /// Writes `bytes` as `path` whole or not at all, in a directory the caller has vouched for: an existing file
    /// keeps its access (Unix), a new one is private.
    fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
        write_whole(imp::atomic(true), path, bytes)
    }
}

/// Makes `path` and any missing parents, each private; what is already there is left as it is.
fn create_all<F: Fs + ?Sized>(path: &Path) -> io::Result<()> {
    let created = match F::create_private_dir(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                create_all::<F>(parent)?;
            }
            F::create_private_dir(path)
        }
        other => other,
    };
    match created {
        Err(error) if error.kind() == ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        other => other,
    }
}

fn write_whole(
    options: atomic_write_file::OpenOptions,
    path: &Path,
    bytes: &[u8],
) -> io::Result<()> {
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.commit()
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_dir;

    /// Whether what `path` names is private, judged on a handle opened without following a link.
    fn private(path: &Path) -> bool {
        Files::open_read(path)
            .map_err(Into::into)
            .and_then(|file| Files::check_private(&file))
            .is_ok()
    }

    fn others(path: &Path) -> Others {
        Files::others_of(&Files::open_read(path).unwrap()).unwrap()
    }

    #[test]
    fn a_private_directory_is_made_once_owned_and_open_to_nobody_else() {
        let scratch = temp_dir("ensure");
        let dir = scratch.path().join("a").join("b");
        Files::ensure_private_dir(&dir).unwrap();
        Files::ensure_private_dir(&dir).unwrap();
        assert!(private(&dir));
        assert!(UserId::current().owns_path(&dir).unwrap());
        assert_eq!(others(&dir), Others::Nothing);
        assert!(Files::ensure_private_dir(Path::new("relative/dir")).is_err());
    }

    #[test]
    fn a_directory_somebody_else_can_change_is_refused_not_repaired() {
        let scratch = temp_dir("open-dir");
        let dir = scratch.path().join("data");
        Files::ensure_private_dir(&dir).unwrap();
        imp::let_others_change(&dir);
        assert_eq!(others(&dir), Others::Change);
        assert!(!private(&dir));
        assert!(Files::ensure_private_dir(&dir).is_err(), "refused");
        assert_eq!(others(&dir), Others::Change, "not repaired");
        assert!(Files::check_user_directory(&dir).is_err());
        assert!(
            Files::ensure_private_dir(&dir.join("inside")).is_err(),
            "nor trusted on the way to something private"
        );
    }

    #[test]
    fn a_private_write_replaces_the_record_whole_and_leaves_no_temporary_file() {
        let scratch = temp_dir("write");
        let dir = scratch.path().join("private");
        Files::create_private_dir(&dir).unwrap();
        let file = dir.join("state.json");
        Files::write_private(&file, b"first").unwrap();
        Files::write_private(&file, b"second").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"second");
        assert!(private(&file));
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["state.json"]);
        let open = scratch.path().join("open");
        Files::create_private_dir(&open).unwrap();
        imp::let_others_change(&open);
        assert!(
            Files::write_private(&open.join("other.json"), b"x").is_err(),
            "never written where others can enter"
        );
    }

    #[test]
    fn a_value_is_written_as_json() {
        let scratch = temp_dir("json");
        let dir = scratch.path().join("private");
        Files::create_private_dir(&dir).unwrap();
        let file = dir.join("value.json");
        Files::write_json(&file, &serde_json::json!({"a": [1]})).unwrap();
        assert_eq!(fs::read(&file).unwrap(), br#"{"a":[1]}"#);
        assert!(private(&file));
    }

    #[test]
    fn a_replaced_file_is_written_whole_and_a_new_one_is_private() {
        let scratch = temp_dir("replace");
        let file = scratch.path().join("config.json");
        Files::replace(&file, b"{}").unwrap();
        assert!(private(&file));
        Files::replace(&file, b"{\"a\":1}").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"{\"a\":1}");
    }

    #[test]
    fn a_trusted_file_is_read_bounded_and_refused_once_somebody_else_can_change_it() {
        let scratch = temp_dir("read");
        let dir = scratch.path().join("private");
        Files::create_private_dir(&dir).unwrap();
        let file = dir.join("record.json");
        assert!(Files::read_trusted(&file, 1024).unwrap().is_none());
        Files::write_private(&file, br#"{"a":1}"#).unwrap();
        assert_eq!(
            Files::read_trusted(&file, 1024).unwrap().unwrap(),
            br#"{"a":1}"#
        );
        assert!(Files::read_trusted(&file, 3).is_err(), "oversized");
        assert!(
            Files::read_trusted(&dir, 1024).is_err(),
            "not a regular file"
        );
        imp::let_others_change(&file);
        assert!(Files::read_trusted(&file, 1024).is_err());
        assert!(!private(&file));
    }

    #[test]
    fn the_private_openers_make_private_files_and_keep_what_is_in_them() {
        let scratch = temp_dir("open");
        let log = scratch.path().join("log");
        writeln!(Files::open_private(&log, Create::Append).unwrap(), "one").unwrap();
        writeln!(Files::open_private(&log, Create::Append).unwrap(), "two").unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "one\ntwo\n");
        assert!(private(&log));
        let held = Files::open_private(&log, Create::ReadWrite).unwrap();
        Files::check_private(&held).unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "one\ntwo\n", "kept");
        let new = scratch.path().join("new");
        Files::open_private(&new, Create::New).unwrap();
        assert!(Files::open_private(&new, Create::New).is_err(), "exclusive");
        assert!(private(&new));
        let program = scratch.path().join("program");
        Files::open_private(&program, Create::NewProgram).unwrap();
        assert!(private(&program));
    }

    #[test]
    fn a_file_is_told_apart_from_the_one_that_takes_its_path() {
        let scratch = temp_dir("identity");
        let (path, next) = (scratch.path().join("log"), scratch.path().join("next"));
        fs::write(&path, b"first").unwrap();
        let first = Files::identity(&File::open(&path).unwrap()).unwrap();
        assert!(first == Files::identity(&File::open(&path).unwrap()).unwrap());
        // Rotated: another file takes the path while the first still exists.
        fs::write(&next, b"second").unwrap();
        fs::rename(&next, &path).unwrap();
        assert!(first != Files::identity(&File::open(&path).unwrap()).unwrap());
    }

    #[test]
    fn a_second_name_is_seen_on_the_handle() {
        let scratch = temp_dir("links");
        let file = scratch.path().join("lock");
        let handle = Files::open_private(&file, Create::ReadWrite).unwrap();
        Files::check_private(&handle).unwrap();
        fs::hard_link(&file, scratch.path().join("other")).unwrap();
        assert!(Files::check_private(&handle).is_err(), "a second name");
    }

    #[test]
    fn a_user_directory_is_the_users_and_named_canonically() {
        let scratch = temp_dir("user");
        assert_eq!(
            Files::check_user_directory(scratch.path()).unwrap(),
            scratch.path().canonicalize().unwrap()
        );
        let file = scratch.path().join("config.toml");
        fs::write(&file, "a = 1\n").unwrap();
        assert!(
            Files::check_user_directory(&file).is_err(),
            "not a directory"
        );
        assert!(Files::check_user_directory(&scratch.path().join("missing")).is_err());
    }
}
