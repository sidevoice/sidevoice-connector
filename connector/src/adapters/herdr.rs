//! Any coding agent running in a herdr pane (<https://github.com/herdrdev/herdr>), reached through herdr itself.
//!
//! herdr is a terminal multiplexer for coding agents. It recognises the agent in each pane (about two dozen kinds),
//! follows whether it is working, idle or blocked, and can submit a prompt to it. Every process in a pane inherits
//! `HERDR_ENV=1`, `HERDR_PANE_ID`, `HERDR_BIN_PATH` and `HERDR_SOCKET_PATH`, so the `sidevoice mcp` an agent starts
//! there knows where it runs, and what it asks herdr about its own pane is what herdr detected.
//!
//! The route is `delivery.kind = "herdr"`, served by the registry's fallback harness (a conversation whose harness id
//! no native adapter claims), so every agent herdr detects works without an adapter of its own:
//!
//! - **Deliver**: `herdr agent prompt <pane> <envelope>` (a bracketed paste and Enter). herdr refuses a blocked agent
//!   with `agent_blocked` and sends nothing; that is a `failed` delivery, and the message stays for `get_messages`.
//! - **Working**: a loop of `herdr agent wait`, which herdr serves from its own events: no polling while nothing changes.
//! - **Blocked**: `status` shows the pane's `agent_status`, so `voice_status` says the agent waits for the person.
//!
//! The program run is always the delivery's `bin` (the `HERDR_BIN_PATH` the pane gave), against the delivery's
//! `socket`; never a `herdr` found on `PATH`. The socket is checked to be served by this user before anything is sent.

use super::conversation::{report_working, Conversation, Reporter};
use super::Identity;
use crate::platform::foreign::{Foreign, Foreigners};
use crate::platform::process::{ChildProcess, Processes};
use anyhow::Result;
use serde_json::{json, Map, Value};
use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// `delivery.kind` of this route.
pub const KIND: &str = "herdr";

/// How long a `herdr agent prompt` may take before its outcome is unknown.
const PROMPT_LIMIT: Duration = Duration::from_secs(15);
/// How long a status read (`agent get`) may take.
const GET_LIMIT: Duration = Duration::from_secs(5);
/// How long one `agent wait` blocks before the watcher looks at whether the conversation stopped.
const WAIT_MS: u64 = 20_000;
/// What the watcher waits before asking again after herdr failed to answer (never under 500 ms).
const RETRY: Duration = Duration::from_secs(2);
/// The most output of herdr that is kept.
const OUTPUT_LIMIT: usize = 256 * 1024;

pub fn capabilities() -> Value {
    json!({"deliver":"supported","working":"supported","endOfTurn":"unsupported","inspectInbound":"unsupported","sessionIdentity":"supported"})
}

pub const EXPERIMENTAL_NOTE: &str = "Voice reaches this conversation by an experimental route: herdr types it into this agent's terminal and sends Enter. If the user is typing at the same moment, the two texts mix. Tell the user once. While the agent waits for the person's approval herdr refuses the message: it stays for get_messages.";

/// Where a pane's herdr is, from the environment every process in the pane inherits.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Pane {
    bin: String,
    socket: String,
    pane: String,
}

impl Pane {
    fn from_env() -> Option<Self> {
        let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        (var("HERDR_ENV")? == "1").then(|| {
            Self::new(
                var("HERDR_BIN_PATH")?,
                var("HERDR_SOCKET_PATH")?,
                var("HERDR_PANE_ID")?,
            )
        })?
    }

    fn new(bin: String, socket: String, pane: String) -> Option<Self> {
        Some(Self { bin, socket, pane })
    }

    /// The route a delivery names, if it has every field.
    fn from_delivery(delivery: &Value) -> Option<Self> {
        let field = |name: &str| {
            delivery
                .get(name)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        Self::new(field("bin")?, field("socket")?, field("pane")?)
    }
}

/// What herdr says about the agent in a pane (`agent get`, `.result.agent`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Agent {
    kind: String,
    status: String,
    terminal: Option<String>,
    session: Option<String>,
}

impl Agent {
    fn parse(response: &Value) -> Option<Self> {
        let agent = response.pointer("/result/agent")?;
        let text = |value: Option<&Value>| {
            value
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        };
        Some(Self {
            kind: text(agent.get("agent"))?,
            status: text(agent.get("agent_status")).unwrap_or_else(|| "unknown".into()),
            terminal: text(agent.get("terminal_id")),
            session: text(agent.pointer("/agent_session/value")),
        })
    }

    /// The stable id of the conversation. The agent's own session reference, when herdr has one, names the
    /// conversation itself: a new conversation in the same pane (`/new`, a resume) is a different thread, and the
    /// same one moved to another pane stays the same. Without it the terminal is the best herdr gives: it survives a
    /// pane move (a pane id does not: it is workspace-qualified) and ends with the process in it. Neither is ever
    /// the pane id.
    fn thread(&self) -> Option<String> {
        self.session.clone().or_else(|| self.terminal.clone())
    }

    /// Whether `delivery` was made for this very agent: the pane may since hold another agent or conversation.
    fn is_the_one_of(&self, delivery: &Value) -> bool {
        let named = |name: &str| delivery.get(name).and_then(Value::as_str);
        named("agent") == Some(self.kind.as_str())
            && named("terminal").is_none_or(|terminal| self.terminal.as_deref() == Some(terminal))
            && match (named("session"), self.session.as_deref()) {
                (Some(then), Some(now)) => then == now,
                _ => true,
            }
    }

    /// Whether the agent is working: `None` when herdr cannot tell (`unknown`, or a state this code does not know).
    fn working(&self) -> Option<bool> {
        working_of(&self.status)
    }
}

/// What a herdr agent status says about working. `idle` and `done` both mean ready for input; `blocked` means the
/// agent waits for the person, so it is not working.
fn working_of(status: &str) -> Option<bool> {
    match status {
        "working" => Some(true),
        "idle" | "done" | "blocked" => Some(false),
        _ => None,
    }
}

/// The identity of the conversation in this herdr pane, if herdr detected an agent in it.
pub fn identity() -> Result<Option<Identity>> {
    let Some(pane) = Pane::from_env() else {
        return Ok(None);
    };
    let Some(response) = blocking_json(&pane, &["agent", "get", &pane.pane]) else {
        return Ok(None);
    };
    Ok(identity_for(&pane, &response))
}

fn identity_for(pane: &Pane, response: &Value) -> Option<Identity> {
    let agent = Agent::parse(response)?;
    // An agent with a native adapter is reached by that adapter: reaching it here would join it as no route at all.
    if super::Registry::standard().find(&agent.kind).is_some() {
        return None;
    }
    let thread = agent.thread()?;
    let mut delivery = json!({"kind":KIND,"bin":pane.bin,"socket":pane.socket,"pane":pane.pane,
        "thread":thread,"agent":agent.kind});
    if let Some(terminal) = &agent.terminal {
        delivery["terminal"] = json!(terminal);
    }
    if let Some(session) = &agent.session {
        delivery["session"] = json!(session);
    }
    let mut identity = Identity::new(&agent.kind, thread, delivery);
    identity.capabilities = capabilities();
    Some(identity)
}

/// Whether `delivery` is the herdr route of conversation `thread`.
pub fn accepts(delivery: &Value, thread: &str) -> super::Route {
    super::Route::known(
        Pane::from_delivery(delivery).is_some()
            && delivery.get("thread").and_then(Value::as_str) == Some(thread),
    )
}

/// What running herdr came to.
enum Run {
    /// It ended: its exit code (`None` when a signal ended it), and what it printed.
    Done {
        code: Option<i32>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    /// It did not end within its limit.
    TimedOut,
    /// It could not be started.
    NotStarted(String),
}

/// A herdr command, run as `bin` against `socket`, for the process to be started by the caller.
fn command(pane: &Pane, args: &[&str]) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(&pane.bin);
    command
        .args(args)
        .env("HERDR_SOCKET_PATH", &pane.socket)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Checks that the socket is served by this user, and runs herdr within `limit`.
async fn run(pane: &Pane, args: &[&str], limit: Duration) -> Run {
    match tokio::time::timeout(Duration::from_secs(3), Foreigners::connect(&pane.socket)).await {
        Ok(Ok(stream)) => drop(stream),
        Ok(Err(error)) => return Run::NotStarted(format!("herdr's socket is not usable: {error}")),
        Err(_) => return Run::NotStarted("herdr's socket did not answer".into()),
    }
    if !Path::new(&pane.bin).is_absolute() {
        return Run::NotStarted("the herdr program is not an absolute path".into());
    }
    let child = match Processes::spawn_tree(command(pane, args)).await {
        Ok(child) => child,
        Err(error) => return Run::NotStarted(format!("could not start herdr: {error}")),
    };
    match child
        .output(limit, Duration::from_millis(500), OUTPUT_LIMIT)
        .await
    {
        Ok(output) => Run::Done {
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        },
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => Run::TimedOut,
        Err(error) => Run::NotStarted(format!("herdr could not be run: {error}")),
    }
}

/// What herdr printed as JSON: its answer on stdout, its error on stderr.
fn json_of(stdout: &[u8], stderr: &[u8]) -> Option<Value> {
    [stdout, stderr].into_iter().find_map(|bytes| {
        let text = String::from_utf8_lossy(bytes);
        serde_json::from_str::<Value>(text.trim()).ok()
    })
}

/// The error code of a herdr answer (`{"error":{"code":…}}`).
fn error_code(response: &Value) -> Option<&str> {
    response.pointer("/error/code").and_then(Value::as_str)
}

/// Errors that say herdr did not write anything to the terminal.
const NOT_SENT: [&str; 4] = [
    "agent_blocked",
    "agent_not_found",
    "agent_not_ready",
    "server_not_running",
];

/// How a `herdr agent prompt` ended, as the delivery result the daemon reads. Only what proves nothing was sent is
/// `failed`; a timeout, or an error that does not say so, may have sent it, so the message is `unknown` and never
/// sent again blindly.
fn prompt_result(run: Run) -> Value {
    match run {
        Run::NotStarted(detail) => json!({"status":"failed","detail":detail}),
        Run::TimedOut => json!({"status":"unknown",
            "detail":"herdr did not answer in time; the message may or may not have been typed into the agent"}),
        Run::Done {
            code,
            stdout,
            stderr,
        } => {
            let response = json_of(&stdout, &stderr);
            if code == Some(0) {
                return json!({"status":"accepted","detail":"herdr submitted it to the agent"});
            }
            match response.as_ref().and_then(error_code) {
                Some("agent_blocked") => json!({"status":"failed",
                    "detail":"the agent is waiting for the person's approval or an answer; nothing was sent, and the message stays for get_messages"}),
                Some(code) if NOT_SENT.contains(&code) => json!({"status":"failed",
                    "detail":format!("herdr could not reach the agent ({code}); nothing was sent")}),
                _ if code == Some(2) => {
                    json!({"status":"failed","detail":"herdr refused the command; nothing was sent"})
                }
                Some(code) => json!({"status":"unknown",
                    "detail":format!("herdr answered {code}; the message may or may not have been typed into the agent")}),
                None => json!({"status":"unknown",
                    "detail":"herdr ended without a clear answer; the message may or may not have been typed into the agent"}),
            }
        }
    }
}

/// The agent in a pane, as herdr says now.
async fn agent_of(pane: &Pane) -> Option<Agent> {
    match run(pane, &["agent", "get", &pane.pane], GET_LIMIT).await {
        Run::Done {
            code: Some(0),
            stdout,
            ..
        } => Agent::parse(&json_of(&stdout, &[])?),
        _ => None,
    }
}

/// Types the message into the agent of the delivery's pane.
pub async fn deliver(delivery: &Value, event: &Value) -> Result<Value> {
    let Some(pane) = Pane::from_delivery(delivery) else {
        anyhow::bail!("herdr route incomplete");
    };
    let envelope = super::envelope(event)?;
    // The pane may hold something else by now (another agent, a new conversation, a shell): not ours to type into.
    match agent_of(&pane).await {
        Some(agent) if agent.is_the_one_of(delivery) => {}
        Some(_) => {
            return Ok(json!({"status":"failed",
                "detail":"the pane now holds a different agent or conversation; nothing was sent"}))
        }
        None => {
            return Ok(json!({"status":"failed",
                "detail":"herdr found no agent in the pane; nothing was sent"}))
        }
    }
    Ok(prompt_result(
        run(
            &pane,
            &["agent", "prompt", &pane.pane, &envelope],
            PROMPT_LIMIT,
        )
        .await,
    ))
}

/// What `status` lists beside the conversation: what herdr says the agent is doing, `blocked` when it waits for
/// the person.
pub async fn status(conversation: &Conversation) -> Option<Map<String, Value>> {
    let pane = Pane::from_delivery(&conversation.delivery)?;
    let agent = agent_of(&pane).await?;
    let mut fields = Map::new();
    fields.insert("agent_status".into(), json!(agent.status));
    Some(fields)
}

/// Follows the pane's agent until the conversation stops, reporting working: `agent wait` returns when the state
/// changes between working and not, so nothing is polled while it holds.
pub fn watch(conversation: Arc<Conversation>, reporter: Arc<dyn Reporter>) {
    let Some(pane) = Pane::from_delivery(&conversation.delivery) else {
        return;
    };
    tokio::spawn(async move {
        let mut reported = None;
        let mut agent = None;
        while !conversation.stopped.load(Ordering::Relaxed) {
            let current = match agent.take() {
                Some(agent) => Some(agent),
                None => agent_of(&pane).await,
            };
            let Some(current) = current else {
                tokio::time::sleep(RETRY).await;
                continue;
            };
            if let Some(working) = current.working() {
                if reported != Some(working) {
                    report_working(&*reporter, &conversation, working).await;
                    reported = Some(working);
                }
            }
            let until = until(current.working());
            let mut args = vec!["agent", "wait", pane.pane.as_str()];
            for state in &until {
                args.extend(["--until", state]);
            }
            let timeout = WAIT_MS.to_string();
            args.extend(["--timeout", &timeout]);
            let limit = Duration::from_millis(WAIT_MS) + Duration::from_secs(10);
            match run(&pane, &args, limit).await {
                Run::Done {
                    code: Some(0),
                    stdout,
                    ..
                } => agent = json_of(&stdout, &[]).and_then(|answer| Agent::parse(&answer)),
                Run::Done { stdout, stderr, .. } => {
                    // A wait that ran out its time is the normal way round; anything else is herdr being
                    // unavailable or the agent gone: ask again later.
                    let timed_out = json_of(&stdout, &stderr)
                        .as_ref()
                        .and_then(error_code)
                        .is_some_and(|code| code == "timeout");
                    if !timed_out {
                        tokio::time::sleep(RETRY).await;
                    }
                }
                Run::TimedOut | Run::NotStarted(_) => tokio::time::sleep(RETRY).await,
            }
        }
    });
}

/// The states whose arrival is worth waking for, given whether the agent is working now: the other side of the
/// working line (`idle`, `done` and `blocked` are all "not working"), and `unknown`, which stops what is reported.
fn until(working: Option<bool>) -> Vec<&'static str> {
    match working {
        Some(true) => vec!["idle", "done", "blocked", "unknown"],
        Some(false) => vec!["working", "unknown"],
        None => vec!["working", "idle", "done", "blocked"],
    }
}

/// Runs herdr to completion for the identity, which is made synchronously, within [`GET_LIMIT`]; its JSON answer.
/// The same socket and program rules as [`run`], except for the connection check, which needs the runtime.
fn blocking_json(pane: &Pane, args: &[&str]) -> Option<Value> {
    if !Path::new(&pane.bin).is_absolute() || Foreigners::check(pane.socket.as_ref()).is_err() {
        return None;
    }
    let mut child = std::process::Command::new(&pane.bin)
        .args(args)
        .env("HERDR_SOCKET_PATH", &pane.socket)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = (&mut stdout)
            .take(OUTPUT_LIMIT as u64)
            .read_to_end(&mut output);
        let _ = sender.send(output);
    });
    let Ok(output) = receiver.recv_timeout(GET_LIMIT) else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let status = child.wait().ok()?;
    status.success().then(|| json_of(&output, &[])).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::stand_in::{StandIn, StandIns as _};
    use crate::platform::target;
    use crate::test_support::stand_in::Script;

    fn pane() -> Pane {
        Pane {
            bin: "/usr/bin/herdr".into(),
            socket: "/run/user/1000/herdr/herdr.sock".into(),
            pane: "w1:p2".into(),
        }
    }

    fn agent_json(kind: &str, status: &str, session: Option<&str>) -> Value {
        let mut agent =
            json!({"agent":kind,"agent_status":status,"pane_id":"w1:p2","terminal_id":"term_abc"});
        if let Some(session) = session {
            agent["agent_session"] =
                json!({"source":"herdr:gemini","agent":kind,"kind":"id","value":session});
        }
        json!({"id":"cli:agent:get","result":{"agent":agent,"type":"agent_info"}})
    }

    #[test]
    fn an_agent_herdr_detected_names_its_conversation() {
        let identity =
            identity_for(&pane(), &agent_json("gemini", "idle", Some("sess-1"))).unwrap();
        assert_eq!(identity.harness, "gemini");
        assert_eq!(identity.thread, "sess-1", "the native session names it");
        assert_eq!(identity.delivery["kind"], "herdr");
        assert_eq!(identity.delivery["bin"], "/usr/bin/herdr");
        assert_eq!(
            identity.delivery["socket"],
            "/run/user/1000/herdr/herdr.sock"
        );
        assert_eq!(identity.delivery["pane"], "w1:p2");
        assert_eq!(identity.capabilities["deliver"], "supported");
        assert_eq!(identity.capabilities["working"], "supported");
        assert_eq!(identity.capabilities["endOfTurn"], "unsupported");
        assert_eq!(identity.capabilities["inspectInbound"], "unsupported");
        assert_eq!(identity.capabilities["sessionIdentity"], "supported");
        assert_eq!(
            accepts(&identity.delivery, &identity.thread),
            crate::adapters::Route::Accepted
        );
        assert!(
            crate::adapters::experimental(&identity, &identity.capabilities)
                .contains(&"deliver".to_owned())
        );
    }

    #[test]
    fn without_a_native_session_the_terminal_names_it_never_the_pane() {
        let identity = identity_for(&pane(), &agent_json("qwen", "working", None)).unwrap();
        assert_eq!(identity.thread, "term_abc");
        assert!(identity.delivery.get("session").is_none());
    }

    #[test]
    fn a_pane_without_a_detected_agent_is_not_claimed() {
        let none = json!({"id":"x","error":{"code":"agent_not_found","message":"no"}});
        assert!(identity_for(&pane(), &none).is_none());
        let nameless = json!({"result":{"agent":{"agent_status":"idle","terminal_id":"t"}}});
        assert!(identity_for(&pane(), &nameless).is_none());
        let no_id = json!({"result":{"agent":{"agent":"gemini","agent_status":"idle"}}});
        assert!(identity_for(&pane(), &no_id).is_none());
    }

    #[test]
    fn an_agent_with_a_native_adapter_is_not_claimed_here() {
        for kind in ["claude", "codex", "cursor"] {
            assert!(identity_for(&pane(), &agent_json(kind, "idle", Some("s"))).is_none());
        }
    }

    #[test]
    fn the_route_needs_every_field_and_its_own_thread() {
        let delivery = json!({"kind":"herdr","bin":"/b","socket":"/s","pane":"w1:p1","thread":"t"});
        // Fields a newer façade adds are ignored.
        let mut extra = delivery.clone();
        extra["future"] = json!({"nested":true});
        assert_eq!(accepts(&extra, "t"), crate::adapters::Route::Accepted);
        assert_eq!(accepts(&delivery, "t"), crate::adapters::Route::Accepted);
        assert_eq!(accepts(&delivery, "other"), crate::adapters::Route::Refused);
        for field in ["bin", "socket", "pane", "thread"] {
            let mut missing = delivery.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert_eq!(
                accepts(&missing, "t"),
                crate::adapters::Route::Refused,
                "{field}"
            );
        }
        let mut empty = delivery.clone();
        empty["pane"] = json!("");
        assert_eq!(accepts(&empty, "t"), crate::adapters::Route::Refused);
    }

    #[test]
    fn a_pane_that_changed_hands_is_not_the_one() {
        let identity =
            identity_for(&pane(), &agent_json("gemini", "idle", Some("sess-1"))).unwrap();
        let same = Agent::parse(&agent_json("gemini", "working", Some("sess-1"))).unwrap();
        assert!(same.is_the_one_of(&identity.delivery));
        let new_conversation = Agent::parse(&agent_json("gemini", "idle", Some("sess-2"))).unwrap();
        assert!(!new_conversation.is_the_one_of(&identity.delivery));
        let other_agent = Agent::parse(&agent_json("qwen", "idle", Some("sess-1"))).unwrap();
        assert!(!other_agent.is_the_one_of(&identity.delivery));
        // A session reported after the join does not make it another conversation.
        let without = identity_for(&pane(), &agent_json("gemini", "idle", None)).unwrap();
        assert!(same.is_the_one_of(&without.delivery));
    }

    #[test]
    fn statuses_map_to_working_and_unknown_to_nothing() {
        assert_eq!(working_of("working"), Some(true));
        for idle in ["idle", "done", "blocked"] {
            assert_eq!(working_of(idle), Some(false), "{idle}");
        }
        assert_eq!(working_of("unknown"), None);
        assert_eq!(working_of("some-future-state"), None);
        assert_eq!(until(Some(true)), ["idle", "done", "blocked", "unknown"]);
        assert_eq!(until(Some(false)), ["working", "unknown"]);
    }

    fn done(code: i32, stdout: &str, stderr: &str) -> Run {
        Run::Done {
            code: Some(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn a_prompt_result_says_only_what_is_known() {
        let ok = r#"{"id":"cli:agent:prompt","result":{"type":"agent_prompted"}}"#;
        assert_eq!(prompt_result(done(0, ok, ""))["status"], "accepted");
        let blocked = r#"{"error":{"code":"agent_blocked","message":"blocked"},"id":"x"}"#;
        let result = prompt_result(done(1, "", blocked));
        assert_eq!(result["status"], "failed");
        assert!(result["detail"].as_str().unwrap().contains("approval"));
        let not_ready = r#"{"error":{"code":"agent_not_ready","message":"n"}}"#;
        assert_eq!(prompt_result(done(1, "", not_ready))["status"], "failed");
        // Anything else may have been typed: never `failed`.
        let stalled = r#"{"error":{"code":"agent_prompt_stalled","message":"s"}}"#;
        assert_eq!(prompt_result(done(1, "", stalled))["status"], "unknown");
        assert_eq!(prompt_result(done(1, "", "not json"))["status"], "unknown");
        assert_eq!(prompt_result(Run::TimedOut)["status"], "unknown");
        assert_eq!(
            prompt_result(Run::Done {
                code: None,
                stdout: vec![],
                stderr: vec![]
            })["status"],
            "unknown"
        );
        assert_eq!(prompt_result(done(2, "", ""))["status"], "failed");
        assert_eq!(
            prompt_result(Run::NotStarted("x".into()))["status"],
            "failed"
        );
    }

    // A Unix socket of a test's own: the stand-in for herdr's API socket.
    #[cfg(unix)]
    #[allow(clippy::disallowed_types)]
    async fn listening(dir: &Path) -> (String, tokio::net::UnixListener) {
        let path = dir.join("herdr.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        (path.to_string_lossy().into_owned(), listener)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_delivery_runs_its_own_program_against_its_own_socket() {
        let dir = crate::test_support::temp_dir("herdr-deliver");
        let (socket, _listener) = listening(dir.path()).await;
        let record = dir.path().join("args");
        let program = dir.path().join(target::exe_name("herdr"));
        // The fake answers `agent get` and `agent prompt` alike; it records the last call.
        StandIn::make(
            &program,
            &json!({"play":"script","record":record,"print":agent_json("gemini","idle",Some("sess-1")).to_string()})
                .to_string(),
        );
        let delivery = json!({"kind":"herdr","bin":program,"socket":socket,"pane":"w1:p2",
            "thread":"sess-1","agent":"gemini","terminal":"term_abc","session":"sess-1"});
        let event =
            json!({"session_id":"s","revision":1,"epoch":1,"message_id":"m1","text":"hello there"});
        let result = deliver(&delivery, &event).await.unwrap();
        assert_eq!(result["status"], "accepted", "{result}");
        let lines = std::fs::read_to_string(&record).unwrap();
        let lines: Vec<&str> = lines.lines().collect();
        assert_eq!(&lines[..3], ["agent", "prompt", "w1:p2"]);
        assert!(lines.join("\n").contains("hello there"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_blocked_agent_is_a_failure_that_sends_nothing() {
        let dir = crate::test_support::temp_dir("herdr-blocked");
        let (socket, _listener) = listening(dir.path()).await;
        let program = dir.path().join("herdr");
        // `agent get` shows the right agent; its prompt is refused. One fake cannot do both by arguments, so the
        // stand-in answers a blocked agent that exits 1 with the refusal on stdout: `agent get` then fails and the
        // route reports that it found no agent. The refusal itself is covered by `a_prompt_result_says_only_what_is_known`.
        StandIn::make(
            &program,
            &json!({"play":"script","exit":1,"print":r#"{"error":{"code":"agent_blocked","message":"b"}}"#})
                .to_string(),
        );
        let delivery = json!({"kind":"herdr","bin":program,"socket":socket,"pane":"w1:p2",
            "thread":"t","agent":"gemini"});
        let event = json!({"session_id":"s","revision":1,"epoch":1,"message_id":"m1","text":"hi"});
        let result = deliver(&delivery, &event).await.unwrap();
        assert_eq!(result["status"], "failed");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_socket_nobody_serves_is_refused_before_anything_runs() {
        let dir = crate::test_support::temp_dir("herdr-nosocket");
        let program = dir.path().join("herdr");
        let marker = dir.path().join("ran");
        Script {
            write: Some((marker.clone(), "ran".into())),
            ..Script::default()
        }
        .make(&program);
        let delivery = json!({"kind":"herdr","bin":program,
            "socket":dir.path().join("missing.sock"),"pane":"w1:p2","thread":"t","agent":"gemini"});
        let event = json!({"session_id":"s","revision":1,"epoch":1,"message_id":"m1","text":"hi"});
        let result = deliver(&delivery, &event).await.unwrap();
        assert_eq!(result["status"], "failed");
        assert!(!marker.exists(), "the program was run");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_program_that_outlasts_its_limit_is_an_unknown_outcome() {
        let dir = crate::test_support::temp_dir("herdr-slow");
        let (socket, _listener) = listening(dir.path()).await;
        let program = dir.path().join("herdr");
        Script {
            sleep_ms: 60_000,
            ..Script::default()
        }
        .make(&program);
        let pane = Pane {
            bin: program.to_string_lossy().into_owned(),
            socket,
            pane: "w1:p2".into(),
        };
        let started = std::time::Instant::now();
        let outcome = run(
            &pane,
            &["agent", "prompt", "w1:p2", "x"],
            Duration::from_millis(1200),
        )
        .await;
        assert!(matches!(outcome, Run::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(20));
        assert_eq!(prompt_result(outcome)["status"], "unknown");
    }

    #[cfg(unix)]
    #[test]
    fn the_identity_asks_herdr_about_its_own_pane() {
        let dir = crate::test_support::temp_dir("herdr-identity");
        let program = dir.path().join("herdr");
        StandIn::make(
            &program,
            &json!({"play":"script","print":agent_json("gemini","idle",Some("sess-9")).to_string()})
                .to_string(),
        );
        let pane = Pane {
            bin: program.to_string_lossy().into_owned(),
            socket: dir.path().join("herdr.sock").to_string_lossy().into_owned(),
            pane: "w1:p2".into(),
        };
        let response = blocking_json(&pane, &["agent", "get", "w1:p2"]).unwrap();
        assert_eq!(identity_for(&pane, &response).unwrap().thread, "sess-9");
        let missing = Pane {
            bin: "herdr".into(),
            ..pane
        };
        assert!(
            blocking_json(&missing, &["agent", "get", "w1:p2"]).is_none(),
            "never from PATH"
        );
    }

    /// Against a real herdr server: `HERDR_REAL_BIN`, `HERDR_REAL_SOCKET` and `HERDR_REAL_PANE` name a pane whose
    /// agent is idle (`HERDR_REAL_EXPECT=failed` when it is blocked). Run with `--ignored`; it needs a herdr server, so CI never does.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "needs a running herdr server"]
    async fn a_real_herdr_takes_the_message_and_refuses_a_blocked_agent() {
        let var = |name: &str| std::env::var(name).unwrap();
        let pane = Pane {
            bin: var("HERDR_REAL_BIN"),
            socket: var("HERDR_REAL_SOCKET"),
            pane: var("HERDR_REAL_PANE"),
        };
        let Run::Done { stdout, .. } = run(&pane, &["agent", "get", &pane.pane], GET_LIMIT).await
        else {
            panic!("herdr did not answer");
        };
        let response = json_of(&stdout, &[]).unwrap();
        let agent = Agent::parse(&response).expect("an agent in the pane");
        let identity = identity_for(&pane, &response).unwrap();
        let event = json!({"session_id":"s","revision":1,"epoch":1,"message_id":"m-real","text":"hello from sidevoice"});
        let result = deliver(&identity.delivery, &event).await.unwrap();
        let expected = std::env::var("HERDR_REAL_EXPECT").unwrap_or_else(|_| "accepted".into());
        assert_eq!(result["status"], expected.as_str(), "{result}");
        println!("{agent:?} -> {result}");
    }
}
