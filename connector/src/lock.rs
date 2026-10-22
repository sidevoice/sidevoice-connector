//! Locks the OS holds and gives up the moment their holder dies, however it dies (`platform::lock`): exclusive,
//! non-blocking, taken natively (no external `flock` program). Nothing is ever stale, so nothing is ever reclaimed,
//! and no process judges another gone and takes its place.
//!
//! This is the host-agnostic side: who the holder is (pid, start time, kind) and how long to wait. The holder's
//! record sits beside the lock file, for people and for teardown, which signals that process only as its verified
//! self (`process.rs`). That record is information, never the lock.

use crate::platform::lock::{self as os, Holder};
use anyhow::Result;
use std::path::Path;
use std::time::Duration;

/// A held lock; released (and its holder record removed) when dropped or when this process dies.
pub use os::Guard as Lock;

/// Tries once: the lock, or `None` when another holder has it.
pub fn try_lock(path: &Path, kind: &str) -> Result<Option<Lock>> {
    os::try_lock(
        path,
        &Holder {
            pid: std::process::id(),
            start: crate::process::self_identity().map(|identity| identity.start),
            kind: kind.to_owned(),
            at: crate::logfile::timestamp(),
        },
    )
}

/// Tries `attempt` every 20 ms until it gives a lock or `limit` passes.
async fn poll(
    limit: Duration,
    mut attempt: impl FnMut() -> Result<Option<Lock>>,
) -> Result<Option<Lock>> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if let Some(lock) = attempt()? {
            return Ok(Some(lock));
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

/// The install lock relative to the pinned data directory, for uninstall.
#[cfg(unix)]
pub async fn wait_uninstall_lock_at(parent: &os::Dir, limit: Duration) -> Result<Option<Lock>> {
    poll(limit, || os::try_lock_in(parent, "install.lock")).await
}

/// The agents lock relative to the pinned data directory, for uninstall.
#[cfg(unix)]
pub async fn wait_uninstall_agents_lock_at(
    parent: &os::Dir,
    limit: Duration,
) -> Result<Option<Lock>> {
    poll(limit, || os::try_lock_in(parent, "agents.lock")).await
}

/// The holder's record, when there is a holder that wrote one: information only.
pub fn holder(path: &Path) -> Option<Holder> {
    os::holder(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure_fs::tests::Scratch;

    #[test]
    fn the_record_says_who_and_what_for() {
        let scratch = Scratch::new("lock");
        let path = scratch.0.join("connector.lock");
        let first = try_lock(&path, "connector").unwrap().expect("first holder");
        assert!(try_lock(&path, "connector").unwrap().is_none());
        let record = holder(&path).expect("holder record");
        assert_eq!(record.pid, std::process::id());
        assert_eq!(record.kind, "connector");
        drop(first);
        assert!(holder(&path).is_none(), "the record goes with the hold");
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
}
