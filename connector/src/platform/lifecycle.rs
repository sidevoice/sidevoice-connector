//! Being asked to stop: what the daemon waits for besides its own work.
//!
//! The interface is the [`Lifecycle`] trait; [`Lifecycles`] is the one OS implementation compiled in, chosen below.
//! Callers import both: `use crate::platform::lifecycle::{Lifecycle, Lifecycles};` then
//! `let stop = Lifecycles::stop_requests()?;`.
//!
//! The request is the OS's standard one: `SIGTERM` (what launchd and systemd send to stop a service) and `SIGINT`
//! (^C) on Unix; on Windows ^C, the console closing, the machine shutting down and the user logging off. A Task
//! Scheduler task is ended with a hard stop that no process sees: for that, and for another process that has no
//! console to signal, the request is made in band, over the local channel (the daemon's `shutdown` method, the core's
//! `POST /api/local/shutdown`), and is not this module's.

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

    /// Nobody has asked: the future waits, and listening does not by itself stop anything.
    #[tokio::test]
    async fn without_a_request_the_future_stays_pending() {
        let stop = Lifecycles::stop_requests().unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(100), stop)
            .await
            .is_err());
    }

    /// `SIGTERM` is the one request that can be raised without a console or a session, so it is the one the test
    /// sends; the Windows requests (a console event) need a real console and are checked on a Windows runner. This
    /// is the only OS-specific case: nothing in the interface can send a request.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_request_to_stop_resolves_it() {
        use rustix::process::{getpid, kill_process, Signal};
        let stop = Lifecycles::stop_requests().unwrap();
        kill_process(getpid(), Signal::TERM).unwrap();
        tokio::time::timeout(Duration::from_secs(5), stop)
            .await
            .expect("the request was lost");
    }
}
