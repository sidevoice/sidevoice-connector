//! Read-only, local facts for the Cursor agent card. No bridge secret or home path enters the status row.

use crate::profile::Profile;
use serde_json::{json, Value};
use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

const MIN_CURSOR: [u32; 3] = [3, 22, 12];
const MAX_DESCRIPTOR: u64 = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BridgeFiles {
    Missing,
    Stale,
    Live,
}

fn bridge_dir(profile: &Profile) -> PathBuf {
    env::var_os("CURSOR_DESKTOP_BRIDGE_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| super::config_home(profile).join("desktop-bridge"))
}

fn inventory(dir: &Path) -> BridgeFiles {
    let Ok(entries) = fs::read_dir(dir) else {
        return BridgeFiles::Missing;
    };
    let mut json_seen = false;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        json_seen = true;
        // A descriptor's token is checked for presence only. The socket path is used locally only.
        let Ok(file) = File::open(&path) else {
            continue;
        };
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > MAX_DESCRIPTOR
        {
            continue;
        }
        let mut bytes = Vec::new();
        if file
            .take(MAX_DESCRIPTOR + 1)
            .read_to_end(&mut bytes)
            .is_err()
        {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let Some(pid) = value.get("pid").and_then(Value::as_u64) else {
            continue;
        };
        let Some(socket) = value.get("socketPath").and_then(Value::as_str) else {
            continue;
        };
        if value.get("protocolVersion") == Some(&json!(1))
            && value
                .get("token")
                .and_then(Value::as_str)
                .is_some_and(|token| !token.is_empty())
            && Path::new(socket).exists()
            && pid > 0
            && pid <= i32::MAX as u64
            && super::process_alive(pid)
        {
            return BridgeFiles::Live;
        }
    }
    if json_seen {
        BridgeFiles::Stale
    } else {
        BridgeFiles::Missing
    }
}

fn parsed_version(raw: &str) -> Option<[u32; 3]> {
    raw.split(|c: char| c.is_whitespace() || c == 'v')
        .find_map(|part| {
            let part = part.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
            let mut pieces = part.split('.');
            Some([
                pieces.next()?.parse().ok()?,
                pieces.next()?.parse().ok()?,
                pieces.next()?.parse().ok()?,
            ])
        })
}

fn sqlite3_available(path: Option<&str>) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_some_and(|path| {
        env::split_paths(path).any(|dir| {
            fs::metadata(dir.join("sqlite3"))
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
    })
}

pub fn status(
    profile: &Profile,
    cursor_version: Option<&str>,
    login_path: Option<&str>,
    registration: &str,
) -> Value {
    assess(
        cursor_version,
        inventory(&bridge_dir(profile)),
        sqlite3_available(login_path),
        registration,
    )
}

fn assess(
    cursor_version: Option<&str>,
    bridge: BridgeFiles,
    sqlite3: bool,
    registration: &str,
) -> Value {
    let version = cursor_version.and_then(parsed_version);
    let version_state = match version {
        Some(found) if found >= MIN_CURSOR => "done",
        Some(_) => "needs_action",
        None => "unknown",
    };
    let bridge_state = if bridge == BridgeFiles::Live {
        "done"
    } else {
        "needs_action"
    };
    let setting_state = if bridge == BridgeFiles::Live {
        "done"
    } else {
        "unknown"
    };
    let registration_state = match registration {
        "connected" => "done",
        "not-connected" | "outdated" => "needs_action",
        _ => "unknown",
    };
    let reason = if version_state == "needs_action" {
        Some("cursor_version_too_old")
    } else if bridge == BridgeFiles::Missing {
        Some("bridge_missing")
    } else if bridge == BridgeFiles::Stale {
        Some("bridge_stale")
    } else if !sqlite3 {
        Some("sqlite3_missing")
    } else if registration_state == "needs_action" {
        Some("sidevoice_not_registered")
    } else if version_state == "unknown" {
        Some("cursor_version_unknown")
    } else if registration_state == "unknown" {
        Some("registration_unknown")
    } else {
        None
    };
    let state = match reason {
        None => "ready",
        Some("cursor_version_unknown" | "registration_unknown") => "unknown",
        Some(_) => "needs_attention",
    };
    let normalized_version =
        version.map(|[major, minor, patch]| format!("{major}.{minor}.{patch}"));
    json!({
        "state":state,
        "reason":reason,
        "cursor_version":normalized_version,
        "minimum_cursor_version":"3.22.12",
        "minimum_connector_version":"0.9.0",
        "steps":[
            {"id":"cursor_version","state":version_state},
            {"id":"enable_desktop_bridge","state":setting_state},
            {"id":"bridge_running","state":bridge_state},
            {"id":"sqlite3","state":if sqlite3 {"done"} else {"needs_action"}},
            {"id":"register_sidevoice","state":registration_state},
            {"id":"join_editor_chat","state":"unknown"},
            {"id":"wait_for_chat_registration","state":"unknown"}
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn temporary_bridge_descriptors_cover_missing_empty_live_and_stale() {
        let root = env::temp_dir().join(format!(
            "sidevoice-cursor-bridge-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let bridge = root.join("desktop-bridge");
        assert_eq!(inventory(&bridge), BridgeFiles::Missing);
        fs::create_dir_all(&bridge).unwrap();
        assert_eq!(inventory(&bridge), BridgeFiles::Missing);
        let socket = root.join("socket");
        fs::write(&socket, b"").unwrap();
        let descriptor = bridge.join("one.json");
        let payload = json!({
            "protocolVersion":1,
            "pid":std::process::id(),
            "socketPath":socket,
            "token":"private-do-not-report",
            "userDataDir":"/private/also-do-not-report",
        });
        fs::write(&descriptor, serde_json::to_vec(&payload).unwrap()).unwrap();
        assert_eq!(inventory(&bridge), BridgeFiles::Live);
        fs::remove_file(&socket).unwrap();
        assert_eq!(inventory(&bridge), BridgeFiles::Stale);
        fs::write(&socket, b"").unwrap();
        fs::remove_file(&descriptor).unwrap();
        symlink(&socket, &descriptor).unwrap();
        assert_eq!(inventory(&bridge), BridgeFiles::Stale);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sqlite_check_uses_only_the_supplied_path() {
        let root = env::temp_dir().join(format!(
            "sidevoice-sqlite-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&root).unwrap();
        let path = root.to_str().unwrap();
        assert!(!sqlite3_available(Some(path)));
        let stub = root.join("sqlite3");
        fs::write(&stub, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(sqlite3_available(Some(path)));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn states_are_keyed_and_conservative() {
        assert_eq!(
            assess(Some("Cursor 3.22.12"), BridgeFiles::Live, true, "connected")["state"],
            "ready"
        );
        assert_eq!(
            assess(Some("3.22.11"), BridgeFiles::Live, true, "connected")["reason"],
            "cursor_version_too_old"
        );
        assert_eq!(
            assess(Some("3.22.12"), BridgeFiles::Missing, true, "connected")["reason"],
            "bridge_missing"
        );
        assert_eq!(
            assess(Some("3.22.12"), BridgeFiles::Stale, true, "connected")["reason"],
            "bridge_stale"
        );
        assert_eq!(
            assess(Some("3.22.12"), BridgeFiles::Live, false, "connected")["reason"],
            "sqlite3_missing"
        );
        assert_eq!(
            assess(None, BridgeFiles::Live, true, "connected")["state"],
            "unknown"
        );
        assert_eq!(
            assess(Some("3.22.12"), BridgeFiles::Live, true, "not-connected")["reason"],
            "sidevoice_not_registered"
        );
        assert_eq!(
            assess(Some("3.22.12"), BridgeFiles::Missing, true, "connected")["steps"][1]["state"],
            "unknown"
        );
    }
}
