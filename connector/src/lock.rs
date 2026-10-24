//! Locks the OS holds and gives up the moment their holder dies, however it dies: an exclusive, non-blocking lock on
//! a lock file, taken with std's `File::try_lock` (`flock(2)` on Unix, `LockFileEx` on Windows), so this module is
//! the same on every OS. Nothing is ever stale, so nothing is ever reclaimed, and no process judges another gone and
//! takes its place.
//!
//! Uninstall may unlink a lock while holding it. Acquirers verify the path still names the locked file,
//! so a waiter on the retired file cannot proceed alongside a holder of its replacement. The lock belongs to the open handle, so a second open of the same
//! file, even in this process, does not get it while the first is held. A lock file must be private, this user's,
//! with no other name (`platform::fs`), checked before and after the lock is taken.
//!
//! Who holds it (pid, start time, kind) is written beside it, `<lock file>.holder`, whole, after the lock is taken,
//! and removed when the lock is let go. Never inside the locked file: Windows locks are mandatory, so nobody else
//! could read it. The record is for people and for teardown, which stops that process only as its verified self
//! (`platform::process`). It is information, never the lock: after a crash it outlives its holder until the next
//! holder replaces it, so a reader trusts the lock first and the record second.

use crate::platform::fs::{Create, Files, Fs};
use crate::platform::process::{Process, Processes};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Who holds a lock, as it says itself. Read as information only; fields are added, never renamed or removed.
#[derive(Debug, Serialize, Deserialize)]
pub struct Holder {
    pub pid: u32,
    /// The holder's start time (`platform::process`), when it could be read.
    pub start: Option<String>,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub at: String,
}

/// A held lock; released (and its holder record removed) when dropped or when this process dies.
#[derive(Debug)]
pub struct Lock {
    file: File,
    record: Option<PathBuf>,
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Still held here, so the record removed is this holder's.
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
    name.into()
}

/// Takes the lock on `file` if nobody holds it. The file is checked before and after, so a name added meanwhile is
/// refused as well.
fn take(file: &File, path: &Path) -> Result<bool> {
    let label = path.display();
    let check = || Files::check_private(file).with_context(|| format!("unsafe lock file {label}"));
    if !names_file(file, path)? {
        return Ok(false);
    }
    check()?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(false),
        Err(TryLockError::Error(error)) => return Err(error).context(format!("lock {label}")),
    }
    if !names_file(file, path)? {
        file.unlock()?;
        return Ok(false);
    }
    check()?;
    Ok(true)
}

fn names_file(file: &File, path: &Path) -> Result<bool> {
    let locked = same_file::Handle::from_file(file.try_clone()?)?;
    match same_file::Handle::from_path(path) {
        Ok(current) => Ok(locked == current),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Tries once: the lock, or `None` when another holder has it.
pub fn try_lock(path: &Path, kind: &str) -> Result<Option<Lock>> {
    let file = Files::open_private(path, Create::ReadWrite)
        .with_context(|| format!("open lock {}", path.display()))?;
    if !take(&file, path)? {
        return Ok(None);
    }
    let pid = std::process::id();
    let holder = Holder {
        pid,
        start: Processes::identity(pid).map(|identity| identity.start),
        kind: kind.to_owned(),
        at: crate::logfile::timestamp(),
    };
    let record = record_path(path);
    // Information only: a lock held without its record is still held.
    let written = Files::write_json(&record, &holder);
    Ok(Some(Lock {
        file,
        record: written.is_ok().then_some(record),
    }))
}

/// Tries `attempt` every 20 ms until it succeeds or `limit` passes.
async fn poll<T>(
    limit: Duration,
    mut attempt: impl FnMut() -> Result<Option<T>>,
) -> Result<Option<T>> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if let Some(done) = attempt()? {
            return Ok(Some(done));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The lock, waiting for it up to `limit`; `None` when another holder kept it all that time.
// For the install lock of the service and install commands, which wait their turn.
#[cfg_attr(not(test), allow(dead_code))]
pub async fn wait_lock(path: &Path, kind: &str, limit: Duration) -> Result<Option<Lock>> {
    poll(limit, || try_lock(path, kind)).await
}

/// The lock on a lock file the caller opened itself, waiting for it up to `limit`. For uninstall, which opens it in
/// the data directory it pinned, so a link added after its checks cannot be used; no record is written, nothing is
/// added to a directory about to be removed.
pub async fn wait_lock_file(file: File, path: &Path, limit: Duration) -> Result<Option<Lock>> {
    let mut file = Some(file);
    poll(limit, || {
        let opened = match file.take() {
            Some(file) => file,
            None => Files::open_private(path, Create::ReadWrite)?,
        };
        Ok(take(&opened, path)?.then_some(Lock {
            file: opened,
            record: None,
        }))
    })
    .await
}

/// The holder's record, when there is a holder that wrote one: information only.
pub fn holder(path: &Path) -> Option<Holder> {
    let bytes = Files::read_trusted(&record_path(path), 4096).ok()??;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    use tempfile::TempDir;

    /// A private directory, as the data directory is (the record is written only in one), inside a scratch one.
    fn private_dir(label: &str) -> (TempDir, PathBuf) {
        let tmp = crate::test_support::temp_dir(label);
        let dir = tmp.path().join("data");
        Files::create_private_dir(&dir).unwrap();
        (tmp, dir)
    }

    #[tokio::test]
    async fn a_waiter_on_a_retired_file_retries_the_current_lock() {
        let (_tmp, dir) = private_dir("retired-lock");
        let path = dir.join("install.lock");
        let first = try_lock(&path, "uninstall").unwrap().unwrap();
        let waiter = Files::open_private(&path, Create::ReadWrite).unwrap();
        // Where the OS allows unlinking a held file, reproduce uninstall retiring it
        // while an installer already has that inode open.
        if std::fs::remove_file(&path).is_err() {
            return;
        }
        let third = try_lock(&path, "install").unwrap().unwrap();
        drop(first);
        assert!(
            !take(&waiter, &path).unwrap(),
            "retired inode cannot grant the lock"
        );
        assert!(wait_lock_file(waiter, &path, Duration::ZERO)
            .await
            .unwrap()
            .is_none());
        assert!(try_lock(&path, "install").unwrap().is_none());
        drop(third);
        assert!(try_lock(&path, "install").unwrap().is_some());
    }

    #[test]
    fn one_holder_at_a_time_and_released_on_drop() {
        let (_tmp, dir) = private_dir("lock");
        let path = dir.join("connector.lock");
        let first = try_lock(&path, "connector").unwrap().expect("first holder");
        Files::check_private(&Files::open_read(&path).unwrap()).expect("created private");
        // A second open of the same file, even in this process, does not get it.
        assert!(try_lock(&path, "connector").unwrap().is_none());
        let record = holder(&path).expect("holder record");
        assert_eq!(record.pid, std::process::id());
        assert_eq!(record.kind, "connector");
        assert!(record.start.is_some());
        drop(first);
        assert!(holder(&path).is_none(), "the record goes with the hold");
        assert!(path.exists(), "the lock file is permanent");
        let _second = try_lock(&path, "install").unwrap().expect("free again");
        assert_eq!(holder(&path).unwrap().kind, "install");
    }

    #[tokio::test]
    async fn waiting_gets_the_lock_once_the_holder_lets_go() {
        let (_tmp, dir) = private_dir("wait");
        let path = dir.join("install.lock");
        let held = try_lock(&path, "install").unwrap().unwrap();
        assert!(wait_lock(&path, "install", Duration::from_millis(100))
            .await
            .unwrap()
            .is_none());
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            drop(held);
        });
        assert!(wait_lock(&path, "install", Duration::from_secs(5))
            .await
            .unwrap()
            .is_some());
        release.await.unwrap();
    }

    #[tokio::test]
    async fn a_lock_on_an_opened_file_writes_no_record() {
        let (_tmp, dir) = private_dir("file");
        let path = dir.join("install.lock");
        let open = || Files::open_private(&path, Create::ReadWrite).unwrap();
        let held = wait_lock_file(open(), &path, Duration::ZERO)
            .await
            .unwrap()
            .expect("free");
        assert!(try_lock(&path, "install").unwrap().is_none());
        assert!(wait_lock_file(open(), &path, Duration::ZERO)
            .await
            .unwrap()
            .is_none());
        assert!(!record_path(&path).exists());
        drop(held);
        assert!(try_lock(&path, "install").unwrap().is_some());
    }

    #[test]
    fn a_lock_file_with_another_name_is_refused_without_changing_it() {
        let (_tmp, dir) = private_dir("hard-link-lock");
        let outside = dir.join("outside");
        Files::write_private(&outside, b"sentinel content").unwrap();
        let lock = dir.join("install.lock");
        std::fs::hard_link(&outside, &lock).unwrap();
        assert!(try_lock(&lock, "uninstall").is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"sentinel content");
    }

    #[test]
    fn an_unreadable_record_is_no_holder() {
        let (_tmp, dir) = private_dir("garbage");
        let path = dir.join("x.lock");
        Files::write_private(&record_path(&path), b"not json").unwrap();
        assert!(holder(&path).is_none());
        Files::write_private(&record_path(&path), &[b' '; 5000]).unwrap();
        assert!(holder(&path).is_none(), "oversized");
    }

    /// What `a_lock_is_released_when_its_holder_dies` runs in a child process (this test binary): takes the lock,
    /// says so, and waits to be killed.
    #[test]
    #[ignore = "a child of the test below, not a test"]
    fn holder_child() {
        let path = std::env::var_os("SV_LOCK_TEST_PATH").unwrap();
        let _held = try_lock(Path::new(&path), "child").unwrap().unwrap();
        // libtest has already written "test <name> ... " on this line.
        println!("HELD");
        std::io::stdout().flush().unwrap();
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    fn a_lock_is_released_when_its_holder_dies() {
        let (_tmp, dir) = private_dir("death");
        let path = dir.join("agents.lock");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "lock::tests::holder_child",
                "--nocapture",
            ])
            .env("SV_LOCK_TEST_PATH", &path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        assert!(
            lines.any(|line| line.unwrap().ends_with("HELD")),
            "the child took the lock"
        );
        assert!(try_lock(&path, "agents").unwrap().is_none());
        assert_eq!(holder(&path).unwrap().pid, child.id());
        child.kill().unwrap();
        child.wait().unwrap();
        // Nothing reclaimed it: the OS let go with its holder.
        let _mine = try_lock(&path, "agents")
            .unwrap()
            .expect("free once the holder died");
        assert_eq!(
            holder(&path).unwrap().pid,
            std::process::id(),
            "and the dead holder's record is replaced"
        );
    }
}
