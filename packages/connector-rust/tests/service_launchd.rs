//! The connector's login service under the real launchd, with the real core, in a private profile (its jobs carry
//! profile-specific labels, never the installed ones): install → both jobs up and linked → the core killed: launchd
//! starts it again and the connector links to it → a person's stop → start → uninstall: no job, definition or
//! socket left, the installation still there (`not-installed`) and, once it is removed too, `absent`. Every place it
//! uses comes from the profile's environment.
//!
//! macOS only. It loads jobs into this user's launchd domain, so outside CI it runs only when asked
//! (`SIDEVOICE_TEST_LAUNCHD=1`).
#![cfg(target_os = "macos")]

mod support;

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use support::*;

struct Service<'a> {
    profile: &'a Profile,
    installed: bool,
}

impl Service<'_> {
    fn run(&self, action: &str) -> Value {
        let output = self
            .profile
            .connector(&["service", action, "--json"])
            .output()
            .expect("run the service command");
        let text = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            text.lines().count(),
            1,
            "service {action} must print one JSON line: {text:?} {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_str(&text).unwrap()
    }

    fn ok(&self, action: &str) -> Value {
        let answer = self.run(action);
        assert_eq!(answer["ok"], true, "service {action}: {answer}");
        answer
    }

    fn wait_state(&self, what: &str, accept: impl Fn(&Value) -> bool) -> Value {
        until(what, 90, || Some(self.run("status")).filter(&accept))
    }

    fn labels(&self) -> Vec<String> {
        let mut labels: Vec<String> = fs::read_dir(self.profile.data().join("service"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.strip_suffix(".plist").map(str::to_string)
            })
            .collect();
        labels.sort();
        labels
    }
}

impl Drop for Service<'_> {
    fn drop(&mut self) {
        if self.installed {
            let _ = self.run("uninstall");
        }
    }
}

fn launchd_has(label: &str) -> bool {
    let uid = unsafe { libc::getuid() };
    Command::new("/bin/launchctl")
        .args(["print", &format!("gui/{uid}/{label}")])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn alive(pid: u64) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// The release layout the private service runs, under the profile's release root (`$XDG_DATA_HOME/sidevoice`):
/// `releases/current/core/bin/sidevoice-core` (here a wrapper that gives the real core its models) and
/// `releases/current/dist/sidevoice-connector`.
fn stage_release(profile: &Profile, core: &Path) {
    let root = release_root(profile);
    let release = root.join("releases/r1");
    for dir in [
        "",
        "releases",
        "releases/r1",
        "releases/r1/core",
        "releases/r1/core/bin",
        "releases/r1/dist",
    ] {
        private_dir(&root.join(dir));
    }
    let env = profile.core_env(core);
    let wrapper = format!(
        "#!/bin/sh\nRUSTVANI_CACHE_DIR='{}' SIDEVOICE_STUN_URLS= VOICE_BROWSER_HEARTBEAT_SECONDS=0 exec '{}' \"$@\"\n",
        env["RUSTVANI_CACHE_DIR"],
        core.join("bin/sidevoice-core-rust").display()
    );
    let core_program = release.join("core/bin/sidevoice-core");
    fs::write(&core_program, wrapper).unwrap();
    fs::set_permissions(&core_program, fs::Permissions::from_mode(0o700)).unwrap();
    let connector = release.join("dist/sidevoice-connector");
    fs::copy(CONNECTOR, &connector).unwrap();
    fs::set_permissions(&connector, fs::Permissions::from_mode(0o700)).unwrap();
    symlink("r1", root.join("releases/current")).unwrap();
}

fn release_root(profile: &Profile) -> PathBuf {
    profile.root.join("xdg/data/sidevoice")
}

#[test]
fn the_login_service_runs_recovers_stops_and_leaves_nothing_behind() {
    let Some(core) = core_dir() else { return };
    if std::env::var_os("CI").is_none() && std::env::var_os("SIDEVOICE_TEST_LAUNCHD").is_none() {
        eprintln!("skipped: loads launchd jobs; set SIDEVOICE_TEST_LAUNCHD=1 to run it outside CI");
        return;
    }
    let profile = Profile::new("launchd");
    stage_release(&profile, &core);
    let mut service = Service {
        profile: &profile,
        installed: false,
    };

    service.installed = true;
    service.ok("install");
    let running = service.wait_state("both jobs running and the core reachable", |status| {
        status["state"] == "running"
            && status["reachable"] == true
            && status["connector"]["running"] == true
    });
    let labels = service.labels();
    assert_eq!(labels.len(), 2, "{labels:?}");
    for label in &labels {
        assert!(label.starts_with("dev.sidevoice.rustproof."), "{label}");
        assert!(launchd_has(label), "launchd does not know {label}");
    }
    let first_core = running["core"]["pid"].as_u64().expect("core pid");
    let first_launch = running["core"]["launch_id"].as_str().unwrap().to_string();
    profile.wait_linked(&first_launch);

    // The core dies: launchd starts it again (KeepAlive on a crash) and the connector job links to the new one.
    unsafe {
        libc::kill(first_core as i32, libc::SIGKILL);
    }
    let restarted = service.wait_state("launchd restarted the core", |status| {
        status["state"] == "running"
            && status["reachable"] == true
            && status["core"]["pid"]
                .as_u64()
                .is_some_and(|pid| pid != first_core)
    });
    profile.wait_linked(restarted["core"]["launch_id"].as_str().unwrap());

    // A person's stop holds: nothing runs until they start it again.
    service.ok("stop");
    let stopped = service.run("status");
    assert_eq!(stopped["state"], "stopped-by-person", "{stopped}");
    assert!(!profile.connector_socket().exists());
    let core_pid = restarted["core"]["pid"].as_u64().unwrap();
    until("the stopped core exited", 15, || {
        (!alive(core_pid)).then_some(())
    });

    service.ok("start");
    service.wait_state("running again after start", |status| {
        status["state"] == "running" && status["reachable"] == true
    });

    service.ok("uninstall");
    service.installed = false;
    // Uninstalling the service removes its jobs, not the installation (the release it ran): the state names that.
    let unserviced = service.run("status");
    assert_eq!(unserviced["state"], "not-installed", "{unserviced}");
    assert_eq!(unserviced["installed"], true, "{unserviced}");
    // Without the installation either, nothing is left.
    fs::remove_file(release_root(&profile).join("releases/current")).unwrap();
    let absent = service.run("status");
    assert_eq!(absent["state"], "absent", "{absent}");
    assert_eq!(absent["installed"], false, "{absent}");
    assert!(service.labels().is_empty());
    for label in &labels {
        assert!(!launchd_has(label), "{label} survived uninstall");
    }
    assert!(!profile.connector_socket().exists());
}
