//! Locks the kernel holds and gives up the moment their holder dies, however it dies: `flock(2)` on a lock file,
//! exclusive and non-blocking, taken natively (no external `flock` program). Nothing is ever stale, so nothing is
//! ever reclaimed, and no process judges another gone and takes its place.
//!
//! Lock files are permanent inodes: created once, never replaced, never deleted (a holder of a deleted inode and a
//! newcomer on its replacement would both hold "the" lock). A flock belongs to the open file description, so a
//! second open of the same file, even in this process, does not get it while the first is held.
//!
//! The holder writes who it is (pid, start time, kind) into the locked file itself, for people and for teardown,
//! which signals that process only as its verified self (`process.rs`). That record is information, never the lock.

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::Duration;

/// A held lock; released (and its holder record cleared) when dropped or when this process dies.
#[derive(Debug)]
pub struct Lock {
    file: File,
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.file.set_len(0);
        let _ = FileExt::unlock(&self.file);
    }
}

fn contended(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

/// Opens (creating it 0600) the lock file without following a link, and checks the descriptor it opened: a regular
/// file of this user's, open to nobody else.
fn open(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("open lock {}", path.display()))?;
    let m = file.metadata()?;
    if !m.is_file() || m.uid() != crate::secure_fs::uid() || m.mode() & 0o077 != 0 {
        bail!("unsafe lock file {}", path.display());
    }
    Ok(file)
}

/// Tries once: the lock, or `None` when another holder has it.
pub fn try_lock(path: &Path, kind: &str) -> Result<Option<Lock>> {
    let mut file = open(path)?;
    match file.try_lock_exclusive() {
        Ok(()) => {}
        Err(error) if contended(&error) => return Ok(None),
        Err(error) => return Err(error).context(format!("lock {}", path.display())),
    }
    let record = json!({"pid":std::process::id(),
        "start":crate::process::self_identity().map(|identity| identity.start),
        "kind":kind,"at":crate::logfile::timestamp()});
    // Into the locked file itself: a replaced file would leave the lock on an inode nobody else opens.
    let _ = file
        .set_len(0)
        .and_then(|_| file.seek(SeekFrom::Start(0)).map(|_| ()))
        .and_then(|_| file.write_all(record.to_string().as_bytes()));
    Ok(Some(Lock { file }))
}

/// The lock, waiting for it up to `limit`; `None` when another holder kept it all that time.
// For the install lock of the service and install commands, which wait their turn.
#[cfg_attr(not(test), allow(dead_code))]
pub async fn wait_lock(path: &Path, kind: &str, limit: Duration) -> Result<Option<Lock>> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if let Some(lock) = try_lock(path, kind)? {
            return Ok(Some(lock));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The holder's record, when there is a holder that wrote one: information only.
pub fn holder(path: &Path) -> Option<Value> {
    let bytes = crate::secure_fs::read_trusted(path, 4096).ok()??;
    let record: Value = serde_json::from_slice(&bytes).ok()?;
    record.get("pid")?.as_u64()?;
    Some(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure_fs::tests::Scratch;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn one_holder_at_a_time_and_released_on_drop() {
        let scratch = Scratch::new("lock");
        let path = scratch.0.join("connector.lock");
        let first = try_lock(&path, "connector").unwrap().expect("first holder");
        assert_eq!(
            std::fs::metadata(&path).unwrap().mode() & 0o777,
            0o600,
            "created private"
        );
        // A second open file description, even in this process, does not get it.
        assert!(try_lock(&path, "connector").unwrap().is_none());
        let record = holder(&path).expect("holder record");
        assert_eq!(record["pid"], std::process::id());
        assert_eq!(record["kind"], "connector");
        let inode = std::fs::metadata(&path).unwrap().ino();
        drop(first);
        assert!(holder(&path).is_none(), "the record goes with the hold");
        let _second = try_lock(&path, "install").unwrap().expect("free again");
        assert_eq!(
            std::fs::metadata(&path).unwrap().ino(),
            inode,
            "the lock file is a permanent inode"
        );
    }

    #[tokio::test]
    async fn waiting_gets_the_lock_once_the_holder_lets_go() {
        let scratch = Scratch::new("wait");
        let path = scratch.0.join("install.lock");
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

    #[test]
    fn a_lock_is_released_when_its_holder_dies() {
        // A forked child takes the lock on a descriptor nobody else shares once the parent closes its copy, then is
        // killed: the kernel releases the lock and nobody reclaims anything. The child calls only async-signal-safe
        // functions.
        let scratch = Scratch::new("death");
        let path = scratch.0.join("agents.lock");
        drop(open(&path).unwrap());
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        let mut pipe = [0i32; 2];
        unsafe {
            assert_eq!(libc::pipe(pipe.as_mut_ptr()), 0);
            let fd = libc::open(c_path.as_ptr(), libc::O_RDWR);
            assert!(fd >= 0);
            let pid = libc::fork();
            assert!(pid >= 0);
            if pid == 0 {
                let held = libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) == 0;
                let byte = [u8::from(held)];
                libc::write(pipe[1], byte.as_ptr().cast(), 1);
                loop {
                    libc::pause();
                }
            }
            libc::close(fd);
            libc::close(pipe[1]);
            let mut byte = [0u8; 1];
            assert_eq!(libc::read(pipe[0], byte.as_mut_ptr().cast(), 1), 1);
            libc::close(pipe[0]);
            assert_eq!(byte[0], 1, "the child took the lock");
            assert!(try_lock(&path, "agents").unwrap().is_none());
            libc::kill(pid, libc::SIGKILL);
            let mut status = 0;
            libc::waitpid(pid, &mut status, 0);
        }
        assert!(try_lock(&path, "agents").unwrap().is_some());
    }

    #[test]
    fn a_lock_file_that_is_a_link_or_open_to_others_is_refused() {
        let scratch = Scratch::new("unsafe");
        let target = scratch.0.join("target");
        std::fs::write(&target, b"").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = scratch.0.join("link.lock");
        symlink(&target, &link).unwrap();
        assert!(try_lock(&link, "connector").is_err());
        let open = scratch.0.join("open.lock");
        std::fs::write(&open, b"").unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(try_lock(&open, "connector").is_err());
    }
}
