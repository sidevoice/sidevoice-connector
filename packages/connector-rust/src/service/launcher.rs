//! What runs without a service manager (containers, `su` shells, Linux without a user bus, or a machine where the
//! service was never installed): the one way anything gets a connector, the core a connector starts when no core job
//! is defined, and stopping both as their verified selves.
//!
//! Getting a connector (the MCP server):
//! 1. the socket answers: done;
//! 2. a person stopped Sidevoice (`node-stopped.json`): nothing is started, and the caller is told how to start it;
//! 3. a connector job is defined: nothing is spawned (its manager runs it, and may be restarting it right now); the
//!    socket is waited for, up to 10 s, then the failure is what the node's status says;
//! 4. no job: a plain connector is spawned, detached, from the selected installation's command; two of them settle
//!    it by its lock.
//!
//! The core without a job: one connector starts it, detached, and leaves it running (a call may be going on); it
//! exits on its own when nothing has used it for a while. The handshake is the ready file `D/core/core.json`; a core
//! that died before serving says why in `D/core/core-failure.json`, and one data directory has one core (its
//! `flock`).

use super::definition::core_arguments;
use super::layout::Layout;
use super::manager;
use super::status::{self, read_ready, serving};
use super::{Failure, Result};
use crate::core_ready::Ready;
use crate::messages::message;
use crate::process::{Expected, Lookup};
use crate::secure_fs::verify_socket;
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::time::{sleep, timeout, Duration, Instant};

/// How long a started core has to be ready: its ready file with its launch id, and its health answering.
const READY_LIMIT: Duration = Duration::from_secs(60);
/// How long a core or connector asked to leave has before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(15);
/// How long a launcher waits for a connector its manager runs, and for one it spawned.
const SERVICE_WAIT: Duration = Duration::from_secs(10);
/// The idle exit of a core started on demand.
const ON_DEMAND_IDLE_EXIT: u32 = 600;
/// What an on-demand child's `PATH` is.
const PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// One request to the connector on its socket: its `result`, or none when nothing answers within `limit`.
pub async fn ask_connector(
    layout: &Layout,
    method: &str,
    params: Value,
    limit: Duration,
) -> Option<Value> {
    let socket = layout.connector_socket();
    verify_socket(&socket).ok()?;
    let ask = async {
        let mut stream = UnixStream::connect(&socket).await.ok()?;
        let request = json!({"id": 1, "method": method, "params": params});
        stream
            .write_all(format!("{request}\n").as_bytes())
            .await
            .ok()?;
        let mut reader = BufReader::new(stream.take(64 * 1024));
        let mut line = String::new();
        reader.read_line(&mut line).await.ok()?;
        let reply: Value = serde_json::from_str(&line).ok()?;
        (reply.get("ok") == Some(&json!(true)))
            .then(|| reply.get("result").cloned().unwrap_or(Value::Null))
    };
    timeout(limit, ask).await.ok().flatten()
}

/// Whether some process holds the connector's lock (one this process can take has no holder).
pub fn connector_lock_held(layout: &Layout) -> bool {
    let path = layout.connector_lock();
    fs::symlink_metadata(&path).is_ok() && matches!(crate::lock::try_lock(&path, "probe"), Ok(None))
}

/// A core of this data directory, by its command line (`--data-dir <D/core>` as a whole argument): a pid alone is
/// not one of ours.
fn is_our_core(layout: &Layout, command_line: &str) -> bool {
    let needle = format!("--data-dir {}", layout.core_data().display());
    command_line
        .split(needle.as_str())
        .skip(1)
        .any(|rest| rest.is_empty() || rest.starts_with(' '))
}

/// Whether `pid` is, now, this user's core of this data directory.
pub fn core_running(layout: &Layout, pid: u32) -> bool {
    match crate::process::lookup(pid) {
        Lookup::Alive(found) => {
            found.uid == crate::secure_fs::uid() && is_our_core(layout, &found.command)
        }
        _ => false,
    }
}

/// Every core of this data directory still alive: the one its ready file names, and any still starting.
pub fn core_processes(layout: &Layout) -> Vec<u32> {
    let mut pids: Vec<u32> = read_ready(layout)
        .map(|ready| ready.pid)
        .filter(|pid| core_running(layout, *pid))
        .into_iter()
        .collect();
    pids.extend(crate::process::find(|command| is_our_core(layout, command)));
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// `signal` to `pid` only while it is provably this data directory's core.
fn signal_core(layout: &Layout, pid: u32, signal: i32) -> bool {
    core_running(layout, pid) && unsafe { libc::kill(pid as i32, signal) } == 0
}

/// Ask a core of this data directory to leave and wait until it has: SIGTERM, a grace period, SIGKILL; each sent
/// only while it is provably that core.
pub async fn terminate_core(layout: &Layout, pid: u32, grace: Duration) {
    if !signal_core(layout, pid, libc::SIGTERM) {
        return;
    }
    let deadline = Instant::now() + grace;
    while core_running(layout, pid) && Instant::now() < deadline {
        sleep(Duration::from_millis(50)).await;
    }
    if core_running(layout, pid) {
        crate::logfile::log(&format!(
            "the core (pid {pid}) did not leave within {} s; killing it",
            grace.as_secs()
        ));
        signal_core(layout, pid, libc::SIGKILL);
        let deadline = Instant::now() + Duration::from_secs(5);
        while core_running(layout, pid) && Instant::now() < deadline {
            sleep(Duration::from_millis(50)).await;
        }
    }
}

/// Why a launch did not come up, as a person reads it: the core's own report for this launch; else the spawn error;
/// else `launch.exited` with its exit status.
fn failure_cause(
    layout: &Layout,
    launch_id: &str,
    exit: Option<&io::Result<std::process::ExitStatus>>,
    key: Option<&str>,
) -> Value {
    let at = crate::logfile::timestamp();
    if key.is_none() {
        if let Some(report) = status::read_failure(layout, &at) {
            if report.get("launch_id").and_then(Value::as_str) == Some(launch_id) {
                return report;
            }
        }
    }
    let describe = |key: &str, step: &str, detail: Value| {
        json!({"key": key, "step": step, "detail": detail, "at": at,
            "message": message(key, &status::detail_params(Some(&detail))), "log_tail": status::log_tail(layout, 10)})
    };
    if let Some(key) = key {
        return describe(key, "ready", Value::Null);
    }
    match exit {
        Some(Err(error)) => describe("launch.exited", "spawn", json!(error.to_string())),
        Some(Ok(code)) => describe(
            "launch.exited",
            "run",
            code.code()
                .map(Value::from)
                .or_else(|| {
                    std::os::unix::process::ExitStatusExt::signal(code)
                        .map(|signal| json!(format!("signal {signal}")))
                })
                .unwrap_or(Value::Null),
        ),
        None => describe("launch.exited", "run", Value::Null),
    }
}

fn spawn_failure(layout: &Layout, program: &Path, error: &io::Error) -> Value {
    let key = match error.kind() {
        io::ErrorKind::NotFound => "launch.missing-executable",
        io::ErrorKind::PermissionDenied => "launch.permission",
        _ => "launch.exited",
    };
    let detail = json!(program.to_string_lossy());
    json!({"key": key, "step": "spawn", "detail": detail, "at": crate::logfile::timestamp(),
        "message": message(key, &status::detail_params(Some(&detail))), "log_tail": status::log_tail(layout, 10)})
}

/// A core failure as an error: its key, and the failure itself for whoever reports it.
fn core_failure(failure: Value) -> Failure {
    let key = failure
        .get("key")
        .and_then(Value::as_str)
        .unwrap_or("launch.exited")
        .to_owned();
    Failure::described(key, failure)
}

fn private_data_dir(path: &Path) -> Result<()> {
    crate::secure_fs::ensure_private_dir(path).map_err(Failure::plain)
}

/// The core serving this data directory without a service manager: the one already serving, or one started here,
/// detached, from the selected release. Another core still starting holds the directory (`bind.core-running`):
/// that one is waited for instead. Nothing here unlinks a socket: only the core holding the directory replaces it.
pub async fn ensure_core(layout: &Layout) -> Result<Ready> {
    if let Some((ready, _)) = serving(layout, Duration::from_secs(2)).await {
        return Ok(ready);
    }
    private_data_dir(&layout.data)?;
    private_data_dir(&layout.core_data())?;
    let program = layout.core_program();
    let launch_id = uuid::Uuid::new_v4().to_string();
    let output = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(layout.core_output())
        .map_err(Failure::plain)?;
    let mut command = Command::new(&program);
    command
        .args(core_arguments(
            layout,
            Some(&launch_id),
            Some(ON_DEMAND_IDLE_EXIT),
        ))
        .stdin(Stdio::null())
        .stdout(output.try_clone().map_err(Failure::plain)?)
        .stderr(output)
        .process_group(0);
    let environment = super::definition::core_environment(layout, &std::env::vars().collect());
    command.env_clear().envs(environment);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return Err(core_failure(spawn_failure(layout, &program, &error))),
    };
    let pid = child.id();
    crate::logfile::log(&format!(
        "started the core (pid {pid:?}, launch {launch_id}); waiting for it to be ready"
    ));
    let deadline = Instant::now() + READY_LIMIT;
    let mut failure = None;
    while Instant::now() < deadline {
        if let Ok(Some(exit)) = child.try_wait() {
            failure = Some(failure_cause(layout, &launch_id, Some(&Ok(exit)), None));
            break;
        }
        if read_ready(layout).is_some_and(|ready| ready.launch_id == launch_id) {
            if let Some((ready, _)) = serving(layout, Duration::from_secs(2)).await {
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
                return Ok(ready);
            }
        }
        sleep(Duration::from_millis(100)).await;
    }
    let failure =
        failure.unwrap_or_else(|| failure_cause(layout, &launch_id, None, Some("ready.timeout")));
    if failure.get("key").and_then(Value::as_str) == Some("bind.core-running") {
        crate::logfile::log("another core holds this data directory: waiting for it instead");
        let deadline = Instant::now() + READY_LIMIT;
        while Instant::now() < deadline {
            if let Some((ready, _)) = serving(layout, Duration::from_secs(2)).await {
                return Ok(ready);
            }
            sleep(Duration::from_millis(200)).await;
        }
        return Err(core_failure(failure_cause(
            layout,
            &launch_id,
            None,
            Some("ready.timeout"),
        )));
    }
    if let Some(pid) =
        pid.filter(|_| failure.get("key").and_then(Value::as_str) == Some("ready.timeout"))
    {
        terminate_core(layout, pid, STOP_GRACE).await;
    }
    let _ = child.try_wait();
    Err(core_failure(failure))
}

/// Whether this machine's core is the connector's to start: no core job, an installation selected, no stop.
pub async fn owns_core(layout: &Layout) -> bool {
    layout.installed()
        && !status::stopped(layout)
        && manager::installed(layout)
            .await
            .is_none_or(|found| found.core.is_none())
}

/// The connector's identity on its socket: `{pid, executable, managed}`.
async fn connector_identity(layout: &Layout) -> Option<(u32, PathBuf, bool)> {
    let identity = ask_connector(layout, "identity", json!({}), Duration::from_secs(1)).await?;
    let pid = identity
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())?;
    let executable = PathBuf::from(identity.get("executable").and_then(Value::as_str)?);
    let managed = identity.get("managed").and_then(Value::as_bool)?;
    Some((pid, executable, managed))
}

/// An on-demand connector to stop: who its socket says it is, and who its lock's holder record says it is.
struct OnDemand {
    pid: u32,
    start: Option<String>,
    executable: Option<String>,
}

impl OnDemand {
    fn expected(&self) -> Expected<'_> {
        Expected {
            start: self.start.as_deref(),
            command_contains: self.executable.as_deref(),
        }
    }

    fn up(&self) -> bool {
        crate::process::is_process(self.pid, &self.expected())
    }
}

/// The connector running outside a manager, if one is: never the manager's own (it says `managed`), and never a
/// pid alone (its start time or its executable must be known to signal it).
async fn on_demand_connector(layout: &Layout) -> Option<OnDemand> {
    let identity = connector_identity(layout).await;
    if matches!(identity, Some((_, _, true))) {
        return None;
    }
    let holder =
        crate::lock::holder(&layout.connector_lock()).filter(|_| connector_lock_held(layout));
    let holder_pid = holder
        .as_ref()
        .and_then(|record| record.get("pid")?.as_u64())
        .and_then(|pid| u32::try_from(pid).ok());
    let holder_start = |pid: u32| {
        holder
            .as_ref()
            .filter(|_| holder_pid == Some(pid))
            .and_then(|record| record.get("start")?.as_str().map(str::to_owned))
    };
    let found = match identity {
        Some((pid, executable, _)) => OnDemand {
            pid,
            start: holder_start(pid),
            executable: Some(executable.to_string_lossy().into_owned()),
        },
        None => {
            let pid = holder_pid?;
            OnDemand {
                pid,
                start: Some(holder_start(pid)?),
                executable: None,
            }
        }
    };
    (found.start.is_some() || found.executable.is_some()).then_some(found)
}

/// What `stop_on_demand` stopped by force, and what is still there.
#[derive(Debug, Default)]
pub struct Teardown {
    pub killed: Vec<u32>,
    pub left: Vec<String>,
}

/// Stop what runs outside a manager: an on-demand connector (asked over its socket to leave, else signalled as the
/// lock's recorded holder; killed after the grace period only while it is still that process), and every core of
/// this data directory. Waits until both are gone and their sockets silent.
pub async fn stop_on_demand(layout: &Layout) -> Teardown {
    let mut teardown = Teardown::default();
    let connector = on_demand_connector(layout).await;
    if let Some(found) = &connector {
        let asked = match &found.executable {
            Some(executable) => ask_connector(
                layout,
                "shutdown",
                json!({"expected_pid": found.pid, "expected_executable": executable}),
                Duration::from_millis(1500),
            )
            .await
            .is_some(),
            None => false,
        };
        if !asked {
            crate::process::signal_verified(found.pid, libc::SIGTERM, &found.expected());
        }
    }
    let up = |connector: &Option<OnDemand>| connector.as_ref().is_some_and(OnDemand::up);
    let deadline = Instant::now() + STOP_GRACE;
    while up(&connector) && Instant::now() < deadline {
        sleep(Duration::from_millis(100)).await;
    }
    if let Some(found) = connector.as_ref().filter(|found| found.up()) {
        if crate::process::signal_verified(found.pid, libc::SIGKILL, &found.expected()) {
            teardown.killed.push(found.pid);
        }
    }
    for pid in core_processes(layout) {
        terminate_core(layout, pid, STOP_GRACE).await;
        if core_running(layout, pid) {
            teardown.left.push(format!("core pid {pid}"));
        }
    }
    for _ in 0..50 {
        if !up(&connector)
            && ask_connector(layout, "status", json!({}), Duration::from_millis(300))
                .await
                .is_none()
        {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    if let Some(found) = connector.as_ref().filter(|found| found.up()) {
        teardown.left.push(format!("connector pid {}", found.pid));
    }
    // A replacement may take the lock or the socket after the first owner exits: that is not a success, unless it
    // is the manager's own connector (a job that was not unloaded), which is not this to stop.
    let answering = ask_connector(layout, "status", json!({}), Duration::from_millis(300))
        .await
        .is_some();
    if (answering || connector_lock_held(layout))
        && !matches!(connector_identity(layout).await, Some((_, _, true)))
    {
        teardown.left.push("connector socket or lock".into());
    }
    teardown
}

/// The argv that starts this machine's connector daemon on demand: the selected installation's command, else this
/// very program (`fallback`).
pub fn connector_argv(layout: &Layout, fallback: Vec<String>) -> Vec<String> {
    let mut argv = match layout.connector_command().filter(|_| layout.installed()) {
        Some(command) => command,
        None => fallback,
    };
    argv.push("connector".into());
    argv
}

/// Get this machine's connector answering on its socket, the only ways allowed (see the module's doc). `connect`
/// says whether the one answering is usable; `fallback` is the argv prefix that runs this program.
pub async fn ensure_connector<F, Fut>(
    layout: &Layout,
    fallback: Vec<String>,
    environment: Vec<(String, PathBuf)>,
    connect: F,
) -> Result<()>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    if connect().await {
        return Ok(());
    }
    if status::stopped(layout) {
        return Err(Failure::keyed("node.stopped", json!({})));
    }
    if manager::installed(layout)
        .await
        .is_some_and(|found| found.connector.is_some())
    {
        let deadline = Instant::now() + SERVICE_WAIT;
        while Instant::now() < deadline {
            sleep(Duration::from_millis(100)).await;
            if connect().await {
                return Ok(());
            }
        }
        let now = status::status(layout, Some(false)).await;
        let state = now
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("failed")
            .to_owned();
        if state == "stopped-by-person" {
            return Err(Failure::keyed("node.stopped", json!({})));
        }
        if state == "service-failed" {
            if let Some(reason) = now.pointer("/failure/key").and_then(Value::as_str) {
                return Err(Failure::keyed_owned(
                    format!("service.{reason}"),
                    json!({"detail": reason, "state": state}),
                ));
            }
        }
        return Err(Failure::keyed(
            "connector.service-down",
            json!({"detail": state, "state": state}),
        ));
    }
    let argv = connector_argv(layout, fallback);
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .env_clear()
        .env("PATH", PATH)
        .envs(environment)
        .envs(layout.settings.clone())
        .envs(
            std::env::var_os("SIDEVOICE_SERVICE_MANAGER")
                .map(|kind| ("SIDEVOICE_SERVICE_MANAGER", kind)),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = command.spawn().map_err(|error| {
        Failure::keyed(
            "connector.not-started",
            json!({"detail": error.to_string()}),
        )
    })?;
    let deadline = Instant::now() + SERVICE_WAIT;
    while Instant::now() < deadline {
        sleep(Duration::from_millis(100)).await;
        if connect().await {
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
            return Ok(());
        }
        if let Ok(Some(_)) = child.try_wait() {
            // Another connector won the lock: it is the one to wait for.
            if connector_lock_held(layout) {
                continue;
            }
            break;
        }
        if status::stopped(layout) {
            break;
        }
    }
    let _ = child.start_kill();
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Err(Failure::keyed("connector.not-started", json!({})))
}

/// The connector's watch over a core it starts: called whenever its link to the core fails. When the core is the
/// connector's to start and none serves, one is started; never more often than every 2 s.
pub struct CoreSupervisor {
    layout: Layout,
    last: tokio::sync::Mutex<Option<Instant>>,
}

impl CoreSupervisor {
    pub fn new(layout: Layout) -> Self {
        Self {
            layout,
            last: tokio::sync::Mutex::new(None),
        }
    }

    pub async fn link_failed(&self) {
        let mut last = self.last.lock().await;
        if last.is_some_and(|at| at.elapsed() < Duration::from_secs(2)) {
            return;
        }
        *last = Some(Instant::now());
        if !owns_core(&self.layout).await {
            return;
        }
        if let Err(error) = ensure_core(&self.layout).await {
            crate::logfile::log(&format!("the local core is not available: {error}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_core_is_ours_only_with_this_data_directory_as_a_whole_argument() {
        let layout =
            Layout::from_vars(|name| (name == "HOME").then(|| "/h".into()), Vec::new()).unwrap();
        assert!(is_our_core(
            &layout,
            "/r/core --data-dir /h/.sidevoice/core --socket /x"
        ));
        assert!(is_our_core(
            &layout,
            "/r/core --data-dir /h/.sidevoice/core"
        ));
        assert!(!is_our_core(
            &layout,
            "/r/core --data-dir /h/.sidevoice/core2 --socket /x"
        ));
        assert!(!is_our_core(&layout, "/r/core --data-dir /other/core"));
    }

    #[test]
    fn the_on_demand_connector_runs_the_selected_command_else_this_program() {
        let layout = Layout::from_vars(
            |name| (name == "HOME").then(|| "/nonexistent-home".into()),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(
            connector_argv(&layout, vec!["/self".into()]),
            ["/self", "connector"]
        );
    }
}
