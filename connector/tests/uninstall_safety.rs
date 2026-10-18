//! The desktop consumes the command's existing one-object JSON failure contract.

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

fn temp_home() -> std::path::PathBuf {
    // The workspace can be group-writable in CI. Put this safety fixture under
    // the system temporary root so an unrelated ancestor is not the failure.
    let home = std::env::temp_dir().join(format!("uninstall-unsafe-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&home).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    home
}

fn uninstall(home: &Path, data: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sidevoice-connector"))
        .args(["uninstall", "--json"])
        .env_clear()
        .env("HOME", home)
        .env("SIDEVOICE_DATA_DIR", data)
        .env_remove("XDG_DATA_HOME")
        .env("SIDEVOICE_SERVICE_MANAGER", "none")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn unsafe_root_refusal_is_a_nonzero_json_failure() {
    let home = temp_home();
    fs::write(home.join("keep"), "untouched").unwrap();
    let output = uninstall(&home, &home);
    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1, "{stdout:?}");
    let answer: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(answer["error"]["key"], "install.failed", "{answer}");
    assert_eq!(fs::read_to_string(home.join("keep")).unwrap(), "untouched");
    fs::remove_dir_all(home).unwrap();
}

#[test]
fn absent_installation_can_be_uninstalled_twice() {
    let home = temp_home();
    for _ in 0..2 {
        let output = uninstall(&home, &home.join(".sidevoice"));
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(output.status.success(), "{stdout}");
        assert_eq!(stdout.lines().count(), 1, "{stdout:?}");
        let answer: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(answer["ok"], true, "{answer}");
        assert_eq!(answer["state"], "absent", "{answer}");
    }
    fs::remove_dir_all(home).unwrap();
}

#[test]
fn installed_cli_advertises_safe_uninstall_capability() {
    let output = Command::new(env!("CARGO_BIN_EXE_sidevoice-connector"))
        .args(["--version", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let answer: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(answer["ok"], true);
    assert_eq!(answer["safe_uninstall"], 1);
}
