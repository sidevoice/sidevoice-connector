//! Public CLI contracts that must work before a Sidevoice installation exists.
use serde_json::Value;
use std::{fs, process::Command};

#[test]
fn identity_is_native_and_does_not_create_user_state() {
    let root = std::env::temp_dir().join(format!("sidevoice-identity-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    for args in [["--version", "--json"], ["metadata", "--json"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_sidevoice-rust-proof"))
            .args(args)
            .env("HOME", &root)
            .env("SIDEVOICE_DATA_DIR", root.join("data"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["ok"], true);
        let identity = value.get("connector").unwrap_or(&value);
        assert_eq!(identity["format"], "rust-native");
        assert_eq!(identity["sea"], false);
    }
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    fs::remove_dir(&root).unwrap();
}

#[test]
fn unknown_command_has_one_json_refusal_and_nonzero_status() {
    let output = Command::new(env!("CARGO_BIN_EXE_sidevoice-rust-proof"))
        .args(["not-a-command", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["key"], "command.unknown");
    assert!(output.stderr.is_empty());
}
