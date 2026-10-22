//! Processes: starting them detached or as a tree, telling who a pid is before anything is done to it, finding and
//! stopping them.
//!
//! The interface is the [`Process`] trait; [`Processes`] is the one OS implementation compiled in, chosen below.
//! What differs per OS is how a child is made to outlive its caller or to lead a tree (`process-wrap`: a process
//! group on Unix; no console, a group of its own, a Job Object on Windows) and a harness adapter's open-files
//! heuristic. Everything else has one body, the trait's default methods, over `sysinfo`, which reads the same things
//! on every OS (owner, start time, command line, parent, state) and sends the stop; there is no `/proc` or `ps` of
//! ours. Callers import both: `use crate::platform::process::{Process, Processes};` then
//! `Processes::spawn_tree(command)`, `Processes::stop(pid, |found| .., grace)`.
//!
//! A pid read from a file is only a hint: pids are reused, and a stale lock or ready file can name somebody
//! else's work. Nothing is stopped on a pid alone: [`Process::is_process`], [`Process::stop`] and
//! [`Process::kill`] act only on a process that is this user's and that the caller's check recognises (its start
//! time, its command line). A zombie (dead, not yet reaped) is no process: it will never do anything again.
//!
//! Stopping is one operation, [`Process::stop`]: ask the process to leave, give it the grace time, then kill it.
//! The request is the OS's standard one (decision "Graceful stop", 2026-10-22): `SIGTERM` on Unix; on Windows a
//! shutdown request over the local channel, which arrives with that channel (connector#222, core#165) and is the
//! caller's to make, in band, before `stop`. So on Windows this layer sends nothing: it waits the grace time, then
//! kills.

#[cfg(unix)]
#[path = "process/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "process/windows.rs"]
mod imp;

pub use imp::Os as Processes;

use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::OnceLock;
use std::time::Duration;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System, Uid, UpdateKind};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};

/// What every OS implements ([`Processes`], in `process/unix.rs` and `process/windows.rs`): the three required
/// methods. The rest is one body for every OS; an OS overrides it only if it must.
pub trait Process {
    /// `command` set to outlive its caller: its own process group (Unix), no console and its own group
    /// (Windows), so closing the terminal or pressing ^C in it does not reach it. Nothing kills it on drop.
    fn detached(command: Command) -> CommandWrap;

    /// `command` set to lead a tree that can be stopped as a whole: a process group (Unix) or a Job Object
    /// (Windows) that holds everything it starts.
    fn tree(command: Command) -> CommandWrap;

    /// Which of `candidates` `pid` has open, as far as the OS shows this user: a question about files named in
    /// advance, never a listing of the process's handles (no OS lists those for another process in a standard way).
    /// The answer keeps the order of `candidates`; `None` when the OS cannot tell (Unix: no `/proc` and no `lsof`;
    /// Windows: no Restart Manager). Unix reads the process's descriptors; Windows asks the Restart Manager which
    /// processes hold the files.
    fn open_among(pid: u32, candidates: &[PathBuf]) -> Option<Vec<PathBuf>>;

    /// Starts `command` so that it outlives this process and its terminal. The caller owns stdio and the
    /// environment.
    fn spawn_detached(command: Command) -> impl Future<Output = io::Result<Child>> + Send {
        spawn(Self::detached(command))
    }

    /// Starts `command` as the leader of a tree that [`Child::kill_tree`] stops whole, and that goes with the
    /// [`Child`] when it is dropped.
    fn spawn_tree(command: Command) -> impl Future<Output = io::Result<Child>> + Send {
        let mut wrap = Self::tree(command);
        wrap.wrap(KillOnDrop);
        spawn(wrap)
    }

    /// Who `pid` is now, whoever owns it; `None` when there is no such process (or a zombie).
    fn identity(pid: u32) -> Option<Identity> {
        let pid = valid(pid)?;
        identity_of(read(ProcessesToUpdate::Some(&[pid])).process(pid)?)
    }

    /// Whether `pid` is, now, this user's process and `is_it` recognises it. False when it is gone, somebody
    /// else's, another process now, or cannot be told.
    fn is_process(pid: u32, is_it: impl Fn(&Identity) -> bool) -> bool {
        with_verified(pid, is_it, |_| true)
    }

    /// Stops `pid`, only while it is this user's and `is_it` recognises it: asks it to leave (`SIGTERM`; Windows has
    /// no request at this layer, see the module), waits up to `grace` for it to go (also for a request the caller
    /// already made in band), then kills it and waits up to 5 s more. Returns whether it had to be killed.
    fn stop(
        pid: u32,
        is_it: impl Fn(&Identity) -> bool + Send + Sync,
        grace: Duration,
    ) -> impl Future<Output = bool> + Send {
        async move {
            // `sysinfo` sends nothing where the OS has no such signal (Windows): the grace still runs, for a request
            // the caller made in band.
            with_verified(pid, &is_it, |process| {
                process.kill_with(Signal::Term).unwrap_or(false)
            });
            gone_within(pid, &is_it, grace).await;
            let killed = Self::kill(pid, &is_it);
            if killed {
                gone_within(pid, &is_it, Duration::from_secs(5)).await;
            }
            killed
        }
    }

    /// Stops `pid` at once (`SIGKILL`, `TerminateProcess`) only while it is this user's and `is_it` recognises it.
    /// Returns whether it was stopped.
    fn kill(pid: u32, is_it: impl Fn(&Identity) -> bool) -> bool {
        with_verified(pid, is_it, |process| process.kill())
    }

    /// This user's live processes whose command line `matches` (never this one): a process still starting, found
    /// before it has written anything that names it.
    fn find(matches: impl Fn(&str) -> bool) -> Vec<u32> {
        let system = read(ProcessesToUpdate::All);
        system
            .processes()
            .iter()
            .filter(|(pid, process)| pid.as_u32() != std::process::id() && ours(process))
            .filter_map(|(pid, process)| Some((pid.as_u32(), identity_of(process)?)))
            .filter(|(_, found)| matches(&found.command))
            .map(|(pid, _)| pid)
            .collect()
    }

    /// How many seconds `pid` has been running, when that can be read.
    fn age(pid: u32) -> Option<u64> {
        let pid = Pid::from_u32(pid);
        Some(
            read(ProcessesToUpdate::Some(&[pid]))
                .process(pid)?
                .run_time(),
        )
    }

    /// The process that started `pid`.
    fn parent(pid: u32) -> Option<u32> {
        let pid = Pid::from_u32(pid);
        Some(
            read(ProcessesToUpdate::Some(&[pid]))
                .process(pid)?
                .parent()?
                .as_u32(),
        )
    }
}

/// A process this module started.
///
/// Not `process-wrap`'s own child: its `wait` on a group or a job waits for every process the leader left behind,
/// and a leader's exit is what callers ask about. [`Child::wait`] reports the leader's exit; the tree is stopped
/// with [`Child::kill_tree`].
pub struct Child(Box<dyn ChildWrapper>);

impl Child {
    pub fn id(&self) -> Option<u32> {
        self.0.id()
    }

    /// Takes the leader's stdin (when it was piped).
    pub fn stdin(&mut self) -> Option<ChildStdin> {
        self.0.stdin().take()
    }

    /// Takes the leader's stdout (when it was piped).
    pub fn stdout(&mut self) -> Option<ChildStdout> {
        self.0.stdout().take()
    }

    /// Takes the leader's stderr (when it was piped).
    pub fn stderr(&mut self) -> Option<ChildStderr> {
        self.0.stderr().take()
    }

    // The leader alone, beneath the tree's wrapper.
    fn leader(&mut self) -> &mut dyn ChildWrapper {
        self.0.inner_mut()
    }

    /// The leader's exit, when it has exited.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.leader().try_wait()
    }

    /// Waits for the leader to exit.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.leader().wait().await
    }

    /// Kills the leader and everything it started (its whole group or job, also after the leader has exited) and
    /// reaps the leader, within 2 s.
    pub async fn kill_tree(&mut self) {
        let _ = self.0.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.leader().wait()).await;
    }
}

/// Who a process is: what a recorded pid is checked against.
pub struct Identity {
    /// Opaque start time: comparable only with another reading on the same machine.
    pub start: String,
    /// The program and its arguments, joined by spaces.
    pub command: String,
}

// A busy executable (open for writing somewhere: being replaced, or just written while a sibling process forked; std
// names it the same on every OS) runs a moment later: five tries 50 ms apart.
async fn spawn(mut wrap: CommandWrap) -> io::Result<Child> {
    let mut attempt = 0;
    loop {
        match wrap.spawn() {
            Ok(child) => return Ok(Child(child)),
            Err(error) if error.kind() == io::ErrorKind::ExecutableFileBusy && attempt < 5 => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

// 0 and 1 are never a process of ours to act on (the idle or init process, or "every process" to `kill`).
fn valid(pid: u32) -> Option<Pid> {
    (pid > 1).then(|| Pid::from_u32(pid))
}

/// The processes `pids` name, read now: owner and command line besides what is always read.
fn read(pids: ProcessesToUpdate<'_>) -> System {
    let mut system = System::new();
    system.refresh_processes_specifics(
        pids,
        true,
        ProcessRefreshKind::nothing()
            .with_user(UpdateKind::Always)
            .with_cmd(UpdateKind::Always),
    );
    system
}

/// Whether `process` belongs to the user this one runs as.
fn ours(process: &sysinfo::Process) -> bool {
    static CURRENT: OnceLock<Option<Uid>> = OnceLock::new();
    let current = CURRENT.get_or_init(|| {
        let pid = sysinfo::get_current_pid().ok()?;
        read(ProcessesToUpdate::Some(&[pid]))
            .process(pid)?
            .user_id()
            .cloned()
    });
    process
        .user_id()
        .zip(current.as_ref())
        .is_some_and(|(owner, current)| owner == current)
}

fn identity_of(process: &sysinfo::Process) -> Option<Identity> {
    if process.status() == sysinfo::ProcessStatus::Zombie {
        return None;
    }
    Some(Identity {
        start: process.start_time().to_string(),
        command: process
            .cmd()
            .iter()
            .map(|part| part.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" "),
    })
}

/// Waits up to `limit` for `pid` to be no longer the process `is_it` recognises.
async fn gone_within(pid: u32, is_it: impl Fn(&Identity) -> bool, limit: Duration) {
    let deadline = tokio::time::Instant::now() + limit;
    while with_verified(pid, &is_it, |_| true) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn with_verified(
    pid: u32,
    is_it: impl Fn(&Identity) -> bool,
    act: impl FnOnce(&sysinfo::Process) -> bool,
) -> bool {
    let Some(pid) = valid(pid) else {
        return false;
    };
    let system = read(ProcessesToUpdate::Some(&[pid]));
    let Some(process) = system.process(pid) else {
        return false;
    };
    ours(process) && identity_of(process).is_some_and(|found| is_it(&found)) && act(process)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// What the children below run: this test binary, asleep.
    #[test]
    #[ignore = "a child of the tests below, not a test"]
    fn sleeper() {
        std::thread::sleep(Duration::from_secs(30));
    }

    fn sleeper_command() -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--ignored", "--exact", "platform::process::tests::sleeper"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        command
    }

    /// A child's identity, once the OS shows its command line.
    async fn sleeper_identity(pid: u32) -> Identity {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Processes::identity(pid) {
                Some(found) if found.command.contains("sleeper") => return found,
                _ => assert!(Instant::now() < deadline, "the child never showed up"),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn gone(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Processes::identity(pid).is_some() {
            assert!(Instant::now() < deadline, "a killed child stayed alive");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[test]
    fn this_process_is_identified_by_start_and_command() {
        let me = Processes::identity(std::process::id()).expect("own identity");
        assert!(!me.start.is_empty());
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_stem().unwrap().to_string_lossy();
        assert!(me.command.contains(&*name), "{}", me.command);
        assert!(Processes::is_process(std::process::id(), |found| found
            .start
            == me.start));
        assert!(!Processes::is_process(std::process::id(), |found| found
            .start
            == "another start"));
    }

    #[tokio::test]
    async fn invalid_pids_are_never_a_process() {
        for pid in [0, 1, u32::MAX] {
            assert!(Processes::identity(pid).is_none(), "{pid}");
            assert!(!Processes::is_process(pid, |_| true));
            assert!(!Processes::kill(pid, |_| true));
            assert!(!Processes::stop(pid, |_| true, Duration::ZERO).await);
        }
    }

    #[test]
    fn this_process_has_a_parent_and_an_age() {
        assert!(Processes::parent(std::process::id()).is_some());
        assert!(Processes::age(std::process::id()).is_some());
        assert_eq!(Processes::parent(u32::MAX), None);
    }

    #[tokio::test]
    async fn a_child_is_stopped_only_as_its_recorded_self_and_is_gone_once_dead() {
        let mut child = Processes::spawn_tree(sleeper_command()).await.unwrap();
        let pid = child.id().unwrap();
        let recorded = sleeper_identity(pid).await;
        let is_recorded =
            |found: &Identity| found.start == recorded.start && found.command.contains("sleeper");
        assert!(!Processes::kill(pid, |found| found.start == recorded.start
            && found.command.contains("not-this-program")));
        assert!(!Processes::stop(pid, |found| found.start == "0", Duration::ZERO).await);
        assert!(child.try_wait().unwrap().is_none(), "nothing was stopped");
        assert!(Processes::is_process(pid, is_recorded));
        assert!(Processes::kill(pid, is_recorded));
        // Dead, whether or not it has been reaped yet: a zombie is no process.
        gone(pid).await;
        assert!(!Processes::is_process(pid, is_recorded));
        assert!(!child.wait().await.unwrap().success());
        assert!(Processes::identity(pid).is_none());
    }

    #[tokio::test]
    async fn a_child_is_stopped_asked_first_where_the_os_can_ask() {
        let mut child = Processes::spawn_tree(sleeper_command()).await.unwrap();
        let pid = child.id().unwrap();
        sleeper_identity(pid).await;
        // Asked (`SIGTERM`, which the sleeper does not handle) on Unix, killed after the grace on Windows: gone
        // either way.
        Processes::stop(
            pid,
            |found| found.command.contains("sleeper"),
            Duration::from_secs(1),
        )
        .await;
        assert!(Processes::identity(pid).is_none());
        assert!(!child.wait().await.unwrap().success());
    }

    #[tokio::test]
    async fn a_process_is_found_by_its_command_line_and_has_an_age_and_a_parent() {
        let mut child = Processes::spawn_tree(sleeper_command()).await.unwrap();
        let pid = child.id().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !Processes::find(|command| command.contains("tests::sleeper")).contains(&pid) {
            assert!(Instant::now() < deadline, "the child was never found");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!Processes::find(|command| command.contains("not-this-program")).contains(&pid));
        assert!(Processes::age(pid).is_some_and(|age| age < 60));
        assert_eq!(Processes::parent(pid), Some(std::process::id()));
        child.kill_tree().await;
    }

    #[tokio::test]
    async fn a_tree_is_stopped_whole_and_a_dropped_tree_goes_with_its_child() {
        let mut child = Processes::spawn_tree(sleeper_command()).await.unwrap();
        let pid = child.id().unwrap();
        child.kill_tree().await;
        assert!(child.try_wait().unwrap().is_some());
        gone(pid).await;
        let child = Processes::spawn_tree(sleeper_command()).await.unwrap();
        let pid = child.id().unwrap();
        drop(child);
        gone(pid).await;
    }

    #[tokio::test]
    async fn a_detached_child_outlives_its_handle() {
        let child = Processes::spawn_detached(sleeper_command()).await.unwrap();
        let pid = child.id().unwrap();
        drop(child);
        sleeper_identity(pid).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            Processes::identity(pid).is_some(),
            "a detached child stays up"
        );
        assert!(Processes::kill(pid, |_| true));
        gone(pid).await;
    }

    #[tokio::test]
    async fn a_missing_program_is_a_spawn_error() {
        let command = Command::new("sidevoice-no-such-program");
        let error = Processes::spawn_tree(command).await.err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn the_candidates_a_process_has_open_are_told_from_those_it_has_not() {
        let scratch = crate::test_support::temp_dir("open-among");
        let (held, not_held, missing) = (
            scratch.path().join("held"),
            scratch.path().join("not-held"),
            scratch.path().join("missing"),
        );
        std::fs::write(&held, b"x").unwrap();
        std::fs::write(&not_held, b"x").unwrap();
        let file = std::fs::File::open(&held).unwrap();
        let candidates = [not_held, held.clone(), missing];
        // `None` is a host that cannot tell (no `lsof`, no Restart Manager): the question has no answer there.
        if let Some(open) = Processes::open_among(std::process::id(), &candidates) {
            assert_eq!(open, [held]);
        }
        assert!(Processes::open_among(std::process::id(), &[]).is_none_or(|open| open.is_empty()));
        drop(file);
        if let Some(open) = Processes::open_among(std::process::id(), &candidates) {
            assert!(open.is_empty(), "{open:?}");
        }
    }
}
