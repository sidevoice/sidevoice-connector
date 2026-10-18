//! Read-only, local facts for the Cursor agent card. No bridge secret or home path enters the status row.

use crate::adapters::registration::ReadinessCheck;
use crate::profile::Profile;
use serde::Deserialize;
use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

const MAX_DESCRIPTOR: u64 = 16 * 1024;
const MAX_ENTRIES: usize = 256;
const FRESHNESS: Duration = Duration::from_secs(2);
const BRIDGE_NOT_RUNNING: ReadinessCheck = ReadinessCheck::NeedsAttention(
    "cursor.bridge_not_running",
    "Voice in the Cursor editor needs Cursor's Desktop Bridge: in Cursor, open Settings › Beta, enable “Allow CLI to access desktop agents”, then restart Cursor.",
);
const CURSOR_UNSUPPORTED: ReadinessCheck = ReadinessCheck::Unsupported(
    "cursor.unsupported",
    "Cursor editor voice is not supported on this computer.",
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BridgeFiles {
    Missing,
    Stale,
    Live,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BridgeEvidence {
    files: BridgeFiles,
    editor_app: bool,
}

#[derive(Default)]
struct CacheState {
    path: Option<PathBuf>,
    snapshot: Option<(Instant, BridgeEvidence)>,
    refreshing: bool,
}

#[derive(Clone, Default)]
struct InventoryCache(Arc<Mutex<CacheState>>);

static CACHE: LazyLock<InventoryCache> = LazyLock::new(InventoryCache::default);

impl InventoryCache {
    /// Return the latest short-lived snapshot immediately. At most one blocking refresh for this
    /// profile runs at a time; a slow filesystem never holds the agent request's gate.
    fn snapshot_or_refresh(
        &self,
        path: PathBuf,
        scan: impl FnOnce() -> BridgeEvidence + Send + 'static,
    ) -> Option<BridgeEvidence> {
        let Ok(mut state) = self.0.lock() else {
            return None;
        };
        if state.path.as_ref() != Some(&path) {
            *state = CacheState {
                path: Some(path.clone()),
                ..CacheState::default()
            };
        }
        let snapshot = state.snapshot.map(|(_, evidence)| evidence);
        let stale = state
            .snapshot
            .is_none_or(|(at, _)| at.elapsed() >= FRESHNESS);
        if stale && !state.refreshing {
            state.refreshing = true;
            let cache = self.clone();
            tokio::task::spawn_blocking(move || {
                let evidence = scan();
                if let Ok(mut state) = cache.0.lock() {
                    if state.path.as_ref() == Some(&path) {
                        state.snapshot = Some((Instant::now(), evidence));
                        state.refreshing = false;
                    }
                }
            });
        }
        snapshot
    }
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
    for entry in entries.take(MAX_ENTRIES).flatten() {
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

pub fn check(profile: &Profile) -> ReadinessCheck {
    if !cfg!(target_os = "macos") {
        return CURSOR_UNSUPPORTED;
    }
    let dir = bridge_dir(profile);
    let home_app = profile.home.join("Applications/Cursor.app");
    check_with(true, dir.clone(), &CACHE, move || {
        probe(&dir, &home_app, Path::new("/Applications/Cursor.app"))
    })
}

fn probe(dir: &Path, home_app: &Path, system_app: &Path) -> BridgeEvidence {
    let files = inventory(dir);
    BridgeEvidence {
        files,
        editor_app: files == BridgeFiles::Live || home_app.is_dir() || system_app.is_dir(),
    }
}

fn check_with(
    macos: bool,
    path: PathBuf,
    cache: &InventoryCache,
    scan: impl FnOnce() -> BridgeEvidence + Send + 'static,
) -> ReadinessCheck {
    if !macos {
        return CURSOR_UNSUPPORTED;
    }
    evaluate(true, cache.snapshot_or_refresh(path, scan))
}

fn evaluate(macos: bool, evidence: Option<BridgeEvidence>) -> ReadinessCheck {
    if !macos {
        CURSOR_UNSUPPORTED
    } else if evidence.is_some_and(|evidence| evidence.files == BridgeFiles::Live) {
        ReadinessCheck::Ok
    } else if evidence.is_none_or(|evidence| evidence.editor_app) {
        // An unscanned bridge stays provisional until the background refresh completes.
        BRIDGE_NOT_RUNNING
    } else {
        CURSOR_UNSUPPORTED
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::symlink;

    #[test]
    fn cursor_fallback_messages_are_plain_and_private() {
        for (check, expected_code) in [
            (BRIDGE_NOT_RUNNING, "cursor.bridge_not_running"),
            (CURSOR_UNSUPPORTED, "cursor.unsupported"),
        ] {
            let (code, message) = match check {
                ReadinessCheck::NeedsAttention(code, message)
                | ReadinessCheck::Unsupported(code, message) => (code, message),
                ReadinessCheck::Ok => panic!("non-OK reason expected"),
            };
            assert_eq!(code, expected_code);
            assert!(message.ends_with('.'));
            assert!(!message.chars().any(|c| matches!(c, '/' | '\\' | '~')));
            assert!(!message.chars().any(|c| c.is_ascii_digit()));
            let lower = message.to_ascii_lowercase();
            for private_word in ["home", "token", "secret", "password", "bearer"] {
                assert!(!lower.contains(private_word));
            }
            assert!(!message
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|word| word.len() >= 40));
        }
    }

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
    fn running_not_running_and_unsupported_are_distinct() {
        let live = Some(BridgeEvidence {
            files: BridgeFiles::Live,
            editor_app: false,
        });
        assert_eq!(evaluate(true, live), ReadinessCheck::Ok);
        for bridge in [BridgeFiles::Missing, BridgeFiles::Stale] {
            assert_eq!(
                evaluate(
                    true,
                    Some(BridgeEvidence {
                        files: bridge,
                        editor_app: true
                    })
                ),
                BRIDGE_NOT_RUNNING
            );
        }
        assert_eq!(evaluate(false, live), CURSOR_UNSUPPORTED);
        assert_eq!(
            evaluate(
                true,
                Some(BridgeEvidence {
                    files: BridgeFiles::Missing,
                    editor_app: false
                })
            ),
            CURSOR_UNSUPPORTED
        );
        assert_eq!(evaluate(true, None), BRIDGE_NOT_RUNNING);
    }

    #[tokio::test]
    async fn user_local_app_and_live_bridge_override_selected_cli_only_evidence() {
        let root = env::temp_dir().join(format!(
            "sidevoice-cursor-user-app-{}",
            uuid::Uuid::new_v4()
        ));
        let home_app = root.join("Applications/Cursor.app");
        let bridge = root.join(".cursor/desktop-bridge");
        fs::create_dir_all(&home_app).unwrap();
        fs::create_dir_all(&bridge).unwrap();
        let _selected_cli = root.join("bin/cursor-agent");
        let socket = root.join("bridge.sock");
        fs::write(&socket, b"").unwrap();
        fs::write(bridge.join("one.json"), serde_json::to_vec(&json!({
            "protocolVersion":1,"pid":std::process::id(),"socketPath":socket,"token":"fixture-secret"
        })).unwrap()).unwrap();
        let system_app = root.join("no-system-app");
        let cache = InventoryCache::default();
        let probe_bridge = bridge.clone();
        let probe_app = home_app.clone();
        let probe_system = system_app.clone();
        assert_eq!(
            check_with(true, bridge.clone(), &cache, move || probe(
                &probe_bridge,
                &probe_app,
                &probe_system
            )),
            BRIDGE_NOT_RUNNING
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if check_with(true, bridge.clone(), &cache, || {
                    panic!("fresh cache rescanned")
                }) == ReadinessCheck::Ok
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        fs::remove_file(&socket).unwrap();
        let stopped = probe(&bridge, &home_app, &system_app);
        assert_eq!(stopped.files, BridgeFiles::Stale);
        assert_eq!(evaluate(true, Some(stopped)), BRIDGE_NOT_RUNNING);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn slow_refresh_never_blocks_runtime_deadline_or_cancellation_and_non_mac_does_not_scan()
    {
        use std::sync::mpsc;
        let cache = InventoryCache::default();
        let path = PathBuf::from("/unused-fixture-key");
        assert_eq!(
            check_with(false, path.clone(), &cache, || panic!("non-Mac scanned")),
            CURSOR_UNSUPPORTED
        );
        assert!(cache.0.lock().unwrap().path.is_none());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let pending = tokio::time::timeout(Duration::from_millis(50), async {
            check_with(true, path.clone(), &cache, move || {
                let _ = started_tx.send(());
                release_rx.recv().unwrap();
                BridgeEvidence {
                    files: BridgeFiles::Live,
                    editor_app: false,
                }
            })
        })
        .await
        .unwrap();
        assert_eq!(pending, BRIDGE_NOT_RUNNING);
        started_rx.await.unwrap();
        assert_eq!(
            check_with(true, path.clone(), &cache, || panic!("duplicate refresh")),
            pending
        );
        let heartbeat = tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            1
        });
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(50), heartbeat)
                .await
                .unwrap()
                .unwrap(),
            1
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(10), std::future::pending::<()>())
                .await
                .is_err()
        );
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        let request = tokio::spawn(async move {
            tokio::select! { _ = cancel_rx => true, _ = tokio::time::sleep(Duration::from_secs(1)) => false }
        });
        cancel_tx.send(()).unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(50), request)
            .await
            .unwrap()
            .unwrap());
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if check_with(true, path.clone(), &cache, || {
                    panic!("fresh cache rescanned")
                }) == ReadinessCheck::Ok
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
}
