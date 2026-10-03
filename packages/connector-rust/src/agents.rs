//! The isolated proof's R2 host-agent coordinator.
//!
//! One gate owns a complete request. CLI children belong to its worker task so a vanished link reply can
//! cancel the work without releasing the gate before the child has exited.

use crate::proof::{atomic_json, private_dir, Profile};
use anyhow::{Context, Result};
use fs2::FileExt;
use serde::Serialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::collections::HashMap;

mod claude;
mod codex;
mod cursor;
#[cfg(test)]
mod tests;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration as StdDuration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify, OwnedMutexGuard};
use tokio::task::JoinHandle;
use tokio::time::{sleep, sleep_until, timeout, Instant};
use uuid::Uuid;

const AGENT_IDS: [&str; 3] = ["claude", "codex", "cursor"];
const REQUEST_LIMIT: StdDuration = StdDuration::from_secs(18);
const CHILD_LIMIT: StdDuration = StdDuration::from_millis(2500);
const OUTPUT_LIMIT: usize = 64 * 1024;
const STATE_LIMIT: u64 = 4 * 1024 * 1024;

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
            self.notify.notify_one();
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentId {
    Claude,
    Codex,
    Cursor,
}

impl AgentId {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "cursor" => Some(Self::Cursor),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
        }
    }

    fn label(self) -> String {
        agent_message(&format!("harness.{}", self.as_str()), &Value::Null)
    }

    fn override_name(self) -> &'static str {
        match self {
            Self::Claude => "SIDEVOICE_CLAUDE_BIN",
            Self::Codex => "SIDEVOICE_CODEX_BIN",
            Self::Cursor => "SIDEVOICE_CURSOR_BIN",
        }
    }

    fn binary_names(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &["claude"],
            Self::Codex => &["codex"],
            Self::Cursor => &["cursor-agent", "cursor"],
        }
    }
}

#[derive(Clone)]
enum Ownership {
    Proof {
        executable: PathBuf,
        root: PathBuf,
        data: PathBuf,
        codex: PathBuf,
    },
    #[cfg(test)]
    Selected {
        record: Vec<String>,
        release_root: PathBuf,
    },
}

/// Immutable command selected for one request. The proof constructor never reads production `current`.
#[derive(Clone)]
struct InstalledCommand {
    command: String,
    args: Vec<String>,
    version: String,
    ownership: Ownership,
}

impl InstalledCommand {
    fn proof(profile: &Profile) -> Result<Self> {
        let executable = std::env::current_exe()?.canonicalize()?;
        Ok(Self {
            command: executable.to_string_lossy().into_owned(),
            args: vec![
                "mcp".into(),
                "--profile-root".into(),
                profile.root.to_string_lossy().into_owned(),
            ],
            version: env!("CARGO_PKG_VERSION").into(),
            ownership: Ownership::Proof {
                executable,
                root: profile.root.clone(),
                data: profile.data.clone(),
                codex: profile.codex.clone(),
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

    fn owns(
        &self,
        agent: AgentId,
        command: &str,
        args: &[String],
        child_env: Option<&Value>,
    ) -> bool {
        match &self.ownership {
            Ownership::Proof {
                executable,
                root,
                data,
                codex,
            } => {
                let same_executable = canonical(command).as_deref() == Some(executable.as_path());
                let selected = same_executable
                    && args
                        == [
                            "mcp".to_owned(),
                            "--profile-root".to_owned(),
                            root.to_string_lossy().into_owned(),
                        ];
                if selected {
                    return true;
                }
                agent == AgentId::Codex
                    && same_executable
                    && args == ["mcp"]
                    && child_env
                        .and_then(|env| env.get("SIDEVOICE_DATA_DIR"))
                        .and_then(Value::as_str)
                        == Some(data.to_string_lossy().as_ref())
                    && child_env
                        .and_then(|env| env.get("CODEX_HOME"))
                        .and_then(Value::as_str)
                        == Some(codex.to_string_lossy().as_ref())
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
    gate: Arc<Mutex<()>>,
    operations: Arc<Mutex<Vec<ActiveOperation>>>,
    stopping: Arc<AtomicBool>,
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

struct CommandOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

#[derive(Serialize, Clone)]
struct Evidence<'a> {
    kind: &'a str,
    path: String,
}

struct Observation {
    row: Value,
    binary: Option<String>,
    present: bool,
    signature: Option<String>,
}

struct InspectionInput<'a> {
    state: &'a Value,
    login_path: Option<&'a str>,
    include_version: bool,
    old_version: Option<&'a str>,
}

impl HostAgents {
    pub fn new(profile: Profile) -> Result<Arc<Self>> {
        let selected = InstalledCommand::proof(&profile)?;
        Ok(Self::with_command(profile, selected))
    }

    fn with_command(profile: Profile, selected: InstalledCommand) -> Arc<Self> {
        Arc::new(Self {
            selected,
            profile,
            gate: Arc::new(Mutex::new(())),
            operations: Arc::new(Mutex::new(Vec::new())),
            stopping: Arc::new(AtomicBool::new(false)),
        })
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

    pub async fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        let operations = {
            let mut active = self.operations.lock().await;
            std::mem::take(&mut *active)
        };
        for operation in &operations {
            operation.cancel.cancel();
        }
        for operation in operations {
            let _ = operation.task.await;
        }
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
                Some(value) => Some(parse_id(value)?),
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
        if !matches!(action, "connect" | "disconnect" | "dismiss") {
            return Err(Failure::keyed(
                "agents.unknown-request",
                json!({"route":method.chars().take(120).collect::<String>()}),
            ));
        }
        let id = params.get("id").and_then(Value::as_str).unwrap_or("");
        let agent = parse_id(id)?;
        let state = self.load_state(cancel, deadline).await?;
        let mut state = self
            .scan(state, Some(agent), true, cancel, deadline)
            .await?;
        let current = seen_agent(&state, agent).ok_or_else(|| not_present(agent))?;
        let registration = current
            .get("registration")
            .and_then(Value::as_str)
            .unwrap_or("unknown");

        if action == "dismiss" {
            let store = self
                .update_state(cancel, deadline, |latest| {
                    if let Some(fresh) = seen_entry(latest, agent) {
                        if fresh.pointer("/agent/registration").and_then(Value::as_str)
                            == Some("not-connected")
                        {
                            if let Some(generation) = fresh.get("generation") {
                                latest["dismissed"][agent.as_str()] = generation.clone();
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
                "not-connected" => {}
                _ => return Err(agent_failure("agents.registration-unknown", agent)),
            }
            if current.get("connect").and_then(Value::as_str) != Some("auto") {
                return Err(agent_failure("agents.manual-required", agent));
            }
            let binary = state
                .pointer(&format!("/binaries/{}", agent.as_str()))
                .and_then(Value::as_str)
                .map(str::to_owned);
            self.connect(agent, binary.as_deref(), cancel, deadline)
                .await?;
            state = self
                .scan(state, Some(agent), false, cancel, deadline)
                .await?;
        match seen_agent(&state, agent)
            .and_then(|row| row.get("registration"))
            .and_then(Value::as_str)
        {
            Some("connected") => {
                return Err(agent_failure("agents.registration-not-confirmed", agent))
            }
            Some("not-connected") => {}
            Some("foreign") => return Err(agent_failure("agents.foreign", agent)),
            Some("invalid") => return Err(agent_failure("agents.invalid", agent)),
            _ => return Err(agent_failure("agents.registration-unknown", agent)),
        }
            let store = self
                .update_state(cancel, deadline, |latest| {
                    if seen_agent(latest, agent)
                        .and_then(|row| row.get("registration"))
                        .and_then(Value::as_str)
                        == Some("connected")
                    {
                        if let Some(dismissed) =
                            latest.get_mut("dismissed").and_then(Value::as_object_mut)
                        {
                            dismissed.remove(agent.as_str());
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
            "connected" => {}
            _ => return Err(agent_failure("agents.registration-unknown", agent)),
        }
        let binary = state
            .pointer(&format!("/binaries/{}", agent.as_str()))
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.disconnect(agent, binary.as_deref(), cancel, deadline)
            .await?;
        state = self
            .scan(state, Some(agent), false, cancel, deadline)
            .await?;
        match seen_agent(&state, agent)
            .and_then(|row| row.get("registration"))
            .and_then(Value::as_str)
        {
            Some("not-connected") => {}
            Some("connected") => return Err(agent_failure("agents.action-failed", agent)),
            Some("foreign") => return Err(agent_failure("agents.foreign", agent)),
            Some("invalid") => return Err(agent_failure("agents.invalid", agent)),
            _ => return Err(agent_failure("agents.registration-unknown", agent)),
        }
        self.response(&state)
    }

    fn response(&self, state: &Value) -> std::result::Result<Value, Failure> {
        let mut rows = Vec::new();
        for id in AGENT_IDS {
            if let Some(seen) = state.pointer(&format!("/seen/{id}")) {
                if seen.get("present") == Some(&Value::Bool(true)) {
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
        let custom = CustomConfig {
            mcp_servers: CustomServers {
                sidevoice: CustomServer {
                    command: &self.selected.command,
                    args: &self.selected.args,
                },
            },
        };
        let snippet = serde_json::to_string_pretty(&custom).map_err(|_| Failure::Internal)?;
        Ok(json!({
            "agents": rows,
            "scanned_at": state.get("scanned_at").cloned().unwrap_or(Value::Null),
            "custom": {
                "command": shell_command(&self.selected.command, &self.selected.args),
                "snippet": snippet,
                "version": self.selected.version,
            }
        }))
    }

    async fn scan(
        &self,
        state: Value,
        watch: Option<AgentId>,
        include_version: bool,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<Value, Failure> {
        let base = state.clone();
        let mut login_path = state
            .get("login_path")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if watch.is_none() || login_path.is_none() {
            if let Some(captured) = self.capture_login_path(cancel, deadline).await? {
                login_path = Some(captured);
            }
        }
        let ids: Vec<_> = watch.map_or_else(|| AGENT_IDS.to_vec(), |id| vec![id.as_str()]);
        let mut observations = Vec::new();
        for name in ids {
            check_live(cancel, deadline)?;
            let agent = AgentId::parse(name).ok_or(Failure::Internal)?;
            let previous_version = base
                .pointer(&format!("/seen/{name}/agent/version"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            observations.push((
                agent,
                self.inspect(
                    agent,
                    InspectionInput {
                        state: &base,
                        login_path: login_path.as_deref(),
                        include_version,
                        old_version: previous_version.as_deref(),
                    },
                    cancel,
                    deadline,
                )
                .await?,
            ));
        }
        self.merge_scan(base, observations, login_path, cancel, deadline)
            .await
    }

    async fn merge_scan(
        &self,
        base: Value,
        observations: Vec<(AgentId, Observation)>,
        captured_path: Option<String>,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<Value, Failure> {
        self.update_state(cancel, deadline, move |latest| {
            let path_unchanged = latest.get("login_path") == base.get("login_path");
            if path_unchanged {
                if let Some(path) = captured_path {
                    latest["login_path"] = json!(path);
                }
            }
            for (id, observation) in observations {
                if !path_unchanged
                    || latest.pointer(&format!("/binaries/{}", id.as_str()))
                        != base.pointer(&format!("/binaries/{}", id.as_str()))
                {
                    continue;
                }
                if let Some(binary) = &observation.binary {
                    latest["binaries"][id.as_str()] = json!(binary);
                } else if let Some(binaries) =
                    latest.get_mut("binaries").and_then(Value::as_object_mut)
                {
                    binaries.remove(id.as_str());
                }
                let signature = observation.signature;
                let previous = latest.pointer(&format!("/seen/{}", id.as_str())).cloned();
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
                latest["seen"][id.as_str()] = json!({
                    "present":observation.present,
                    "signature":signature,
                    "generation":generation,
                    "detected_at":detected_at,
                    "agent":observation.row,
                });
            }
            latest["scanned_at"] = json!(iso_now());
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
        update: F,
    ) -> std::result::Result<Value, Failure>
    where
        F: FnOnce(&mut Value),
    {
        check_live(cancel, deadline)?;
        let _lock = self.state_lock(cancel, deadline).await?;
        check_live(cancel, deadline)?;
        let mut state = load_state_file(&self.profile)?;
        update(&mut state);
        check_live(cancel, deadline)?;
        let path = self.profile.data.join("agents.json");
        if path.exists() {
            private_state_file(&path)?;
        }
        atomic_json(&path, &state).map_err(|_| Failure::Internal)?;
        Ok(state)
    }

    async fn state_lock(
        &self,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<File, Failure> {
        let path = self.profile.data.join("agents.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|_| Failure::Internal)?;
        private_state_file(&path)?;
        loop {
            check_live(cancel, deadline)?;
            match lock.try_lock_exclusive() {
                Ok(()) => return Ok(lock),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
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

    async fn connect(
        &self,
        id: AgentId,
        binary: Option<&str>,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<(), Failure> {
        check_live(cancel, deadline)?;
        match id {
            AgentId::Claude => self.claude_connect(binary, cancel, deadline).await,
            AgentId::Codex => self.codex_connect(binary, cancel, deadline).await,
            AgentId::Cursor => {
                check_live(cancel, deadline)?;
                self.cursor_connect()
            }
        }
    }

    async fn disconnect(
        &self,
        id: AgentId,
        binary: Option<&str>,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<(), Failure> {
        check_live(cancel, deadline)?;
        match id {
            AgentId::Claude => self.claude_disconnect(binary, cancel, deadline).await,
            AgentId::Codex => self.codex_disconnect(binary, cancel, deadline).await,
            AgentId::Cursor => {
                check_live(cancel, deadline)?;
                self.cursor_disconnect()
            }
        }
    }

    async fn run_cli_required(
        &self,
        binary: &str,
        args: &[impl AsRef<str>],
        cancel: &Cancellation,
        deadline: Instant,
        agent: AgentId,
    ) -> std::result::Result<(), Failure> {
        let args = args
            .iter()
            .map(|value| value.as_ref().to_owned())
            .collect::<Vec<_>>();
        let output = match run_command(&self.profile, binary, &args, cancel, deadline).await {
            Ok(output) => output,
            Err(CommandFailure::Cancelled) => return Err(Failure::Cancelled),
            Err(_) => return Err(agent_failure("agents.action-failed", agent)),
        };
        check_live(cancel, deadline)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(agent_failure("agents.action-failed", agent))
        }
    }
}

#[derive(Serialize)]
struct CustomConfig<'a> {
    #[serde(rename = "mcpServers")]
    mcp_servers: CustomServers<'a>,
}

#[derive(Serialize)]
struct CustomServers<'a> {
    sidevoice: CustomServer<'a>,
}

#[derive(Serialize)]
struct CustomServer<'a> {
    command: &'a str,
    args: &'a [String],
}

fn parse_id(value: &str) -> std::result::Result<AgentId, Failure> {
    AgentId::parse(value).ok_or_else(|| {
        Failure::keyed(
            "agents.unknown",
            json!({"id":value.chars().take(120).collect::<String>()}),
        )
    })
}

fn not_present(id: AgentId) -> Failure {
    Failure::keyed(
        "agents.not-present",
        json!({"id":id.as_str(),"agent":id.label()}),
    )
}

fn agent_failure(key: &'static str, id: AgentId) -> Failure {
    Failure::keyed(key, json!({"id":id.as_str(),"agent":id.label()}))
}

fn agent_message(key: &str, params: &Value) -> String {
    static MESSAGES: OnceLock<HashMap<String, String>> = OnceLock::new();
    let messages = MESSAGES.get_or_init(|| {
        serde_json::from_str(include_str!("../../connector/messages/agent-errors.json"))
            .expect("embedded agent message bundle must be valid JSON")
    });
    let Some(template) = messages.get(key) else {
        return key.to_owned();
    };
    let mut rendered = String::new();
    let mut remaining = template.as_str();
    loop {
        let Some(open) = remaining.find('{') else {
            rendered.push_str(remaining);
            break;
        };
        rendered.push_str(&remaining[..open]);
        let tail = &remaining[open + 1..];
        let Some(close) = tail.find('}') else {
            rendered.push_str(&remaining[open..]);
            break;
        };
        let name = &tail[..close];
        if let Some(value) = params.get(name) {
            rendered.push_str(
                value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string())
                    .as_str(),
            );
        } else {
            rendered.push_str(&remaining[open..open + close + 2]);
        }
        remaining = &tail[close + 1..];
    }
    rendered
}

fn seen_entry(state: &Value, id: AgentId) -> Option<&Value> {
    state.pointer(&format!("/seen/{}", id.as_str()))
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
    let metadata = fs::symlink_metadata(path).map_err(|_| Failure::Internal)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(Failure::Internal);
    }
    Ok(())
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
    let message = agent_message(key, &params);
    json!({"error":{"key":key,"params":params,"message":message}})
}

fn shell_command(command: &str, args: &[String]) -> String {
    std::iter::once(command)
        .chain(args.iter().map(String::as_str))
        .map(|value| format!("'{}'", value.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn looks_absent(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "no such server",
        "no such mcp server",
        "no mcp server",
        "no such mcp entry",
        "not found",
        "does not exist",
        "no entry",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
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

fn executable(value: &str) -> bool {
    let path = Path::new(value);
    if !path.is_absolute() {
        return false;
    }
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && (metadata.mode() & 0o111 != 0))
}

fn find_in_path(names: &[&str], path: Option<&str>) -> Option<String> {
    for directory in path?.split(':').filter(|value| !value.is_empty()) {
        for name in names {
            let candidate = Path::new(directory).join(name);
            let value = candidate.to_string_lossy().into_owned();
            if executable(&value) {
                return Some(value);
            }
        }
    }
    None
}

async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut reader: R) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(output);
        }
        let keep = OUTPUT_LIMIT.saturating_sub(output.len()).min(count);
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
    if let Some(pid) = child.id() {
        // Each probe gets its own process group so a shell wrapper cannot leave a delayed writer behind.
        kill_process_group(pid);
    }
    let _ = child.start_kill();
    if timeout(StdDuration::from_secs(2), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}

fn kill_process_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

async fn run_command(
    profile: &Profile,
    binary: &str,
    args: &[String],
    cancel: &Cancellation,
    deadline: Instant,
) -> std::result::Result<CommandOutput, CommandFailure> {
    if cancel.is_cancelled() {
        return Err(CommandFailure::Cancelled);
    }
    profile
        .validate_private()
        .map_err(|_| CommandFailure::Start)?;
    let mut command = Command::new(binary);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    profile.command_env(&mut command);
    // Recheck immediately before passing these profile roots to a host CLI. The binary may have
    // been selected during an earlier scan, before an agent config directory was replaced.
    profile
        .validate_private()
        .map_err(|_| CommandFailure::Start)?;
    let mut child = command.spawn().map_err(|_| CommandFailure::Start)?;
    let process_group = child.id().ok_or(CommandFailure::Io)?;
    let stdout = child.stdout.take().ok_or(CommandFailure::Io)?;
    let stderr = child.stderr.take().ok_or(CommandFailure::Io)?;
    let mut stdout_task = tokio::spawn(read_capped(stdout));
    let mut stderr_task = tokio::spawn(read_capped(stderr));
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
    kill_process_group(process_group);
    let stdout = collect_pipe(&mut stdout_task)
        .await
        .ok_or(CommandFailure::Io)?;
    let stderr = collect_pipe(&mut stderr_task)
        .await
        .ok_or(CommandFailure::Io)?;
    Ok(CommandOutput {
        status,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

fn read_profile_file(path: &Path, root: &Path, limit: u64) -> Result<String> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
        || metadata.len() > limit
        || !path.canonicalize()?.starts_with(root)
    {
        anyhow::bail!("unsafe profile file");
    }
    fs::read_to_string(path).context("read profile file")
}

fn read_cursor_config(file: &Path, root: &Path) -> Result<Option<(Value, u32, PathBuf)>> {
    let link_metadata = match fs::symlink_metadata(file) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let target = if link_metadata.file_type().is_symlink() {
        file.canonicalize()?
    } else {
        file.to_path_buf()
    };
    let metadata = fs::symlink_metadata(&target)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
        || metadata.len() > (1 << 20)
        || !target.canonicalize()?.starts_with(root)
    {
        anyhow::bail!("unsafe Cursor profile file");
    }
    let text = fs::read_to_string(&target)?;
    let config: Value = serde_json::from_str(&text)?;
    if !config.is_object() {
        anyhow::bail!("Cursor configuration is not an object");
    }
    Ok(Some((
        config,
        metadata.permissions().mode() & 0o777,
        target,
    )))
}

fn write_cursor_config(profile: &Profile, file: &Path, config: &Value) -> Result<()> {
    profile.validate_private()?;
    let root = &profile.root;
    let (target, mode) = match read_cursor_config(file, root)? {
        Some((_, mode, target)) => (target, mode),
        None => (file.to_path_buf(), 0o600),
    };
    let parent = target.parent().context("Cursor config parent")?;
    private_dir(parent)?;
    if !parent.canonicalize()?.starts_with(root) {
        anyhow::bail!("Cursor config escaped proof root");
    }
    let temporary = parent.join(format!(".mcp.json.{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        profile.validate_private()?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        serde_json::to_writer_pretty(&mut output, config)?;
        use std::io::Write;
        output.write_all(b"\n")?;
        output.sync_all()?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))?;
        profile.validate_private()?;
        fs::rename(&temporary, &target)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn toml_to_json(value: &toml::Value) -> Option<Value> {
    match value {
        toml::Value::String(value) => Some(json!(value)),
        toml::Value::Integer(value) => Some(json!(value)),
        toml::Value::Float(value) => Some(json!(value)),
        toml::Value::Boolean(value) => Some(json!(value)),
        toml::Value::Datetime(value) => Some(json!(value.to_string())),
        toml::Value::Array(values) => values
            .iter()
            .map(toml_to_json)
            .collect::<Option<Vec<_>>>()
            .map(Value::Array),
        toml::Value::Table(values) => values
            .iter()
            .map(|(key, value)| toml_to_json(value).map(|value| (key.clone(), value)))
            .collect::<Option<Map<_, _>>>()
            .map(Value::Object),
    }
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
    async fn inspect(
        &self,
        id: AgentId,
        input: InspectionInput<'_>,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<Observation, Failure> {
        self.profile
            .validate_private()
            .map_err(|_| agent_failure("agents.invalid", id))?;
        let binary = self.resolve_binary(id, input.state, input.login_path);
        let registration = match id {
            AgentId::Claude => {
                self.claude_registration(binary.as_deref(), cancel, deadline)
                    .await?
            }
            AgentId::Codex => {
                self.codex_registration(binary.as_deref(), cancel, deadline)
                    .await?
            }
            AgentId::Cursor => self
                .cursor_registration()
                .unwrap_or_else(|_| "invalid".into()),
        };
        let version = if input.include_version {
            match binary.as_deref() {
                Some(binary) => self.binary_version(binary, cancel, deadline).await?,
                None => None,
            }
        } else {
            input.old_version.map(str::to_owned)
        };
        let mut evidence = Vec::new();
        let config = match id {
            AgentId::Claude => &self.profile.claude,
            AgentId::Codex => &self.profile.codex,
            AgentId::Cursor => &self.profile.cursor,
        };
        if config.exists() {
            evidence.push(Evidence {
                kind: "config-dir",
                path: config.to_string_lossy().into_owned(),
            });
        }
        if let Some(binary) = &binary {
            evidence.push(Evidence {
                kind: "binary",
                path: binary.clone(),
            });
        }
        if id == AgentId::Cursor
            && Path::new("/Applications/Cursor.app").exists()
            && !evidence
                .iter()
                .any(|item| item.path == "/Applications/Cursor.app")
        {
            evidence.push(Evidence {
                kind: "app",
                path: "/Applications/Cursor.app".into(),
            });
        }
        let present = !evidence.is_empty();
        let instructions = self.instructions(id, &registration, binary.as_deref());
        let evidence_json = serde_json::to_value(&evidence).map_err(|_| Failure::Internal)?;
        let signature = if present {
            let identity = format!(
                "{{\"id\":{},\"version\":{},\"evidence\":{}}}",
                serde_json::to_string(id.as_str()).map_err(|_| Failure::Internal)?,
                serde_json::to_string(&version).map_err(|_| Failure::Internal)?,
                serde_json::to_string(&evidence).map_err(|_| Failure::Internal)?
            );
            Some(hex::encode(Sha256::digest(identity.as_bytes())))
        } else {
            None
        };
        let row = json!({
            "id":id.as_str(),
            "label":id.label(),
            "version":version,
            "registration":registration,
            "connect":if binary.is_some() || id == AgentId::Cursor {"auto"} else {"manual"},
            "instructions":instructions,
            "evidence":evidence_json,
            "present":present,
            "dismissed":false,
            "actionable":false,
        });
        Ok(Observation {
            row,
            binary,
            present,
            signature,
        })
    }

    fn instructions(&self, id: AgentId, registration: &str, binary: Option<&str>) -> Value {
        let selected = &self.selected;
        match id {
            AgentId::Claude => {
                let mut args = vec!["mcp", "add", "--scope", "user", "sidevoice", "--"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                args.push(selected.command.clone());
                args.extend(selected.args.clone());
                json!({
                    "command":shell_command(binary.unwrap_or("claude"), &args),
                    "file":Value::Null,
                    "snippet":shell_command(binary.unwrap_or("claude"), &args),
                })
            }
            AgentId::Codex => {
                if registration == "foreign" {
                    let cli = binary.unwrap_or("codex");
                    let remove = vec!["mcp", "remove", "sidevoice"]
                        .into_iter()
                        .map(str::to_owned)
                        .collect::<Vec<_>>();
                    let mut add = vec!["mcp", "add", "sidevoice", "--"]
                        .into_iter()
                        .map(str::to_owned)
                        .collect::<Vec<_>>();
                    add.push(selected.command.clone());
                    add.extend(selected.args.clone());
                    let replace_message = agent_message(
                        "agents.manual.codex.replace-existing",
                        &json!({
                            "remove":shell_command(cli, &remove),
                            "add":shell_command(cli, &add),
                        }),
                    );
                    return json!({
                        "command":replace_message,
                        "file":Value::Null,
                        "snippet":Value::Null,
                    });
                }
                let command = shell_command(&selected.command, &selected.args);
                let snippet = format!(
                    "[mcp_servers.sidevoice]\ncommand = {}\nargs = {}",
                    serde_json::to_string(&selected.command).unwrap_or_else(|_| "\"\"".into()),
                    serde_json::to_string(&selected.args).unwrap_or_else(|_| "[]".into())
                );
                json!({
                    "command":command,
                    "file":self.profile.codex.join("config.toml").to_string_lossy(),
                    "snippet":snippet,
                })
            }
            AgentId::Cursor => {
                let config = CustomConfig {
                    mcp_servers: CustomServers {
                        sidevoice: CustomServer {
                            command: &selected.command,
                            args: &selected.args,
                        },
                    },
                };
                json!({
                    "command":shell_command(&selected.command,&selected.args),
                    "file":self.profile.cursor.join("mcp.json").to_string_lossy(),
                    "snippet":serde_json::to_string_pretty(&config).unwrap_or_default(),
                })
            }
        }
    }

    fn resolve_binary(
        &self,
        id: AgentId,
        state: &Value,
        login_path: Option<&str>,
    ) -> Option<String> {
        let override_path = std::env::var(id.override_name()).ok();
        if let Some(path) = override_path.as_deref().filter(|path| executable(path)) {
            return Some(path.to_owned());
        }
        if let Some(path) = state
            .pointer(&format!("/binaries/{}", id.as_str()))
            .and_then(Value::as_str)
            .filter(|path| executable(path))
        {
            return Some(path.to_owned());
        }
        if let Some(path) = find_in_path(id.binary_names(), login_path) {
            return Some(path);
        }
        self.known_paths(id)
            .into_iter()
            .find(|path| executable(path))
    }

    fn known_paths(&self, id: AgentId) -> Vec<String> {
        let local = self.profile.home.join("AppData/Local");
        let bin = self.profile.home.join(".local/bin");
        let mut paths = match id {
            AgentId::Claude => vec![
                self.profile.home.join(".claude/local/claude"),
                bin.join("claude"),
                PathBuf::from("/usr/local/bin/claude"),
                PathBuf::from("/opt/homebrew/bin/claude"),
            ],
            AgentId::Codex => vec![
                bin.join("codex"),
                PathBuf::from("/usr/local/bin/codex"),
                PathBuf::from("/opt/homebrew/bin/codex"),
                local.join("Programs/Codex/codex.exe"),
            ],
            AgentId::Cursor => vec![
                bin.join("cursor-agent"),
                bin.join("cursor"),
                PathBuf::from("/usr/local/bin/cursor-agent"),
                PathBuf::from("/opt/homebrew/bin/cursor-agent"),
                PathBuf::from("/usr/local/bin/cursor"),
                PathBuf::from("/opt/homebrew/bin/cursor"),
                PathBuf::from("/Applications/Cursor.app/Contents/Resources/app/bin/cursor"),
                local.join("Programs/cursor/resources/app/bin/cursor.cmd"),
            ],
        };
        paths
            .drain(..)
            .map(|path| path.to_string_lossy().into_owned())
            .collect()
    }

    async fn capture_login_path(
        &self,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<Option<String>, Failure> {
        let Some(shell) = std::env::var("SHELL")
            .ok()
            .filter(|value| Path::new(value).is_absolute())
        else {
            return Ok(None);
        };
        let output = run_command(
            &self.profile,
            &shell,
            &["-lc".into(), "printf %s \"$PATH\"".into()],
            cancel,
            deadline,
        )
        .await;
        match output {
            Ok(output) if output.status.success() => {
                let value = output.stdout.trim();
                Ok((!value.is_empty()).then(|| value.to_owned()))
            }
            Ok(_) | Err(CommandFailure::Start | CommandFailure::Deadline | CommandFailure::Io) => {
                Ok(None)
            }
            Err(CommandFailure::Cancelled) => Err(Failure::Cancelled),
        }
    }

    async fn binary_version(
        &self,
        binary: &str,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<Option<String>, Failure> {
        match run_command(
            &self.profile,
            binary,
            &["--version".into()],
            cancel,
            deadline,
        )
        .await
        {
            Ok(output) if output.status.success() => Ok(output
                .stdout
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(|line| line.chars().take(200).collect())),
            Ok(_) | Err(CommandFailure::Start | CommandFailure::Deadline | CommandFailure::Io) => {
                Ok(None)
            }
            Err(CommandFailure::Cancelled) => Err(Failure::Cancelled),
        }
    }
}
