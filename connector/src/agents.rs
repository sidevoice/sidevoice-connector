//! The host-agent coordinator: which agents (each harness with a `Registration`) are on this computer, and registering
//! Sidevoice with them or removing it, only ever touching an entry Sidevoice wrote.
//!
//! One gate owns a complete request. CLI children belong to its worker task so a vanished link reply can
//! cancel the work without releasing the gate before the child has exited.

use crate::adapters::health::{self, Health};
use crate::adapters::markers::{self, Roots};
use crate::adapters::registration::{
    hand_command, mcp_servers_json, with_readiness, AgentError, AgentHost, CliError, CliOutput,
    Launch, Manual, Registration,
};
use crate::adapters::Registry;
use crate::messages::message;
use crate::platform::data_dir;
use crate::platform::env::{Env, Environment, LoginPath};
use crate::platform::fs::{Files, Fs};
use crate::platform::target::{Host, Target as _};
use crate::profile::Profile;
use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

#[cfg(test)]
pub(crate) mod tests;
use crate::platform::process::{Child, Process, Processes};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration as StdDuration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{Mutex, Notify, OwnedMutexGuard};
use tokio::task::JoinHandle;
use tokio::time::{sleep, sleep_until, timeout, Instant};
use uuid::Uuid;

const REQUEST_LIMIT: StdDuration = StdDuration::from_secs(18);
const CHILD_LIMIT: StdDuration = StdDuration::from_millis(2500);
const OUTPUT_LIMIT: usize = 64 * 1024;
const STATE_LIMIT: u64 = 4 * 1024 * 1024;
/// The longest a health check of one agent takes.
const HEALTH_LIMIT: StdDuration = StdDuration::from_secs(5);
/// What a request keeps after its health checks, to save what it found.
const HEALTH_MARGIN: StdDuration = StdDuration::from_secs(1);
/// Less than this left and a scan keeps the last health it found rather than check.
const HEALTH_MINIMUM: StdDuration = StdDuration::from_millis(500);

#[derive(Clone)]
struct Cancellation {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl Cancellation {
    fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
        }
    }

    fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::SeqCst) {
            // Every waiter: a scan checks several agents at once, each waiting on the same request.
            self.notify.notify_waiters();
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    async fn cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

struct CancelOnDrop(Option<Cancellation>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancel) = self.0.take() {
            cancel.cancel();
        }
    }
}

struct ActiveOperation {
    cancel: Cancellation,
    task: JoinHandle<()>,
}

/// An agent, by the id of its harness.
type AgentId = &'static str;

/// Unit-test-only binary fixture. Production CLI detection uses the captured login PATH.
#[cfg(test)]
fn override_name(id: AgentId) -> String {
    format!("SIDEVOICE_{}_BIN", id.to_ascii_uppercase())
}

/// How the agent list names an agent.
fn label(id: AgentId) -> String {
    message(&format!("harness.{id}"), &Value::Null)
}

#[derive(Clone)]
enum Ownership {
    /// This very binary, run by path: what a connector registers when nothing is installed.
    Executable { executable: PathBuf },
    /// The installation's command (`install.json`'s, the selected release's binary): an entry naming it, a release's
    /// binary below `R/releases`, or this very binary is ours.
    Installed {
        releases: PathBuf,
        executable: Option<PathBuf>,
    },
    #[cfg(test)]
    Selected {
        record: Vec<String>,
        release_root: PathBuf,
    },
}

/// Immutable command selected for one request.
#[derive(Clone)]
struct InstalledCommand {
    command: String,
    args: Vec<String>,
    version: String,
    ownership: Ownership,
}

impl InstalledCommand {
    fn executable() -> Result<Self> {
        let executable = std::env::current_exe()?.canonicalize()?;
        Ok(Self {
            command: executable.to_string_lossy().into_owned(),
            args: vec!["mcp".into()],
            version: crate::identity::VERSION.into(),
            ownership: Ownership::Executable { executable },
        })
    }

    /// The installation's command, when `install.json` names one program (what `install` writes): what every
    /// agent registers; an upgrade changes it, and `install` writes again the entries that name an older release.
    fn installed(profile: &Profile) -> Option<Self> {
        let layout = crate::service::layout::Layout::from_profile(profile);
        let command = layout.connector_command()?;
        let [program] = command.as_slice() else {
            return None;
        };
        Some(Self {
            command: program.clone(),
            args: vec!["mcp".into()],
            version: profile.connector_version().into(),
            ownership: Ownership::Installed {
                releases: layout.releases.join("releases"),
                executable: std::env::current_exe()
                    .and_then(|path| path.canonicalize())
                    .ok(),
            },
        })
    }

    #[cfg(test)]
    fn selected(record: Vec<String>, release_root: PathBuf) -> Self {
        let (command, args) = if record.len() == 2 {
            ("node".into(), vec![record[1].clone(), "mcp".into()])
        } else {
            (record[0].clone(), vec!["mcp".into()])
        };
        Self {
            command,
            args,
            version: "test-version".into(),
            ownership: Ownership::Selected {
                record,
                release_root,
            },
        }
    }

    fn owns(&self, command: &str, args: &[String]) -> bool {
        match &self.ownership {
            Ownership::Executable { executable } => {
                canonical(command).as_deref() == Some(executable.as_path()) && args == ["mcp"]
            }
            Ownership::Installed {
                releases,
                executable,
            } => {
                if args != ["mcp"] {
                    return false;
                }
                if command == self.command {
                    return true;
                }
                let Some(found) = canonical(command) else {
                    return false;
                };
                if executable.as_ref() == Some(&found) {
                    return true;
                }
                let releases = canonical(&releases.to_string_lossy()).unwrap_or(releases.clone());
                let Ok(relative) = found.strip_prefix(&releases) else {
                    return false;
                };
                let parts = relative.components().collect::<Vec<_>>();
                parts.len() == 3
                    && parts[0]
                        .as_os_str()
                        .to_string_lossy()
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || ".+-_".contains(ch))
                    && parts[1].as_os_str() == "bin"
                    && parts[2].as_os_str() == Host::exe_name("sidevoice-connector").as_str()
            }
            #[cfg(test)]
            Ownership::Selected {
                record,
                release_root,
            } => {
                if command == self.command && args == self.args {
                    return true;
                }
                if record.len() == 2 {
                    path_basename(command) == Some("node")
                        && args.len() == 2
                        && args[1] == "mcp"
                        && js_release_program(&args[0], release_root)
                } else {
                    args == ["mcp"] && js_release_program(command, release_root)
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct HostAgents {
    profile: Profile,
    selected: InstalledCommand,
    registry: Arc<Registry>,
    gate: Arc<Mutex<()>>,
    operations: Arc<Mutex<Vec<ActiveOperation>>>,
    stopping: Arc<AtomicBool>,
    /// The installer's own requests (`install`, `uninstall`): a person's stop does not hold them back.
    installer: bool,
}

#[derive(Debug)]
enum Failure {
    Cancelled,
    Busy,
    Internal,
    Key(&'static str, Map<String, Value>),
}

impl Failure {
    fn keyed(key: &'static str, params: Value) -> Self {
        Self::Key(key, params.as_object().cloned().unwrap_or_default())
    }
}

#[derive(Debug)]
enum CommandFailure {
    Cancelled,
    Deadline,
    Start,
    Io,
}

#[derive(Serialize, Clone)]
struct Evidence<'a> {
    kind: &'a str,
}

struct Observation {
    row: Value,
    /// How the agent starts Sidevoice's entry, when its configuration names one.
    launch: Option<Launch>,
    binary: Option<String>,
    present: bool,
    signature: Option<String>,
}

impl HostAgents {
    pub fn new(profile: Profile) -> Result<Arc<Self>> {
        let selected = match InstalledCommand::installed(&profile) {
            Some(installed) => installed,
            None => InstalledCommand::executable()?,
        };
        Ok(Self::with_command(profile, selected))
    }

    fn with_command(profile: Profile, selected: InstalledCommand) -> Arc<Self> {
        Arc::new(Self {
            selected,
            profile,
            registry: Arc::new(Registry::standard()),
            gate: Arc::new(Mutex::new(())),
            operations: Arc::new(Mutex::new(Vec::new())),
            stopping: Arc::new(AtomicBool::new(false)),
            installer: false,
        })
    }

    /// For `install` and `uninstall`: the installation's command, and requests that go ahead while Sidevoice is
    /// stopped (uninstall stops it first).
    pub fn for_installer(profile: Profile) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            installer: true,
            ..Arc::unwrap_or_clone(Self::new(profile)?)
        }))
    }

    /// Whether a person's stop cancels this request.
    fn stopped(&self) -> std::result::Result<bool, Failure> {
        Ok(!self.installer
            && self
                .profile
                .service_stopped()
                .map_err(|_| Failure::Internal)?)
    }

    #[cfg(test)]
    fn with_selected(profile: Profile, selected: InstalledCommand) -> Arc<Self> {
        Self::with_command(profile, selected)
    }

    pub async fn handle(self: &Arc<Self>, method: &str, params: Value) -> Value {
        let cancel = Cancellation::new();
        let _cancel_on_drop = CancelOnDrop(Some(cancel.clone()));
        let deadline = Instant::now() + REQUEST_LIMIT;
        let this = self.clone();
        let method = method.to_owned();
        let (answer_tx, answer_rx) = tokio::sync::oneshot::channel();
        let worker_cancel = cancel.clone();
        let mut operations = self.operations.lock().await;
        if self.stopping.load(Ordering::SeqCst) {
            return error_value("agents.busy", json!({}));
        }
        let task = tokio::spawn(async move {
            let answer = this
                .run_request(&method, params, &worker_cancel, deadline)
                .await;
            let _ = answer_tx.send(answer);
        });
        operations.retain(|operation| !operation.task.is_finished());
        operations.push(ActiveOperation {
            cancel: cancel.clone(),
            task,
        });
        drop(operations);
        match answer_rx.await {
            Ok(Ok(value)) => value,
            Ok(Err(Failure::Cancelled)) => {
                error_value("agents.action-failed", json!({"agent":"host agent"}))
            }
            Ok(Err(Failure::Busy)) => error_value("agents.busy", json!({})),
            Ok(Err(Failure::Internal)) => {
                error_value("agents.action-failed", json!({"agent":"host agent"}))
            }
            Ok(Err(Failure::Key(key, params))) => error_value(key, Value::Object(params)),
            Err(_) => error_value("agents.action-failed", json!({"agent":"host agent"})),
        }
    }

    /// Every operation cancelled at once (each kills its child tree), then waited for until `deadline`; one still
    /// running then is aborted, which drops its child and with it the tree (`Processes::spawn_tree` kills on drop).
    pub async fn shutdown_until(&self, deadline: Instant) {
        self.stopping.store(true, Ordering::SeqCst);
        let operations = {
            let mut active = self.operations.lock().await;
            std::mem::take(&mut *active)
        };
        for operation in &operations {
            operation.cancel.cancel();
        }
        for mut operation in operations {
            if tokio::time::timeout_at(deadline, &mut operation.task)
                .await
                .is_err()
            {
                operation.task.abort();
                let _ = operation.task.await;
            }
        }
    }

    /// [`Self::shutdown_until`] with no deadline worth the name, for tests that wait for every operation to end.
    #[cfg(test)]
    pub async fn shutdown(&self) {
        self.shutdown_until(Instant::now() + StdDuration::from_secs(3600))
            .await;
    }

    async fn run_request(
        &self,
        method: &str,
        params: Value,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<Value, Failure> {
        let gate = self.gate.clone();
        let permit: OwnedMutexGuard<()> = tokio::select! {
            _ = cancel.cancelled() => return Err(Failure::Cancelled),
            _ = sleep_until(deadline) => return Err(Failure::Busy),
            permit = gate.lock_owned() => permit,
        };
        let result = self.dispatch(method, params, cancel, deadline).await;
        drop(permit);
        result
    }

    async fn dispatch(
        &self,
        method: &str,
        params: Value,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<Value, Failure> {
        if method == "agents.list" {
            let watch = match params.get("watch").and_then(Value::as_str) {
                Some(value) => Some(parse_id(&self.registry, value)?),
                None => None,
            };
            let rescan = params.get("rescan") == Some(&Value::Bool(true));
            let mut state = self.load_state(cancel, deadline).await?;
            let has_cache = state
                .get("scanned_at")
                .is_some_and(|value| !value.is_null());
            if rescan || watch.is_some() || !has_cache {
                state = self.scan(state, watch, true, cancel, deadline).await?;
            }
            return self.response(&state);
        }

        let action = method.strip_prefix("agents.").unwrap_or("");
        if !matches!(action, "connect" | "disconnect" | "dismiss" | "check") {
            return Err(Failure::keyed(
                "agents.unknown-request",
                json!({"route":method.chars().take(120).collect::<String>()}),
            ));
        }
        let id = params.get("id").and_then(Value::as_str).unwrap_or("");
        let agent = parse_id(&self.registry, id)?;
        let state = self.load_state(cancel, deadline).await?;
        let mut state = self
            .scan(state, Some(agent), action == "check", cancel, deadline)
            .await?;
        if action != "check" {
            require_scanned(&state, agent)?;
        }
        let current = seen_agent(&state, agent).ok_or_else(|| not_present(agent))?;
        if action == "check" {
            return self.response(&state);
        }
        let registration = current
            .get("registration")
            .and_then(Value::as_str)
            .unwrap_or("unknown");

        if action == "dismiss" {
            let store = self
                .update_state(cancel, deadline, false, |latest| {
                    if let Some(fresh) = seen_entry(latest, agent) {
                        if fresh.pointer("/agent/registration").and_then(Value::as_str)
                            == Some("not-connected")
                        {
                            if let Some(generation) = fresh.get("generation") {
                                latest["dismissed"][agent] = generation.clone();
                            }
                        }
                    }
                })
                .await?;
            return self.response(&store);
        }

        if action == "connect" {
            match registration {
                "connected" => return self.response(&state),
                "foreign" => return Err(agent_failure("agents.foreign", agent)),
                "invalid" => return Err(agent_failure("agents.invalid", agent)),
                "unknown" => return Err(agent_failure("agents.registration-unknown", agent)),
                // An entry an older Sidevoice wrote is written again.
                "not-connected" | "outdated" => {}
                _ => return Err(agent_failure("agents.registration-unknown", agent)),
            }
            if current.get("connect").and_then(Value::as_str) != Some("auto") {
                return Err(agent_failure("agents.manual-required", agent));
            }
            let binary = state
                .pointer(&format!("/binaries/{agent}"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            self.connect(agent, binary.as_deref(), cancel, deadline)
                .await?;
            state = self
                .scan(state, Some(agent), true, cancel, deadline)
                .await?;
            require_verified(&state, agent)?;
            match seen_agent(&state, agent)
                .and_then(|row| row.get("registration"))
                .and_then(Value::as_str)
            {
                Some("connected") => {}
                Some("not-connected" | "outdated") => {
                    return Err(agent_failure("agents.registration-not-confirmed", agent))
                }
                Some("foreign") => return Err(agent_failure("agents.foreign", agent)),
                Some("invalid") => return Err(agent_failure("agents.invalid", agent)),
                _ => return Err(agent_failure("agents.registration-unknown", agent)),
            }
            let store = self
                .update_state(cancel, deadline, false, |latest| {
                    if seen_agent(latest, agent)
                        .and_then(|row| row.get("registration"))
                        .and_then(Value::as_str)
                        == Some("connected")
                    {
                        if let Some(dismissed) =
                            latest.get_mut("dismissed").and_then(Value::as_object_mut)
                        {
                            dismissed.remove(agent);
                        }
                    }
                })
                .await?;
            return self.response(&store);
        }

        match registration {
            "not-connected" => return self.response(&state),
            "foreign" => return Err(agent_failure("agents.foreign", agent)),
            "invalid" => return Err(agent_failure("agents.invalid", agent)),
            "unknown" => return Err(agent_failure("agents.registration-unknown", agent)),
            // Ours, current or not: removed.
            "connected" | "outdated" => {}
            _ => return Err(agent_failure("agents.registration-unknown", agent)),
        }
        let binary = state
            .pointer(&format!("/binaries/{agent}"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.disconnect(agent, binary.as_deref(), cancel, deadline)
            .await?;
        state = self
            .scan(state, Some(agent), false, cancel, deadline)
            .await?;
        require_verified(&state, agent)?;
        match seen_agent(&state, agent)
            .and_then(|row| row.get("registration"))
            .and_then(Value::as_str)
        {
            Some("not-connected") => {}
            Some("connected" | "outdated") => {
                return Err(agent_failure("agents.action-failed", agent))
            }
            Some("foreign") => return Err(agent_failure("agents.foreign", agent)),
            Some("invalid") => return Err(agent_failure("agents.invalid", agent)),
            _ => return Err(agent_failure("agents.registration-unknown", agent)),
        }
        self.response(&state)
    }

    fn response(&self, state: &Value) -> std::result::Result<Value, Failure> {
        let mut rows = Vec::new();
        for (id, _) in self.registry.agents() {
            if let Some(seen) = state.pointer(&format!("/seen/{id}")) {
                if state.get("login_path").and_then(Value::as_str).is_some()
                    && seen.get("present") == Some(&Value::Bool(true))
                {
                    if let Some(mut row) = seen.get("agent").cloned() {
                        let dismissed = seen
                            .get("generation")
                            .and_then(Value::as_str)
                            .zip(
                                state
                                    .pointer(&format!("/dismissed/{id}"))
                                    .and_then(Value::as_str),
                            )
                            .is_some_and(|(generation, saved)| generation == saved);
                        let actionable = row.get("present") == Some(&Value::Bool(true))
                            && row.get("registration").and_then(Value::as_str)
                                == Some("not-connected")
                            && !dismissed;
                        row["dismissed"] = json!(dismissed);
                        row["actionable"] = json!(actionable);
                        rows.push(row);
                    }
                }
            }
        }
        let command = hand_command(&self.selected.command, &self.selected.args);
        let snippet = mcp_servers_json(&self.selected.command, &self.selected.args, self.mcp_env())
            .map_err(|_| Failure::Internal)?;
        Ok(json!({
            "agents": rows,
            "scanned_at": state.get("scanned_at").cloned().unwrap_or(Value::Null),
            "scan_failed": state.get("scan_failed") == Some(&Value::Bool(true)),
            "custom": {
                "command": command,
                "note": command.is_none().then(|| message("agents.manual.unsafe-command", &json!({"program": self.selected.command}))),
                "snippet": snippet,
                "version": self.selected.version,
            }
        }))
    }

    async fn scan(
        &self,
        state: Value,
        watch: Option<AgentId>,
        check: bool,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<Value, Failure> {
        if self.stopped()? {
            return Err(Failure::Cancelled);
        }
        let base = state.clone();
        let login_path = self.capture_login_path(cancel, deadline).await?;
        if login_path.is_none() {
            // A failed capture says nothing about what is installed: keep the last scan and record the failure, which
            // stays until a scan succeeds. Web needs a `scanned_at`, so a machine never scanned gets the time of this try.
            return self
                .update_state(cancel, deadline, true, |latest| {
                    latest["scan_failed"] = json!(true);
                    if latest.get("scanned_at").is_none_or(Value::is_null) {
                        latest["scanned_at"] = json!(iso_now());
                    }
                })
                .await;
        }
        // After a failed scan every agent is looked at again, not only the watched one.
        let ids: Vec<_> = (if base.get("login_path") != Some(&json!(login_path))
            || base.get("scan_failed") == Some(&Value::Bool(true))
        {
            None
        } else {
            watch
        })
        .map_or_else(
            || self.registry.agents().map(|(id, _)| id).collect(),
            |id| vec![id],
        );
        let full = ids.len() == self.registry.agents().count();
        let mut observations = Vec::new();
        for name in ids {
            check_live(cancel, deadline)?;
            let agent = name;
            observations.push((
                agent,
                self.inspect(agent, login_path.as_deref(), cancel, deadline)
                    .await?,
            ));
        }
        self.health(
            &base,
            &mut observations,
            login_path.as_deref(),
            check,
            cancel,
            deadline,
        )
        .await?;
        self.merge_scan(base, observations, login_path, full, cancel, deadline)
            .await
    }

    /// Each connected agent's `health`: checked now, all at once, when `check` and the request has the time; else the
    /// last one found. An agent that is not connected has none.
    async fn health(
        &self,
        base: &Value,
        observations: &mut [(AgentId, Observation)],
        login_path: Option<&str>,
        check: bool,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<(), Failure> {
        let limit = deadline
            .saturating_duration_since(Instant::now())
            .saturating_sub(HEALTH_MARGIN)
            .min(HEALTH_LIMIT);
        let check = check && limit >= HEALTH_MINIMUM;
        let environment = self.health_environment(login_path);
        let environment = &environment;
        let checks = observations.iter().map(|(id, observation)| {
            let launch = observation
                .launch
                .as_ref()
                .filter(|_| observation.row["registration"] == "connected");
            let previous = base
                .pointer(&format!("/seen/{id}/agent/health"))
                .cloned()
                .unwrap_or(Value::Null);
            async move {
                let Some(launch) = launch else {
                    return Ok(Value::Null);
                };
                if !check {
                    return Ok(previous);
                }
                let found = health::check(launch, environment, limit, cancel.cancelled())
                    .await
                    .map_err(|_| Failure::Cancelled)?;
                let (state, error) = match found {
                    Health::Working => ("working", None),
                    Health::NotStarting(error) => ("not-starting", Some(error)),
                };
                Ok(json!({"state":state,"error":error,"checked_at":iso_now()}))
            }
        });
        let found = futures_util::future::join_all(checks).await;
        for ((_, observation), health) in observations.iter_mut().zip(found) {
            observation.row["health"] = health?;
        }
        Ok(())
    }

    /// What a health check starts a server with before its entry's own environment: what the agent would have had
    /// from this profile (its homes, `HOME`), the person's login PATH as the scan captured it, and the marker that
    /// keeps Sidevoice's own server from starting the daemon.
    fn health_environment(&self, login_path: Option<&str>) -> Vec<(String, OsString)> {
        let mut environment = self
            .profile
            .variables()
            .iter()
            .map(|(key, path)| ((*key).to_owned(), path.as_os_str().to_owned()))
            .collect::<Vec<_>>();
        if let Some(path) = login_path {
            environment.push(("PATH".into(), path.into()));
        }
        environment.push((crate::mcp::HEALTH_CHECK.into(), "1".into()));
        environment
    }

    async fn merge_scan(
        &self,
        base: Value,
        observations: Vec<(AgentId, Observation)>,
        captured_path: Option<String>,
        full: bool,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<Value, Failure> {
        self.update_state(cancel, deadline, true, move |latest| {
            if captured_path.is_none() {
                return;
            }
            let path_unchanged = latest.get("login_path") == base.get("login_path");
            if path_unchanged {
                latest["login_path"] = json!(captured_path);
            }
            for (id, observation) in observations {
                if !path_unchanged
                    || latest.pointer(&format!("/binaries/{}", id))
                        != base.pointer(&format!("/binaries/{}", id))
                {
                    continue;
                }
                if let Some(binary) = &observation.binary {
                    latest["binaries"][id] = json!(binary);
                } else if let Some(binaries) =
                    latest.get_mut("binaries").and_then(Value::as_object_mut)
                {
                    binaries.remove(id);
                }
                let signature = observation.signature;
                let previous = latest.pointer(&format!("/seen/{}", id)).cloned();
                let same = previous.as_ref().is_some_and(|seen| {
                    seen.get("present") == Some(&Value::Bool(true))
                        && seen.get("signature").and_then(Value::as_str) == signature.as_deref()
                });
                let generation = if observation.present {
                    if same {
                        previous
                            .as_ref()
                            .and_then(|seen| seen.get("generation"))
                            .cloned()
                            .unwrap_or_else(|| json!(Uuid::new_v4().to_string()))
                    } else {
                        json!(Uuid::new_v4().to_string())
                    }
                } else {
                    previous
                        .as_ref()
                        .and_then(|seen| seen.get("generation"))
                        .cloned()
                        .unwrap_or(Value::Null)
                };
                let detected_at = if observation.present && !same {
                    json!(iso_now())
                } else {
                    previous
                        .as_ref()
                        .and_then(|seen| seen.get("detected_at"))
                        .cloned()
                        .unwrap_or(Value::Null)
                };
                latest["seen"][id] = json!({
                    "present":observation.present,
                    "signature":signature,
                    "generation":generation,
                    "detected_at":detected_at,
                    "agent":observation.row,
                });
            }
            latest["scanned_at"] = json!(iso_now());
            // Only a scan of every agent, on the path it started from, recovers from a failed one.
            if full && path_unchanged {
                latest["scan_failed"] = json!(false);
            }
        })
        .await
    }

    async fn load_state(
        &self,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<Value, Failure> {
        let _lock = self.state_lock(cancel, deadline).await?;
        load_state_file(&self.profile)
    }

    async fn update_state<F>(
        &self,
        cancel: &Cancellation,
        deadline: Instant,
        service_scan: bool,
        update: F,
    ) -> std::result::Result<Value, Failure>
    where
        F: FnOnce(&mut Value),
    {
        check_live(cancel, deadline)?;
        let _lock = self.state_lock(cancel, deadline).await?;
        check_live(cancel, deadline)?;
        if service_scan && self.stopped()? {
            return Err(Failure::Cancelled);
        }
        let mut state = load_state_file(&self.profile)?;
        update(&mut state);
        check_live(cancel, deadline)?;
        let path = self.profile.data.join("agents.json");
        if path.exists() {
            private_state_file(&path)?;
        }
        Files::write_json(&path, &state).map_err(|_| Failure::Internal)?;
        Ok(state)
    }

    async fn state_lock(
        &self,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<crate::lock::Lock, Failure> {
        let path = self.profile.data.join("agents.lock");
        loop {
            check_live(cancel, deadline)?;
            match crate::lock::try_lock(&path, "agents") {
                Ok(Some(lock)) => return Ok(lock),
                Ok(None) => {
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(Failure::Cancelled),
                        _ = sleep_until(deadline) => return Err(Failure::Busy),
                        _ = sleep(StdDuration::from_millis(10)) => {}
                    }
                }
                Err(_) => return Err(Failure::Internal),
            }
        }
    }

    /// The registration of `id`, which a scan or a request only names when one exists.
    fn registration(&self, id: AgentId) -> std::result::Result<&dyn Registration, Failure> {
        self.registry
            .agent(id)
            .map(|(_, registration)| registration)
            .ok_or(Failure::Internal)
    }

    /// What a registration of `id` borrows for one request.
    fn host<'a>(
        &'a self,
        registration: &dyn Registration,
        cancel: &'a Cancellation,
        deadline: Instant,
    ) -> RequestHost<'a> {
        RequestHost {
            agents: self,
            root: registration.config_root(&self.profile).to_path_buf(),
            cancel,
            deadline,
        }
    }

    async fn connect(
        &self,
        id: AgentId,
        binary: Option<&str>,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<(), Failure> {
        check_live(cancel, deadline)?;
        let registration = self.registration(id)?;
        let host = self.host(registration, cancel, deadline);
        registration
            .connect(&host, binary)
            .await
            .map_err(|error| failure(error, id))
    }

    async fn disconnect(
        &self,
        id: AgentId,
        binary: Option<&str>,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<(), Failure> {
        check_live(cancel, deadline)?;
        let registration = self.registration(id)?;
        let host = self.host(registration, cancel, deadline);
        registration
            .disconnect(&host, binary)
            .await
            .map_err(|error| failure(error, id))
    }
}

/// The coordinator as one request on one agent lends it to that agent's registration.
struct RequestHost<'a> {
    agents: &'a HostAgents,
    root: PathBuf,
    cancel: &'a Cancellation,
    deadline: Instant,
}

impl AgentHost for RequestHost<'_> {
    fn profile(&self) -> &Profile {
        &self.agents.profile
    }

    fn command(&self) -> &str {
        &self.agents.selected.command
    }

    fn args(&self) -> &[String] {
        &self.agents.selected.args
    }

    fn owns(&self, command: &str, args: &[String]) -> bool {
        self.agents.selected.owns(command, args)
    }

    fn mcp_env(&self) -> BTreeMap<String, String> {
        self.agents.mcp_env()
    }

    fn run<'a>(
        &'a self,
        binary: &'a str,
        args: Vec<String>,
    ) -> futures_util::future::BoxFuture<'a, std::result::Result<CliOutput, CliError>> {
        Box::pin(async move {
            run_command(
                &self.agents.profile,
                Some(&self.root),
                binary,
                &args,
                self.cancel,
                self.deadline,
            )
            .await
            .map_err(|error| match error {
                CommandFailure::Cancelled => CliError::Cancelled,
                CommandFailure::Deadline | CommandFailure::Start | CommandFailure::Io => {
                    CliError::Failed
                }
            })
        })
    }

    fn live(&self) -> std::result::Result<(), AgentError> {
        check_live(self.cancel, self.deadline).map_err(|error| match error {
            Failure::Busy => AgentError::Busy,
            _ => AgentError::Cancelled,
        })
    }
}

/// A registration's failure, about agent `id`.
fn failure(error: AgentError, id: AgentId) -> Failure {
    match error {
        AgentError::Cancelled => Failure::Cancelled,
        AgentError::Busy => Failure::Busy,
        AgentError::Key(key) => agent_failure(key, id),
    }
}

fn parse_id(registry: &Registry, value: &str) -> std::result::Result<AgentId, Failure> {
    registry.agent(value).map(|(id, _)| id).ok_or_else(|| {
        Failure::keyed(
            "agents.unknown",
            json!({"id":value.chars().take(120).collect::<String>()}),
        )
    })
}

/// An action on an agent trusts only a scan that happened: while the last one failed, what is cached may be stale.
fn require_scanned(state: &Value, id: AgentId) -> std::result::Result<(), Failure> {
    scanned_or(state, id, "agents.scan-failed")
}

/// The same check after the action ran: the change was applied, only the look afterwards failed.
fn require_verified(state: &Value, id: AgentId) -> std::result::Result<(), Failure> {
    scanned_or(state, id, "agents.scan-failed-after")
}

fn scanned_or(state: &Value, id: AgentId, key: &'static str) -> std::result::Result<(), Failure> {
    if state.get("scan_failed") == Some(&Value::Bool(true)) {
        return Err(agent_failure(key, id));
    }
    Ok(())
}

fn not_present(id: AgentId) -> Failure {
    Failure::keyed("agents.not-present", json!({"id":id,"agent":label(id)}))
}

fn agent_failure(key: &'static str, id: AgentId) -> Failure {
    Failure::keyed(key, json!({"id":id,"agent":label(id)}))
}

fn seen_entry(state: &Value, id: AgentId) -> Option<&Value> {
    state.pointer(&format!("/seen/{}", id))
}

fn seen_agent(state: &Value, id: AgentId) -> Option<&Value> {
    let seen = seen_entry(state, id)?;
    (seen.get("present") == Some(&Value::Bool(true)))
        .then(|| seen.get("agent"))
        .flatten()
}

fn empty_state() -> Value {
    json!({
        "version":1,
        "scanned_at":Value::Null,
        "login_path":Value::Null,
        "binaries":{},
        "seen":{},
        "dismissed":{},
    })
}

fn normalized_state(mut value: Value) -> Value {
    if value.get("version") != Some(&json!(1)) || !value.is_object() {
        return empty_state();
    }
    for key in ["binaries", "seen", "dismissed"] {
        if !value.get(key).is_some_and(Value::is_object) {
            value[key] = json!({});
        }
    }
    if !value
        .as_object()
        .is_some_and(|object| object.contains_key("scanned_at"))
    {
        value["scanned_at"] = Value::Null;
    }
    if !value
        .as_object()
        .is_some_and(|object| object.contains_key("login_path"))
    {
        value["login_path"] = Value::Null;
    }
    value
}

/// How an adapter runs a CLI outside registration: only the executable found on the captured
/// login PATH, and that PATH so a script launcher finds its interpreter. A missing program stays
/// missing; the service manager's unrelated PATH is never used as a fallback.
pub struct AgentCommand {
    pub program: Option<String>,
    pub path: Option<String>,
}

pub fn agent_command(profile: &Profile, agent: &str) -> AgentCommand {
    let id = Registry::standard().agent(agent).map(|(id, _)| id);
    let state = load_state_file(profile).unwrap_or_else(|_| empty_state());
    let path = state
        .get("login_path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .map(str::to_owned);
    let registry = Registry::standard();
    let program = id
        .and_then(|id| registry.agent(id).map(|(_, registration)| registration))
        .and_then(|registration| find_in_path(registration.binary_names(), path.as_deref()));
    AgentCommand { program, path }
}

fn load_state_file(profile: &Profile) -> std::result::Result<Value, Failure> {
    let path = profile.data.join("agents.json");
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(empty_state()),
        Err(_) => return Err(Failure::Internal),
        Ok(_) => {}
    }
    private_state_file(&path)?;
    let file = File::open(&path).map_err(|_| Failure::Internal)?;
    if file.metadata().map_err(|_| Failure::Internal)?.len() > STATE_LIMIT {
        return Err(Failure::Internal);
    }
    let mut bytes = Vec::new();
    file.take(STATE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Failure::Internal)?;
    let parsed = serde_json::from_slice(&bytes).map_err(|_| Failure::Internal)?;
    Ok(normalized_state(parsed))
}

fn private_state_file(path: &Path) -> std::result::Result<(), Failure> {
    let file = Files::open_read(path).map_err(|_| Failure::Internal)?;
    Files::check_private(&file).map_err(|_| Failure::Internal)
}

fn check_live(cancel: &Cancellation, deadline: Instant) -> std::result::Result<(), Failure> {
    if cancel.is_cancelled() {
        Err(Failure::Cancelled)
    } else if Instant::now() >= deadline {
        Err(Failure::Busy)
    } else {
        Ok(())
    }
}

fn error_value(key: &str, params: Value) -> Value {
    let message = message(key, &params);
    json!({"error":{"key":key,"params":params,"message":message}})
}

fn canonical(value: &str) -> Option<PathBuf> {
    Path::new(value).canonicalize().ok()
}

#[cfg(test)]
fn path_basename(value: &str) -> Option<&str> {
    Path::new(value).file_name()?.to_str()
}

#[cfg(test)]
fn js_release_program(program: &str, root: &Path) -> bool {
    let path = Path::new(program);
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let components = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let release_name = |value: &str| {
        !value.is_empty()
            && value
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || ".+-_".contains(character))
    };
    match components.as_slice() {
        [slot, dist, executable] => {
            (slot == "current" || release_name(slot))
                && dist == "dist"
                && (executable == "cli.mjs" || executable == "sidevoice")
        }
        [releases, slot, dist, executable] => {
            releases == "releases"
                && release_name(slot)
                && dist == "dist"
                && (executable == "cli.mjs" || executable == "sidevoice")
        }
        _ => false,
    }
}

#[cfg(test)]
pub(crate) fn executable(value: &str) -> bool {
    let path = Path::new(value);
    if !path.is_absolute() {
        return false;
    }
    which::which(path).is_ok()
}

pub(crate) fn find_in_path(names: &[&str], path: Option<&str>) -> Option<String> {
    let path = path?;
    names.iter().find_map(|name| {
        Environment::find_program(name, OsStr::new(path))
            .map(|found| found.to_string_lossy().into_owned())
    })
}

async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return if truncated {
                Err(std::io::Error::other("command output truncated"))
            } else {
                Ok(output)
            };
        }
        let keep = limit.saturating_sub(output.len()).min(count);
        truncated |= keep < count;
        output.extend_from_slice(&buffer[..keep]);
    }
}

async fn collect_pipe(task: &mut JoinHandle<std::io::Result<Vec<u8>>>) -> Option<Vec<u8>> {
    match timeout(StdDuration::from_secs(1), &mut *task).await {
        Ok(Ok(Ok(output))) => Some(output),
        _ => {
            task.abort();
            None
        }
    }
}

async fn kill_and_reap(child: &mut Child) {
    // Each probe leads its own tree so a shell wrapper cannot leave a delayed writer behind.
    child.kill_tree().await;
}

fn validate_command_profile(
    profile: &Profile,
    agent_config: Option<&Path>,
) -> std::result::Result<(), CommandFailure> {
    let validation = match agent_config {
        Some(config) => profile.validate_for_agent(config),
        None => profile.validate_existing_private(),
    };
    validation.map_err(|_| CommandFailure::Start)
}

async fn run_command(
    profile: &Profile,
    agent_config: Option<&Path>,
    binary: &str,
    args: &[String],
    cancel: &Cancellation,
    deadline: Instant,
) -> std::result::Result<CliOutput, CommandFailure> {
    run_command_with_limit(
        profile,
        agent_config,
        binary,
        args,
        cancel,
        deadline,
        OUTPUT_LIMIT,
    )
    .await
}

async fn run_command_with_limit(
    profile: &Profile,
    agent_config: Option<&Path>,
    binary: &str,
    args: &[String],
    cancel: &Cancellation,
    deadline: Instant,
    output_limit: usize,
) -> std::result::Result<CliOutput, CommandFailure> {
    if cancel.is_cancelled() {
        return Err(CommandFailure::Cancelled);
    }
    validate_command_profile(profile, agent_config)?;
    let mut command = Command::new(binary);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    profile.command_env(&mut command);
    // Recheck immediately before passing these profile roots to a host CLI. The binary may have
    // been selected during an earlier scan, before an agent config directory was replaced.
    validate_command_profile(profile, agent_config)?;
    let mut child = Processes::spawn_tree(command)
        .await
        .map_err(|_| CommandFailure::Start)?;
    let stdout = child.stdout().ok_or(CommandFailure::Io)?;
    let stderr = child.stderr().ok_or(CommandFailure::Io)?;
    let mut stdout_task = tokio::spawn(read_capped(stdout, output_limit));
    let mut stderr_task = tokio::spawn(read_capped(stderr, OUTPUT_LIMIT));
    let child_deadline = (Instant::now() + CHILD_LIMIT).min(deadline);
    let status = tokio::select! {
        _ = cancel.cancelled() => {
            kill_and_reap(&mut child).await;
            let _ = collect_pipe(&mut stdout_task).await;
            let _ = collect_pipe(&mut stderr_task).await;
            return Err(CommandFailure::Cancelled);
        }
        _ = sleep_until(child_deadline) => {
            kill_and_reap(&mut child).await;
            let _ = collect_pipe(&mut stdout_task).await;
            let _ = collect_pipe(&mut stderr_task).await;
            return Err(if Instant::now() >= deadline { CommandFailure::Deadline } else { CommandFailure::Io });
        }
        status = child.wait() => match status {
            Ok(status) => status,
            Err(_) => {
                kill_and_reap(&mut child).await;
                let _ = collect_pipe(&mut stdout_task).await;
                let _ = collect_pipe(&mut stderr_task).await;
                return Err(CommandFailure::Io);
            }
        },
    };
    // The direct CLI is reaped now. Kill any wrapper grandchildren before waiting on their pipes so
    // a detached helper cannot complete a delayed configuration write after this request returns.
    child.kill_tree().await;
    let stdout = collect_pipe(&mut stdout_task)
        .await
        .ok_or(CommandFailure::Io)?;
    let stderr = collect_pipe(&mut stderr_task)
        .await
        .ok_or(CommandFailure::Io)?;
    Ok(CliOutput {
        status,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

fn iso_now() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let days = millis.div_euclid(86_400_000);
    let day_millis = millis.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    let hour = day_millis / 3_600_000;
    let minute = day_millis % 3_600_000 / 60_000;
    let second = day_millis % 60_000 / 1000;
    let fraction = day_millis % 1000;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{fraction:03}Z")
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

impl HostAgents {
    /// What a registration hands the MCP server an agent starts: this installation's own places, so an agent opened
    /// from any shell reaches this installation's connector. The agents' homes stay the agent's own.
    pub(super) fn mcp_env(&self) -> BTreeMap<String, String> {
        [(data_dir::VARIABLE, &self.profile.data)]
            .into_iter()
            .map(|(key, path)| (key.to_owned(), path.to_string_lossy().into_owned()))
            .collect()
    }

    async fn inspect(
        &self,
        id: AgentId,
        login_path: Option<&str>,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<Observation, Failure> {
        self.profile
            .validate_existing_private()
            .map_err(|_| agent_failure("agents.invalid", id))?;
        let agent = self.registration(id)?;
        let host = self.host(agent, cancel, deadline);
        let binary = self.resolve_binary(id, agent, login_path);
        let entry = agent
            .state(&host, binary.as_deref())
            .await
            .map_err(|error| failure(error, id))?;
        let registration = entry.registration;
        // The manual steps follow the state itself: an `outdated` entry is removed before ours is added.
        let manual = agent.manual(&host, &registration, binary.as_deref());
        // `outdated`: an entry of ours that is not the current one (disabled, an older release, another environment).
        // Connect writes it again; like a current one, it is ours to remove: `owned`.
        let owned = registration == "connected" || registration == "outdated";
        // Discovery never executes a candidate binary. An apparent CLI can itself launch a GUI.
        let version: Option<String> = None;
        let mut evidence = Vec::new();
        if binary.is_some() {
            evidence.push(Evidence { kind: "cli" });
        }
        for kind in markers::matching_kinds(
            agent.install_markers(),
            std::env::consts::OS,
            &Roots::process(&self.profile.home),
        ) {
            evidence.push(Evidence { kind });
        }
        // Configuration directories are not install markers: they can survive an uninstall.
        let present = !evidence.is_empty();
        // The connector acts itself where it reaches the agent: its CLI found, or a file it edits.
        let able = binary.is_some() || agent.connects_without_binary();
        let evidence_json = serde_json::to_value(&evidence).map_err(|_| Failure::Internal)?;
        let signature = if present {
            let identity = format!(
                "{{\"id\":{},\"version\":{},\"evidence\":{},\"binary\":{}}}",
                serde_json::to_string(id).map_err(|_| Failure::Internal)?,
                serde_json::to_string(&version).map_err(|_| Failure::Internal)?,
                serde_json::to_string(&evidence).map_err(|_| Failure::Internal)?,
                serde_json::to_string(&binary).map_err(|_| Failure::Internal)?
            );
            Some(hex::encode(Sha256::digest(identity.as_bytes())))
        } else {
            None
        };
        let row = json!({
            "id":id,
            "label":label(id),
            "version":version,
            "registration":registration,
            "owned":owned,
            "connect":if able {"auto"} else {"manual"},
            "can":{"connect":able,"update":able,"disconnect":able},
            "manual":manual,
            // The older forms of `manual.connect` and `manual.disconnect`, which older clients read: each sequence as one
            // step, so running it does the whole operation.
            "instructions":Manual::as_one(&manual.connect),
            "removal":Manual::as_one(&manual.disconnect),
            "evidence":evidence_json,
            "present":present,
            "dismissed":false,
            "actionable":false,
            "health":Value::Null,
        });
        Ok(Observation {
            row: with_readiness(row, agent.readiness_checks(&self.profile)),
            launch: entry.launch,
            binary,
            present,
            signature,
        })
    }

    fn resolve_binary(
        &self,
        id: AgentId,
        agent: &dyn Registration,
        login_path: Option<&str>,
    ) -> Option<String> {
        #[cfg(test)]
        if let Some(path) = std::env::var(override_name(id))
            .ok()
            .filter(|path| executable(path))
        {
            return Some(path);
        }
        let _ = id;
        find_in_path(agent.binary_names(), login_path)
    }

    async fn capture_login_path(
        &self,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<Option<String>, Failure> {
        let (program, args) = match Environment::login_path() {
            None => return Ok(None),
            Some(LoginPath::Known(path)) => return Ok(Some(path)),
            Some(LoginPath::Run { program, args }) => (program, args),
        };
        let output = run_command(
            &self.profile,
            None,
            &program.to_string_lossy(),
            &args,
            cancel,
            deadline,
        )
        .await;
        match output {
            Ok(output) if output.status.success() => Ok(LoginPath::parse(&output.stdout)),
            Ok(_) | Err(CommandFailure::Start | CommandFailure::Deadline | CommandFailure::Io) => {
                Ok(None)
            }
            Err(CommandFailure::Cancelled) => Err(Failure::Cancelled),
        }
    }
}
