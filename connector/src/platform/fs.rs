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

use super::user::{User, UserId};
use anyhow::{bail, Context, Result};
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

/// What every OS implements ([`Files`], in `fs/unix.rs` and `fs/windows.rs`): the required methods. The rest is one
/// body for every OS; an OS overrides it only if it must.
pub trait Fs {
    /// Makes one directory, private, failing when something is already there.
    fn create_private_dir(path: &Path) -> io::Result<()>;

    /// Opens a file of ours as `how` says.
    fn open_private(path: &Path, how: Create) -> io::Result<File>;

    /// Opens `path` for reading: a link there is refused, not followed, and nothing is waited on (a FIFO or a device
    /// put in a file's place).
    fn open_read(path: &Path) -> io::Result<File>;

    /// What others may do with what `path` names; a link is judged itself, not what it points to.
    fn others(path: &Path) -> io::Result<Others>;

    /// What others may do with the file behind an open handle.
    fn others_of(file: &File) -> io::Result<Others>;

    /// How many names the file behind `file` has.
    fn link_count(file: &File) -> io::Result<u64>;

    /// A directory on the way to something private: this user's or the system's, and nobody else able to replace
    /// what is in it. Refused with the reason otherwise.
    fn check_ancestor(dir: &Path) -> Result<()>;

    /// How a record is written whole: a new file private, an existing one's access kept when `keep_access` (Unix;
    /// on Windows a new file takes what its directory gives, which in a private directory is private).
    fn atomic(keep_access: bool) -> atomic_write_file::OpenOptions;

    /// Makes `path` and any missing parents, each private. What is already there is left as it is.
    fn create_private_dir_all(path: &Path) -> io::Result<()> {
        match Self::create_private_dir(path) {
            Err(error) if error.kind() == ErrorKind::NotFound => {
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    Self::create_private_dir_all(parent)?;
                }
                match Self::create_private_dir(path) {
                    Err(error) if error.kind() == ErrorKind::AlreadyExists && path.is_dir() => {
                        Ok(())
                    }
                    other => other,
                }
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
            other => other,
        }
    }

    /// A private directory, made (with any missing parents) when it does not exist, then checked with its
    /// ancestors. An existing directory with the wrong owner or access is refused, not repaired.
    fn ensure_private_dir(path: &Path) -> Result<()> {
        if !path.is_absolute() {
            bail!("{} is not an absolute path", path.display());
        }
        Self::create_private_dir_all(path)?;
        Self::check_private_dir(path)?;
        let mut current = path.parent();
        while let Some(dir) = current.filter(|dir| !dir.as_os_str().is_empty()) {
            Self::check_ancestor(dir)?;
            current = dir.parent();
        }
        Ok(())
    }

    /// A real directory (not a link), this user's, open to nobody else.
    fn check_private_dir(path: &Path) -> Result<()> {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("missing private directory {}", path.display()))?;
        if !metadata.is_dir() || !private::<Self>(path)? {
            bail!("unsafe private directory {}", path.display());
        }
        Ok(())
    }

    /// A regular file (not a link), this user's, open to nobody else.
    fn check_private_file(path: &Path) -> Result<()> {
        if !fs::symlink_metadata(path)?.is_file() || !private::<Self>(path)? {
            bail!("unsafe private file {}", path.display());
        }
        Ok(())
    }

    /// Whether the file behind `file` is a regular file of this user's, open to nobody else, with no other name.
    fn is_private(file: &File) -> io::Result<bool> {
        Ok(file.metadata()?.is_file()
            && UserId::current().owns_file(file)?
            && Self::others_of(file)? == Others::Nothing
            && Self::link_count(file)? == 1)
    }

    /// A directory of the person's that we read or write in (a home, an agent's configuration): a real directory,
    /// this user's, that nobody else can change. Returns its canonical path.
    fn check_user_directory(path: &Path) -> Result<PathBuf> {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("missing user directory {}", path.display()))?;
        if !metadata.is_dir()
            || !UserId::current().owns_path(path)?
            || Self::others(path)? == Others::Change
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
        Self::check_private_dir(path.parent().context("file parent")?)?;
        write_whole(Self::atomic(false), path, bytes).map_err(Into::into)
    }

    /// Writes `bytes` as `path` whole or not at all, in a directory the caller has vouched for: an existing file
    /// keeps its access (Unix), a new one is private.
    fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
        write_whole(Self::atomic(true), path, bytes)
    }
}

/// This user's, and open to nobody else.
fn private<F: Fs + ?Sized>(path: &Path) -> io::Result<bool> {
    Ok(UserId::current().owns_path(path)? && F::others(path)? == Others::Nothing)
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
            Files::create_private_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_private_directory_is_made_once_owned_and_open_to_nobody_else() {
        let scratch = Scratch::new("ensure");
        let dir = scratch.0.join("a").join("b");
        Files::ensure_private_dir(&dir).unwrap();
        Files::ensure_private_dir(&dir).unwrap();
        Files::check_private_dir(&dir).unwrap();
        assert!(UserId::current().owns_path(&dir).unwrap());
        assert_eq!(Files::others(&dir).unwrap(), Others::Nothing);
        assert!(Files::ensure_private_dir(Path::new("relative/dir")).is_err());
        assert!(Files::check_private_file(&dir).is_err(), "not a file");
    }

    #[test]
    fn a_directory_somebody_else_can_change_is_refused_not_repaired() {
        let scratch = Scratch::new("open-dir");
        let dir = scratch.0.join("data");
        Files::ensure_private_dir(&dir).unwrap();
        imp::let_others_change(&dir);
        assert_eq!(Files::others(&dir).unwrap(), Others::Change);
        assert!(Files::check_private_dir(&dir).is_err());
        assert!(Files::ensure_private_dir(&dir).is_err(), "refused");
        assert_eq!(Files::others(&dir).unwrap(), Others::Change, "not repaired");
        assert!(Files::check_user_directory(&dir).is_err());
        assert!(
            Files::ensure_private_dir(&dir.join("inside")).is_err(),
            "nor trusted on the way to something private"
        );
    }

    #[test]
    fn a_private_write_replaces_the_record_whole_and_leaves_no_temporary_file() {
        let scratch = Scratch::new("write");
        let file = scratch.0.join("state.json");
        Files::write_private(&file, b"first").unwrap();
        Files::write_private(&file, b"second").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"second");
        Files::check_private_file(&file).unwrap();
        let names: Vec<_> = fs::read_dir(&scratch.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["state.json"]);
        let open = scratch.0.join("open");
        Files::create_private_dir(&open).unwrap();
        imp::let_others_change(&open);
        assert!(
            Files::write_private(&open.join("other.json"), b"x").is_err(),
            "never written where others can enter"
        );
    }

    #[test]
    fn a_replaced_file_is_written_whole_and_a_new_one_is_private() {
        let scratch = Scratch::new("replace");
        let file = scratch.0.join("config.json");
        Files::replace(&file, b"{}").unwrap();
        Files::check_private_file(&file).unwrap();
        Files::replace(&file, b"{\"a\":1}").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"{\"a\":1}");
    }

    #[test]
    fn a_trusted_file_is_read_bounded_and_refused_once_somebody_else_can_change_it() {
        let scratch = Scratch::new("read");
        let file = scratch.0.join("record.json");
        assert!(Files::read_trusted(&file, 1024).unwrap().is_none());
        Files::write_private(&file, br#"{"a":1}"#).unwrap();
        assert_eq!(
            Files::read_trusted(&file, 1024).unwrap().unwrap(),
            br#"{"a":1}"#
        );
        assert!(Files::read_trusted(&file, 3).is_err(), "oversized");
        assert!(
            Files::read_trusted(&scratch.0, 1024).is_err(),
            "not a regular file"
        );
        imp::let_others_change(&file);
        assert!(Files::read_trusted(&file, 1024).is_err());
        assert!(Files::check_private_file(&file).is_err());
    }

    #[test]
    fn the_private_openers_make_private_files_and_keep_what_is_in_them() {
        let scratch = Scratch::new("open");
        let log = scratch.0.join("log");
        writeln!(Files::open_private(&log, Create::Append).unwrap(), "one").unwrap();
        writeln!(Files::open_private(&log, Create::Append).unwrap(), "two").unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "one\ntwo\n");
        Files::check_private_file(&log).unwrap();
        let held = Files::open_private(&log, Create::ReadWrite).unwrap();
        assert!(Files::is_private(&held).unwrap());
        assert_eq!(fs::read_to_string(&log).unwrap(), "one\ntwo\n", "kept");
        let new = scratch.0.join("new");
        Files::open_private(&new, Create::New).unwrap();
        assert!(Files::open_private(&new, Create::New).is_err(), "exclusive");
        Files::check_private_file(&new).unwrap();
        let program = scratch.0.join("program");
        Files::open_private(&program, Create::NewProgram).unwrap();
        Files::check_private_file(&program).unwrap();
    }

    #[test]
    fn a_second_name_is_seen_on_the_handle() {
        let scratch = Scratch::new("links");
        let file = scratch.0.join("lock");
        let handle = Files::open_private(&file, Create::ReadWrite).unwrap();
        assert_eq!(Files::link_count(&handle).unwrap(), 1);
        fs::hard_link(&file, scratch.0.join("other")).unwrap();
        assert_eq!(Files::link_count(&handle).unwrap(), 2);
        assert!(!Files::is_private(&handle).unwrap());
    }

    #[test]
    fn a_user_directory_is_the_users_and_named_canonically() {
        let scratch = Scratch::new("user");
        assert_eq!(
            Files::check_user_directory(&scratch.0).unwrap(),
            scratch.0.canonicalize().unwrap()
        );
        let file = scratch.0.join("config.toml");
        fs::write(&file, "a = 1\n").unwrap();
        assert!(
            Files::check_user_directory(&file).is_err(),
            "not a directory"
        );
        assert!(Files::check_user_directory(&scratch.0.join("missing")).is_err());
    }
}
