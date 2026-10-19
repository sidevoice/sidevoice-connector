//! `pair <room-url> <code> [--json]` against a room on loopback, as the desktop app and a person run it: the code
//! and this machine's identity go to the room's `POST /api/connectors/pair`, the credential it answers is written
//! private, and every failure says a stable key. Plain http goes nowhere but loopback (and the hosts named as
//! trusted); a refused, unreachable or empty answer writes nothing.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::process::Output;
use std::time::Duration;

use serde_json::{json, Value};
use support::*;

fn json_line(output: &Output) -> Value {
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(text.lines().count(), 1, "one JSON line: {text:?}");
    serde_json::from_str(&text).unwrap_or_else(|_| panic!("not JSON: {text}"))
}

fn refused(output: &Output, key: &str) -> Value {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let value = json_line(output);
    assert_eq!(value["ok"], false, "{value}");
    assert_eq!(value["error"]["key"], key, "{value}");
    assert!(
        value["error"]["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty() && message.is_ascii()),
        "{value}"
    );
    value
}

#[test]
fn pairing_redeems_the_code_and_writes_the_credential_private() {
    let profile = Profile::new("pair");
    let room = fake_room(
        200,
        json!({"connector_id": "conn-1", "token": "secret-token", "protocol": 3, "dial_key": "dk"}),
    );
    // The desktop app's own argument order: the flag last.
    let output = profile
        .connector(&[
            "pair",
            &format!("{}/some/page", room.url),
            "ABCD-1234",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value = json_line(&output);
    assert_eq!(
        value,
        json!({"ok": true, "room": room.url, "connector_id": "conn-1"})
    );

    let (target, body) = room.requests.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(target, "POST /api/connectors/pair");
    assert_eq!(body["code"], "ABCD-1234", "{body}");
    assert_eq!(
        body["harnesses"],
        json!(["claude", "codex", "cursor"]),
        "{body}"
    );
    for field in ["host", "platform", "version"] {
        assert!(
            body[field].as_str().is_some_and(|value| !value.is_empty()),
            "{body}"
        );
    }

    let file = profile.data().join("credentials.json");
    let mode = std::fs::metadata(&file).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    let saved = read_json(&file).unwrap();
    assert_eq!(
        saved,
        json!({"url": room.url, "connector_id": "conn-1", "token": "secret-token", "protocol": 3, "dial_key": "dk"})
    );

    // Said to a person: one English line, and a new pairing replaces the previous one.
    let again = fake_room(200, json!({"connector_id": "conn-2", "token": "t2"}));
    let output = profile
        .connector(&["pair", &again.url, "WXYZ"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.starts_with(&format!("Paired with {} as connector conn-2;", again.url)),
        "{text}"
    );
    let saved = read_json(&file).unwrap();
    assert_eq!(saved["connector_id"], "conn-2");
    assert_eq!(saved["protocol"], Value::Null);
    assert!(saved.get("dial_key").is_none(), "{saved}");
}

#[test]
fn a_failed_pairing_says_why_and_writes_nothing() {
    let profile = Profile::new("pairfail");
    let file = profile.data().join("credentials.json");

    let room = fake_room(400, json!({"detail": "That code has expired."}));
    let output = profile
        .connector(&["pair", &room.url, "OLD", "--json"])
        .output()
        .unwrap();
    let value = refused(&output, "pair.refused");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("That code has expired."),
        "{value}"
    );

    let empty = fake_room(200, json!({"connector_id": "conn-1"}));
    let output = profile
        .connector(&["pair", &empty.url, "CODE", "--json"])
        .output()
        .unwrap();
    refused(&output, "pair.bad-answer");

    // Nothing listens there any more.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let output = profile
        .connector(&["pair", &closed, "CODE", "--json"])
        .output()
        .unwrap();
    refused(&output, "pair.unreachable");

    // In clear to anything but loopback or a trusted host: refused before anything is sent.
    for room in ["http://room.example", "http://10.1.2.3:8080"] {
        let output = profile
            .connector(&["pair", room, "CODE", "--json"])
            .output()
            .unwrap();
        refused(&output, "pair.insecure");
    }
    let output = profile
        .connector(&["pair", "room.example", "CODE", "--json"])
        .output()
        .unwrap();
    refused(&output, "pair.bad-url");
    let output = profile
        .connector(&["pair", &room.url, " ", "--json"])
        .output()
        .unwrap();
    refused(&output, "pair.missing");
    let output = profile
        .connector(&["pair", &room.url, "--json"])
        .output()
        .unwrap();
    refused(&output, "connector.usage");
    assert!(!file.exists(), "a failed pairing wrote {}", file.display());

    // Without --json: one English line on stderr.
    let output = profile
        .connector(&["pair", "http://room.example", "CODE"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        text.starts_with(
            "sidevoice: http://room.example would carry this machine's credential in clear"
        ),
        "{text}"
    );
}
