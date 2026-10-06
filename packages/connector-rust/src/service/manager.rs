//! This user's service manager: which one there is, what it says about each job, and the commands that load,
//! start, restart and unload them. Every call has an absolute deadline (30 s: launchd holds a `kickstart` of a job
//! started less than its ThrottleInterval ago until that interval has passed, and systemd's `stop` waits
//! TimeoutStopSec), and a manager that cannot be asked is never taken to mean a job is absent.

use super::definition::{self, START_LIMIT};
use super::layout::Layout;
use super::{Failure, Result};
use serde_json::json;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{sleep, timeout, Duration, Instant};

const MANAGER_LIMIT: Duration = Duration::from_secs(30);
const OUTPUT_LIMIT: usize = 64 * 1024;
/// Linux: Sidevoice prints the linger command and never runs it. The one place to change that decision.
const RUN_LINGER: bool = false;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Launchd,
    Systemd,
    None,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Launchd => "launchd",
            Kind::Systemd => "systemd",
            Kind::None => "none",
        }
    }

    fn named(value: &str) -> Option<Self> {
        match value {
            "launchd" => Some(Kind::Launchd),
            "systemd" => Some(Kind::Systemd),
            "none" => Some(Kind::None),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Job {
    Core,
    Connector,
}

impl Job {
    /// Both jobs, in the order they are started (stopped in reverse).
    pub const ALL: [Job; 2] = [Job::Core, Job::Connector];

    pub fn label(self) -> &'static str {
        match self {
            Job::Core => "dev.sidevoice.core",
            Job::Connector => "dev.sidevoice.connector",
        }
    }

    pub fn unit(self) -> &'static str {
        match self {
            Job::Core => "sidevoice-core.service",
            Job::Connector => "sidevoice-connector.service",
        }
    }

    fn name(self, kind: Kind) -> &'static str {
        if kind == Kind::Launchd {
            self.label()
        } else {
            self.unit()
        }
    }
}

/// What the platform would offer, without asking it: `SIDEVOICE_SERVICE_MANAGER` (`launchd`, `systemd`, `none`)
/// names one; else launchd on macOS, systemd on Linux.
pub fn platform_kind() -> Kind {
    if let Some(kind) = std::env::var("SIDEVOICE_SERVICE_MANAGER")
        .ok()
        .as_deref()
        .and_then(Kind::named)
    {
        return kind;
    }
    if cfg!(target_os = "macos") {
        Kind::Launchd
    } else if cfg!(target_os = "linux") {
        Kind::Systemd
    } else {
        Kind::None
    }
}

/// The manager this user has now: on Linux, systemd only when a user manager answers.
pub async fn kind() -> Kind {
    let kind = platform_kind();
    if kind == Kind::Systemd && std::env::var_os("SIDEVOICE_SERVICE_MANAGER").is_none() {
        return if systemctl(&["show-environment"]).await.ok {
            Kind::Systemd
        } else {
            Kind::None
        };
    }
    kind
}

#[derive(Debug, Default)]
pub struct Output {
    pub ok: bool,
    pub code: Option<i32>,
    pub output: String,
}

impl Output {
    pub fn trimmed(&self) -> String {
        self.output
            .trim()
            .chars()
            .filter(|ch| !ch.is_control() || *ch == '\n')
            .take(400)
            .collect()
    }
}

async fn read_bounded<R: tokio::io::AsyncRead + Unpin>(mut reader: R) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut scratch = [0u8; 4096];
    while let Ok(count) = reader.read(&mut scratch).await {
        if count == 0 {
            break;
        }
        let room = OUTPUT_LIMIT.saturating_sub(kept.len());
        kept.extend_from_slice(&scratch[..count.min(room)]);
    }
    kept
}

/// Run a manager's own command; never fails, never outlives its deadline.
async fn manage(program: &str, args: &[&str]) -> Output {
    let spawned = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            return Output {
                ok: false,
                code: None,
                output: format!("{program}: {error}"),
            }
        }
    };
    let stdout = tokio::spawn(read_bounded(child.stdout.take().expect("piped stdout")));
    let stderr = tokio::spawn(read_bounded(child.stderr.take().expect("piped stderr")));
    let status = match timeout(MANAGER_LIMIT, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            return Output {
                ok: false,
                code: None,
                output: format!("{program}: {error}"),
            }
        }
        Err(_) => {
            let _ = child.kill().await;
            return Output {
                ok: false,
                code: None,
                output: format!("{program} {} timed out", args.join(" ")),
            };
        }
    };
    let mut output = stdout.await.unwrap_or_default();
    output.extend(stderr.await.unwrap_or_default());
    Output {
        ok: status.success(),
        code: status.code(),
        output: String::from_utf8_lossy(&output).into_owned(),
    }
}

async fn launchctl(args: &[&str]) -> Output {
    manage("/bin/launchctl", args).await
}

async fn systemctl(args: &[&str]) -> Output {
    let mut all = vec!["--user"];
    all.extend_from_slice(args);
    manage("systemctl", &all).await
}

fn domain() -> String {
    format!("gui/{}", unsafe { libc::geteuid() })
}

fn target(job: Job) -> String {
    format!("{}/{}", domain(), job.label())
}

/// Where the user manager reads units from: `$XDG_CONFIG_HOME/systemd/user` as the manager sees it (its own
/// environment, which a shell's may differ from), else as this process sees it.
async fn systemd_unit_dir(layout: &Layout) -> PathBuf {
    static MANAGER_CONFIG: OnceLock<Option<PathBuf>> = OnceLock::new();
    if MANAGER_CONFIG.get().is_none() {
        let shown = systemctl(&["show-environment"]).await;
        let value = |name: &str| {
            shown
                .output
                .lines()
                .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        };
        let config = shown
            .ok
            .then(|| {
                value("XDG_CONFIG_HOME").or_else(|| value("HOME").map(|home| home.join(".config")))
            })
            .flatten();
        let _ = MANAGER_CONFIG.set(config);
    }
    MANAGER_CONFIG
        .get()
        .cloned()
        .flatten()
        .unwrap_or_else(|| layout.config_home.clone())
        .join("systemd/user")
}

/// Where a job's definition is for this manager.
pub async fn definition_path(layout: &Layout, kind: Kind, job: Job) -> Option<PathBuf> {
    match kind {
        Kind::Launchd => Some(
            layout
                .home
                .join("Library/LaunchAgents")
                .join(format!("{}.plist", job.label())),
        ),
        Kind::Systemd => Some(systemd_unit_dir(layout).await.join(job.unit())),
        Kind::None => None,
    }
}

/// The jobs defined on this machine, found by their definitions alone.
#[derive(Clone, Debug)]
pub struct Installed {
    pub kind: Kind,
    pub core: Option<PathBuf>,
    pub connector: Option<PathBuf>,
}

impl Installed {
    pub fn file(&self, job: Job) -> Option<&PathBuf> {
        match job {
            Job::Core => self.core.as_ref(),
            Job::Connector => self.connector.as_ref(),
        }
    }
}

pub async fn installed(layout: &Layout) -> Option<Installed> {
    let kind = platform_kind();
    let mut found = Installed {
        kind,
        core: None,
        connector: None,
    };
    for job in Job::ALL {
        let file = definition_path(layout, kind, job)
            .await
            .filter(|file| std::fs::symlink_metadata(file).is_ok());
        match job {
            Job::Core => found.core = file,
            Job::Connector => found.connector = file,
        }
    }
    (found.core.is_some() || found.connector.is_some()).then_some(found)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Presence {
    Loaded { active: bool },
    Absent,
    Unknown(String),
}

/// Whether the manager has a job, as it answers: absent only when it says so (launchd's "Could not find service",
/// 113; systemd's `LoadState=not-found`).
pub async fn presence(kind: Kind, job: Job) -> Presence {
    match kind {
        Kind::Launchd => {
            let printed = launchctl(&["print", &target(job)]).await;
            if printed.ok {
                let active = printed
                    .output
                    .lines()
                    .any(|line| line.trim() == "state = running");
                return Presence::Loaded { active };
            }
            if printed.code == Some(113)
                || printed
                    .output
                    .to_ascii_lowercase()
                    .contains("could not find service")
            {
                return Presence::Absent;
            }
            Presence::Unknown(if printed.trimmed().is_empty() {
                format!("launchctl exited {:?}", printed.code)
            } else {
                printed.trimmed()
            })
        }
        Kind::Systemd => {
            let shown = systemctl(&["show", "-p", "LoadState,ActiveState", job.unit()]).await;
            let field = |name: &str| field(&shown.output, name);
            match (shown.ok, field("LoadState")) {
                (true, Some(load)) if load == "not-found" => Presence::Absent,
                (true, Some(_)) => Presence::Loaded {
                    active: matches!(
                        field("ActiveState").as_deref(),
                        Some("active" | "activating" | "deactivating" | "reloading")
                    ),
                },
                _ => Presence::Unknown(if shown.trimmed().is_empty() {
                    format!("systemctl exited {:?}", shown.code)
                } else {
                    shown.trimmed()
                }),
            }
        }
        Kind::None => Presence::Absent,
    }
}

fn field(text: &str, name: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.trim_start().strip_prefix(name)?.strip_prefix('='))
        .map(|value| value.trim().to_owned())
}

/// A job as its manager reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JobState {
    pub loaded: bool,
    pub running: bool,
    pub pid: Option<u32>,
    pub exit: Option<i64>,
    pub signal: Option<String>,
    pub runs: Option<u64>,
    /// Not running, and the manager will start it again.
    pub restarting: bool,
    /// Why the manager does not run it: `not-loaded`, `start-limit`.
    pub reason: Option<&'static str>,
}

/// `launchctl print` read. A job that is loaded, not running, and last ended badly is one launchd starts again
/// (its `KeepAlive`), throttled.
pub fn parse_launchd(ok: bool, printed: &str) -> JobState {
    if !ok {
        return JobState {
            reason: Some("not-loaded"),
            ..JobState::default()
        };
    }
    let field = |name: &str| {
        printed.lines().find_map(|line| {
            let (key, value) = line.trim().split_once(" = ")?;
            (key == name).then(|| value.trim().to_owned())
        })
    };
    let pid = field("pid")
        .and_then(|pid| pid.parse::<u32>().ok())
        .filter(|pid| *pid > 0);
    let running = field("state").as_deref() == Some("running") && pid.is_some();
    let exit = field("last exit code").and_then(|code| code.parse::<i64>().ok());
    let signal = field("last terminating signal").filter(|signal| !signal.is_empty());
    JobState {
        loaded: true,
        running,
        pid: pid.filter(|_| running),
        restarting: !running && (exit.is_some_and(|code| code != 0) || signal.is_some()),
        exit,
        signal,
        runs: field("runs")
            .and_then(|runs| runs.parse().ok())
            .filter(|runs| *runs > 0),
        reason: None,
    }
}

/// `systemctl --user show` read, the same shape.
pub fn parse_systemd(shown: &str) -> JobState {
    let field = |name: &str| field(shown, name);
    let load = field("LoadState");
    let sub = field("SubState");
    let pid = field("ExecMainPID")
        .and_then(|pid| pid.parse::<u32>().ok())
        .filter(|pid| *pid > 0);
    let running = matches!(
        field("ActiveState").as_deref(),
        Some("active" | "activating" | "reloading")
    ) && sub.as_deref() != Some("auto-restart")
        && pid.is_some();
    let loaded = load.as_deref().is_some_and(|load| load != "not-found");
    JobState {
        loaded,
        running,
        pid: pid.filter(|_| running),
        exit: field("ExecMainStatus").and_then(|code| code.parse().ok()),
        signal: None,
        runs: field("NRestarts").and_then(|runs| runs.parse().ok()),
        restarting: sub.as_deref() == Some("auto-restart"),
        reason: if !loaded {
            Some("not-loaded")
        } else if field("Result").as_deref() == Some("start-limit-hit") {
            Some("start-limit")
        } else {
            None
        },
    }
}

pub async fn state(kind: Kind, job: Job) -> JobState {
    match kind {
        Kind::Launchd => {
            let printed = launchctl(&["print", &target(job)]).await;
            parse_launchd(printed.ok, &printed.output)
        }
        Kind::Systemd => {
            let shown = systemctl(&[
                "show",
                "-p",
                "LoadState,ActiveState,SubState,Result,ExecMainStatus,ExecMainPID,NRestarts",
                job.unit(),
            ])
            .await;
            parse_systemd(if shown.ok { &shown.output } else { "" })
        }
        Kind::None => JobState::default(),
    }
}

/// `launchctl bootout` returns before launchd has let the job go, and a `bootstrap` in that interval fails: the job
/// is waited out, until launchd says it is gone (which a manager that cannot be asked never says).
async fn boot_out(job: Job) -> std::result::Result<(), String> {
    let out = launchctl(&["bootout", &target(job)]).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match presence(Kind::Launchd, job).await {
            Presence::Absent => return Ok(()),
            now if Instant::now() >= deadline => {
                return Err(match now {
                    Presence::Unknown(detail) => format!("not confirmed: {detail}"),
                    _ if !out.trimmed().is_empty() => out.trimmed(),
                    _ => "still loaded".into(),
                })
            }
            _ => sleep(Duration::from_millis(100)).await,
        }
    }
}

async fn boot_in(layout: &Layout, job: Job) -> std::result::Result<(), String> {
    let file = definition_path(layout, Kind::Launchd, job)
        .await
        .expect("launchd definitions have a path");
    let file = file.to_string_lossy().into_owned();
    let mut booted = launchctl(&["bootstrap", &domain(), &file]).await;
    for _ in 0..10 {
        if booted.ok || matches!(presence(Kind::Launchd, job).await, Presence::Loaded { .. }) {
            return Ok(());
        }
        sleep(Duration::from_millis(500)).await;
        booted = launchctl(&["bootstrap", &domain(), &file]).await;
    }
    if booted.ok || matches!(presence(Kind::Launchd, job).await, Presence::Loaded { .. }) {
        return Ok(());
    }
    Err(booted.trimmed())
}

/// Each of `jobs` started from its definition as it is now: a changed definition is loaded again (launchd reads a
/// plist only when it is bootstrapped; systemd reloads), and with `restart` a running job is restarted (launchd
/// `kickstart -k`; systemd `reset-failed` + `restart`, which clears a start limit).
pub async fn start(
    layout: &Layout,
    kind: Kind,
    changed: &[Job],
    restart: bool,
    jobs: &[Job],
) -> Result<()> {
    if jobs.contains(&Job::Core) && (restart || changed.contains(&Job::Core)) {
        // A core about to start again: its last failure report is about a start that is over.
        let _ = std::fs::remove_file(layout.core_failure());
    }
    let fail = |job: Job, detail: String| {
        Failure::keyed(
            "service.not-loaded",
            json!({"detail": format!("{}: {detail}", job.name(kind))}),
        )
    };
    match kind {
        Kind::Launchd => {
            for &job in jobs {
                let loaded = matches!(presence(kind, job).await, Presence::Loaded { .. });
                if changed.contains(&job) && loaded {
                    boot_out(job).await.map_err(|detail| fail(job, detail))?;
                }
                if !matches!(presence(kind, job).await, Presence::Loaded { .. }) {
                    boot_in(layout, job)
                        .await
                        .map_err(|detail| fail(job, detail))?;
                    continue;
                }
                let target = target(job);
                let mut args = vec!["kickstart"];
                if restart {
                    args.push("-k");
                }
                args.push(&target);
                let kicked = launchctl(&args).await;
                if !kicked.ok {
                    return Err(fail(job, kicked.trimmed()));
                }
            }
        }
        Kind::Systemd => {
            if !changed.is_empty() {
                systemctl(&["daemon-reload"]).await;
            }
            for &job in jobs {
                systemctl(&["enable", job.unit()]).await;
                systemctl(&["reset-failed", job.unit()]).await;
                let verb = if restart || changed.contains(&job) {
                    "restart"
                } else {
                    "start"
                };
                let out = systemctl(&[verb, job.unit()]).await;
                if !out.ok {
                    let reason = state(kind, job).await.reason;
                    let program = match definition_path(layout, kind, job).await {
                        Some(file) => std::fs::read_to_string(file)
                            .ok()
                            .and_then(|text| definition::definition_program(kind, &text)),
                        None => None,
                    };
                    let key =
                        match reason.or_else(|| definition::program_problem(program.as_deref())) {
                            Some("start-limit") => "service.start-limit",
                            Some("executable-missing") => "service.executable-missing",
                            Some("permission-denied") => "service.permission-denied",
                            _ => "service.not-loaded",
                        };
                    return Err(Failure::keyed(key, json!({"detail": out.trimmed()})));
                }
            }
        }
        Kind::None => return Err(Failure::keyed("service.no-manager", json!({}))),
    }
    Ok(())
}

/// Both jobs unloaded, the connector first: launchd `bootout`, waited out; systemd `stop` (or `disable --now`).
/// The first refusal, or none.
pub async fn unload(installed: &Installed, disable: bool) -> Option<String> {
    for job in Job::ALL.iter().rev().copied() {
        if installed.file(job).is_none() {
            continue;
        }
        match installed.kind {
            Kind::Launchd => match presence(Kind::Launchd, job).await {
                Presence::Unknown(detail) => {
                    return Some(format!(
                        "{}: the manager could not be asked ({detail})",
                        job.label()
                    ))
                }
                Presence::Absent => continue,
                Presence::Loaded { .. } => {
                    if let Err(detail) = boot_out(job).await {
                        return Some(format!("{}: {detail}", job.label()));
                    }
                }
            },
            Kind::Systemd => {
                let out = if disable {
                    systemctl(&["disable", "--now", job.unit()]).await
                } else {
                    systemctl(&["stop", job.unit()]).await
                };
                // Stopped is what the manager says afterwards, not what the command returned.
                match presence(Kind::Systemd, job).await {
                    Presence::Unknown(detail) => {
                        return Some(format!("{}: not confirmed ({detail})", job.unit()))
                    }
                    Presence::Loaded { active: true } => {
                        return Some(format!("{}: still active", job.unit()))
                    }
                    Presence::Loaded { .. } if !out.ok => {
                        return Some(format!("{}: {}", job.unit(), out.trimmed()))
                    }
                    _ => {}
                }
            }
            Kind::None => {}
        }
    }
    None
}

/// After the definitions are deleted: systemd forgets the units.
pub async fn forget(kind: Kind) -> Result<()> {
    if kind != Kind::Systemd {
        return Ok(());
    }
    let reloaded = systemctl(&["daemon-reload"]).await;
    if !reloaded.ok {
        return Err(Failure::keyed(
            "service.unload-failed",
            json!({"detail": reloaded.trimmed()}),
        ));
    }
    for job in Job::ALL {
        systemctl(&["reset-failed", job.unit()]).await;
    }
    Ok(())
}

/// systemd's start limit, for a status that names the manager's attempts.
pub fn limit(kind: Kind) -> Option<u64> {
    (kind == Kind::Systemd).then_some(START_LIMIT)
}

/// Linux: whether this user's services run without a session, and the command that would make them (never run).
pub async fn linger() -> serde_json::Value {
    let user = std::env::var("USER")
        .ok()
        .filter(|user| !user.is_empty())
        .or_else(user_name)
        .unwrap_or_default();
    let shown = manage("loginctl", &["show-user", &user, "-p", "Linger"]).await;
    let enabled = shown.output.lines().any(|line| line.trim() == "Linger=yes");
    let command = format!("loginctl enable-linger {user}");
    if !enabled && RUN_LINGER {
        manage("loginctl", &["enable-linger", &user]).await;
    }
    json!({"enabled": enabled, "command": command,
        "reason": super::text::message("service.linger-reason", &serde_json::Value::Null)})
}

fn user_name() -> Option<String> {
    let entry = unsafe { libc::getpwuid(libc::geteuid()) };
    if entry.is_null() {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr((*entry).pw_name) };
    name.to_str().ok().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launchd_reports_read_as_job_states() {
        let running = "gui/501/dev.sidevoice.core = {\n\tstate = running\n\truns = 3\n\tpid = 4242\n\tlast exit code = 0\n}";
        let state = parse_launchd(true, running);
        assert_eq!(
            (state.loaded, state.running, state.pid, state.runs),
            (true, true, Some(4242), Some(3))
        );
        assert!(!state.restarting);

        let crashed = "\tstate = not running\n\truns = 2\n\tlast exit code = 1\n\tlast terminating signal = Killed: 9\n";
        let state = parse_launchd(true, crashed);
        assert_eq!(
            (state.running, state.pid, state.exit),
            (false, None, Some(1))
        );
        assert_eq!(state.signal.as_deref(), Some("Killed: 9"));
        assert!(state.restarting);

        let gave_up = "\tstate = not running\n\tlast exit code = 0\n";
        assert!(!parse_launchd(true, gave_up).restarting);
        assert_eq!(
            parse_launchd(false, "Could not find service").reason,
            Some("not-loaded")
        );
    }

    #[test]
    fn systemd_reports_read_as_job_states() {
        let running = "LoadState=loaded\nActiveState=active\nSubState=running\nResult=success\nExecMainStatus=0\nExecMainPID=77\nNRestarts=0\n";
        let state = parse_systemd(running);
        assert_eq!(
            (state.loaded, state.running, state.pid, state.reason),
            (true, true, Some(77), None)
        );

        let waiting = "LoadState=loaded\nActiveState=activating\nSubState=auto-restart\nResult=exit-code\nExecMainStatus=1\nExecMainPID=0\nNRestarts=2\n";
        let state = parse_systemd(waiting);
        assert_eq!(
            (state.running, state.restarting, state.runs, state.exit),
            (false, true, Some(2), Some(1))
        );

        let limited = "LoadState=loaded\nActiveState=failed\nSubState=failed\nResult=start-limit-hit\nExecMainPID=0\n";
        assert_eq!(parse_systemd(limited).reason, Some("start-limit"));
        assert_eq!(
            parse_systemd("LoadState=not-found\n").reason,
            Some("not-loaded")
        );
        assert_eq!(
            parse_systemd("").reason,
            Some("not-loaded"),
            "a manager that did not answer"
        );
    }

    #[test]
    fn job_names_are_the_ones_the_app_and_older_installs_know() {
        assert_eq!(Job::Core.label(), "dev.sidevoice.core");
        assert_eq!(Job::Connector.label(), "dev.sidevoice.connector");
        assert_eq!(Job::Core.unit(), "sidevoice-core.service");
        assert_eq!(Job::Connector.unit(), "sidevoice-connector.service");
        assert_eq!(limit(Kind::Systemd), Some(5));
        assert_eq!(limit(Kind::Launchd), None);
    }
}
