//! `install` and `uninstall` from a package laid out as the release archive lays it out (this build and the real
//! pinned core), in a private profile with no service manager (`SIDEVOICE_SERVICE_MANAGER=none`): the release is
//! staged and selected, the connector started from it starts its core and both answer, the agents found are
//! registered with the installation's command; installing again recovers the same release; uninstalling stops both,
//! removes the registration, the releases and the data, and leaves the locks and the stop.
//!
//! Upgrades, rollback and pruning are the unit tests' (`src/install.rs`): this binary is one version.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::Value;
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

/// `program args… --json` in the profile: its one JSON line and whether it succeeded.
fn run(profile: &Profile, program: &Path, args: &[&str]) -> (bool, Value) {
    let output = profile
        .command(program)
        .envs(settings())
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
    let path = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("install-{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

/// Whatever the test left running is stopped (an uninstall), even when it fails half-way.
struct Teardown<'a>(&'a Profile, PathBuf);

impl Drop for Teardown<'_> {
    fn drop(&mut self) {
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
fn install_runs_the_pair_registers_the_agents_and_uninstall_leaves_only_the_locks() {
    let Some((archive, record)) = pinned_core() else {
        return;
    };
    let at = scratch("pair");
    let root = package(&at, &archive, &record, record["sha256"].as_str().unwrap());
    let program = root.join("bin/sidevoice-connector");
    let profile = Profile::new("install");
    let _teardown = Teardown(&profile, program.clone());

    let (ok, installed) = run(&profile, &program, &["install"]);
    assert!(ok, "{installed}");
    let version = installed["version"].as_str().unwrap().to_owned();
    assert_eq!(installed["action"], "installed", "{installed}");
    assert_eq!(installed["service"], "none", "{installed}");
    let releases = releases(&profile);
    let stable = releases.join("current/bin/sidevoice-connector");
    assert_eq!(installed["command"][0], stable.to_string_lossy().as_ref());
    let release = releases.join("releases").join(&version);
    assert_eq!(
        fs::read_link(releases.join("current")).unwrap(),
        Path::new("releases").join(&version)
    );
    assert!(release.join("core/bin/sidevoice-core-rust").is_file());
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
    let binary = release
        .join("bin/sidevoice-connector")
        .canonicalize()
        .unwrap();
    until("the release's connector answers", 30, || {
        connector_ipc(&profile.connector_socket(), "identity").filter(|identity| {
            identity["executable"].as_str().map(Path::new) == Some(binary.as_path())
        })
    });
    mcp.stop();

    // Cursor (its configuration directory is there) runs the installation's command.
    assert_eq!(
        installed["agents"]["cursor"]["outcome"], "registered",
        "{installed}"
    );
    let cursor = profile.root.join("cursor/config/mcp.json");
    let config = read_json(&cursor).unwrap();
    assert_eq!(
        config["mcpServers"]["sidevoice"]["command"],
        stable.to_string_lossy().as_ref(),
        "{config}"
    );
    assert_eq!(config["mcpServers"]["sidevoice"]["args"][0], "mcp");

    // Again: the same release, restarted and verified (a new core).
    let (ok, again) = run(&profile, &program, &["install"]);
    assert!(ok, "{again}");
    assert_eq!(again["action"], "reinstalled", "{again}");
    assert_eq!(
        again["agents"]["cursor"]["outcome"], "registered",
        "{again}"
    );
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
    assert!(!profile.connector_socket().exists());
    let config = read_json(&cursor).unwrap();
    assert!(config["mcpServers"].get("sidevoice").is_none(), "{config}");
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
