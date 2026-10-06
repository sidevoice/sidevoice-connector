//! The test bench (`sidevoice-bench`, src/bench/) against the real pinned core: it installs this build with the core
//! into a private profile (no service manager), opens a call; a conversation joins through the installation's MCP
//! server (with an HTTP receiver as its harness); what is typed on the bench is delivered to it; its `voice_say`
//! reply and the input's receipt show on the bench.

mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::sync::mpsc::channel;
use std::time::Duration;

use serde_json::{json, Value};
use support::package::pinned_core;
use support::*;

const BENCH: &str = env!("CARGO_BIN_EXE_sidevoice-bench");

/// One request to the bench's page server: status and JSON body.
fn bench_http(address: &str, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let body = body.map(Value::to_string).unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (head, payload) = response.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(payload).unwrap_or(Value::Null))
}

/// Uninstalls from the profile (stopping its connector and core) whatever happens, before the profile is removed.
struct Uninstall<'a>(&'a Profile);

impl Drop for Uninstall<'_> {
    fn drop(&mut self) {
        let _ = Command::new(BENCH)
            .arg("--profile")
            .arg(&self.0.root)
            .arg("--uninstall")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .output();
    }
}

#[test]
fn what_is_typed_on_the_bench_reaches_a_conversation_and_its_reply_shows() {
    if pinned_core().is_none() {
        return;
    }
    let profile = Profile::new("bench");
    // The test's profile has the bench's layout; its bench/ directory marks it as one the bench may use.
    private_dir(&profile.root.join("bench"));
    let _uninstall = Uninstall(&profile);
    let mut child = Command::new(BENCH)
        .arg("--profile")
        .arg(&profile.root)
        .args(["--port", "0"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start the bench");
    let stdout = child.stdout.take().unwrap();
    let _bench = Process(child);
    let (tx, lines) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let address = loop {
        let line = lines
            .recv_timeout(Duration::from_secs(180))
            .expect("the bench never said it was ready");
        if let Some(url) = line.strip_prefix("Bench ready: http://") {
            break url.trim_end_matches('/').to_owned();
        }
    };

    // The bench's call is open on the core the installation started.
    until("the bench's call", 60, || {
        let (_, state) = bench_http(&address, "GET", "/api/state", None);
        (state["call"] == true).then_some(())
    });

    // A conversation joins through the installation's own MCP server, as an agent registered with it would.
    let install: Value =
        serde_json::from_slice(&std::fs::read(profile.data().join("install.json")).unwrap())
            .unwrap();
    let command: Vec<&str> = install["command"]
        .as_array()
        .unwrap()
        .iter()
        .map(|part| part.as_str().unwrap())
        .collect();
    let receiver = http_receiver();
    let thread = "bench-thread";
    let mut mcp_command = profile.command(command[0]);
    mcp_command
        .args(&command[1..])
        .arg("mcp")
        .env("SIDEVOICE_SERVICE_MANAGER", "none")
        .env("SIDEVOICE_CORE_PORT", "0")
        .env("SIDEVOICE_STUN_URLS", "")
        .env("SIDEVOICE_THREAD", thread)
        .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
    let mut mcp = Mcp::start(mcp_command);
    mcp.initialize("bench-test");
    let joined = mcp.tool("voice_connect", json!({"title": "Bench test"}));
    assert_eq!(joined["conversation"], thread, "{joined}");

    // The bench lists it.
    until("the conversation on the bench", 30, || {
        let (_, state) = bench_http(&address, "GET", "/api/state", None);
        state["bindings"]
            .as_array()?
            .iter()
            .find(|binding| binding["thread"] == thread && binding["connected"] == true)
            .map(|_| ())
    });

    // Typed on the bench, delivered to the conversation as voice input.
    let (status, sent) = bench_http(
        &address,
        "POST",
        "/api/say",
        Some(&json!({"thread": thread, "text": "Hello from the bench"})),
    );
    assert_eq!(status, 200, "{sent}");
    let delivered = receiver
        .bodies
        .recv_timeout(Duration::from_secs(30))
        .expect("the conversation received nothing");
    assert_eq!(delivered["thread_id"], thread, "{delivered}");
    assert_eq!(delivered["message_id"], sent["message_id"], "{delivered}");
    assert!(
        delivered["text"]
            .as_str()
            .is_some_and(|text| text.contains("Hello from the bench")),
        "{delivered}"
    );

    // The conversation answers; the bench shows the reply and the input's receipt.
    let said = mcp.tool(
        "voice_say",
        json!({"session_id": delivered["session_id"], "revision": delivered["revision"],
               "text": "A reply for the bench"}),
    );
    assert_eq!(said["text_saved"], true, "{said}");
    let history = until("the reply on the bench", 30, || {
        let (_, history) = bench_http(
            &address,
            "GET",
            &format!("/api/history?thread={thread}"),
            None,
        );
        let rows = history["messages"].as_array()?.clone();
        rows.iter()
            .any(|row| row["role"] != "user" && row["text"] == "A reply for the bench")
            .then_some(rows)
    });
    let input = history
        .iter()
        .find(|row| row["role"] == "user")
        .expect("the input in the history");
    assert!(
        ["delivered", "unconfirmed", "read"].contains(&input["status"].as_str().unwrap_or("")),
        "{input}"
    );
    let (_, state) = bench_http(&address, "GET", "/api/state", None);
    assert!(
        state["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["event"]["type"] == "voice-input-receipt"),
        "{state}"
    );
    mcp.stop();
}
