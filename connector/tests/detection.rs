//! Production binary detection: the login shell's PATH is the sole CLI source.

mod support;

use serde_json::Value;
use std::fs;
use std::path::Path;
use support::{executable, Profile, CONNECTOR};

fn program(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
    executable(path);
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
    let launched = profile.root.join("cli-launched");
    program(
        &bin,
        &format!("#!/bin/sh\ntouch '{}'\n", launched.display()),
    );
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
    assert!(!launched.exists(), "discovery launched the candidate CLI");
    fs::write(&shell, "#!/bin/sh\nexit 1\n").unwrap();
    let failed = listed(&profile, Some(&shell), Some(&bin));
    assert!(
        !has(&failed, "claude"),
        "stale cache or fallback was used: {failed}"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn cursor_install_directory_is_passive_and_needs_a_captured_path() {
    let profile = Profile::new("markerdetect");
    let install_dir = profile.root.join("home/.local/share/cursor");
    fs::create_dir_all(&install_dir).unwrap();
    let no_shell = listed(&profile, None, None);
    assert!(
        !has(&no_shell, "cursor"),
        "failed PATH capture listed a marker: {no_shell}"
    );
    let shell = profile.root.join("login-shell");
    program(&shell, "#!/bin/sh\nprintf '\\036SIDEVOICE_PATH_BEGIN\\036%s\\036SIDEVOICE_PATH_END\\036' /usr/bin\n");
    let found = listed(&profile, Some(&shell), None);
    let cursor = found["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "cursor")
        .unwrap();
    assert_eq!(
        cursor["evidence"],
        serde_json::json!([{"kind":"install_dir"}])
    );
    assert!(!cursor["evidence"]
        .to_string()
        .contains(&profile.root.join("home").to_string_lossy().to_string()));
    fs::remove_dir_all(&install_dir).unwrap();
    let removed = listed(&profile, Some(&shell), None);
    assert!(
        !has(&removed, "cursor"),
        "removed install marker stayed listed: {removed}"
    );
}

#[test]
fn startup_output_is_not_mistaken_for_the_login_path() {
    let profile = Profile::new("noisypath");
    let bin = profile.root.join("found");
    program(&bin.join("claude"), "#!/bin/sh\nexit 0\n");
    let shell = profile.root.join("login-shell");
    program(&shell, &format!(
        "#!/bin/sh\necho 'Welcome to your shell'\necho 'startup stderr' >&2\nprintf '\\036SIDEVOICE_PATH_BEGIN\\036%s\\036SIDEVOICE_PATH_END\\036' '{}'\n",
        bin.display()
    ));
    let found = listed(&profile, Some(&shell), None);
    assert!(
        has(&found, "claude"),
        "framed first PATH component was lost: {found}"
    );
    fs::write(&shell, "#!/bin/sh\necho 'Welcome to your shell'\n").unwrap();
    let refused = listed(&profile, Some(&shell), None);
    assert!(
        !has(&refused, "claude"),
        "banner-only output passed as PATH: {refused}"
    );
}
