//! This user's service manager: which one there is, what it says about each job, and the commands that load,
//! start, restart and unload them, and that say whether they start at the next login. Every call has an absolute deadline (30 s: launchd holds a `kickstart` of a job
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

/// [`launchctl`] and [`systemctl`] with owned arguments: the runners the generic steps below take, which tests fake.
async fn launchd_run(args: Vec<String>) -> Output {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    launchctl(&args).await
}

async fn systemd_run(args: Vec<String>) -> Output {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    systemctl(&args).await
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

/// How a job is bootstrapped, from whether this start enables it for login and, if not, whether it starts at login
/// now. launchd refuses to bootstrap a job disabled for login, so one the person turned off is enabled for the
/// bootstrap and disabled again right after: it runs now and still not at the next login. When that cannot be read,
/// enabling it could leave it on for good: refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Boot {
    Plain,
    Reenabled,
    Refused,
}

pub fn boot_plan(login: bool, now: Option<bool>) -> Boot {
    match (login, now) {
        (true, _) | (false, Some(true)) => Boot::Plain,
        (false, Some(false)) => Boot::Reenabled,
        (false, None) => Boot::Refused,
    }
}

/// The job loaded from its plist; with `reenable` ([`Boot::Reenabled`]), enabled for the bootstrap and disabled again.
async fn boot_in(layout: &Layout, job: Job, reenable: bool) -> std::result::Result<(), String> {
    if reenable {
        let enabled = launchctl(&["enable", &target(job)]).await;
        if !enabled.ok {
            return Err(format!("enable: {}", enabled.trimmed()));
        }
    }
    let booted = bootstrap(layout, job).await;
    if reenable {
        let disabled = launchctl(&["disable", &target(job)]).await;
        if !disabled.ok {
            return Err(format!("disable: {}", disabled.trimmed()));
        }
    }
    booted
}

async fn bootstrap(layout: &Layout, job: Job) -> std::result::Result<(), String> {
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

/// What systemd does before `jobs` start, through `systemctl` (its arguments after `--user`): with `reload`, it reads
/// the definitions again, and with `login` it enables each job for the next login, then checks that it is (without,
/// a job the person turned off for login stays off). A refusal of any of it is the start failing, even when the job
/// would run now: it would not start at the next login, or would run the definition from before. The job a refusal
/// is about, if it is about one, and what the manager said.
async fn prepare_systemd<F, Fut>(
    systemctl: F,
    reload: bool,
    login: bool,
    jobs: &[Job],
) -> std::result::Result<(), (Option<Job>, String)>
where
    F: Fn(Vec<String>) -> Fut,
    Fut: std::future::Future<Output = Output>,
{
    if reload {
        let out = systemctl(vec!["daemon-reload".into()]).await;
        if !out.ok {
            return Err((None, format!("daemon-reload: {}", out.trimmed())));
        }
    }
    if !login {
        return Ok(());
    }
    for &job in jobs {
        let out = systemctl(vec!["enable".into(), job.unit().into()]).await;
        if !out.ok {
            return Err((Some(job), format!("enable: {}", out.trimmed())));
        }
        let state = systemctl(vec!["is-enabled".into(), job.unit().into()]).await;
        if !state.ok || state.trimmed() != "enabled" {
            return Err((Some(job), format!("is-enabled: {}", state.trimmed())));
        }
    }
    Ok(())
}

/// Each of `jobs` started from its definition as it is now: a changed definition is loaded again (launchd reads a
/// plist only when it is bootstrapped; systemd reloads), and with `restart` a running job is restarted (launchd
/// `kickstart -k`; systemd `reset-failed` + `restart`, which clears a start limit). With `login`, each is also enabled
/// for the next login; without, whether it starts at login is left as it is ([`set_start_at_login`]).
pub async fn start(
    layout: &Layout,
    kind: Kind,
    changed: &[Job],
    restart: bool,
    login: bool,
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
                if login {
                    // An override left by an earlier installation would keep the job from loading at the next login.
                    let enabled = launchctl(&["enable", &target(job)]).await;
                    if !enabled.ok {
                        return Err(Failure::keyed(
                            "service.not-enabled",
                            json!({"detail": format!("{}: enable: {}", job.label(), enabled.trimmed())}),
                        ));
                    }
                }
                let loaded = matches!(presence(kind, job).await, Presence::Loaded { .. });
                if changed.contains(&job) && loaded {
                    boot_out(job).await.map_err(|detail| fail(job, detail))?;
                }
                if !matches!(presence(kind, job).await, Presence::Loaded { .. }) {
                    let now = if login {
                        Some(true)
                    } else {
                        job_login(&launchd_run, kind, job).await
                    };
                    let plan = boot_plan(login, now);
                    if plan == Boot::Refused {
                        return Err(Failure::keyed(
                            "service.login-unknown",
                            json!({"detail": job.label()}),
                        ));
                    }
                    boot_in(layout, job, plan == Boot::Reenabled)
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
            if let Err((job, detail)) =
                prepare_systemd(systemd_run, !changed.is_empty(), login, jobs).await
            {
                let detail = match job {
                    Some(job) => format!("{}: {detail}", job.name(kind)),
                    None => detail,
                };
                return Err(Failure::keyed(
                    "service.not-enabled",
                    json!({"detail": detail}),
                ));
            }
            for &job in jobs {
                // It clears an earlier start limit; a job that never failed has none, so its answer does not matter.
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

/// Whether a job starts at the next login, read from `launchctl print-disabled <domain>`: its label overridden
/// `disabled` (`true` on older macOS) does not, `enabled` (`false`) does, and a label with no override loads with its
/// plist. `None`: the output is not that list.
pub fn parse_launchd_login(printed: &str, label: &str) -> Option<bool> {
    if !printed.contains("disabled services") {
        return None;
    }
    let quoted = format!("\"{label}\"");
    let value = printed.lines().find_map(|line| {
        line.trim()
            .strip_prefix(&quoted)?
            .trim_start()
            .strip_prefix("=>")
            .map(str::trim)
    });
    match value {
        None | Some("enabled" | "false") => Some(true),
        Some("disabled" | "true") => Some(false),
        Some(_) => None,
    }
}

/// Whether a unit starts at the next login, from `systemctl --user is-enabled`: `enabled` or `disabled`; anything
/// else (`masked`, `static`, `enabled-runtime`, no answer) is `None`.
pub fn parse_systemd_login(shown: &str) -> Option<bool> {
    match shown.lines().next().map(str::trim) {
        Some("enabled") => Some(true),
        Some("disabled") => Some(false),
        _ => None,
    }
}

/// Whether one job starts at the next login, as its manager (`run`, its arguments) answers now.
async fn job_login<F, Fut>(run: &F, kind: Kind, job: Job) -> Option<bool>
where
    F: Fn(Vec<String>) -> Fut,
    Fut: std::future::Future<Output = Output>,
{
    match kind {
        Kind::Launchd => {
            let printed = run(vec!["print-disabled".into(), domain()]).await;
            printed
                .ok
                .then(|| parse_launchd_login(&printed.output, job.label()))
                .flatten()
        }
        Kind::Systemd => {
            // `is-enabled` exits non-zero for `disabled`: its answer is the text.
            let shown = run(vec!["is-enabled".into(), job.unit().into()]).await;
            parse_systemd_login(&shown.output)
        }
        Kind::None => None,
    }
}

/// Sidevoice's start at login from its jobs' answers: off when any job is off (Sidevoice would not come up whole), on
/// when every job is on, and otherwise (a job its manager cannot tell about, or no job) not known.
pub fn combine_login(jobs: impl IntoIterator<Item = Option<bool>>) -> Option<bool> {
    let jobs: Vec<Option<bool>> = jobs.into_iter().collect();
    if jobs.contains(&Some(false)) {
        Some(false)
    } else if jobs.is_empty() || jobs.contains(&None) {
        None
    } else {
        Some(true)
    }
}

async fn login_with<F, Fut>(run: &F, kind: Kind, jobs: &[Job]) -> Option<bool>
where
    F: Fn(Vec<String>) -> Fut,
    Fut: std::future::Future<Output = Output>,
{
    let mut each = Vec::new();
    for &job in jobs {
        each.push(job_login(run, kind, job).await);
    }
    combine_login(each)
}

/// The jobs whose definitions are here.
fn defined(installed: &Installed) -> Vec<Job> {
    Job::ALL
        .into_iter()
        .filter(|&job| installed.file(job).is_some())
        .collect()
}

/// The jobs whose definitions are missing: an incomplete service, which `service install` repairs.
pub fn missing(installed: &Installed) -> Vec<Job> {
    Job::ALL
        .into_iter()
        .filter(|&job| installed.file(job).is_none())
        .collect()
}

pub fn names(jobs: &[Job], kind: Kind) -> String {
    jobs.iter()
        .map(|job| job.name(kind))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Sidevoice's start at login, through `run`: `None` for an incomplete service (with a job missing, whether
/// Sidevoice comes up at login is not that of either job), else [`combine_login`] over both.
async fn login_of<F, Fut>(run: &F, installed: &Installed) -> Option<bool>
where
    F: Fn(Vec<String>) -> Fut,
    Fut: std::future::Future<Output = Output>,
{
    if !missing(installed).is_empty() {
        return None;
    }
    login_with(run, installed.kind, &Job::ALL).await
}

/// Whether both jobs start at the next login, as their manager says now; `None` when it cannot tell, or a job is not
/// defined.
pub async fn start_at_login(installed: &Installed) -> Option<bool> {
    match installed.kind {
        Kind::Launchd => login_of(&launchd_run, installed).await,
        Kind::Systemd => login_of(&systemd_run, installed).await,
        Kind::None => None,
    }
}

/// Whether a start enables its jobs for the next login: on a new service (no job was defined), and otherwise only
/// when the jobs that were defined all start at login now. Off, or not known (launchd's override unreadable, systemd
/// `enabled-runtime`, …): left as it is, never turned on.
pub fn enable_on_start(new: bool, defined_now: Option<bool>) -> bool {
    new || defined_now == Some(true)
}

async fn login_to_keep<F, Fut>(run: &F, installed: Option<&Installed>) -> bool
where
    F: Fn(Vec<String>) -> Fut,
    Fut: std::future::Future<Output = Output>,
{
    match installed {
        None => enable_on_start(true, None),
        Some(found) => enable_on_start(false, login_with(run, found.kind, &defined(found)).await),
    }
}

/// [`enable_on_start`] for the jobs defined here (`None`: none was).
pub async fn keeps_login(installed: Option<&Installed>) -> bool {
    match installed.map(|found| found.kind) {
        Some(Kind::Launchd) => login_to_keep(&launchd_run, installed).await,
        Some(Kind::Systemd) => login_to_keep(&systemd_run, installed).await,
        Some(Kind::None) => false,
        None => true,
    }
}

/// Each of `jobs` set to start at the next login (`on`) or not, through its manager (`run`): launchd `enable` /
/// `disable` of its label, which only says whether it loads at login; systemd `enable` / `disable` of its unit, never
/// with `--now`. Nothing is loaded, started, stopped or unloaded: a running job keeps running, a stopped one stays
/// stopped. Each change is read back. The first job refused or not confirmed, and what the manager said.
async fn set_login<F, Fut>(
    run: F,
    kind: Kind,
    jobs: &[Job],
    on: bool,
) -> std::result::Result<(), (Job, String)>
where
    F: Fn(Vec<String>) -> Fut,
    Fut: std::future::Future<Output = Output>,
{
    let verb = if on { "enable" } else { "disable" };
    for &job in jobs {
        let name = match kind {
            Kind::Launchd => target(job),
            _ => job.unit().into(),
        };
        let out = run(vec![verb.into(), name]).await;
        if !out.ok {
            return Err((job, format!("{verb}: {}", out.trimmed())));
        }
        let now = job_login(&run, kind, job).await;
        if now != Some(on) {
            let now = now.map_or("unknown", |now| if now { "on" } else { "off" });
            return Err((job, format!("{verb}: not confirmed (now {now})")));
        }
    }
    Ok(())
}

/// The jobs defined here set to start at the next login or not, and nothing else ([`set_login`]); a refusal, said
/// with its job's name.
pub async fn set_start_at_login(
    installed: &Installed,
    on: bool,
) -> std::result::Result<(), String> {
    let jobs = defined(installed);
    let kind = installed.kind;
    let set = match kind {
        Kind::Launchd => set_login(launchd_run, kind, &jobs, on).await,
        Kind::Systemd => set_login(systemd_run, kind, &jobs, on).await,
        Kind::None => Ok(()),
    };
    set.map_err(|(job, detail)| format!("{}: {detail}", job.name(kind)))
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
        "reason": crate::messages::message("service.linger-reason", &serde_json::Value::Null)})
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

    /// A `systemctl` that answers each call from `script` (its arguments → whether it succeeds, and what it prints;
    /// anything else succeeds silently), and logs every call.
    fn scripted(
        script: Vec<(String, bool, &'static str)>,
        log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> impl Fn(Vec<String>) -> std::future::Ready<Output> {
        move |args: Vec<String>| {
            let call = args.join(" ");
            log.lock().unwrap().push(call.clone());
            let (ok, output) = script
                .iter()
                .find(|(line, _, _)| *line == call)
                .map_or((true, ""), |(_, ok, output)| (*ok, *output));
            std::future::ready(Output {
                ok,
                code: Some(i32::from(!ok)),
                output: output.into(),
            })
        }
    }

    fn enabled() -> Vec<(String, bool, &'static str)> {
        Job::ALL
            .iter()
            .map(|job| (format!("is-enabled {}", job.unit()), true, "enabled\n"))
            .collect()
    }

    #[tokio::test]
    async fn systemd_reloads_enables_and_checks_each_job() {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let prepared =
            prepare_systemd(scripted(enabled(), log.clone()), true, true, &Job::ALL).await;
        assert_eq!(prepared, Ok(()));
        let calls = log.lock().unwrap().clone();
        assert_eq!(calls[0], "daemon-reload");
        for job in Job::ALL {
            assert!(
                calls.contains(&format!("enable {}", job.unit())),
                "{calls:?}"
            );
            assert!(
                calls.contains(&format!("is-enabled {}", job.unit())),
                "{calls:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_start_that_keeps_login_off_enables_nothing() {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let prepared = prepare_systemd(scripted(vec![], log.clone()), true, false, &Job::ALL).await;
        assert_eq!(prepared, Ok(()));
        assert_eq!(*log.lock().unwrap(), vec!["daemon-reload".to_owned()]);
    }

    const PRINT_DISABLED_OFF: &str = "disabled services = {\n\t\"com.apple.Siri.agent\" => enabled\n\t\"dev.sidevoice.core\" => disabled\n\t\"dev.sidevoice.connector\" => disabled\n}\n";
    const PRINT_DISABLED_ON: &str = "disabled services = {\n\t\"dev.sidevoice.core\" => enabled\n\t\"dev.sidevoice.connector\" => enabled\n}\n";

    #[tokio::test]
    async fn login_off_and_on_only_disable_and_enable_each_unit_under_systemd() {
        let off = Job::ALL
            .iter()
            .map(|job| (format!("is-enabled {}", job.unit()), false, "disabled\n"))
            .collect();
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let set = set_login(scripted(off, log.clone()), Kind::Systemd, &Job::ALL, false).await;
        assert_eq!(set, Ok(()));
        assert_eq!(
            *log.lock().unwrap(),
            vec![
                "disable sidevoice-core.service",
                "is-enabled sidevoice-core.service",
                "disable sidevoice-connector.service",
                "is-enabled sidevoice-connector.service",
            ],
            "no --now, no stop: the running service is left running"
        );

        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let set = set_login(
            scripted(enabled(), log.clone()),
            Kind::Systemd,
            &Job::ALL,
            true,
        )
        .await;
        assert_eq!(set, Ok(()));
        let calls = log.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                "enable sidevoice-core.service",
                "is-enabled sidevoice-core.service",
                "enable sidevoice-connector.service",
                "is-enabled sidevoice-connector.service",
            ],
            "no --now, no start: a stopped service stays stopped"
        );
    }

    #[tokio::test]
    async fn login_off_and_on_only_disable_and_enable_each_label_under_launchd() {
        let domain = domain();
        let printed = |output| vec![(format!("print-disabled {domain}"), true, output)];
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let set = set_login(
            scripted(printed(PRINT_DISABLED_OFF), log.clone()),
            Kind::Launchd,
            &Job::ALL,
            false,
        )
        .await;
        assert_eq!(set, Ok(()));
        assert_eq!(
            *log.lock().unwrap(),
            vec![
                format!("disable {domain}/dev.sidevoice.core"),
                format!("print-disabled {domain}"),
                format!("disable {domain}/dev.sidevoice.connector"),
                format!("print-disabled {domain}"),
            ],
            "no bootout: the loaded jobs stay loaded and running"
        );

        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let set = set_login(
            scripted(printed(PRINT_DISABLED_ON), log.clone()),
            Kind::Launchd,
            &[Job::Core],
            true,
        )
        .await;
        assert_eq!(set, Ok(()));
        assert_eq!(
            *log.lock().unwrap(),
            vec![
                format!("enable {domain}/dev.sidevoice.core"),
                format!("print-disabled {domain}"),
            ],
            "no bootstrap or kickstart"
        );
    }

    #[tokio::test]
    async fn a_login_change_refused_or_not_read_back_is_a_refusal() {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let refused = vec![(
            "disable sidevoice-core.service".to_owned(),
            false,
            "Access denied",
        )];
        let set = set_login(
            scripted(refused, log.clone()),
            Kind::Systemd,
            &Job::ALL,
            false,
        )
        .await;
        assert_eq!(set, Err((Job::Core, "disable: Access denied".into())));

        // The manager accepted it and still says the unit starts at login.
        let set = set_login(
            scripted(enabled(), log.clone()),
            Kind::Systemd,
            &Job::ALL,
            false,
        )
        .await;
        assert_eq!(
            set,
            Err((Job::Core, "disable: not confirmed (now on)".into()))
        );

        // A manager that cannot be read back is not taken to have done it.
        let set = set_login(scripted(vec![], log), Kind::Launchd, &Job::ALL, true).await;
        assert_eq!(
            set,
            Err((Job::Core, "enable: not confirmed (now unknown)".into()))
        );
    }

    #[tokio::test]
    async fn start_at_login_is_read_from_each_job_never_guessed() {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let both = login_with(&scripted(enabled(), log.clone()), Kind::Systemd, &Job::ALL).await;
        assert_eq!(both, Some(true));

        let mut one_off = enabled();
        one_off.retain(|(line, _, _)| *line != "is-enabled sidevoice-connector.service");
        one_off.push((
            "is-enabled sidevoice-connector.service".into(),
            false,
            "disabled\n",
        ));
        let mixed = login_with(&scripted(one_off, log.clone()), Kind::Systemd, &Job::ALL).await;
        assert_eq!(mixed, Some(false), "Sidevoice would not come up whole");

        let masked = vec![(
            "is-enabled sidevoice-core.service".to_owned(),
            false,
            "masked\n",
        )];
        let unknown = login_with(&scripted(masked, log.clone()), Kind::Systemd, &[Job::Core]).await;
        assert_eq!(unknown, None);

        let failed = vec![(format!("print-disabled {}", domain()), false, "")];
        let unknown = login_with(&scripted(failed, log), Kind::Launchd, &Job::ALL).await;
        assert_eq!(unknown, None, "launchctl could not be asked");
    }

    fn service(kind: Kind, core: bool, connector: bool) -> Installed {
        Installed {
            kind,
            core: core.then(|| PathBuf::from("/defs/core")),
            connector: connector.then(|| PathBuf::from("/defs/connector")),
        }
    }

    #[tokio::test]
    async fn a_start_turns_login_on_only_for_a_new_service_or_one_that_has_it_on() {
        // install, upgrade (`define_and_start`), start and restart all decide through this.
        assert!(enable_on_start(true, None), "a new service starts at login");
        assert!(enable_on_start(false, Some(true)));
        assert!(
            !enable_on_start(false, Some(false)),
            "the person's off holds"
        );
        assert!(
            !enable_on_start(false, None),
            "not known is never turned on"
        );

        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let both = service(Kind::Systemd, true, true);
        assert!(login_to_keep(&scripted(enabled(), log.clone()), Some(&both)).await);
        assert!(login_to_keep(&scripted(vec![], log.clone()), None).await);

        // Enabled for this boot only, or a manager that does not answer: left as it is.
        let runtime = Job::ALL
            .iter()
            .map(|job| {
                (
                    format!("is-enabled {}", job.unit()),
                    true,
                    "enabled-runtime\n",
                )
            })
            .collect();
        assert!(!login_to_keep(&scripted(runtime, log.clone()), Some(&both)).await);
        let silent = Job::ALL
            .iter()
            .map(|job| (format!("is-enabled {}", job.unit()), false, ""))
            .collect();
        assert!(!login_to_keep(&scripted(silent, log.clone()), Some(&both)).await);
        let unreadable = vec![(format!("print-disabled {}", domain()), false, "")];
        let launchd = service(Kind::Launchd, true, true);
        assert!(!login_to_keep(&scripted(unreadable, log.clone()), Some(&launchd)).await);

        // And a start that leaves it as it is enables nothing.
        log.lock().unwrap().clear();
        let prepared =
            prepare_systemd(scripted(vec![], log.clone()), false, false, &Job::ALL).await;
        assert_eq!(prepared, Ok(()));
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn launchd_bootstraps_a_job_off_for_login_reenabled_and_refuses_when_it_cannot_tell() {
        assert_eq!(boot_plan(true, None), Boot::Plain);
        assert_eq!(boot_plan(false, Some(true)), Boot::Plain);
        assert_eq!(boot_plan(false, Some(false)), Boot::Reenabled);
        assert_eq!(boot_plan(false, None), Boot::Refused);
    }

    #[tokio::test]
    async fn an_incomplete_service_never_reports_start_at_login() {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        for (core, connector, expected) in [
            (true, true, Some(true)),
            (true, false, None),
            (false, true, None),
            (false, false, None),
        ] {
            let installed = service(Kind::Systemd, core, connector);
            let login = login_of(&scripted(enabled(), log.clone()), &installed).await;
            assert_eq!(login, expected, "core {core}, connector {connector}");
        }
        assert_eq!(missing(&service(Kind::Systemd, true, true)), vec![]);
        let none = missing(&service(Kind::Systemd, false, false));
        assert_eq!(
            names(&none, Kind::Systemd),
            "sidevoice-core.service, sidevoice-connector.service"
        );
        assert_eq!(
            names(
                &missing(&service(Kind::Launchd, true, false)),
                Kind::Launchd
            ),
            "dev.sidevoice.connector"
        );
        assert_eq!(
            names(
                &missing(&service(Kind::Launchd, false, true)),
                Kind::Launchd
            ),
            "dev.sidevoice.core"
        );
    }

    #[test]
    fn login_answers_read_as_on_off_or_unknown() {
        assert_eq!(
            parse_launchd_login(PRINT_DISABLED_OFF, "dev.sidevoice.core"),
            Some(false)
        );
        assert_eq!(
            parse_launchd_login(PRINT_DISABLED_ON, "dev.sidevoice.core"),
            Some(true)
        );
        let older = "disabled services = {\n\t\"dev.sidevoice.core\" => true\n\t\"dev.sidevoice.connector\" => false\n}\n";
        assert_eq!(
            parse_launchd_login(older, "dev.sidevoice.core"),
            Some(false)
        );
        assert_eq!(
            parse_launchd_login(older, "dev.sidevoice.connector"),
            Some(true)
        );
        assert_eq!(
            parse_launchd_login("disabled services = {\n}\n", "dev.sidevoice.core"),
            Some(true),
            "no override: it loads with its plist"
        );
        assert_eq!(parse_launchd_login("", "dev.sidevoice.core"), None);
        assert_eq!(
            parse_launchd_login(
                "disabled services = {\n\t\"dev.sidevoice.core\" => ???\n}",
                "dev.sidevoice.core"
            ),
            None
        );

        assert_eq!(parse_systemd_login("enabled\n"), Some(true));
        assert_eq!(parse_systemd_login("disabled\n"), Some(false));
        for other in ["masked\n", "static\n", "enabled-runtime\n", ""] {
            assert_eq!(parse_systemd_login(other), None, "{other:?}");
        }

        assert_eq!(combine_login([]), None, "no job");
        assert_eq!(combine_login([Some(true), None]), None);
        assert_eq!(combine_login([None, Some(false)]), Some(false));
    }

    #[tokio::test]
    async fn a_refused_reload_or_enable_fails_the_start_even_when_the_job_would_run() {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut script = enabled();
        script.push(("daemon-reload".into(), false, "Access denied"));
        let reload = prepare_systemd(scripted(script, log.clone()), true, true, &Job::ALL).await;
        assert_eq!(reload, Err((None, "daemon-reload: Access denied".into())));

        let job = Job::ALL[0];
        let mut script = enabled();
        script.push((
            format!("enable {}", job.unit()),
            false,
            "Failed to create symlink: Permission denied",
        ));
        let refused = prepare_systemd(scripted(script, log.clone()), false, true, &Job::ALL).await;
        assert_eq!(
            refused,
            Err((
                Some(job),
                "enable: Failed to create symlink: Permission denied".into()
            ))
        );

        // An enable the manager accepted while the job is still not enabled for the next login.
        let mut script = enabled();
        script.retain(|(line, _, _)| *line != format!("is-enabled {}", job.unit()));
        script.push((format!("is-enabled {}", job.unit()), false, "disabled"));
        let checked = prepare_systemd(scripted(script, log), false, true, &Job::ALL).await;
        assert_eq!(checked, Err((Some(job), "is-enabled: disabled".into())));
    }

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
