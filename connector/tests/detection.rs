//! Production binary detection: the login shell's PATH is the sole CLI source.

mod support;

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use support::{Profile, CONNECTOR};

fn program(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn listed(profile: &Profile, shell: Option<&Path>, override_bin: Option<&Path>) -> Value {
    let mut command = profile.command(CONNECTOR);
    command.args(["agents", "--json"]);
    if let Some(shell) = shell {
        command.env("SHELL", shell);
    }
    if let Some(binary) = override_bin {
        command.env("SIDEVOICE_CLAUDE_BIN", binary);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn has(answer: &Value, id: &str) -> bool {
    answer["agents"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["id"] == id)
}

#[test]
fn cli_detection_uses_only_fresh_login_path_not_overrides_fixed_paths_or_cache() {
    let profile = Profile::new("detect");
    let bin = profile.root.join("found/claude");
    program(&bin, "#!/bin/sh\nexit 0\n");
    let fixed = profile.root.join("home/.local/bin/claude");
    program(&fixed, "#!/bin/sh\nexit 0\n");
    let shell = profile.root.join("login-shell");
    program(
        &shell,
        &format!(
            "#!/bin/sh\nprintf '\\036SIDEVOICE_PATH_BEGIN\\036%s\\036SIDEVOICE_PATH_END\\036' {}\n",
            bin.parent().unwrap().display()
        ),
    );

    let missing = listed(&profile, None, Some(&bin));
    assert!(!has(&missing, "claude"), "override was used: {missing}");
    let found = listed(&profile, Some(&shell), None);
    assert!(has(&found, "claude"), "login PATH was ignored: {found}");
    fs::write(&shell, "#!/bin/sh\nexit 1\n").unwrap();
    let failed = listed(&profile, Some(&shell), Some(&bin));
    assert!(
        !has(&failed, "claude"),
        "stale cache or fallback was used: {failed}"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn cursor_desktop_registration_needs_a_scan_path_and_hidden_entries_are_not_detected() {
    let profile = Profile::new("desktopdetect");
    let entry = profile.root.join("xdg/data/applications/cursor.desktop");
    fs::create_dir_all(entry.parent().unwrap()).unwrap();
    fs::write(
        &entry,
        "[Desktop Entry]\nType=Application\nName=Cursor\nExec=cursor %F\n",
    )
    .unwrap();
    let no_shell = listed(&profile, None, None);
    assert!(
        !has(&no_shell, "cursor"),
        "failed PATH capture must leave the list empty: {no_shell}"
    );
    let shell = profile.root.join("login-shell");
    program(&shell, "#!/bin/sh\nprintf '\\036SIDEVOICE_PATH_BEGIN\\036%s\\036SIDEVOICE_PATH_END\\036' /usr/bin\n");
    let found = listed(&profile, Some(&shell), None);
    assert!(
        has(&found, "cursor"),
        "OS registration was ignored: {found}"
    );
    assert!(found["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "cursor")
        .unwrap()["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .any(|evidence| evidence["kind"] == "app"));
    fs::write(
        &entry,
        "[Desktop Entry]\nType=Application\nName=Cursor\nExec=cursor\nHidden=true\n",
    )
    .unwrap();
    let hidden = listed(&profile, Some(&shell), None);
    assert!(
        !has(&hidden, "cursor"),
        "hidden app was guessed present: {hidden}"
    );
}
