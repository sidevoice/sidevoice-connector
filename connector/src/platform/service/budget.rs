//! The restart budget every manager keeps a crashing job to: at most [`LIMIT`] starts in [`WINDOW`] (systemd's
//! `StartLimitBurst=5` in `StartLimitIntervalSec=600`), then the job stays down until a person or the app starts it
//! again, or the window has passed (the next login). systemd enforces it itself; launchd and Task Scheduler have no
//! cap, so their jobs run through [`guarded`], which keeps the starts in a small file. Over the budget it records
//! why in that file and in the job's log, and exits 0: launchd's `SuccessfulExit=false` and Task Scheduler's
//! `RestartOnFailure` bring a job back only after a failure, so an exit 0 stops them. A start by a person or the app
//! ([`reset`]) clears the file. A run that lasts the whole window leaves nothing of its start in it, so a job that
//! ran cleanly for [`WINDOW`] begins again with a full budget.

use serde::{Deserialize, Serialize};
use std::future::Future;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Starts allowed in a [`WINDOW`].
pub const LIMIT: usize = 5;
/// The window, in seconds.
pub const WINDOW: u64 = 600;

/// What a job's budget file holds: the starts (Unix seconds) still in the window, and why the job is down, if it is.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Restarts {
    pub starts: Vec<u64>,
    pub down: Option<Down>,
}

/// Why a job is down: when the budget ran out, and what happened.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Down {
    pub at: u64,
    pub reason: String,
}

/// Whether a start is allowed.
#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    Run,
    /// Over the budget; the window has room again in `retry_after` seconds.
    Down {
        retry_after: u64,
    },
}

impl Restarts {
    /// A start at `now` (Unix seconds), counted when the budget has room. A start recorded in the future (the clock
    /// went back) is not trusted and is forgotten, so a clock set back never keeps a job down.
    pub fn admit(&mut self, now: u64) -> Admission {
        self.starts
            .retain(|start| *start <= now && now - *start < WINDOW);
        if self.starts.len() >= LIMIT {
            let oldest = self.starts.iter().copied().min().unwrap_or(now);
            if self.down.is_none() {
                self.down = Some(Down {
                    at: now,
                    reason: format!(
                        "it started {LIMIT} times in {} minutes and stopped each time",
                        WINDOW / 60
                    ),
                });
            }
            return Admission::Down {
                retry_after: WINDOW - (now - oldest),
            };
        }
        self.starts.push(now);
        self.down = None;
        Admission::Run
    }
}

/// The budget file as it is now: nothing there, or nothing readable, is a full budget.
pub fn load(file: &Path) -> Restarts {
    std::fs::read(file)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save(file: &Path, restarts: &Restarts) -> io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temp = file.with_extension("tmp");
    std::fs::write(&temp, serde_json::to_vec_pretty(restarts)?)?;
    std::fs::rename(temp, file)
}

/// A person or the app starts the job: a full budget again.
pub fn reset(file: &Path) {
    let _lock = lock(file);
    let _ = std::fs::remove_file(file);
}

/// An advisory lock on the budget, held while it is read, changed and written: the guard of a job and the app's
/// `reset` (or a second guard, the moment a manager starts one before the old one has left) do not lose each other's
/// writes. It is a file beside the budget, so that the budget itself can be replaced and removed; the lock goes with
/// the handle. A lock that cannot be taken does not stop anything: the budget then is as unguarded as it was.
fn lock(file: &Path) -> Option<std::fs::File> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).ok()?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(file.with_extension("lock"))
        .ok()?;
    lock.lock().ok()?;
    Some(lock)
}

/// Seconds since the Unix epoch.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// `run` (the job, its exit code) when the budget in `file` has room at `now`; otherwise it is not run, the reason is
/// written to `said` and to the file, and the exit is 0 so that the manager does not bring the job back. A budget
/// file that cannot be written does not stop the job: a job that cannot be counted still runs.
pub async fn guarded<F, Fut>(
    file: &Path,
    now: u64,
    said: &mut impl io::Write,
    run: F,
) -> io::Result<i32>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = io::Result<i32>>,
{
    let (restarts, admission) = {
        let _lock = lock(file);
        let mut restarts = load(file);
        let admission = restarts.admit(now);
        let _ = save(file, &restarts);
        (restarts, admission)
    };
    match admission {
        Admission::Run => run().await,
        Admission::Down { retry_after } => {
            let reason = restarts.down.as_ref().map_or("", |down| &down.reason);
            let _ = writeln!(
                said,
                "not started: {reason}. It stays down until it is started again (the app, or `sidevoice-connector service start`), or until the next login after {retry_after} s."
            );
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sixth_start_in_the_window_is_refused_and_says_why() {
        let mut restarts = Restarts::default();
        for second in 0..LIMIT as u64 {
            assert_eq!(restarts.admit(1_000 + second * 10), Admission::Run);
        }
        assert!(restarts.down.is_none());
        assert_eq!(restarts.admit(1_100), Admission::Down { retry_after: 500 });
        assert_eq!(restarts.starts.len(), LIMIT, "a refusal is not a start");
        assert!(restarts
            .down
            .as_ref()
            .unwrap()
            .reason
            .contains("5 times in 10 minutes"));
    }

    #[test]
    fn starts_leave_the_window_and_the_budget_returns() {
        let mut restarts = Restarts::default();
        for _ in 0..LIMIT {
            restarts.admit(1_000);
        }
        assert_eq!(
            restarts.admit(1_000 + WINDOW - 1),
            Admission::Down { retry_after: 1 }
        );
        assert_eq!(restarts.admit(1_000 + WINDOW), Admission::Run);
        assert!(restarts.down.is_none(), "running again clears the reason");
    }

    #[test]
    fn a_clock_set_back_does_not_keep_a_job_down() {
        let mut restarts = Restarts::default();
        for _ in 0..LIMIT {
            restarts.admit(50_000);
        }
        assert_eq!(restarts.admit(1_000), Admission::Run);
        assert_eq!(restarts.starts, [1_000]);
    }

    #[tokio::test]
    async fn a_job_over_budget_is_not_run_and_exits_clean_until_it_is_reset() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub/job.restarts.json");
        let mut said = Vec::new();
        for _ in 0..LIMIT {
            let code = guarded(&file, 1_000, &mut said, || async { Ok(3) }).await;
            assert_eq!(code.unwrap(), 3);
        }
        let code = guarded(&file, 1_001, &mut said, || async { panic!("not run") }).await;
        assert_eq!(code.unwrap(), 0);
        let said = String::from_utf8(said).unwrap();
        assert!(
            said.contains("not started") && said.contains("stays down"),
            "{said}"
        );
        assert!(
            load(&file).down.is_some(),
            "the state is there for the app to show"
        );
        reset(&file);
        let code = guarded(&file, 1_002, &mut Vec::new(), || async { Ok(7) }).await;
        assert_eq!(code.unwrap(), 7);
    }

    #[tokio::test]
    async fn an_unreadable_budget_file_is_a_full_budget() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("job.json");
        std::fs::write(&file, "not json").unwrap();
        let code = guarded(&file, 1, &mut Vec::new(), || async { Ok(0) }).await;
        assert_eq!(code.unwrap(), 0);
        assert_eq!(load(&file).starts, [1]);
    }
}
