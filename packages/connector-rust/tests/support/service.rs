//! What the service tests share: an installation staged inside a private profile (the real core, this connector),
//! the service commands run against it, and this user's real service manager.
//!
//! The installation is what an installer leaves: `R/current -> R/releases/r1` (`R` is `$XDG_DATA_HOME/sidevoice`)
//! with the core at `core/bin/sidevoice-core-rust` (here a wrapper that runs the real one with no STUN and no
//! browser keepalive) and its models at `core/models`, and `D/install.json` naming the command that runs this
//! connector.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

use super::{private_dir, until, Profile, CONNECTOR};

/// `R`.
pub fn releases(profile: &Profile) -> PathBuf {
    profile.root.join("xdg/data/sidevoice")
}

/// Stage the release and the installation record; returns `R/current`.
pub fn stage_installation(profile: &Profile, core: &Path) -> PathBuf {
    let releases = releases(profile);
    let release = releases.join("releases/r1");
    private_dir(&release.join("core/bin"));
    let wrapper = format!(
        "#!/bin/sh\nSIDEVOICE_STUN_URLS= VOICE_BROWSER_HEARTBEAT_SECONDS=0 exec '{}' \"$@\"\n",
        core.join("bin/sidevoice-core-rust").display()
    );
    let program = release.join("core/bin/sidevoice-core-rust");
    fs::write(&program, wrapper).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    symlink(core.join("models"), release.join("core/models")).unwrap();
    symlink("releases/r1", releases.join("current")).unwrap();
    let record = profile.data().join("install.json");
    fs::write(&record, json!({"command": [CONNECTOR]}).to_string()).unwrap();
    fs::set_permissions(&record, fs::Permissions::from_mode(0o600)).unwrap();
    releases.join("current")
}

/// The environment of this profile's commands, plus `extra` (a manager's bus, a manager override).
pub fn command(profile: &Profile, extra: &BTreeMap<String, String>, args: &[&str]) -> Command {
    let mut command = profile.command(CONNECTOR);
    command
        .envs(extra)
        .env("SIDEVOICE_CORE_PORT", "0")
        .args(args);
    command
}

/// `service <verb> --json`: its one JSON line.
pub fn service(profile: &Profile, extra: &BTreeMap<String, String>, verb: &str) -> Value {
    let output = command(profile, extra, &["service", verb, "--json"])
        .stdin(Stdio::null())
        .output()
        .expect("run the service command");
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        text.lines().count(),
        1,
        "service {verb} must print one JSON line: {text:?} {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let answer: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        output.status.success(),
        answer["ok"] == true,
        "service {verb}: the exit status follows `ok`: {answer}"
    );
    answer
}

pub fn ok(profile: &Profile, extra: &BTreeMap<String, String>, verb: &str) -> Value {
    let answer = service(profile, extra, verb);
    assert_eq!(answer["ok"], true, "service {verb}: {answer}");
    answer
}

pub fn wait_state(
    profile: &Profile,
    extra: &BTreeMap<String, String>,
    what: &str,
    accept: impl Fn(&Value) -> bool,
) -> Value {
    let mut last = Value::Null;
    let found = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        until(what, 90, || {
            last = service(profile, extra, "status");
            Some(last.clone()).filter(&accept)
        })
    }));
    found.unwrap_or_else(|_| panic!("timed out: {what}; last status {last}"))
}

pub fn alive(pid: u64) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

pub fn wait_gone(pid: u64, what: &str) {
    until(what, 30, || (!alive(pid)).then_some(()));
}

/// This user's systemd user manager, reachable: the variables `systemctl --user` needs. On a CI runner with no
/// session, lingering is enabled for this user first (the runner's own account, on a disposable machine).
pub fn systemd_user_manager() -> Option<BTreeMap<String, String>> {
    let uid = unsafe { libc::getuid() };
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{uid}"));
    let bus = format!("unix:path={runtime}/bus");
    let env = BTreeMap::from([
        ("XDG_RUNTIME_DIR".to_owned(), runtime.clone()),
        ("DBUS_SESSION_BUS_ADDRESS".to_owned(), bus),
    ]);
    let answers = || {
        Command::new("systemctl")
            .args(["--user", "show-environment"])
            .envs(&env)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    if answers() {
        return Some(env);
    }
    // Only a CI runner (a disposable machine) is changed to get one.
    std::env::var_os("CI")?;
    let user = std::env::var("USER").unwrap_or_else(|_| "runner".into());
    let enabled = Command::new("loginctl")
        .args(["enable-linger", &user])
        .status()
        .is_ok_and(|status| status.success())
        || Command::new("sudo")
            .args(["-n", "loginctl", "enable-linger", &user])
            .status()
            .is_ok_and(|status| status.success());
    assert!(enabled, "could not enable lingering for {user}");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        if answers() {
            return Some(env);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    panic!("the systemd user manager of {user} did not answer at {runtime}");
}
