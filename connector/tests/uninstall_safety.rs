//! The desktop consumes the command's existing one-object JSON failure contract.

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

#[test]
fn unsafe_root_refusal_is_a_nonzero_json_failure() {
    let home = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("uninstall-unsafe-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&home).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(home.join("keep"), "untouched").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sidevoice-connector"))
        .args(["uninstall", "--json"])
        .env_clear()
        .env("HOME", &home)
        .env("SIDEVOICE_DATA_DIR", &home)
        .env_remove("XDG_DATA_HOME")
        .env("SIDEVOICE_SERVICE_MANAGER", "none")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1, "{stdout:?}");
    let answer: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(answer["error"]["key"], "install.failed", "{answer}");
    assert_eq!(fs::read_to_string(home.join("keep")).unwrap(), "untouched");
    fs::remove_dir_all(home).unwrap();
}
