//! Whether a registered MCP server works: started as its agent would start it, it answers the MCP handshake
//! (`initialize`, `notifications/initialized`) and lists its tools. No tool is called; the server is stopped right
//! after, with every process it started. Tested in `tests/health.rs`, against fake servers that are that test's own
//! binary.

use super::registration::Launch;
use crate::platform::process::{Child, Process, Processes};
use futures_util::StreamExt;
use rmcp::model::{InitializeResult, ListToolsResult, ProtocolVersion};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::future::Future;
use std::io::ErrorKind;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::time::{timeout, Instant};
use tokio_util::codec::{FramedRead, LinesCodec};

/// The name the check gives itself in `initialize`.
const CLIENT: &str = "sidevoice-health";
/// The longest line the check reads from the server, its newline included.
const LINE_LIMIT: usize = 1 << 20;

/// What the check found.
#[derive(Debug, PartialEq, Eq)]
pub enum Health {
    /// The server started and answered `initialize` and `tools/list`.
    Working,
    /// The server did not start or did not answer: why, in a few English words (`command not found`, `exited with
    /// status 1`, `timed out`, `protocol mismatch`).
    NotStarting(String),
}

/// The check was cancelled before it found anything.
#[derive(Debug)]
pub struct Cancelled;

/// Why the handshake stopped.
enum Stop {
    /// The server closed its output (it exited, most likely).
    Closed,
    /// It answered something that is not this protocol.
    Mismatch,
}

/// Starts `launch` with `environment` and then the entry's own environment over it, as an agent does, and runs the
/// handshake within `limit`. `cancelled` resolving stops it.
pub async fn check(
    launch: &Launch,
    environment: &[(String, OsString)],
    limit: Duration,
    cancelled: impl Future<Output = ()>,
) -> Result<Health, Cancelled> {
    let deadline = Instant::now() + limit;
    let mut command = Command::new(&launch.command);
    if let Some(directory) = &launch.cwd {
        // Told apart from a missing command, which is what spawning in a missing directory would say.
        if !directory.is_dir() {
            return Ok(Health::NotStarting("working directory not found".into()));
        }
        command.current_dir(directory);
    }
    command
        .args(&launch.args)
        .envs(environment.iter().map(|(key, value)| (key, value)))
        .envs(&launch.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Its own tree, so a wrapper (a shell, Node's launcher) leaves nothing behind.
    let mut child = match Processes::spawn_tree(command).await {
        Ok(child) => child,
        Err(error) => return Ok(Health::NotStarting(spawn_error(&error))),
    };
    let (Some(stdin), Some(stdout)) = (child.stdin(), child.stdout()) else {
        child.kill_tree().await;
        return Ok(Health::NotStarting("could not start".into()));
    };
    let outcome = tokio::select! {
        _ = cancelled => Err(Cancelled),
        answered = tokio::time::timeout_at(deadline, handshake(stdin, stdout)) => Ok(match answered {
            Err(_) => Health::NotStarting("timed out".into()),
            Ok(Ok(())) => Health::Working,
            Ok(Err(Stop::Mismatch)) => Health::NotStarting("protocol mismatch".into()),
            Ok(Err(Stop::Closed)) => Health::NotStarting(closed(&mut child, deadline).await),
        }),
    };
    child.kill_tree().await;
    outcome
}

/// `initialize`, `notifications/initialized` and `tools/list`, one JSON-RPC message per line.
async fn handshake(mut stdin: ChildStdin, stdout: ChildStdout) -> Result<(), Stop> {
    let mut reader = FramedRead::new(stdout, LinesCodec::new_with_max_length(LINE_LIMIT - 1));
    send(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":ProtocolVersion::LATEST_WITH_INITIALIZE.as_str(),
            "capabilities":{},
            "clientInfo":{"name":CLIENT,"version":crate::identity::VERSION},
        }}),
    )
    .await?;
    // The result as the protocol defines it (`protocolVersion`, `capabilities`, `serverInfo` with its name and
    // version), in a version that has this handshake.
    let initialized: InitializeResult =
        serde_json::from_value(answer(&mut reader, 1).await?).map_err(|_| Stop::Mismatch)?;
    if !ProtocolVersion::KNOWN_VERSIONS
        .iter()
        .any(|version| *version == initialized.protocol_version && version.has_initialize())
    {
        return Err(Stop::Mismatch);
    }
    send(
        &mut stdin,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await?;
    send(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    )
    .await?;
    // Every tool as the protocol defines it: an object with its name and input schema.
    serde_json::from_value::<ListToolsResult>(answer(&mut reader, 2).await?)
        .map(|_| ())
        .map_err(|_| Stop::Mismatch)
}

async fn send(stdin: &mut ChildStdin, message: Value) -> Result<(), Stop> {
    let mut line = message.to_string();
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|_| Stop::Closed)?;
    stdin.flush().await.map_err(|_| Stop::Closed)
}

/// The result of request `id`. What the server sends of its own (notifications, requests) is passed over.
async fn answer(reader: &mut FramedRead<ChildStdout, LinesCodec>, id: u64) -> Result<Value, Stop> {
    loop {
        let line = match reader.next().await {
            Some(Ok(line)) => line,
            None => return Err(Stop::Closed),
            Some(Err(_)) => return Err(Stop::Mismatch),
        };
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = serde_json::from_str(&line).map_err(|_| Stop::Mismatch)?;
        if message.get("jsonrpc") != Some(&json!("2.0")) {
            return Err(Stop::Mismatch);
        }
        if message.get("method").is_some() {
            continue;
        }
        if message.get("id") != Some(&json!(id)) {
            return Err(Stop::Mismatch);
        }
        return match message.get("result") {
            Some(result) if result.is_object() => Ok(result.clone()),
            _ => Err(Stop::Mismatch),
        };
    }
}

fn spawn_error(error: &std::io::Error) -> String {
    match error.kind() {
        ErrorKind::NotFound => "command not found",
        ErrorKind::PermissionDenied => "permission denied",
        _ => "could not start",
    }
    .into()
}

/// The server closed its output: how it ended, when it did before the deadline.
async fn closed(child: &mut Child, deadline: Instant) -> String {
    match timeout(
        deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_secs(1)),
        child.wait(),
    )
    .await
    {
        Ok(Ok(status)) => exited(status),
        _ => "closed its output".into(),
    }
}

fn exited(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exited with status {code}"),
        // No exit code: ended by something else (a signal, on Unix), as std words it.
        None => format!("ended ({status})"),
    }
}
