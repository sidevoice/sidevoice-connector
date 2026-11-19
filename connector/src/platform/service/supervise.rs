//! How the guard (`run-guarded`, the program launchd runs for a job) waits for the job's program, and how it ends.
//!
//! The guard is the job's main process for the manager, but the program runs in a group of its own
//! ([`crate::platform::process::Processes::spawn_tree`]), and a manager that stops a job signals the main process
//! (and its group) only. A guard that died of that signal would take nothing with it: the program would live on,
//! holding its port and lock. So on Unix the guard handles `SIGTERM` and `SIGINT`: it passes the signal on to the
//! program's group, gives it [`GRACE`] to leave, ends the tree if it does not, and only then leaves itself.
//!
//! And it ends the way its program did: a program that died of a signal is a guard that dies of the same signal
//! ([`raise`], after the cleanup), so that the manager records the real cause (launchd's last terminating signal)
//! and not an exit 1. On Windows a program has an exit code and nothing else, and the guard exits with it.

use crate::platform::process::Child;
use std::io;
use std::time::Duration;

/// How long the program has to leave after it was asked to, before its tree is ended.
pub const GRACE: Duration = Duration::from_secs(10);

/// How the program ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    Code(i32),
    /// Killed by this signal (Unix).
    Signal(i32),
}

#[cfg(unix)]
#[path = "supervise/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "supervise/windows.rs"]
mod imp;

/// The signals a guard answers, installed before the program starts so that none arrives unhandled.
pub struct Signals(imp::Signals);

impl Signals {
    pub fn install() -> io::Result<Signals> {
        imp::Signals::install().map(Signals)
    }

    /// Waits for `child`. A stop signal is passed on to its group, and the child waited for for [`GRACE`] (its tree
    /// ended after that); the result is how the child ended.
    pub async fn supervise(&mut self, child: &mut Child) -> io::Result<Ended> {
        self.0.supervise(child).await
    }
}

/// Dies of `signal`, as the program did (Unix; nothing on Windows): the handler is put back to its default first.
/// Returns only where the signal did not end the process.
pub fn raise(signal: i32) {
    imp::raise(signal);
}

#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::platform::process::{ChildProcess, Processes};
    use tokio::process::Command;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[tokio::test]
    async fn a_program_ends_with_its_code_or_its_signal() {
        let mut signals = Signals::install().unwrap();
        for (script, ended) in [
            ("exit 3", Ended::Code(3)),
            ("kill -SEGV $$", Ended::Signal(libc::SIGSEGV)),
        ] {
            let mut child = Processes::spawn_tree(shell(script)).await.unwrap();
            assert_eq!(signals.supervise(&mut child).await.unwrap(), ended);
        }
    }

    #[tokio::test]
    async fn a_stop_signal_is_passed_to_the_programs_group_and_waited_for() {
        let mut signals = Signals::install().unwrap();
        let mut child = Processes::spawn_tree(shell("sleep 60 & wait"))
            .await
            .unwrap();
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let me = rustix::process::getpid();
            rustix::process::kill_process(me, rustix::process::Signal::TERM).unwrap();
        });
        let started = std::time::Instant::now();
        let ended = signals.supervise(&mut child).await.unwrap();
        assert_eq!(ended, Ended::Signal(libc::SIGTERM), "it left of the signal");
        assert!(started.elapsed() < GRACE, "it was not left to the deadline");
    }

    #[test]
    #[ignore = "a child of the test below, not a test"]
    fn dies_of_the_signal_it_is_given() {
        raise(libc::SIGTERM);
    }

    #[test]
    fn the_guard_dies_of_the_signal_its_program_died_of() {
        use std::os::unix::process::ExitStatusExt;
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "platform::service::supervise::tests::dies_of_the_signal_it_is_given",
            ])
            .status()
            .unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM));
    }
}
