//! Whether a registered MCP server works: started as its agent would start it, it answers the MCP handshake
//! (`initialize`, `notifications/initialized`) and lists its tools. No tool is called; the server is stopped right
//! after, with every process it started.

use super::registration::Launch;
use rmcp::model::ProtocolVersion;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::future::Future;
use std::io::ErrorKind;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::{timeout, Instant};

/// The name the check gives itself in `initialize`.
const CLIENT: &str = "sidevoice-health";
/// The longest line the check reads from the server.
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
    command
        .args(&launch.args)
        .envs(environment.iter().map(|(key, value)| (key, value)))
        .envs(&launch.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        // Its own process group, so a wrapper (a shell, Node's launcher) leaves nothing behind.
        .process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return Ok(Health::NotStarting(spawn_error(&error))),
    };
    let group = child.id();
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        stop(&mut child, group).await;
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
    stop(&mut child, group).await;
    outcome
}

/// `initialize`, `notifications/initialized` and `tools/list`, one JSON-RPC message per line.
async fn handshake(mut stdin: ChildStdin, stdout: ChildStdout) -> Result<(), Stop> {
    let mut reader = BufReader::new(stdout);
    send(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":ProtocolVersion::LATEST_WITH_INITIALIZE.as_str(),
            "capabilities":{},
            "clientInfo":{"name":CLIENT,"version":crate::identity::VERSION},
        }}),
    )
    .await?;
    let initialized = answer(&mut reader, 1).await?;
    let agreed = initialized.get("protocolVersion").and_then(Value::as_str);
    if !ProtocolVersion::KNOWN_VERSIONS
        .iter()
        .any(|version| Some(version.as_str()) == agreed && version.has_initialize())
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
    let listed = answer(&mut reader, 2).await?;
    if listed.get("tools").is_some_and(Value::is_array) {
        Ok(())
    } else {
        Err(Stop::Mismatch)
    }
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
async fn answer(reader: &mut BufReader<ChildStdout>, id: u64) -> Result<Value, Stop> {
    loop {
        let line = match crate::bounded_line(reader, LINE_LIMIT).await {
            Ok(Some(line)) => line,
            Ok(None) => return Err(Stop::Closed),
            Err(_) => return Err(Stop::Mismatch),
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
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exited with status {code}"),
        (None, Some(signal)) => format!("killed by signal {signal}"),
        (None, None) => "exited".into(),
    }
}

/// Stops the server and everything it started (its process group, `group`), and reaps it.
async fn stop(child: &mut Child, group: Option<u32>) {
    if let Some(pid) = group {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
    let _ = timeout(Duration::from_secs(2), child.wait()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "sidevoice-health-test-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        /// An executable `sh` script named `name` with `body`.
        fn script(&self, name: &str, body: &str) -> Launch {
            let path = self.0.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            launch(&path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn launch(path: &Path) -> Launch {
        Launch {
            command: path.to_string_lossy().into_owned(),
            args: vec!["mcp".into()],
            env: BTreeMap::new(),
        }
    }

    /// A server that answers `initialize` (with `version`) and `tools/list`, and logs what it read and its
    /// environment to `$LOG`.
    fn fake_server(version: &str) -> String {
        format!(
            r#"read -r initialize
printf '%s\n' "$initialize" > "$LOG"
printf '%s\n' "args=$*" "marker=$HEALTH_MARKER" "entry=$ENTRY_VALUE" >> "$LOG"
printf '%s\n' '{{"jsonrpc":"2.0","method":"notifications/message","params":{{"level":"info","data":"hi"}}}}'
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"{version}","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"fake","version":"1"}}}}}}'
read -r initialized
printf '%s\n' "$initialized" >> "$LOG"
read -r list
printf '%s\n' "$list" >> "$LOG"
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"tools":[{{"name":"voice_say","inputSchema":{{"type":"object"}}}}]}}}}'
sleep 30"#
        )
    }

    async fn run(launch: &Launch, environment: &[(String, OsString)], limit: Duration) -> Health {
        check(launch, environment, limit, std::future::pending())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_server_that_answers_the_handshake_works_and_is_stopped() {
        let scratch = Scratch::new();
        let log = scratch.0.join("log");
        let mut launch = scratch.script("server", &fake_server("2025-11-25"));
        launch
            .env
            .insert("ENTRY_VALUE".into(), "from the entry".into());
        let environment = vec![
            ("LOG".to_owned(), log.clone().into_os_string()),
            ("HEALTH_MARKER".to_owned(), OsString::from("1")),
            ("ENTRY_VALUE".to_owned(), OsString::from("overridden")),
        ];
        let started = std::time::Instant::now();
        assert_eq!(
            run(&launch, &environment, Duration::from_secs(5)).await,
            Health::Working
        );
        // It does not wait for the server's `sleep`: the server is stopped as soon as it answered.
        assert!(started.elapsed() < Duration::from_secs(4));
        let log = std::fs::read_to_string(log).unwrap();
        let lines = log.lines().collect::<Vec<_>>();
        let initialize: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(initialize["method"], "initialize");
        assert_eq!(initialize["params"]["clientInfo"]["name"], CLIENT);
        assert_eq!(initialize["params"]["protocolVersion"], "2025-11-25");
        assert_eq!(lines[1], "args=mcp");
        assert_eq!(lines[2], "marker=1");
        assert_eq!(lines[3], "entry=from the entry");
        let initialized: Value = serde_json::from_str(lines[4]).unwrap();
        assert_eq!(initialized["method"], "notifications/initialized");
        let list: Value = serde_json::from_str(lines[5]).unwrap();
        assert_eq!(list["method"], "tools/list");
        assert!(!log.contains("tools/call"));
    }

    #[tokio::test]
    async fn a_missing_command_is_not_found() {
        let scratch = Scratch::new();
        assert_eq!(
            run(
                &launch(&scratch.0.join("absent")),
                &[],
                Duration::from_secs(5)
            )
            .await,
            Health::NotStarting("command not found".into())
        );
    }

    #[tokio::test]
    async fn a_server_that_exits_early_says_its_status() {
        let scratch = Scratch::new();
        let launch = scratch.script("server", "echo 'no daemon' >&2\nexit 3");
        assert_eq!(
            run(&launch, &[], Duration::from_secs(5)).await,
            Health::NotStarting("exited with status 3".into())
        );
    }

    #[tokio::test]
    async fn a_silent_server_times_out() {
        let scratch = Scratch::new();
        let launch = scratch.script("server", "sleep 30");
        let started = std::time::Instant::now();
        assert_eq!(
            run(&launch, &[], Duration::from_millis(300)).await,
            Health::NotStarting("timed out".into())
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn garbage_or_an_unknown_version_is_a_protocol_mismatch() {
        let scratch = Scratch::new();
        let garbage = scratch.script("garbage", "read -r line\necho 'Welcome!'\nsleep 30");
        assert_eq!(
            run(&garbage, &[], Duration::from_secs(5)).await,
            Health::NotStarting("protocol mismatch".into())
        );
        let unknown = scratch.script(
            "unknown",
            &fake_server("1999-01-01").replace("\"$LOG\"", "/dev/null"),
        );
        assert_eq!(
            run(&unknown, &[], Duration::from_secs(5)).await,
            Health::NotStarting("protocol mismatch".into())
        );
        let refused = scratch.script(
            "refused",
            r#"read -r line
printf '%s\n' '{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"no"}}'
sleep 30"#,
        );
        assert_eq!(
            run(&refused, &[], Duration::from_secs(5)).await,
            Health::NotStarting("protocol mismatch".into())
        );
    }

    #[tokio::test]
    async fn cancelling_stops_the_check() {
        let scratch = Scratch::new();
        let launch = scratch.script("server", "sleep 30");
        let started = std::time::Instant::now();
        let result = check(
            &launch,
            &[],
            Duration::from_secs(5),
            tokio::time::sleep(Duration::from_millis(100)),
        )
        .await;
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
