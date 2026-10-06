//! The command line as people and programs use it, with no core: `--version`, `agents`, and how a failure is said
//! (English text on stderr; with `--json`, one object with a stable key on stdout). And the connector daemon's own
//! guards: a data directory others can enter is refused, one connector serves a data directory, and its log is
//! private.

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Output;

use serde_json::Value;
use support::*;

fn json_line(output: &Output) -> Value {
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(text.lines().count(), 1, "one JSON line: {text:?}");
    serde_json::from_str(&text).unwrap_or_else(|_| panic!("not JSON: {text}"))
}

#[test]
fn version_names_the_release_and_its_json_the_build() {
    let profile = Profile::new("version");
    let plain = profile.connector(&["--version"]).output().unwrap();
    assert!(plain.status.success());
    let text = String::from_utf8_lossy(&plain.stdout);
    assert!(text.starts_with("sidevoice-connector "), "{text}");
    let version = text.trim().trim_start_matches("sidevoice-connector ");

    let json = profile
        .connector(&["--version", "--json"])
        .output()
        .unwrap();
    assert!(json.status.success());
    let value = json_line(&json);
    assert_eq!(value["ok"], true, "{value}");
    assert_eq!(value["version"], version, "{value}");
    assert!(value["target"].is_string() && value["source_sha"].is_string());
}

#[test]
fn agents_lists_in_english_or_as_json_and_failures_carry_a_stable_key() {
    let profile = Profile::new("agents");
    let listed = profile.connector(&["agents", "--json"]).output().unwrap();
    assert!(listed.status.success());
    let value = json_line(&listed);
    assert_eq!(value["ok"], true, "{value}");
    let ids: Vec<&str> = value["agents"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["id"].as_str())
        .collect();
    assert_eq!(ids, ["claude", "codex", "cursor"], "{value}");

    let text = profile.connector(&["agents"]).output().unwrap();
    assert!(text.status.success());
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(text.contains("Claude Code: "), "{text}");

    let refused = profile
        .connector(&["agents", "connect", "nobody", "--json"])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(1));
    let value = json_line(&refused);
    assert_eq!(value["ok"], false, "{value}");
    assert_eq!(value["error"]["key"], "agents.unknown", "{value}");
    assert_eq!(
        value["error"]["message"], "Unknown agent: nobody.",
        "{value}"
    );

    let said = profile
        .connector(&["agents", "connect", "nobody"])
        .output()
        .unwrap();
    assert_eq!(said.status.code(), Some(1));
    assert!(said.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&said.stderr).trim(),
        "sidevoice: Unknown agent: nobody."
    );
}

#[test]
fn the_connector_refuses_a_data_directory_others_can_enter() {
    let profile = Profile::new("unsafe");
    fs::set_permissions(profile.data(), fs::Permissions::from_mode(0o755)).unwrap();
    let output = profile.connector(&["connector"]).output().unwrap();
    assert!(!output.status.success());
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(said.starts_with("sidevoice: "), "{said}");
    assert!(said.contains("unsafe private directory"), "{said}");
    assert!(!profile.connector_socket().exists());
    assert_eq!(
        fs::metadata(profile.data()).unwrap().permissions().mode() & 0o777,
        0o755,
        "refused, not repaired"
    );
    let json = profile
        .connector(&["connector", "--json"])
        .output()
        .unwrap();
    assert_eq!(json_line(&json)["error"]["key"], "connector.failed");
}

#[test]
fn one_connector_serves_a_data_directory_and_its_log_is_private() {
    let profile = Profile::new("single");
    let first = profile.start_connector();
    let second = profile.connector(&["connector"]).output().unwrap();
    assert!(!second.status.success());
    let said = String::from_utf8_lossy(&second.stderr);
    assert!(
        said.contains(&format!(
            "another Sidevoice connector (pid {}) already serves",
            first.pid()
        )),
        "{said}"
    );
    // With no core to link to, the connector says so in its log, which nobody else can read.
    let log = profile.data().join("connector.log");
    let text = until("the connector logged the missing core", 30, || {
        fs::read_to_string(&log)
            .ok()
            .filter(|text| text.contains("[sidevoice] Core link: "))
    });
    assert!(!text.contains("rust proof"), "{text}");
    assert_eq!(fs::metadata(&log).unwrap().permissions().mode() & 0o077, 0);
    let status = connector_ipc(&profile.connector_socket(), "status").expect("status");
    assert_eq!(status["connected"], false, "{status}");
    assert_eq!(status["core"], Value::Null, "{status}");
}
