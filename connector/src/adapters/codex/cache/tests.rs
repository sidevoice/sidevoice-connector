use super::*;
use crate::adapters::codex::{deliver_with_cache, HOME};
use crate::platform::env::{Env, Environment};
use crate::platform::fs::{Files, Fs};
use crate::platform::target::{Host, Target};
use serde_json::json;
use std::fs;

#[ctor::ctor(unsafe)]
fn play_codex() {
    crate::test_support::stand_in::serve(|name, part, args| {
        if name != "codex-version-cache" {
            return None;
        }
        use std::io::Write;
        let mut record = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(part["record"].as_str().unwrap())
            .unwrap();
        writeln!(
            record,
            "{}",
            args.first().map(String::as_str).unwrap_or_default()
        )
        .unwrap();
        if args == ["--version"] {
            if let Some(release) = part["release"].as_str() {
                while !Path::new(release).exists() {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            if part["probe_fail"] == true {
                return Some(1);
            }
            println!("codex-cli {}", part["version"].as_str().unwrap());
            return Some(0);
        }
        Some(i32::from(
            Path::new(part["fail"].as_str().unwrap()).exists(),
        ))
    });
}

struct Fixture {
    dir: tempfile::TempDir,
    profile: Profile,
    cache: Cache,
}

impl Fixture {
    fn new() -> Self {
        let dir = crate::test_support::temp_dir("codex-versions");
        let profile = Profile::from_vars(
            |key| match key {
                Environment::HOME => Some(dir.path().as_os_str().to_owned()),
                "SIDEVOICE_DATA_DIR" => Some(dir.path().join("data").into_os_string()),
                _ => None,
            },
            &[HOME],
        )
        .unwrap();
        Files::ensure_private_dir(&profile.data).unwrap();
        let socket = steer::socket_path(profile.agent_home(HOME.var));
        Files::ensure_private_dir(socket.parent().unwrap()).unwrap();
        fs::write(socket, "").unwrap(); // A stale socket falls back to queue, without a protocol error.
        let fixture = Self {
            dir,
            profile,
            cache: Cache::new(),
        };
        fixture.path(&[fixture.dir.path()]);
        fixture
    }

    fn path(&self, paths: &[&Path]) {
        Files::write_json(&self.profile.data.join("agents.json"), &json!({"version":1,"login_path":std::env::join_paths(paths).unwrap().to_string_lossy()})).unwrap();
    }

    fn program(&self, dir: &Path, version: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let program = dir.join(Host::exe_name("codex"));
        crate::test_support::stand_in::make(
            &program,
            &json!({
                "play":"codex-version-cache", "version":version,
                "record":self.dir.path().join("calls"), "fail":self.dir.path().join("fail")
            }),
        );
        program
    }

    async fn deliver(&self) -> Result<serde_json::Value> {
        deliver_with_cache(
            &json!({"kind":"codex-queue","thread":"thread"}),
            &json!({"session_id":"session","revision":1,"epoch":1,"message_id":"message","text":"hello"}),
            &self.profile, &self.cache,
        ).await
    }

    fn counts(&self) -> (usize, usize) {
        let calls = fs::read_to_string(self.dir.path().join("calls")).unwrap();
        (
            calls.lines().filter(|line| *line == "--version").count(),
            calls.lines().filter(|line| *line == "queue").count(),
        )
    }
}

#[tokio::test]
async fn second_delivery_reuses_version_but_runs_queue() {
    let fixture = Fixture::new();
    fixture.program(fixture.dir.path(), "0.157.0");
    fixture.deliver().await.unwrap();
    fixture.deliver().await.unwrap();
    assert_eq!(fixture.counts(), (1, 2));
}

#[tokio::test]
async fn newly_installed_earlier_path_entry_is_resolved_on_next_delivery() {
    let fixture = Fixture::new();
    let earlier = fixture.dir.path().join("earlier");
    fixture.path(&[&earlier, fixture.dir.path()]);
    fixture.program(fixture.dir.path(), "0.157.0");
    fixture.deliver().await.unwrap();
    fixture.program(&earlier, "0.158.0");
    fixture.deliver().await.unwrap();
    assert_eq!(fixture.counts(), (2, 2));
    assert_eq!(
        fixture.cache.0.lock().unwrap().as_ref().unwrap().1,
        (0, 158, 0)
    );
}

#[tokio::test]
async fn changed_agents_record_invalidates_the_version_and_refreshes_path() {
    let fixture = Fixture::new();
    fixture.program(fixture.dir.path(), "0.157.0");
    fixture.deliver().await.unwrap();
    // Even an update resolving to the same binary invalidates the version.
    fixture.path(&[fixture.dir.path(), &fixture.dir.path().join("other")]);
    fixture.deliver().await.unwrap();
    assert_eq!(fixture.counts(), (2, 2));
    let other = fixture.dir.path().join("new-install");
    fixture.program(&other, "0.158.0");
    fixture.path(&[&other]);
    fixture.deliver().await.unwrap();
    assert_eq!(fixture.counts(), (3, 3));
}

#[tokio::test]
async fn replacement_with_same_size_and_mtime_is_probed_again() {
    let fixture = Fixture::new();
    let program = fixture.program(fixture.dir.path(), "0.157.0");
    fixture.deliver().await.unwrap();
    let before = fs::metadata(&program).unwrap();
    // Keep both files alive before replacement so the inode/file index cannot be reused.
    // Windows may preserve the old creation time when the replacement is renamed.
    let replacement = fixture.program(&fixture.dir.path().join("staging"), "0.158.0");
    fs::File::options()
        .write(true)
        .open(&replacement)
        .unwrap()
        .set_modified(before.modified().unwrap())
        .unwrap();
    assert_eq!(fs::metadata(&replacement).unwrap().len(), before.len());
    fs::remove_file(&program).unwrap();
    fs::rename(replacement, &program).unwrap();
    assert_eq!(
        fs::metadata(&program).unwrap().modified().unwrap(),
        before.modified().unwrap()
    );
    fixture.deliver().await.unwrap();
    assert_eq!(fixture.counts(), (2, 2));
    assert_eq!(
        fixture.cache.0.lock().unwrap().as_ref().unwrap().1,
        (0, 158, 0)
    );
}

#[tokio::test]
async fn failed_delivery_discards_a_previously_valid_version() {
    let fixture = Fixture::new();
    fixture.program(fixture.dir.path(), "0.157.0");
    fixture.deliver().await.unwrap();
    assert!(fixture.cache.0.lock().unwrap().is_some());
    fs::write(fixture.dir.path().join("fail"), "").unwrap();
    assert!(fixture.deliver().await.is_err());
    assert!(fixture.cache.0.lock().unwrap().is_none());
    assert_eq!(fixture.counts(), (1, 2)); // Probe succeeded before the failure and was reused.
    fs::remove_file(fixture.dir.path().join("fail")).unwrap();
    fixture.deliver().await.unwrap();
    assert_eq!(fixture.counts(), (2, 3));
}

#[tokio::test]
async fn unsuccessful_and_unparseable_probes_are_retried() {
    let fixture = Fixture::new();
    let program = fixture.program(fixture.dir.path(), "unknown");
    for _ in 0..2 {
        fixture.deliver().await.unwrap();
    }
    assert_eq!(fixture.counts(), (2, 2));
    crate::test_support::stand_in::make(
        &program,
        &json!({
            "play":"codex-version-cache", "version":"0.157.0", "probe_fail":true,
            "record":fixture.dir.path().join("calls"), "fail":fixture.dir.path().join("fail")
        }),
    );
    for _ in 0..2 {
        fixture.deliver().await.unwrap();
    }
    assert_eq!(fixture.counts(), (4, 4));
    assert!(fixture.cache.0.lock().unwrap().is_none());
}

#[tokio::test]
async fn slow_probe_does_not_hold_the_cache_mutex() {
    use std::sync::Arc;
    let fixture = Fixture::new();
    let program = fixture.program(fixture.dir.path(), "0.157.0");
    let release = fixture.dir.path().join("release");
    crate::test_support::stand_in::make(
        &program,
        &json!({
            "play":"codex-version-cache", "version":"0.157.0", "release":release,
            "record":fixture.dir.path().join("calls"), "fail":fixture.dir.path().join("fail")
        }),
    );
    let key = Key::read(&program, &fixture.profile);
    let cache = Arc::new(Cache::new());
    let worker_cache = cache.clone();
    let worker = tokio::spawn(async move {
        worker_cache
            .version(&key, tokio::process::Command::new(program))
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !fixture.dir.path().join("calls").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let available = cache.0.try_lock().is_ok();
    fs::write(release, "").unwrap();
    assert_eq!(worker.await.unwrap().unwrap(), (0, 157, 0));
    assert!(available, "probe held the global cache lock");
}

#[tokio::test]
async fn version_and_protocol_steer_failures_discard_cached_versions() {
    let fixture = Fixture::new();
    let program = fixture.program(fixture.dir.path(), "0.157.0");
    let key = Key::read(&program, &fixture.profile);
    for error in [
        anyhow::Error::new(steer::Failure::VersionTooOld),
        anyhow::Error::new(steer::Failure::VersionUnknown),
        anyhow::Error::new(steer::Failure::Rpc {
            method: "turn/steer",
            code: Some(-32601),
        }),
        anyhow::Error::new(steer::Failure::InvalidReply),
        anyhow::Error::new(tokio_tungstenite::tungstenite::Error::Protocol(
            tokio_tungstenite::tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
        )),
    ] {
        fixture
            .cache
            .version(&key, tokio::process::Command::new(&program))
            .await
            .unwrap();
        assert!(fixture.cache.0.lock().unwrap().is_some());
        fixture.cache.steer_failed(&key, &error);
        assert!(fixture.cache.0.lock().unwrap().is_none());
    }
    assert_eq!(fixture.counts(), (5, 0));
}
