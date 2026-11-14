//! The canary: the real connector binary run with `SIDEVOICE_LOG=trace`, its real log file scanned for what must never
//! be in it (the shared logging guideline, section 10). Its own test binary and one test, so nothing else shares the
//! process's environment or the log. Positive controls come first: a scan of an empty log would pass.
#![allow(clippy::disallowed_macros)] // test output is not logging
#![cfg(unix)]
mod support;

use sidevoice_connector::platform::ipc::{Address, Endpoint};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use support::*;

const TOKEN: &str = "canary-secret-7f3a";

/// `bytes` as `Vec<u8>`'s `Debug` prints them.
fn decimal(text: &str) -> String {
    format!("{:?}", text.as_bytes())
        .trim_matches(['[', ']'])
        .to_owned()
}

#[test]
fn the_connector_log_holds_no_secret_and_no_home() {
    let profile = Profile::new("canary");
    // A ready file naming a core that is not there, with a canary for its token: the link reads it on every attempt.
    let ready = profile.core_data().join("core.json");
    let endpoint = <Endpoint as Address>::new(&profile.core_data(), "local");
    fs::write(
        &ready,
        serde_json::json!({"pid":1,"launch_id":"canary-launch","endpoint":endpoint.to_string(),
            "connector_id":"canary-connector","token":TOKEN})
        .to_string(),
    )
    .unwrap();
    fs::set_permissions(&ready, fs::Permissions::from_mode(0o600)).unwrap();
    let mut command = profile.connector(&["connector"]);
    command.env("SIDEVOICE_LOG", "trace");
    let log = profile.data().join("connector.log");
    let mut process = Process(
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(fs::File::create(profile.root.join("stderr.txt")).unwrap())
            .spawn()
            .expect("start the connector"),
    );
    let text = until(
        "the connector logged its start and the lost link",
        30,
        || {
            fs::read_to_string(&log).ok().filter(|text| {
                text.contains("connector started") && text.contains("core link lost")
            })
        },
    );
    process.stop();
    let text = format!(
        "{text}{}",
        fs::read_to_string(profile.root.join("stderr.txt")).unwrap()
    );
    let home = profile.root.join("home").to_string_lossy().into_owned();
    assert!(text.contains("version="), "{text}");
    for forbidden in [TOKEN, home.as_str(), &decimal(TOKEN), &decimal(&home)] {
        assert!(!text.contains(forbidden), "{forbidden} leaked:\n{text}");
    }
    assert!(is_private(&log));
}
