//! `sidevoice service install | uninstall | start | stop | restart | status [--json]`: Sidevoice at login, as two
//! jobs of this user's service manager (launchd on macOS, systemd's user manager on Linux), and nothing of ours
//! supervising either; and, where there is no manager, Sidevoice on demand (`launcher`).
//!
//! A stop is a person's: `D/node-stopped.json` is written before the manager is asked, and while it is there the
//! launcher starts nothing; `service start` clears it, and so does the connector job starting at the next login.
//! Every command that changes something holds the install lock. Linux keeps a user's services only while that user
//! has a session; running them without one is `loginctl enable-linger`, a system setting the machine's owner
//! enables: printed, with the reason, and never run.
//!
//! What the node is doing is never stored (`status`): read fresh from the manager, the core's failure report and
//! its health, the same answer for `service status --json` and for a connector's `node.status`.

pub mod definition;
pub mod launcher;
pub mod layout;
pub mod manager;
pub mod process;
pub mod status;
pub mod text;

use crate::profile::Profile;
use crate::secure_fs::{atomic_json, private_dir};
use fs2::FileExt;
use layout::Layout;
use manager::{Job, Kind};
use serde_json::{json, Value};
use std::fmt::{Display, Formatter};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use tokio::time::{sleep, Duration, Instant};

/// How long `install` waits for the core to settle, and `start` and `restart`.
const INSTALL_SETTLE: Duration = Duration::from_secs(60);
const SETTLE: Duration = Duration::from_secs(10);
const LOCK_WAIT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Install,
    Start,
    Stop,
    Restart,
    Status,
    Uninstall,
}

/// A refusal: a stable key, its parameters, and (for a core that failed) the failure as the core described it.
#[derive(Debug)]
pub struct Failure {
    pub key: String,
    params: Value,
    failure: Option<Value>,
}

impl Failure {
    pub fn keyed(key: &str, params: Value) -> Self {
        Self::keyed_owned(key.to_owned(), params)
    }

    pub fn keyed_owned(key: String, params: Value) -> Self {
        Self {
            key,
            params,
            failure: None,
        }
    }

    fn described(key: String, failure: Value) -> Self {
        Self {
            params: json!({"detail": failure.get("detail").cloned().unwrap_or(Value::Null)}),
            key,
            failure: Some(failure),
        }
    }

    pub fn plain(error: impl Display) -> Self {
        let detail: String = error
            .to_string()
            .chars()
            .filter(|ch| !ch.is_control())
            .take(240)
            .collect();
        Self::keyed("service.failed", json!({"detail": detail}))
    }

    pub fn message(&self) -> String {
        match self
            .failure
            .as_ref()
            .and_then(|failure| failure.get("message"))
            .and_then(Value::as_str)
        {
            Some(message) => message.to_owned(),
            None => text::message(&self.key, &self.params),
        }
    }

    /// `{ok: false, error: {key, message}}`.
    pub fn value(&self) -> Value {
        json!({"ok": false, "error": {"key": self.key, "message": self.message()}})
    }
}

impl Display for Failure {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for Failure {}

pub type Result<T> = std::result::Result<T, Failure>;

/// `D`, created private when missing.
fn data_dir(layout: &Layout) -> Result<()> {
    if fs::symlink_metadata(&layout.data).is_err() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&layout.data)
            .map_err(Failure::plain)?;
    }
    private_dir(&layout.data).map_err(Failure::plain)
}

fn open_lock(layout: &Layout) -> Result<File> {
    data_dir(layout)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(layout.install_lock())
        .map_err(Failure::plain)
}

/// The install lock, waited for up to 30 s.
async fn install_lock(layout: &Layout) -> Result<File> {
    let file = open_lock(layout)?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(error)
                if error.kind() != io::ErrorKind::WouldBlock
                    && error.raw_os_error() != Some(libc::EWOULDBLOCK) =>
            {
                return Err(Failure::plain(error))
            }
            Err(_) if Instant::now() >= deadline => {
                return Err(Failure::keyed(
                    "service.busy",
                    json!({"detail": "another install or service command"}),
                ))
            }
            Err(_) => sleep(Duration::from_millis(50)).await,
        }
    }
}

fn write_stop(layout: &Layout) -> Result<()> {
    data_dir(layout)?;
    atomic_json(&layout.stop_marker(), &json!({"at": status::now()})).map_err(Failure::plain)
}

fn clear_stop(layout: &Layout) -> Result<()> {
    match fs::remove_file(layout.stop_marker()) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(Failure::plain(error)),
        _ => Ok(()),
    }
}

/// Wait, up to `limit`, for the core's state to settle: what start, restart and install answer with.
async fn settle(layout: &Layout, limit: Duration) -> Value {
    let deadline = Instant::now() + limit;
    let mut now = status::status(layout, None).await;
    while !status::settled(&now) && Instant::now() < deadline {
        sleep(Duration::from_millis(200)).await;
        now = status::status(layout, None).await;
    }
    now
}

fn answer(state: &Value, service: Kind) -> Value {
    let mut answer = json!({"ok": true, "state": state.get("state").cloned().unwrap_or(Value::Null),
        "service": service.as_str()});
    if let Some(failure) = state.get("failure").filter(|failure| !failure.is_null()) {
        answer["failure"] = failure.clone();
    }
    answer
}

/// `service install`: both definitions written for the selected installation, anything running on demand stopped,
/// both jobs started; answered with the core's state once it settles. Linux says what linger would add.
pub async fn install(layout: &Layout) -> Result<Value> {
    let _lock = install_lock(layout).await?;
    if !layout.installed() {
        return Err(Failure::keyed("service.no-installation", json!({})));
    }
    let kind = manager::kind().await;
    if kind == Kind::None {
        return Err(Failure::keyed("service.no-manager", json!({})));
    }
    let texts = definition::texts(layout, kind)?;
    clear_stop(layout)?;
    let had = manager::installed(layout).await;
    let mut changed = Vec::new();
    for job in Job::ALL {
        let file = manager::definition_path(layout, kind, job)
            .await
            .expect("a manager has definition paths");
        if definition::write(&file, &texts[&job])? {
            changed.push(job);
        }
    }
    if had.is_none() {
        stop_on_demand_or_fail(layout).await?;
    }
    manager::start(layout, kind, &changed, false, &Job::ALL).await?;
    let now = settle(layout, INSTALL_SETTLE).await;
    let mut result = answer(&now, kind);
    if kind == Kind::Systemd {
        result["linger"] = manager::linger().await;
    }
    Ok(result)
}

async fn stop_on_demand_or_fail(layout: &Layout) -> Result<launcher::Teardown> {
    let down = launcher::stop_on_demand(layout).await;
    if !down.left.is_empty() {
        return Err(Failure::keyed(
            "service.unload-failed",
            json!({"detail": format!("still running: {}", down.left.join(", "))}),
        ));
    }
    Ok(down)
}

fn killed_note(result: &mut Value, down: &launcher::Teardown) {
    if !down.killed.is_empty() {
        let pids = down
            .killed
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        result["note"] = json!(text::message("service.killed", &json!({"pids": pids})));
    }
}

/// `service start`: a stop no longer holds; the manager starts both jobs. With no jobs, nothing is started here:
/// the next conversation starts Sidevoice on demand.
pub async fn start(layout: &Layout) -> Result<Value> {
    let installed = manager::installed(layout).await;
    {
        let _lock = install_lock(layout).await?;
        clear_stop(layout)?;
        if let Some(found) = &installed {
            manager::start(layout, found.kind, &[], false, &Job::ALL).await?;
        }
    }
    let now = settle(layout, SETTLE).await;
    Ok(answer(
        &now,
        installed.map_or(Kind::None, |found| found.kind),
    ))
}

/// `service stop`: the person's stop, written first so nothing starts Sidevoice again meanwhile; then the manager
/// stops both jobs (launchd: bootout, so KeepAlive does not bring them back until the next login). With no jobs,
/// what runs on demand is stopped.
pub async fn stop(layout: &Layout) -> Result<Value> {
    let _lock = install_lock(layout).await?;
    write_stop(layout)?;
    let installed = manager::installed(layout).await;
    if let Some(found) = &installed {
        if let Some(refused) = manager::unload(found, false).await {
            return Err(Failure::keyed(
                "service.unload-failed",
                json!({"detail": refused}),
            ));
        }
    }
    let down = stop_on_demand_or_fail(layout).await?;
    let mut result = json!({"ok": true, "state": "stopped-by-person",
        "service": installed.map_or(Kind::None, |found| found.kind).as_str()});
    killed_note(&mut result, &down);
    Ok(result)
}

/// `service restart`, the person's retry: the core job restarted (its start limit cleared first); stopped, it is a
/// start. With no jobs, the core running on demand is ended, and the connector starts it again when it needs it.
pub async fn restart(layout: &Layout) -> Result<Value> {
    if status::stopped(layout) {
        return start(layout).await;
    }
    let installed = manager::installed(layout).await;
    {
        let _lock = install_lock(layout).await?;
        match &installed {
            Some(found) if found.core.is_some() => {
                manager::start(layout, found.kind, &[], true, &[Job::Core]).await?;
            }
            _ => {
                for pid in launcher::core_processes(layout) {
                    launcher::terminate_core(layout, pid, Duration::from_secs(15)).await;
                }
            }
        }
    }
    let now = settle(layout, SETTLE).await;
    Ok(answer(
        &now,
        installed.map_or(Kind::None, |found| found.kind),
    ))
}

/// `service uninstall`: the stop written, both jobs unloaded (any refusal stops here, with nothing deleted), what
/// runs on demand stopped, both definitions deleted and the manager told. `keep_stopped`: the caller goes on
/// taking the installation apart and clears the marker itself. Idempotent.
pub async fn uninstall(layout: &Layout, keep_stopped: bool) -> Result<Value> {
    let _lock = install_lock(layout).await?;
    write_stop(layout)?;
    let installed = manager::installed(layout).await;
    let kind = installed.as_ref().map_or(Kind::None, |found| found.kind);
    if let Some(found) = &installed {
        if let Some(refused) = manager::unload(found, true).await {
            return Err(Failure::keyed(
                "service.unload-failed",
                json!({"detail": refused}),
            ));
        }
    }
    let down = stop_on_demand_or_fail(layout).await?;
    if let Some(found) = &installed {
        for file in [&found.core, &found.connector].into_iter().flatten() {
            if definition::existing(file)? {
                fs::remove_file(file).map_err(Failure::plain)?;
            }
        }
        manager::forget(kind).await?;
    }
    if !keep_stopped {
        clear_stop(layout)?;
    }
    let mut result = json!({"ok": true, "state": if layout.installed() { "not-installed" } else { "absent" },
        "service": kind.as_str()});
    killed_note(&mut result, &down);
    Ok(result)
}

/// One service command against the installation this environment names: its JSON answer.
pub async fn run(action: Action) -> Value {
    let layout = match Layout::from_env() {
        Ok(layout) => layout,
        Err(error) => return Failure::plain(error).value(),
    };
    let result = match action {
        Action::Status => Ok(status::status(&layout, None).await),
        Action::Install => install(&layout).await,
        Action::Start => start(&layout).await,
        Action::Stop => stop(&layout).await,
        Action::Restart => restart(&layout).await,
        Action::Uninstall => uninstall(&layout, false).await,
    };
    result.unwrap_or_else(|failure| failure.value())
}

/// An answer as a person reads it: the state, its failure, and what to do about linger.
pub fn human(answer: &Value) -> String {
    if answer.get("ok") != Some(&json!(true)) {
        return answer
            .pointer("/error/message")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| text::message("service.failed", &Value::Null));
    }
    let state = answer
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("failed");
    let mut out = text::message(
        &format!("service.state.{state}"),
        &json!({"service": answer["service"]}),
    );
    if let Some(failure) = answer.get("failure").filter(|failure| failure.is_object()) {
        let reason = failure
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| text::message(failure["key"].as_str().unwrap_or(""), &Value::Null));
        out.push_str(&format!(" ({reason})"));
    }
    if let Some(note) = answer.get("note").and_then(Value::as_str) {
        out.push('\n');
        out.push_str(note);
    }
    if let Some(linger) = answer
        .get("linger")
        .filter(|linger| linger["enabled"] == json!(false))
    {
        out.push_str(&format!(
            "\n\n{}\n    {}",
            linger["reason"].as_str().unwrap_or_default(),
            linger["command"].as_str().unwrap_or_default()
        ));
    }
    out
}

/// `node.status` for a connector answering it itself.
pub async fn node_status(profile: &Profile) -> Value {
    status::status(&Layout::from_profile(profile), Some(true)).await
}

/// A connector started by its manager at login clears a person's stop, unless a service command (which holds the
/// install lock) is under way: then the stop being written now is the one that holds. Whether it may serve.
pub fn managed_start_clears_stop(profile: &Profile) -> anyhow::Result<bool> {
    let layout = Layout::from_profile(profile);
    if !status::stopped(&layout) {
        return Ok(true);
    }
    let lock = open_lock(&layout)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if lock.try_lock_exclusive().is_ok() {
            let cleared = clear_stop(&layout).map(|()| true);
            let _ = FileExt::unlock(&lock);
            return Ok(cleared?);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(!status::stopped(&layout));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// The supervisor a connector keeps over a core it starts (none while a core job is defined).
pub fn core_supervisor(profile: &Profile) -> launcher::CoreSupervisor {
    launcher::CoreSupervisor::new(Layout::from_profile(profile))
}

/// Get this machine's connector answering for an MCP server.
pub async fn ensure_connector(profile: &Profile) -> anyhow::Result<()> {
    let layout = Layout::from_profile(profile);
    let executable = std::env::current_exe()?.to_string_lossy().into_owned();
    let fallback = if profile.is_installed() {
        vec![executable, "--installed".into()]
    } else {
        vec![executable]
    };
    let environment: Vec<(String, PathBuf)> = [
        ("HOME", &profile.home),
        ("CLAUDE_CONFIG_DIR", &profile.claude),
        ("CODEX_HOME", &profile.codex),
        ("CURSOR_CONFIG_DIR", &profile.cursor),
        ("CURSOR_DATA_DIR", &profile.cursor_data),
        ("XDG_CONFIG_HOME", &profile.xdg_config),
        ("XDG_DATA_HOME", &profile.xdg_data),
        ("SIDEVOICE_DATA_DIR", &profile.data),
    ]
    .into_iter()
    .map(|(name, path)| (name.to_owned(), path.clone()))
    .collect();
    launcher::ensure_connector(&layout, fallback, environment, || verify_connector(profile))
        .await
        .map_err(anyhow::Error::new)
}

/// The connector on the socket answers, and (installed) runs the selected release.
async fn verify_connector(profile: &Profile) -> bool {
    let layout = Layout::from_profile(profile);
    let Some(identity) =
        launcher::ask_connector(&layout, "identity", json!({}), Duration::from_secs(1)).await
    else {
        return false;
    };
    let Some(selected) = &profile.installed else {
        return true;
    };
    let text = |name: &str| {
        identity
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    text("version").as_deref() == Some(selected.connector.as_str())
        && text("runtime_kind").as_deref() == Some(selected.runtime_kind.as_str())
        && text("runtime_sha256") == selected.runtime_sha256
        && text("release_id").as_deref() == Some(selected.id.as_str())
}
