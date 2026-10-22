//! Locks the OS holds and gives up the moment their holder dies, however it dies: an exclusive, non-blocking
//! advisory lock on a permanent lock file, taken through `std::fs::File::try_lock` (`flock(2)` on Unix,
//! `LockFileEx` on Windows). Nothing is ever stale, so nothing is ever reclaimed.
//!
//! Lock files are permanent inodes: created once, never replaced, never deleted (a holder of a deleted file and a
//! newcomer on its replacement would both hold "the" lock). The lock belongs to the open handle, so a second open
//! of the same file, even in this process, does not get it while the first is held.
//!
//! Who holds it is written to a sibling file, `<lock file>.holder`, written whole after the lock is taken and
//! removed when the guard drops. Never inside the locked file: Windows locks are mandatory, and a reader of the
//! record would be refused. The record is information for people and for teardown, never the lock; after a crash
//! it may outlive its holder, so a reader that must know trusts the lock first and the record second.
//!
//! The file checks (this user's, regular, private, no link followed) are local to this module for now.
// TODO(platform::fs): when `platform::fs` and `UserId::owns_file` are on main, call them instead of the local
// checks and the local private/atomic write below, and add the Windows owner and private-ACL checks.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// What a holder says about itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub pid: u32,
    /// Opaque start time of `pid`, comparable only with another reading on the same machine.
    pub start: Option<String>,
    pub kind: String,
    pub at: String,
}

/// A held lock; released (and its holder record removed) when dropped or when this process dies.
#[derive(Debug)]
pub struct Guard {
    file: File,
    record: Option<PathBuf>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        // Still holding the lock here, so nobody else's record is removed.
        if let Some(record) = &self.record {
            let _ = std::fs::remove_file(record);
        }
        let _ = self.file.unlock();
    }
}

/// `<lock file>.holder`.
fn record_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".holder");
    PathBuf::from(name)
}

fn lock_file(file: File, label: &str) -> Result<Option<File>> {
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(None),
        Err(TryLockError::Error(error)) => {
            return Err(error).context(format!("lock {label}"));
        }
    }
    // A link added between the first check and the lock is refused as well.
    check(&file, label)?;
    Ok(Some(file))
}

/// Tries once: the lock, writing `holder` beside it, or `None` when another holder has it.
pub fn try_lock(path: &Path, holder: &Holder) -> Result<Option<Guard>> {
    let label = path.display().to_string();
    let file = open(path)?;
    check(&file, &label)?;
    let Some(file) = lock_file(file, &label)? else {
        return Ok(None);
    };
    let record = record_path(path);
    // Information only: a lock held without its record is still held.
    let written = serde_json::to_vec(holder)
        .map_err(std::io::Error::from)
        .and_then(|bytes| write_record(&record, &bytes));
    Ok(Some(Guard {
        file,
        record: written.is_ok().then_some(record),
    }))
}

/// The record of whoever holds (or held, if it died without dropping) the lock at `path`.
pub fn holder(path: &Path) -> Option<Holder> {
    let bytes = read_record(&record_path(path)).ok()??;
    serde_json::from_slice(&bytes).ok()
}

/// Writes whole or not at all: a temporary file beside it, renamed over.
fn write_record(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut temporary: OsString = path.as_os_str().to_owned();
    temporary.push(format!(".{}.tmp", std::process::id()));
    let temporary = PathBuf::from(temporary);
    let result = create_private(&temporary).and_then(|mut file| {
        file.write_all(bytes)?;
        file.flush()?;
        drop(file);
        std::fs::rename(&temporary, path)
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

const RECORD_LIMIT: u64 = 4096;

fn read_bounded(mut file: File) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    (&mut file).take(RECORD_LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > RECORD_LIMIT {
        bail!("holder record is larger than {RECORD_LIMIT} bytes");
    }
    Ok(bytes)
}

#[cfg(unix)]
mod os {
    use super::*;
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    /// A directory the lock file is opened relative to, held open so it cannot be swapped for another.
    pub type Dir = OwnedFd;

    pub(super) fn open(path: &Path) -> Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .with_context(|| format!("open lock {}", path.display()))
    }

    /// A regular file of this user's, open to nobody else, with no other name.
    pub(super) fn check(file: &File, label: &str) -> Result<()> {
        let m = file.metadata()?;
        if !m.is_file()
            || !crate::platform::user::current().owns(&m)
            || m.mode() & 0o077 != 0
            || m.nlink() != 1
        {
            bail!("unsafe lock file {label}");
        }
        Ok(())
    }

    pub(super) fn create_private(path: &Path) -> std::io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    }

    pub(super) fn read_record(path: &Path) -> Result<Option<Vec<u8>>> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let m = file.metadata()?;
        if !m.is_file() || !crate::platform::user::current().owns(&m) || m.mode() & 0o022 != 0 {
            bail!("unsafe holder record {}", path.display());
        }
        read_bounded(file).map(Some)
    }

    /// Uninstall pins the checked data directory and takes its locks relative to it, without writing a record,
    /// so nothing is added to a directory about to be removed and a link added after validation cannot be
    /// used through this action.
    pub fn try_lock_in(dir: &Dir, name: &str) -> Result<Option<Guard>> {
        let entry = CString::new(name).context("lock name")?;
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                entry.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("open uninstall lock");
        }
        let file = unsafe { File::from_raw_fd(fd) };
        check(&file, name)?;
        Ok(lock_file(file, name)?.map(|file| Guard { file, record: None }))
    }
}

#[cfg(windows)]
mod os {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;

    /// Open the link itself, not what it points at.
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    pub(super) fn open(path: &Path) -> Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .with_context(|| format!("open lock {}", path.display()))
    }

    /// A regular file, not a link. The owner and private-ACL checks wait for `platform::fs`/`UserId::owns_file`;
    /// the link count is not readable on stable Rust.
    pub(super) fn check(file: &File, label: &str) -> Result<()> {
        if !file.metadata()?.is_file() {
            bail!("unsafe lock file {label}");
        }
        Ok(())
    }

    pub(super) fn create_private(path: &Path) -> std::io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
    }

    pub(super) fn read_record(path: &Path) -> Result<Option<Vec<u8>>> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !file.metadata()?.is_file() {
            bail!("unsafe holder record {}", path.display());
        }
        read_bounded(file).map(Some)
    }
}

use os::{check, create_private, open, read_record};
#[cfg(unix)]
pub use os::{try_lock_in, Dir};

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    struct Tmp(PathBuf);

    impl Tmp {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "sv-platform-lock-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            Self(dir)
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn me(kind: &str) -> Holder {
        Holder {
            pid: std::process::id(),
            start: Some("t0".into()),
            kind: kind.into(),
            at: "now".into(),
        }
    }

    #[test]
    fn one_holder_at_a_time_and_released_on_drop() {
        let tmp = Tmp::new("one");
        let path = tmp.0.join("connector.lock");
        let first = try_lock(&path, &me("connector"))
            .unwrap()
            .expect("first holder");
        // A second open file description, even in this process, does not get it.
        assert!(try_lock(&path, &me("connector")).unwrap().is_none());
        assert_eq!(holder(&path), Some(me("connector")));
        drop(first);
        assert!(holder(&path).is_none(), "the record goes with the hold");
        assert!(path.exists(), "the lock file is permanent");
        assert!(try_lock(&path, &me("install")).unwrap().is_some());
    }

    #[test]
    fn the_record_is_a_sibling_and_the_lock_file_stays_empty() {
        let tmp = Tmp::new("sibling");
        let path = tmp.0.join("agents.lock");
        let _held = try_lock(&path, &me("agents")).unwrap().unwrap();
        let record = tmp.0.join("agents.lock.holder");
        let written: Holder = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
        assert_eq!(written, me("agents"));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        let names: Vec<_> = std::fs::read_dir(&tmp.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "no temporary file left: {names:?}");
    }

    #[test]
    fn an_unreadable_or_foreign_record_is_no_holder() {
        let tmp = Tmp::new("garbage");
        let path = tmp.0.join("x.lock");
        std::fs::write(tmp.0.join("x.lock.holder"), b"not json").unwrap();
        assert!(holder(&path).is_none());
        std::fs::write(tmp.0.join("x.lock.holder"), vec![b' '; 5000]).unwrap();
        assert!(holder(&path).is_none());
    }

    /// Run by `a_lock_is_released_when_its_holder_dies` in a child process of this same test binary: takes the
    /// lock, says so, and waits to be killed. Does nothing in a normal run.
    #[test]
    fn child_holds_the_lock() {
        let Some(path) = std::env::var_os("SV_LOCK_TEST_CHILD") else {
            return;
        };
        let _held = try_lock(Path::new(&path), &me("child")).unwrap().unwrap();
        // libtest has already written "test <name> ... " on this line.
        println!("HELD");
        std::io::stdout().flush().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(60));
    }

    #[test]
    fn a_lock_is_released_when_its_holder_dies() {
        let tmp = Tmp::new("death");
        let path = tmp.0.join("agents.lock");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "platform::lock::tests::child_holds_the_lock",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("SV_LOCK_TEST_CHILD", &path)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        assert!(
            lines.any(|line| line.unwrap().ends_with("HELD")),
            "the child took the lock"
        );
        assert!(try_lock(&path, &me("agents")).unwrap().is_none());
        assert_eq!(holder(&path).unwrap().pid, child.id());
        child.kill().unwrap();
        child.wait().unwrap();
        let mine = try_lock(&path, &me("agents")).unwrap();
        assert!(mine.is_some(), "the kernel let go with its holder");
        assert_eq!(
            holder(&path).unwrap().pid,
            std::process::id(),
            "and the dead holder's record is replaced"
        );
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::fs::{symlink, PermissionsExt};

        #[test]
        fn created_private_and_a_permanent_inode() {
            use std::os::unix::fs::MetadataExt;
            let tmp = Tmp::new("mode");
            let path = tmp.0.join("connector.lock");
            let first = try_lock(&path, &me("connector")).unwrap().unwrap();
            assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
            assert_eq!(
                std::fs::metadata(tmp.0.join("connector.lock.holder"))
                    .unwrap()
                    .mode()
                    & 0o777,
                0o600
            );
            let inode = std::fs::metadata(&path).unwrap().ino();
            drop(first);
            let _second = try_lock(&path, &me("install")).unwrap().unwrap();
            assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        }

        #[test]
        fn a_lock_file_that_is_a_link_or_open_to_others_is_refused() {
            let tmp = Tmp::new("unsafe");
            let target = tmp.0.join("target");
            std::fs::write(&target, b"").unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
            let link = tmp.0.join("link.lock");
            symlink(&target, &link).unwrap();
            assert!(try_lock(&link, &me("connector")).is_err());
            let open = tmp.0.join("open.lock");
            std::fs::write(&open, b"").unwrap();
            std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(try_lock(&open, &me("connector")).is_err());
        }

        #[test]
        fn a_hard_linked_lock_refuses_without_changing_the_outside_file() {
            let tmp = Tmp::new("hard-link");
            let outside = tmp.0.join("outside");
            std::fs::write(&outside, b"sentinel content").unwrap();
            std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o600)).unwrap();
            let lock = tmp.0.join("install.lock");
            std::fs::hard_link(&outside, &lock).unwrap();
            assert!(try_lock(&lock, &me("uninstall")).is_err());
            assert_eq!(std::fs::read(&outside).unwrap(), b"sentinel content");
        }

        #[test]
        fn a_planted_record_link_is_replaced_not_followed() {
            let tmp = Tmp::new("record-link");
            let victim = tmp.0.join("victim");
            std::fs::write(&victim, b"keep").unwrap();
            let path = tmp.0.join("connector.lock");
            symlink(&victim, tmp.0.join("connector.lock.holder")).unwrap();
            assert!(holder(&path).is_none(), "a link is no record");
            let held = try_lock(&path, &me("connector")).unwrap().unwrap();
            assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
            assert!(holder(&path).is_some());
            drop(held);
            assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        }

        #[test]
        fn locking_relative_to_a_pinned_directory_writes_no_record() {
            use std::os::fd::OwnedFd;
            let tmp = Tmp::new("in");
            let dir: OwnedFd = File::open(&tmp.0).unwrap().into();
            let first = try_lock_in(&dir, "install.lock").unwrap().unwrap();
            assert!(try_lock_in(&dir, "install.lock").unwrap().is_none());
            assert!(try_lock(&tmp.0.join("install.lock"), &me("x"))
                .unwrap()
                .is_none());
            drop(first);
            assert!(!tmp.0.join("install.lock.holder").exists());
            assert!(try_lock_in(&dir, "install.lock").unwrap().is_some());
        }
    }
}
