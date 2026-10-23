//! `install` and `uninstall` from a package laid out as the release archive lays it out (this build and the real
//! pinned core), in a private profile with no service manager (`SIDEVOICE_SERVICE_MANAGER=none`): the release is
//! staged and selected, the connector started from it starts its core and both answer, the agents found are listed
//! and none is connected; one connected with `agents connect` runs the installation's command; installing again
//! recovers the same release and keeps that registration; uninstalling stops both, removes the registration, the
//! releases and the data, and leaves the locks and the stop.
//!
//! Upgrades, rollback and pruning are the unit tests' (`src/install.rs`): this binary is one version.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::{json, Value};
use support::package::{package, pinned_core};
use support::service::{releases, wait_gone};
use support::*;

/// What every command of this test runs with: no service manager, any free port for the core, no STUN.
fn settings() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("SIDEVOICE_SERVICE_MANAGER".to_owned(), "none".to_owned()),
        ("SIDEVOICE_CORE_PORT".to_owned(), "0".to_owned()),
        ("SIDEVOICE_STUN_URLS".to_owned(), String::new()),
    ])
}

/// Where a test installs Cursor (`cursor_installed`): its CLI on the fake login PATH.
fn cursor_program(profile: &Profile) -> PathBuf {
    profile.root.join("cursor-bin/cursor-agent")
}

/// Cursor installed in the profile: a stand-in of its CLI.
fn cursor_installed(profile: &Profile) {
    let program = cursor_program(profile);
    fs::create_dir_all(program.parent().unwrap()).unwrap();
    fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
    Files::make_executable(&program);
    let shell = profile.root.join("login-shell");
    fs::write(
        &shell,
        "#!/bin/sh\nprintf '\\036SIDEVOICE_PATH_BEGIN\\036%s\\036SIDEVOICE_PATH_END\\036' \"$SIDEVOICE_TEST_LOGIN_PATH\"\n",
    )
    .unwrap();
    Files::make_executable(&shell);
}

/// `program args… --json` in the profile: its one JSON line and whether it succeeded.
fn run(profile: &Profile, program: &Path, args: &[&str]) -> (bool, Value) {
    let output = profile
        .command(program)
        .envs(settings())
        .env("SHELL", profile.root.join("login-shell"))
        .env(
            "SIDEVOICE_TEST_LOGIN_PATH",
            format!(
                "{}:/usr/bin:/bin",
                cursor_program(profile).parent().unwrap().display()
            ),
        )
        .args(args)
        .arg("--json")
        .stdin(Stdio::null())
        .output()
        .expect("run the command");
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        text.lines().count(),
        1,
        "{args:?} must print one JSON line and nothing else: {text:?} {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let answer: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(output.status.success(), answer["ok"] == true, "{answer}");
    (output.status.success(), answer)
}

fn scratch(label: &str) -> PathBuf {
    let path = support::short_temp_dir().join(format!("install-{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

/// Whatever the test left running is stopped (an uninstall), even when it fails half-way.
struct Teardown<'a>(&'a Profile, PathBuf);

impl Drop for Teardown<'_> {
    fn drop(&mut self) {
        // The uninstall below removes the logs the failure needs.
        if std::thread::panicking() {
            eprintln!("{}", self.0.log_tail());
            eprintln!("{}", tree(&self.0.data(), 4));
        }
        let _ = self
            .0
            .command(&self.1)
            .envs(settings())
            .args(["uninstall", "--json"])
            .stdin(Stdio::null())
            .output();
    }
}

#[test]
fn install_runs_the_pair_connects_no_agent_and_uninstall_leaves_only_the_locks() {
    let Some((archive, record)) = pinned_core() else {
        return;
    };
    let at = scratch("pair");
    let root = package(&at, &archive, &record, record["sha256"].as_str().unwrap());
    let program = root.join(connector_program());
    let profile = Profile::new("install");
    cursor_installed(&profile);
    let _teardown = Teardown(&profile, program.clone());

    let (ok, installed) = run(&profile, &program, &["install"]);
    assert!(ok, "{installed}");
    let version = installed["version"].as_str().unwrap().to_owned();
    assert_eq!(installed["action"], "installed", "{installed}");
    assert_eq!(installed["service"], "none", "{installed}");
    let releases = releases(&profile);
    let release = releases.join("releases").join(&version);
    let stable = release.join(connector_program());
    assert_eq!(installed["command"][0], stable.to_string_lossy().as_ref());
    assert_eq!(
        read_json(&releases.join("current.json")).unwrap(),
        json!({"release": version})
    );
    assert!(release.join(format!("core/{}", core_program())).is_file());
    let record_file = read_json(&profile.data().join("install.json")).unwrap();
    assert_eq!(
        record_file["command"], installed["command"],
        "{record_file}"
    );

    // The core answers; a conversation running the registered command gets the release's own connector.
    let status = until("the installed core answers", 60, || {
        let (_, status) = run(&profile, &stable, &["service", "status"]);
        (status["reachable"] == true).then_some(status)
    });
    let mut conversation = profile.command(&stable);
    conversation.envs(settings()).arg("mcp");
    let mut mcp = Mcp::start(conversation);
    mcp.initialize("install");
    let binary = release.join(connector_program()).canonicalize().unwrap();
    until("the release's connector answers", 30, || {
        connector_ipc(&profile.connector_endpoint(), "identity").filter(|identity| {
            identity["executable"].as_str().map(Path::new) == Some(binary.as_path())
        })
    });
    mcp.stop();

    // Cursor (its CLI is installed) is found and left without Sidevoice, with the command that
    // connects it; connected with it, it runs the installation's command. Windows reads the login PATH from the
    // registry, so a stand-in shell cannot put Cursor's CLI on it: nothing of Cursor is checked there.
    let cursor = profile.root.join("cursor/config/mcp.json");
    if cfg!(unix) {
        assert_eq!(
            installed["agents"]["cursor"]["outcome"], "not-connected",
            "{installed}"
        );
        assert_eq!(
            installed["agents"]["cursor"]["command"], "npx sidevoice agents connect cursor",
            "{installed}"
        );
        assert!(
            read_json(&cursor).is_none_or(|config| config["mcpServers"].get("sidevoice").is_none()),
            "installing connected Cursor"
        );
        let (ok, connected) = run(&profile, &stable, &["agents", "connect", "cursor"]);
        assert!(ok, "{connected}");
        let config = read_json(&cursor).unwrap();
        assert_eq!(
            config["mcpServers"]["sidevoice"]["command"],
            stable.to_string_lossy().as_ref(),
            "{config}"
        );
        assert_eq!(config["mcpServers"]["sidevoice"]["args"][0], "mcp");
    }

    // Again: the same release, restarted and verified (a new core).
    let (ok, again) = run(&profile, &program, &["install"]);
    assert!(ok, "{again}");
    assert_eq!(again["action"], "reinstalled", "{again}");
    if cfg!(unix) {
        assert_eq!(
            again["agents"]["cursor"]["outcome"], "registered",
            "{again}"
        );
    }
    let first_core = status["core"]["pid"].as_u64().unwrap();
    wait_gone(first_core, "the core before the reinstall exited");
    let restarted = until("the pair answers again", 60, || {
        let (_, status) = run(&profile, &stable, &["service", "status"]);
        (status["reachable"] == true).then_some(status)
    });

    // Uninstall: both stopped, the registration, the releases and the data gone; the locks and the stop stay.
    let (ok, removed) = run(&profile, &stable, &["uninstall"]);
    assert!(ok, "{removed}");
    assert_eq!(removed["state"], "absent", "{removed}");
    assert_eq!(
        removed["agents"]["cursor"]["outcome"], "removed",
        "{removed}"
    );
    wait_gone(
        restarted["core"]["pid"].as_u64().unwrap(),
        "the core exited at uninstall",
    );
    assert!(!releases.exists(), "{} is left", releases.display());
    assert!(!endpoint_exists(&profile.connector_endpoint()));
    if cfg!(unix) {
        let config = read_json(&cursor).unwrap();
        assert!(config["mcpServers"].get("sidevoice").is_none(), "{config}");
    }
    let mut left: Vec<String> = fs::read_dir(profile.data())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    for name in &left {
        assert!(
            [
                "install.lock",
                "connector.lock",
                "agents.lock",
                "node-stopped.json"
            ]
            .contains(&name.as_str()),
            "{name} is left in the data directory: {left:?}"
        );
    }
    assert!(left.contains(&"node-stopped.json".to_owned()), "{left:?}");
    fs::remove_dir_all(&at).unwrap();
}

/// Every file below `dir` with its size, for a failure's message.
fn tree(dir: &Path, depth: usize) -> String {
    let mut out = String::new();
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
        out.push_str(&format!("{} ({size})\n", path.display()));
        if depth > 0 && path.is_dir() {
            out.push_str(&tree(&path, depth - 1));
        }
    }
    out
}
