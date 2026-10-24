//! Per-process delivery command and version. Only successful resolutions and version probes are retained.
use super::{run, steer, HOME};
use crate::agents::AgentCommand;
use crate::profile::Profile;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::sync::Mutex;

pub(super) static CACHE: Mutex<Cache> = Mutex::const_new(Cache(None));

#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    real_path: PathBuf,
    len: u64,
    modified: SystemTime,
}

impl Stamp {
    fn read(path: &Path) -> Option<Self> {
        let real_path = fs::canonicalize(path).ok()?;
        let meta = fs::metadata(&real_path).ok()?;
        Some(Self {
            real_path,
            len: meta.len(),
            modified: meta.modified().ok()?,
        })
    }
}

#[derive(Clone)]
pub(super) struct CachedCommand {
    program: String,
    path: Option<String>,
    pub version: Option<(u64, u64, u64)>,
    home: PathBuf,
    data: PathBuf,
    state: Option<Stamp>,
    binary: Option<Stamp>,
}

impl CachedCommand {
    pub fn command(&self, profile: &Profile) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.program);
        if let Some(path) = &self.path {
            command.env("PATH", path);
        }
        command.env(HOME.var, profile.agent_home(HOME.var));
        command
    }

    fn current(&self, profile: &Profile) -> bool {
        self.home == profile.home
            && self.data == profile.data
            && self.state == Stamp::read(&profile.data.join("agents.json"))
            && self.binary.is_some()
            && self.binary == Stamp::read(Path::new(&self.program))
    }
}

pub(super) struct Cache(Option<CachedCommand>);

impl Cache {
    pub fn invalidate(&mut self) {
        self.0 = None;
    }

    fn resolve(
        &mut self,
        profile: &Profile,
        resolve: impl FnOnce() -> AgentCommand,
    ) -> Result<&mut CachedCommand> {
        if !self.0.as_ref().is_some_and(|entry| entry.current(profile)) {
            self.invalidate();
            let command = resolve();
            let program = command
                .program
                .context("Codex not on the captured login PATH")?;
            self.0 = Some(CachedCommand {
                binary: Stamp::read(Path::new(&program)),
                program,
                path: command.path,
                version: None,
                home: profile.home.clone(),
                data: profile.data.clone(),
                state: Stamp::read(&profile.data.join("agents.json")),
            });
        }
        Ok(self.0.as_mut().expect("resolved command"))
    }

    pub async fn prepare(&mut self, profile: &Profile, probe: bool) -> Result<CachedCommand> {
        self.prepare_with(profile, probe, || {
            crate::agents::agent_command(profile, "codex")
        })
        .await
    }

    async fn prepare_with(
        &mut self,
        profile: &Profile,
        probe: bool,
        resolve: impl FnOnce() -> AgentCommand,
    ) -> Result<CachedCommand> {
        let entry = self.resolve(profile, resolve)?;
        if probe && entry.version.is_none() {
            let mut command = entry.command(profile);
            command.arg("--version");
            if let Ok((status, stdout)) = run(command, true, Duration::from_secs(10)).await {
                if status.success() {
                    entry.version = steer::parse_version(&String::from_utf8_lossy(&stdout));
                }
            }
        }
        Ok(entry.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::env::{Env, Environment};
    use crate::platform::target::{Host, Target};
    use crate::test_support::stand_in::Script;
    use std::cell::Cell;

    #[tokio::test]
    async fn deliveries_reuse_resolution_and_version_until_the_binary_changes() {
        let dir = crate::test_support::temp_dir("codex-delivery-cache");
        let profile = Profile::from_vars(
            |key| match key {
                Environment::HOME => Some(dir.path().as_os_str().to_owned()),
                "SIDEVOICE_DATA_DIR" => Some(dir.path().join("data").into_os_string()),
                _ => None,
            },
            &[HOME],
        )
        .unwrap();
        let program = dir.path().join(Host::exe_name("codex"));
        let calls = dir.path().join("calls");
        let install = |version: &str, exit| {
            Script {
                print: format!("codex-cli {version}\n"),
                record: Some(calls.clone()),
                append: true,
                exit,
                ..Default::default()
            }
            .make(&program);
        };
        let resolutions = Cell::new(0);
        let resolve = || {
            resolutions.set(resolutions.get() + 1);
            AgentCommand {
                program: Some(program.to_string_lossy().into_owned()),
                path: None,
            }
        };
        let mut cache = Cache(None);
        install("0.157.0", 0);
        for _ in 0..2 {
            assert_eq!(
                cache
                    .prepare_with(&profile, true, resolve)
                    .await
                    .unwrap()
                    .version,
                Some((0, 157, 0))
            );
        }
        assert_eq!(resolutions.get(), 1);
        assert_eq!(fs::read_to_string(&calls).unwrap(), "--version\n");

        // A different size guarantees invalidation even on a filesystem with coarse mtimes.
        install("0.1000.0", 0);
        assert_eq!(
            cache
                .prepare_with(&profile, true, resolve)
                .await
                .unwrap()
                .version,
            Some((0, 1000, 0))
        );
        assert_eq!(resolutions.get(), 2);
        assert_eq!(fs::read_to_string(&calls).unwrap().lines().count(), 2);

        // A failed delivery discards both resolution and capability.
        cache.invalidate();
        cache.prepare_with(&profile, true, resolve).await.unwrap();
        assert_eq!(resolutions.get(), 3);
        assert_eq!(fs::read_to_string(&calls).unwrap().lines().count(), 3);

        // A failing probe is retried even when the executable is unchanged.
        install("0.158.0", 1);
        for _ in 0..2 {
            assert_eq!(
                cache
                    .prepare_with(&profile, true, resolve)
                    .await
                    .unwrap()
                    .version,
                None
            );
        }
        assert_eq!(fs::read_to_string(&calls).unwrap().lines().count(), 5);

        install("unknown", 0);
        for _ in 0..2 {
            assert_eq!(
                cache
                    .prepare_with(&profile, true, resolve)
                    .await
                    .unwrap()
                    .version,
                None
            );
        }
        assert_eq!(fs::read_to_string(&calls).unwrap().lines().count(), 7);
        cache.invalidate();
        for _ in 0..2 {
            assert!(cache
                .resolve(&profile, || AgentCommand {
                    program: None,
                    path: None
                })
                .is_err());
        }
        assert!(cache.0.is_none());
    }
}

#[cfg(test)]
mod delivery_tests {
    use super::*;
    use crate::platform::env::{Env, Environment};
    use crate::platform::fs::{Files, Fs};
    use crate::platform::target::{Host, Target};
    use crate::test_support::stand_in::Script;
    use serde_json::json;

    #[tokio::test]
    async fn second_delivery_skips_version_and_replaced_binary_is_probed_again() {
        let dir = crate::test_support::temp_dir("codex-cached-deliveries");
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
        Files::write_json(
            &profile.data.join("agents.json"),
            &json!({"version":1,"login_path":dir.path()}),
        )
        .unwrap();
        let socket = steer::socket_path(profile.agent_home(HOME.var));
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        // A stale socket makes steer fall back immediately; queue remains a real per-message CLI call.
        fs::write(socket, "").unwrap();
        let program = dir.path().join(Host::exe_name("codex"));
        let calls = dir.path().join("calls");
        let install = |version: &str, exit| {
            Script {
                print: format!("codex-cli {version}\n"),
                record: Some(calls.clone()),
                append: true,
                exit,
                ..Default::default()
            }
            .make(&program);
        };
        let route = json!({"kind":"codex-queue","thread":"thread"});
        let event = json!({"session_id":"session","revision":1,"epoch":1,"message_id":"message","text":"hello"});
        let counts = || {
            let calls = fs::read_to_string(&calls).unwrap();
            (
                calls.lines().filter(|line| *line == "--version").count(),
                calls.lines().filter(|line| *line == "queue").count(),
            )
        };
        install("0.157.0", 0);
        for _ in 0..2 {
            super::super::deliver(&route, &event, &profile)
                .await
                .unwrap();
        }
        assert_eq!(counts(), (1, 2));
        install("0.1000.0", 0);
        super::super::deliver(&route, &event, &profile)
            .await
            .unwrap();
        assert_eq!(counts(), (2, 3));
        // Failing a delivery clears its otherwise valid cached version.
        install("0.158.0", 1);
        assert!(super::super::deliver(&route, &event, &profile)
            .await
            .is_err());
        assert!(CACHE.lock().await.0.is_none());
        install("0.159.0", 0);
        super::super::deliver(&route, &event, &profile)
            .await
            .unwrap();
        assert_eq!(counts(), (4, 5));
    }
}
