//! A round trip against the real Claude Code CLI, through the connector's real delivery path (issue #286, step 1).
//!
//! A real core and the real connector daemon run in a private profile; a real `claude` session, with the connector's
//! MCP server (`sidevoice-connector mcp`) as its only MCP server, joins voice with `voice_connect`. The test then
//! speaks into the call as the person would (`/api/presentation/text`): the core hands the message to the connector,
//! which injects it through the session's messaging socket (`CLAUDE_CODE_MESSAGING_SOCKET`). The message asks for a
//! random token through `voice_say`. The test asserts, on the core's side and the connector's:
//!
//! - the token comes back as the agent's `voice_say` reply saved in the core (never the wording around it);
//! - the core marks the delivered message `read`: the connector confirmed the injection;
//! - the connector reports the session working, then not working, for that turn;
//! - the connector reports the model, and it is the one Claude Code ran.
//!
//! Claude Code runs headless: `claude -p --input-format stream-json --output-format stream-json`. That mode keeps a
//! messaging socket for the life of the process and takes an injected message as a new turn, so no terminal is needed.
//!
//! It uses a real account and spends a few cents of quota, so it never runs in CI or in a plain `cargo test`: it is
//! `#[ignore]`d and runs only when asked for by name, before a release or a change to the harness adapters.
//!
//! ```text
//! cargo xtask core        # once: the pinned core this test starts (target/sidevoice-core)
//! cargo test -p sidevoice-connector --test claude_roundtrip -- --ignored --nocapture
//! ```
//!
//! Settings (environment):
//! - `SIDEVOICE_TEST_CLAUDE_BIN`: the `claude` program (default: `claude` on `PATH`, else `~/.local/bin/claude`).
//! - `SIDEVOICE_TEST_CLAUDE_CONFIG_DIR`: a Claude Code configuration directory logged in to the test account
//!   (default `$CLAUDE_CONFIG_DIR`, else `~/.claude`). Only its `.credentials.json` is copied into the test's private
//!   profile; the real configuration, `~/.claude.json` and its MCP servers are never touched. Where Claude Code keeps
//!   its login in the OS keychain (macOS) there is no such file: point this at a directory holding one.
//! - `SIDEVOICE_TEST_CLAUDE_MODEL`: the model to run (default `haiku`, the cheapest).
mod support;

use serde_json::{json, Value};
use sidevoice_connector::platform::env::Env as _;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};
use support::*;

fn home() -> PathBuf {
    PathBuf::from(
        std::env::var_os(sidevoice_connector::platform::env::Environment::HOME)
            .expect("a home directory"),
    )
}

fn claude_program() -> PathBuf {
    if let Some(program) = std::env::var_os("SIDEVOICE_TEST_CLAUDE_BIN") {
        return program.into();
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join("claude"))
        .chain([home().join(".local/bin/claude")])
        .find(|candidate| candidate.is_file())
        .expect("no `claude` on PATH: set SIDEVOICE_TEST_CLAUDE_BIN")
}

fn logged_in_config() -> PathBuf {
    let dir = std::env::var_os("SIDEVOICE_TEST_CLAUDE_CONFIG_DIR")
        .or_else(|| std::env::var_os("CLAUDE_CONFIG_DIR"))
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".claude"));
    let credentials = dir.join(".credentials.json");
    assert!(
        credentials.is_file(),
        "no login at {}: log in with `claude auth login` or set SIDEVOICE_TEST_CLAUDE_CONFIG_DIR",
        credentials.display()
    );
    credentials
}

/// What the headless session says on stdout (stream-json, one object per line), read on a thread.
struct Session {
    lines: Receiver<Value>,
    /// Every model an assistant message of this session named.
    models: Vec<String>,
    id: Option<String>,
    stdin: std::process::ChildStdin,
    _process: Process,
}

impl Session {
    fn start(profile: &Profile, model: &str) -> Self {
        let credentials = logged_in_config();
        let config = profile.root.join("claude");
        std::fs::copy(credentials, config.join(".credentials.json")).expect("copy the login");
        std::fs::write(
            config.join(".claude.json"),
            r#"{"hasCompletedOnboarding":true,"theme":"dark"}"#,
        )
        .unwrap();

        // The MCP server as the connector registers it, with the profile's environment (no PATH: Claude Code adds
        // its own); Claude Code passes its session id and messaging socket on to the server it starts.
        let mut env = profile.env();
        env.remove("PATH");
        let mcp_config = profile.root.join("mcp.json");
        std::fs::write(
            &mcp_config,
            json!({"mcpServers": {"sidevoice": {
                "type": "stdio", "command": CONNECTOR, "args": ["mcp"], "env": env}}})
            .to_string(),
        )
        .unwrap();

        let mut child = profile
            .command(claude_program())
            .args([
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--strict-mcp-config",
                "--allowedTools",
                "mcp__sidevoice",
                "--model",
                model,
                "--mcp-config",
            ])
            .arg(&mcp_config)
            .current_dir(&profile.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start claude");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    let _ = tx.send(value);
                }
            }
        });
        Self {
            lines,
            models: Vec::new(),
            id: None,
            stdin,
            _process: Process(child),
        }
    }

    fn say(&mut self, text: &str) {
        let message = json!({"type": "user", "message": {"role": "user", "content": text}});
        writeln!(self.stdin, "{message}").unwrap();
    }

    /// Reads until the turn ends (a `result`), noting the session id and the models on the way.
    fn finish_turn(&mut self, seconds: u64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            let value = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("claude's turn did not end");
            if value["type"] == "system" && value["subtype"] == "init" {
                self.id = value["session_id"].as_str().map(str::to_owned);
            }
            if let Some(model) = value.pointer("/message/model").and_then(Value::as_str) {
                self.models.push(model.to_owned());
            }
            if value["type"] == "result" {
                assert_ne!(value["is_error"], true, "claude failed: {value}");
                return value;
            }
        }
    }
}

fn binding(profile: &Profile, thread: &str) -> Option<Value> {
    let status = connector_ipc(&profile.connector_endpoint(), "status")?;
    status["bindings"]
        .as_array()?
        .iter()
        .find(|binding| binding["thread"] == thread)
        .cloned()
}

fn history(profile: &Profile, call: &Presentation, thread: &str) -> Vec<Value> {
    let page = core_json(
        &profile.core_endpoint(),
        "GET",
        &format!("/api/presentation/history?thread_id={thread}"),
        None,
        Some(&call.token),
    );
    page["messages"].as_array().cloned().unwrap_or_default()
}

#[test]
#[ignore = "uses a real Claude account: run by name, see the header"]
fn a_message_through_the_core_reaches_a_real_claude_session_and_its_reply_comes_back() {
    let core = core_dir().expect("the pinned core: run `cargo xtask core`");
    let profile = Profile::new("claudert");
    let _core = profile.start_core(&core, "claude-rt");
    let _connector = profile.start_connector();
    profile.wait_linked("claude-rt");
    let call = Presentation::open(&profile.core_endpoint());
    let model = std::env::var("SIDEVOICE_TEST_CLAUDE_MODEL").unwrap_or_else(|_| "haiku".into());

    // The session joins voice by itself, as an agent does when the person asks.
    let mut session = Session::start(&profile, &model);
    session.say(
        "Call the voice_connect tool, then reply with only the word ready. Do not call voice_say.",
    );
    session.finish_turn(120);
    let thread = session.id.clone().expect("claude's session id");
    let joined = binding(&profile, &thread).expect("the connector has no binding for the session");
    assert_eq!(joined["harness"], "claude", "{joined}");
    assert_eq!(joined["delivery"]["kind"], "claude-uds", "{joined}");
    // Settled: the turn above is over, so what follows is the working state of the delivered message alone.
    until("the connector reports the session idle", 30, || {
        (binding(&profile, &thread)?["working"] == false).then_some(())
    });

    // The person speaks in the call.
    let token = format!("RT-{}", &uuid::Uuid::new_v4().simple().to_string()[..10]).to_uppercase();
    let endpoint = profile.core_endpoint();
    let selected = core_json(
        &endpoint,
        "POST",
        "/api/presentation/select",
        Some(&json!({"session_id": call.session, "thread_id": thread})),
        Some(&call.token),
    );
    let message_id = uuid::Uuid::new_v4().to_string();
    let accepted = core_json(
        &endpoint,
        "POST",
        "/api/presentation/text",
        Some(&json!({
            "text": format!("Reply with the token {token} using voice_say."),
            "session_id": call.session, "thread_id": thread,
            "binding_id": selected["binding"]["binding_id"], "message_id": message_id})),
        Some(&call.token),
    );
    assert_eq!(accepted["accepted"], true, "{accepted}");

    // Working goes true, the token comes back through voice_say, and working goes false again.
    let mut working = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    let (mut said, mut turn_over) = (None, false);
    while Instant::now() < deadline && !(said.is_some() && turn_over) {
        if let Some(now) = binding(&profile, &thread).and_then(|b| b["working"].as_bool()) {
            if working.last() != Some(&now) {
                working.push(now);
            }
        }
        said = said.or_else(|| {
            history(&profile, &call, &thread).into_iter().find(|row| {
                row["role"] == "assistant"
                    && row["text"].as_str().is_some_and(|t| t.contains(&token))
            })
        });
        turn_over = said.is_some() && working.ends_with(&[true, false]);
        std::thread::sleep(Duration::from_millis(100));
    }
    let said =
        said.unwrap_or_else(|| panic!("the token {token} never came back; working: {working:?}"));
    assert_eq!(said["thread"], thread.as_str(), "{said}");
    assert!(working.ends_with(&[true, false]), "working was {working:?}");

    // The core confirmed the delivery: the connector read the message into the session.
    let delivered = history(&profile, &call, &thread)
        .into_iter()
        .find(|row| {
            row["role"] == "user" && row["text"].as_str().is_some_and(|t| t.contains(&token))
        })
        .expect("the delivered message is in the core's history");
    assert_eq!(delivered["status"], "read", "{delivered}");

    // The model the connector reports is the one Claude Code ran.
    let engine = binding(&profile, &thread).unwrap()["engine"].clone();
    let reported = engine["model"].as_str().unwrap_or_default();
    assert!(reported.starts_with("claude-"), "{engine}");
    session.finish_turn(60);
    assert!(
        session.models.iter().any(|ran| ran == reported),
        "connector says {reported}, claude ran {:?}",
        session.models
    );
}
