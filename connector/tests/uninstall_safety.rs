//! The desktop consumes the command's existing one-object JSON failure contract.

use serde_json::Value;
use sidevoice_connector::platform::data_dir;
mod support;

use sidevoice_connector::platform::env::{Env as _, Environment};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use support::TestFiles;

fn temp_home() -> std::path::PathBuf {
    let home = support::short_temp_dir().join(format!("uninstall-unsafe-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&home).unwrap();
    support::Files::shut_others_out(&home);
    home
}

fn uninstall(home: &Path, data: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sidevoice-connector"))
        .args(["uninstall", "--json"])
        .env_clear()
        .envs(Environment::base())
        .env(Environment::HOME, home)
        .env("SIDEVOICE_DATA_DIR", data)
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
        let output = uninstall(
            &home,
            &home.join(data_dir::standard().unwrap().file_name().unwrap()),
        );
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

#[test]
fn the_home_is_refused_even_with_a_record_naming_its_releases() {
    let home = temp_home();
    fs::write(home.join("keep"), "untouched").unwrap();
    let record = serde_json::json!({"command": ["x"], "releases": home.join("install")});
    fs::write(home.join("install.json"), record.to_string()).unwrap();
    support::Files::shut_others_out(&home.join("install.json"));
    let output = uninstall(&home, &home);
    assert!(!output.status.success());
    let answer: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(fs::read_to_string(home.join("keep")).unwrap(), "untouched");
    assert!(home.join("install.json").is_file());
    fs::remove_dir_all(home).unwrap();
}

#[test]
fn a_directory_with_only_leftovers_and_no_record_is_left_exactly_as_it_is() {
    let home = temp_home();
    let data = home.join("unrelated");
    fs::create_dir(&data).unwrap();
    support::Files::shut_others_out(&data);
    fs::write(data.join("node-stopped.json"), "theirs").unwrap();
    let output = uninstall(&home, &data);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{stdout}");
    let answer: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(answer["installed"], false, "{answer}");
    let names: Vec<_> = fs::read_dir(&data)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, ["node-stopped.json"], "nothing written");
    assert_eq!(
        fs::read_to_string(data.join("node-stopped.json")).unwrap(),
        "theirs"
    );
    fs::remove_dir_all(home).unwrap();
}
