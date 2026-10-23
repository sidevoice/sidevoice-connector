//! What the service tests share: an installation staged inside a private profile (the real core, this connector),
//! the service commands run against it, and this user's real service manager.
//!
//! The installation is what an installer leaves: `R/current.json` naming `R/releases/r1` (`R` is `D/install`)
//! with the core at `core/bin/sidevoice-core-rust` (a copy of the real one; the profile's environment gives it no STUN and no
//! browser keepalive) and its models at `core/models`, and `D/install.json` naming the command that runs this
//! connector.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::time::Duration;

use serde_json::{json, Value};

use sidevoice_connector::platform::fs::{Files, Fs as _};

use super::{alive, core_program, private_dir, until, Profile, TestFiles, CONNECTOR};

/// `R`.
pub fn releases(profile: &Profile) -> PathBuf {
    profile.data().join("install")
}

/// Stage the release and the installation record; returns `R/current.json`.
pub fn stage_installation(profile: &Profile, core: &Path) -> PathBuf {
    let releases = releases(profile);
    let release = releases.join("releases/r1");
    private_dir(&release.join("core/bin"));
    let program = release.join(format!("core/{}", core_program()));
    fs::copy(core.join(core_program()), &program).unwrap();
    Files::make_executable(&program);
    Files::symlink(core.join("models"), release.join("core/models"));
    Files::write_private(
        &releases.join("current.json"),
        json!({"release": "r1"}).to_string().as_bytes(),
    )
    .unwrap();
    let record = profile.data().join("install.json");
    Files::write_private(
        &record,
        json!({"command": [CONNECTOR]}).to_string().as_bytes(),
    )
    .unwrap();
    releases.join("current.json")
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

/// `service <verb> --json` (`verb` may be words, `login off`): its one JSON line.
pub fn service(profile: &Profile, extra: &BTreeMap<String, String>, verb: &str) -> Value {
    let mut args = vec!["service"];
    args.extend(verb.split_whitespace());
    args.push("--json");
    let output = command(profile, extra, &args)
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

pub fn wait_gone(pid: u64, what: &str) {
    until(what, 30, || (!alive(pid)).then_some(()));
}

// systemd is Linux's (`getuid` names the user's bus); there is no such manager to ask elsewhere.
#[cfg(unix)]
/// This user's systemd user manager, reachable: the variables `systemctl --user` needs. On a CI runner with no
/// session, lingering is enabled for this user first (the runner's own account, on a disposable machine).
pub fn systemd_user_manager() -> Option<BTreeMap<String, String>> {
    // An integration test is its own crate and cannot reach `platform`.
    #[allow(clippy::disallowed_methods)]
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
