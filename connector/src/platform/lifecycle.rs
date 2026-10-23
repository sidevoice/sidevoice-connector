//! Being asked to stop: what the daemon waits for besides its own work.
//!
//! The interface is the [`Lifecycle`] trait; [`Lifecycles`] is the one OS implementation compiled in, chosen below.
//! Callers import both: `use crate::platform::lifecycle::{Lifecycle, Lifecycles};` then
//! `let stop = Lifecycles::stop_requests()?;`.
//!
//! On every OS, another process's request that this one leave, as `sidevoice-platform`'s `stop` hears it (what
//! [`Process::stop`](super::process::Process::stop) sends): `SIGTERM` on Unix (also what launchd and systemd send),
//! this process's stop event on Windows, which reaches a Task Scheduler task too. Besides it, the console's: `SIGINT`
//! (^C) on Unix; on Windows ^C, the console closing, the machine shutting down and the user logging off.

#[cfg(unix)]
#[path = "lifecycle/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "lifecycle/windows.rs"]
mod imp;

pub use imp::Os as Lifecycles;

use std::future::Future;
use std::io;

/// What every OS implements ([`Lifecycles`], in `lifecycle/unix.rs` and `lifecycle/windows.rs`).
pub trait Lifecycle {
    /// Starts listening for the OS's requests to stop this process, and returns what resolves when one arrives.
    /// Listening begins here, not when the future is first polled: a request that comes in between is not lost, and
    /// the OS's default action (the process ends at once) no longer applies from here on. Every request after the
    /// first one is ignored with it. An error when the OS refuses the registration.
    fn stop_requests() -> io::Result<impl Future<Output = ()> + Send>;
}

#[cfg(test)]
mod tests {
    use super::{Lifecycle, Lifecycles};
    use std::time::Duration;

    /// The request is process-wide: listening and asking happen in a child process of their own (this test binary,
    /// running only the case below), so that no other test's listener or daemon sees it and nothing here races.
    #[test]
    fn listening_waits_for_a_request_and_a_request_resolves_it() {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "platform::lifecycle::tests::listening_child",
            ])
            .env("SIDEVOICE_TEST_LIFECYCLE_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "the child said {}: {}",
            child.status,
            String::from_utf8_lossy(&child.stdout)
        );
    }

    /// What the test above runs. Nobody has asked: the future waits, and listening does not by itself stop anything.
    /// Then another process's request, as `Process::stop` sends it (`SIGTERM`, or the stop event), here sent by this
    /// process to itself; the console events need a real console and are checked on a Windows runner.
    #[tokio::test]
    #[ignore = "a child of the test above, not a test"]
    async fn listening_child() {
        use sidevoice_platform::stop::{StopRequest, StopRequests};
        assert!(std::env::var_os("SIDEVOICE_TEST_LIFECYCLE_CHILD").is_some());
        let stop = Lifecycles::stop_requests().unwrap();
        tokio::pin!(stop);
        assert!(tokio::time::timeout(Duration::from_millis(100), &mut stop)
            .await
            .is_err());
        assert!(StopRequests::ask(std::process::id()).unwrap());
        tokio::time::timeout(Duration::from_secs(5), stop)
            .await
            .expect("the request was lost");
    }
}
