//! Processes: starting them detached or as a tree, telling who a pid is before anything is done to it, finding and
//! stopping them.
//!
//! What differs per OS is one submodule, chosen once below (the trait [`Process`]): how a child is made to lead
//! its tree, and a harness adapter's open-files heuristic. Everything else is one implementation over `sysinfo`, which reads
//! the same things on every OS (owner, start time, command line, parent, state) and sends the stop; there is no
//! `/proc` or `ps` of ours. Spawning is `process-wrap`: a process group on Unix, a Job Object on Windows.
//!
//! A pid read from a file is only a hint: pids are reused, and a stale lock or ready file can name somebody
//! else's work. Nothing is stopped on a pid alone: [`is_process`], [`terminate`] and [`kill`] act only on a
//! process that is this user's and that the caller's check recognises (its start time, its command line). A
//! zombie (dead, not yet reaped) is no process: it will never do anything again.
//!
//! Graceful stop is not a platform operation: the in-band `shutdown` request over the local channel is. What
//! [`terminate`] does is the second path under it: `SIGTERM` where the OS has signals, the hard stop where it
//! does not (Windows has no signal to send to another process).

#[cfg(unix)]
#[path = "process/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "process/windows.rs"]
mod imp;

use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::OnceLock;
use std::time::Duration;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System, Uid, UpdateKind};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};

/// What every OS implements (`imp::Os` in `process/unix.rs` and `process/windows.rs`). The public functions below
/// are this, documented for callers; nothing else is OS-specific.
trait Process {
    /// `command` set to outlive its caller: its own process group (Unix), no console and its own group
    /// (Windows), so closing the terminal or pressing ^C in it does not reach it. Nothing kills it on drop.
    fn detached(command: Command) -> CommandWrap;
    /// `command` set to lead a tree that can be stopped as a whole: a process group (Unix) or a Job Object
    /// (Windows) that holds everything it starts.
    fn tree(command: Command) -> CommandWrap;
    /// The files `pid` has open, as far as the OS shows them to this user. `None` where it does not (Windows:
    /// nothing in the standard API lists another process's handles, and the one caller, an adapter's chat lookup, is
    /// a heuristic that this decision leaves out there).
    fn open_files(pid: u32) -> Option<Vec<PathBuf>>;
}

/// A process this module started. The leader's exit is what [`Child::wait`] reports, not the tree's; the tree is
/// stopped with [`Child::kill_tree`].
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

    // The leader alone, beneath the tree's wrapper: a Job Object's own `wait` would also wait for every process the
    // leader left behind, and a leader's exit is what callers ask about.
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

/// Starts `command` so that it outlives this process and its terminal. The caller owns stdio and the environment.
pub async fn spawn_detached(command: Command) -> io::Result<Child> {
    spawn(<imp::Os as Process>::detached(command)).await
}

/// Starts `command` as the leader of a tree that [`Child::kill_tree`] stops whole, and that goes with the
/// [`Child`] when it is dropped.
pub async fn spawn_tree(command: Command) -> io::Result<Child> {
    let mut wrap = <imp::Os as Process>::tree(command);
    wrap.wrap(KillOnDrop);
    spawn(wrap).await
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

/// Who a process is: what a recorded pid is checked against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// Whether the process belongs to the user this one runs as.
    pub ours: bool,
    /// Opaque start time: comparable only with another reading on the same machine.
    pub start: String,
    /// The program and its arguments, joined by spaces.
    pub command: String,
}

fn valid_pid(pid: u32) -> bool {
    pid > 1
}

fn refresh_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing()
        .with_user(UpdateKind::Always)
        .with_cmd(UpdateKind::Always)
}

/// The processes `pids` name, read now.
fn read(pids: ProcessesToUpdate<'_>) -> System {
    let mut system = System::new();
    system.refresh_processes_specifics(pids, true, refresh_kind());
    system
}

fn current_user() -> Option<&'static Uid> {
    static CURRENT: OnceLock<Option<Uid>> = OnceLock::new();
    CURRENT
        .get_or_init(|| {
            let pid = sysinfo::get_current_pid().ok()?;
            let system = read(ProcessesToUpdate::Some(&[pid]));
            system.process(pid)?.user_id().cloned()
        })
        .as_ref()
}

fn identity_of(process: &sysinfo::Process) -> Option<Identity> {
    if process.status() == sysinfo::ProcessStatus::Zombie {
        return None;
    }
    Some(Identity {
        ours: process
            .user_id()
            .zip(current_user())
            .is_some_and(|(owner, current)| owner == current),
        start: process.start_time().to_string(),
        command: process
            .cmd()
            .iter()
            .map(|part| part.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" "),
    })
}

/// Who `pid` is now; `None` when there is no such process (or a zombie).
pub fn identity(pid: u32) -> Option<Identity> {
    if !valid_pid(pid) {
        return None;
    }
    let pid = Pid::from_u32(pid);
    identity_of(read(ProcessesToUpdate::Some(&[pid])).process(pid)?)
}

/// This process, as a lock records it.
pub fn self_identity() -> Option<Identity> {
    static SELF: OnceLock<Option<Identity>> = OnceLock::new();
    SELF.get_or_init(|| identity(std::process::id())).clone()
}

/// What a pid was recorded as: its start time and, optionally, something its command line must contain.
#[derive(Clone, Debug, Default)]
pub struct Expected<'a> {
    pub start: Option<&'a str>,
    pub command_contains: Option<&'a str>,
}

impl Expected<'_> {
    pub fn matches(&self, found: &Identity) -> bool {
        self.start.is_none_or(|start| found.start == start)
            && self
                .command_contains
                .is_none_or(|part| found.command.contains(part))
    }
}

/// Whether `pid` is, now, this user's process and `is_it` recognises it. False when it is gone, somebody else's,
/// another process now, or cannot be told.
pub fn is_process(pid: u32, is_it: impl Fn(&Identity) -> bool) -> bool {
    with_verified(pid, is_it, |_| true)
}

/// Asks `pid` to leave (`SIGTERM`; on Windows, which has no such request, it is stopped) only while it is this
/// user's and `is_it` recognises it. Returns whether the request was sent.
pub fn terminate(pid: u32, is_it: impl Fn(&Identity) -> bool) -> bool {
    with_verified(pid, is_it, |process| {
        process
            .kill_with(Signal::Term)
            .unwrap_or_else(|| process.kill())
    })
}

/// Stops `pid` at once (`SIGKILL`, `TerminateProcess`) only while it is this user's and `is_it` recognises it.
/// Returns whether it was stopped.
pub fn kill(pid: u32, is_it: impl Fn(&Identity) -> bool) -> bool {
    with_verified(pid, is_it, |process| process.kill())
}

fn with_verified(
    pid: u32,
    is_it: impl Fn(&Identity) -> bool,
    act: impl FnOnce(&sysinfo::Process) -> bool,
) -> bool {
    if !valid_pid(pid) {
        return false;
    }
    let pid = Pid::from_u32(pid);
    let system = read(ProcessesToUpdate::Some(&[pid]));
    let Some(process) = system.process(pid) else {
        return false;
    };
    identity_of(process).is_some_and(|found| found.ours && is_it(&found)) && act(process)
}

/// This user's live processes whose command line `matches` (never this one): a process still starting, found
/// before it has written anything that names it.
pub fn find(matches: impl Fn(&str) -> bool) -> Vec<u32> {
    let system = read(ProcessesToUpdate::All);
    system
        .processes()
        .iter()
        .filter(|(pid, _)| pid.as_u32() != std::process::id())
        .filter_map(|(pid, process)| Some((pid.as_u32(), identity_of(process)?)))
        .filter(|(_, found)| found.ours && matches(&found.command))
        .map(|(pid, _)| pid)
        .collect()
}

/// How many seconds `pid` has been running, when that can be read.
pub fn age(pid: u32) -> Option<u64> {
    let pid = Pid::from_u32(pid);
    Some(
        read(ProcessesToUpdate::Some(&[pid]))
            .process(pid)?
            .run_time(),
    )
}

/// The process that started `pid`.
pub fn parent(pid: u32) -> Option<u32> {
    let pid = Pid::from_u32(pid);
    Some(
        read(ProcessesToUpdate::Some(&[pid]))
            .process(pid)?
            .parent()?
            .as_u32(),
    )
}

/// The files `pid` has open, when the OS shows them (not on Windows).
pub fn open_files(pid: u32) -> Option<Vec<PathBuf>> {
    <imp::Os as Process>::open_files(pid)
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

    async fn gone(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while identity(pid).is_some() {
            assert!(Instant::now() < deadline, "a killed child stayed alive");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[test]
    fn this_process_is_identified_by_owner_start_and_command() {
        let me = self_identity().expect("own identity");
        assert!(me.ours);
        assert!(!me.start.is_empty());
        assert!(me
            .command
            .contains(&*std::env::current_exe().unwrap().to_string_lossy()));
        let exact = Expected {
            start: Some(&me.start),
            command_contains: None,
        };
        assert!(is_process(std::process::id(), |found| exact.matches(found)));
        let other = Expected {
            start: Some("another start"),
            command_contains: None,
        };
        assert!(!is_process(std::process::id(), |found| other.matches(found)));
    }

    #[test]
    fn invalid_pids_are_never_a_process() {
        for pid in [0, 1, u32::MAX] {
            assert_eq!(identity(pid), None, "{pid}");
            assert!(!is_process(pid, |_| true));
            assert!(!kill(pid, |_| true));
            assert!(!terminate(pid, |_| true));
        }
    }

    #[test]
    fn this_process_has_a_parent_and_an_age() {
        assert!(parent(std::process::id()).is_some());
        assert!(age(std::process::id()).is_some());
        assert_eq!(parent(u32::MAX), None);
    }

    #[tokio::test]
    async fn a_child_is_stopped_only_as_its_recorded_self_and_is_gone_once_dead() {
        let mut child = spawn_tree(sleeper_command()).await.unwrap();
        let pid = child.id().unwrap();
        let found = loop {
            match identity(pid) {
                Some(found) if found.command.contains("sleeper") => break found,
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        };
        assert!(found.ours);
        let wrong_command = Expected {
            start: Some(&found.start),
            command_contains: Some("not-this-program"),
        };
        assert!(!kill(pid, |found| wrong_command.matches(found)));
        let wrong_start = Expected {
            start: Some("0"),
            command_contains: None,
        };
        assert!(!terminate(pid, |found| wrong_start.matches(found)));
        assert!(child.try_wait().unwrap().is_none(), "nothing was stopped");
        let exact = Expected {
            start: Some(&found.start),
            command_contains: Some("sleeper"),
        };
        assert!(is_process(pid, |found| exact.matches(found)));
        assert!(kill(pid, |found| exact.matches(found)));
        // Dead, whether or not it has been reaped yet: a zombie is no process.
        gone(pid).await;
        assert!(!is_process(pid, |found| exact.matches(found)));
        let status = child.wait().await.unwrap();
        assert!(!status.success());
        assert_eq!(identity(pid), None);
    }

    #[tokio::test]
    async fn a_child_is_asked_to_leave() {
        let mut child = spawn_tree(sleeper_command()).await.unwrap();
        let pid = child.id().unwrap();
        assert!(terminate(pid, |_| true));
        assert!(!child.wait().await.unwrap().success());
    }

    #[tokio::test]
    async fn a_process_is_found_by_its_command_line_and_has_an_age() {
        let mut child = spawn_tree(sleeper_command()).await.unwrap();
        let pid = child.id().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !find(|command| command.contains("tests::sleeper")).contains(&pid) {
            assert!(Instant::now() < deadline, "the child was never found");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!find(|command| command.contains("not-this-program")).contains(&pid));
        assert!(age(pid).is_some_and(|age| age < 60));
        assert_eq!(parent(pid), Some(std::process::id()));
        child.kill_tree().await;
    }

    #[tokio::test]
    async fn a_tree_is_stopped_whole_and_a_dropped_tree_goes_with_its_child() {
        let mut child = spawn_tree(sleeper_command()).await.unwrap();
        let pid = child.id().unwrap();
        child.kill_tree().await;
        assert!(child.try_wait().unwrap().is_some());
        gone(pid).await;
        let child = spawn_tree(sleeper_command()).await.unwrap();
        let pid = child.id().unwrap();
        drop(child);
        gone(pid).await;
    }

    #[tokio::test]
    async fn a_detached_child_outlives_its_handle() {
        let child = spawn_detached(sleeper_command()).await.unwrap();
        let pid = child.id().unwrap();
        drop(child);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(identity(pid).is_some(), "a detached child stays up");
        assert!(kill(pid, |_| true));
        gone(pid).await;
    }

    #[tokio::test]
    async fn a_missing_program_is_a_spawn_error() {
        let command = Command::new("sidevoice-no-such-program");
        let error = spawn_tree(command).await.err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn open_files_are_listed_where_the_os_shows_them() {
        let file = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        if let Some(files) = open_files(std::process::id()) {
            let exe = std::env::current_exe().unwrap().canonicalize().unwrap();
            assert!(files.iter().any(|path| path == &exe));
        }
        drop(file);
    }
}
