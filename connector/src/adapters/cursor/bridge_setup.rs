//! Read-only, local facts for the Cursor agent card. No bridge secret or home path enters the status row.

use crate::profile::Profile;
use serde::Deserialize;
use serde_json::{json, Value};
use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_DESCRIPTOR: u64 = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BridgeFiles {
    Missing,
    Stale,
    Live,
}

/// Parse only fields needed to decide whether a bridge instance is usable. All other JSON fields
/// (including Cursor's user data directory) are ignored, and none of these values leaves this module.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadinessDescriptor {
    protocol_version: Option<u64>,
    pid: Option<u64>,
    socket_path: Option<PathBuf>,
    token: Option<String>,
}

fn bridge_dir(profile: &Profile) -> PathBuf {
    env::var("CURSOR_DESKTOP_BRIDGE_DIR")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| profile.home.join(".cursor/desktop-bridge"))
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
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > MAX_DESCRIPTOR
        {
            continue;
        }
        let Ok(file) = File::open(&path) else {
            continue;
        };
        let mut bytes = Vec::new();
        if file
            .take(MAX_DESCRIPTOR + 1)
            .read_to_end(&mut bytes)
            .is_err()
        {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<ReadinessDescriptor>(&bytes) else {
            continue;
        };
        let Some(pid) = value.pid else {
            continue;
        };
        let Some(socket) = value.socket_path else {
            continue;
        };
        if value.protocol_version == Some(1)
            && value.token.is_some_and(|token| !token.is_empty())
            && socket.exists()
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

pub fn status(profile: &Profile, editor_available: bool) -> Value {
    // Agent discovery has no particular editor chat. Its chat_known value is therefore unknown.
    // The per-chat voice status remains the source of a known/unknown chat binding.
    let bridge = inventory(&bridge_dir(profile));
    assess(cfg!(target_os = "macos"), editor_available, bridge, None)
}

fn assess(
    macos: bool,
    editor_available: bool,
    bridge: BridgeFiles,
    chat_known: Option<bool>,
) -> Value {
    let state = if !macos || !editor_available {
        "unsupported"
    } else if bridge == BridgeFiles::Live {
        "running"
    } else {
        "not_running"
    };
    json!({
        "state": state,
        "chat_known": if state == "running" { chat_known } else { None },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

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
    fn states_and_chat_knowledge_have_only_the_two_contract_fields() {
        for (macos, editor, bridge, known, expected) in [
            (
                true,
                true,
                BridgeFiles::Live,
                Some(true),
                json!({"state":"running","chat_known":true}),
            ),
            (
                true,
                true,
                BridgeFiles::Live,
                Some(false),
                json!({"state":"running","chat_known":false}),
            ),
            (
                true,
                true,
                BridgeFiles::Live,
                None,
                json!({"state":"running","chat_known":null}),
            ),
            (
                true,
                true,
                BridgeFiles::Missing,
                None,
                json!({"state":"not_running","chat_known":null}),
            ),
            (
                true,
                true,
                BridgeFiles::Stale,
                Some(true),
                json!({"state":"not_running","chat_known":null}),
            ),
            (
                false,
                true,
                BridgeFiles::Live,
                Some(true),
                json!({"state":"unsupported","chat_known":null}),
            ),
            (
                true,
                false,
                BridgeFiles::Live,
                Some(true),
                json!({"state":"unsupported","chat_known":null}),
            ),
        ] {
            assert_eq!(assess(macos, editor, bridge, known), expected);
        }
    }

    #[test]
    fn disabled_setting_without_bridge_files_is_not_running() {
        assert_eq!(
            assess(true, true, BridgeFiles::Missing, None)["state"],
            "not_running"
        );
    }
}
