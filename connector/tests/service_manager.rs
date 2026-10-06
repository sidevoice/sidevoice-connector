//! The login service under this user's real service manager (launchd on macOS, the systemd user manager on Linux),
//! with the real core and an installation staged in a private profile: install → both jobs up, the core
//! reachable and the connector linked → the core killed: the manager starts it again and the connector links to
//! it → a person's stop holds → start → restart (a new core) → uninstall: no job, definition or socket left, the
//! installation still there (`not-installed`) and, once it is removed too, `absent`.
//!
//! The jobs carry the real names (`dev.sidevoice.core`, `sidevoice-core.service`, …), so outside CI it runs only
//! when asked (`SIDEVOICE_TEST_SERVICE=1`) and never over jobs this user already has. On a Linux runner with no
//! session, lingering is enabled for the runner's user so its user manager runs (`support::service`).
#![cfg(any(target_os = "macos", target_os = "linux"))]

mod support;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;
use support::service::*;
use support::*;

const JOBS: [(&str, &str); 2] = [
    ("dev.sidevoice.core", "sidevoice-core.service"),
    ("dev.sidevoice.connector", "sidevoice-connector.service"),
];

/// Whether the manager has the job loaded.
fn manager_has(extra: &BTreeMap<String, String>, job: (&str, &str)) -> bool {
    if cfg!(target_os = "macos") {
        let uid = unsafe { libc::getuid() };
        Command::new("/bin/launchctl")
            .args(["print", &format!("gui/{uid}/{}", job.0)])
            .output()
            .is_ok_and(|output| output.status.success())
    } else {
        Command::new("systemctl")
            .args(["--user", "show", "-p", "LoadState", job.1])
            .envs(extra)
            .output()
            .is_ok_and(|output| {
                let text = String::from_utf8_lossy(&output.stdout);
                output.status.success() && !text.contains("not-found")
            })
    }
}

/// Where the manager found each job's definition.
fn definitions(profile: &Profile, extra: &BTreeMap<String, String>) -> Vec<PathBuf> {
    JOBS.iter()
        .filter_map(|job| {
            if cfg!(target_os = "macos") {
                let file = profile
                    .root
                    .join(format!("home/Library/LaunchAgents/{}.plist", job.0));
                file.exists().then_some(file)
            } else {
                let output = Command::new("systemctl")
                    .args(["--user", "show", "-p", "FragmentPath", "--value", job.1])
                    .envs(extra)
                    .output()
                    .ok()?;
                let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                (!path.is_empty() && std::path::Path::new(&path).exists())
                    .then(|| PathBuf::from(path))
            }
        })
        .collect()
}

struct Installed<'a> {
    profile: &'a Profile,
    extra: BTreeMap<String, String>,
    active: bool,
}

impl Drop for Installed<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = service(self.profile, &self.extra, "uninstall");
        }
    }
}

#[test]
fn the_login_service_runs_recovers_stops_and_leaves_nothing_behind() {
    let Some(core) = core_dir() else { return };
    if std::env::var_os("CI").is_none() && std::env::var_os("SIDEVOICE_TEST_SERVICE").is_none() {
        eprintln!("skipped: loads jobs into this user's service manager; set SIDEVOICE_TEST_SERVICE=1 to run it");
        return;
    }
    let extra = if cfg!(target_os = "linux") {
        match systemd_user_manager() {
            Some(env) => env,
            None => {
                eprintln!("skipped: no systemd user manager answers");
                return;
            }
        }
    } else {
        BTreeMap::new()
    };
    let kind = if cfg!(target_os = "macos") {
        "launchd"
    } else {
        "systemd"
    };
    for job in JOBS {
        assert!(
            !manager_has(&extra, job),
            "{job:?} is already loaded: not touching this user's own service"
        );
    }
    let profile = Profile::new("svc");
    let current = stage_installation(&profile, &core);

    let before = service(&profile, &extra, "status");
    assert_eq!(
        (before["state"].as_str(), before["installed"].as_bool()),
        (Some("not-installed"), Some(true)),
        "{before}"
    );
    assert_eq!(before["service"], kind, "{before}");

    let mut installed = Installed {
        profile: &profile,
        extra: extra.clone(),
        active: true,
    };
    let answer = ok(&profile, &extra, "install");
    assert_eq!(answer["service"], kind, "{answer}");
    if kind == "systemd" {
        assert!(
            answer["linger"]["command"]
                .as_str()
                .is_some_and(|command| command.starts_with("loginctl enable-linger ")),
            "{answer}"
        );
    }
    let running = wait_state(
        &profile,
        &extra,
        "both jobs running and the core reachable",
        |status| {
            status["state"] == "running"
                && status["reachable"] == true
                && status["connector"]["running"] == true
        },
    );
    for job in JOBS {
        assert!(
            manager_has(&extra, job),
            "the manager does not know {job:?}"
        );
    }
    let files = definitions(&profile, &extra);
    assert_eq!(files.len(), 2, "{files:?}");
    let first_core = running["core"]["pid"].as_u64().expect("core pid");
    profile.wait_linked(running["core"]["launch_id"].as_str().unwrap());

    // The core dies: its manager starts it again, and the connector job links to the new one.
    unsafe {
        libc::kill(first_core as i32, libc::SIGKILL);
    }
    let restarted = wait_state(
        &profile,
        &extra,
        "the manager restarted the core",
        |status| {
            status["state"] == "running"
                && status["core"]["pid"]
                    .as_u64()
                    .is_some_and(|pid| pid != first_core)
        },
    );
    profile.wait_linked(restarted["core"]["launch_id"].as_str().unwrap());

    // A person's stop holds: nothing runs until they start it again.
    let stopped = ok(&profile, &extra, "stop");
    assert_eq!(stopped["state"], "stopped-by-person", "{stopped}");
    assert_eq!(
        service(&profile, &extra, "status")["state"],
        "stopped-by-person"
    );
    assert!(!profile.connector_socket().exists());
    wait_gone(
        restarted["core"]["pid"].as_u64().unwrap(),
        "the stopped core exited",
    );

    ok(&profile, &extra, "start");
    let started = wait_state(&profile, &extra, "running again after start", |status| {
        status["state"] == "running"
            && status["reachable"] == true
            && status["connector"]["running"] == true
    });

    // The person's retry: a new core.
    let retried = ok(&profile, &extra, "restart");
    assert!(retried["state"].is_string(), "{retried}");
    wait_state(&profile, &extra, "a new core after restart", |status| {
        status["state"] == "running" && status["core"]["launch_id"] != started["core"]["launch_id"]
    });

    let removed = ok(&profile, &extra, "uninstall");
    installed.active = false;
    assert_eq!(removed["state"], "not-installed", "{removed}");
    for job in JOBS {
        assert!(!manager_has(&extra, job), "{job:?} survived uninstall");
    }
    for file in &files {
        assert!(!file.exists(), "{} survived uninstall", file.display());
    }
    assert!(!profile.connector_socket().exists());
    // Uninstalling the service removes its jobs, not the installation: the state names that.
    let unserviced = service(&profile, &extra, "status");
    assert_eq!(
        (
            unserviced["state"].as_str(),
            unserviced["installed"].as_bool()
        ),
        (Some("not-installed"), Some(true)),
        "{unserviced}"
    );
    // Without the installation either, nothing is left.
    std::fs::remove_file(&current).unwrap();
    let absent: Value = service(&profile, &extra, "status");
    assert_eq!(
        (absent["state"].as_str(), absent["installed"].as_bool()),
        (Some("absent"), Some(false)),
        "{absent}"
    );
    // And the service cannot be installed with nothing to run.
    let refused = service(&profile, &extra, "install");
    assert_eq!(
        refused["error"]["key"], "service.no-installation",
        "{refused}"
    );
}
