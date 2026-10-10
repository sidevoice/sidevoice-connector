//! `install` and `uninstall`: this package on this computer, and in front of the agents on it.
//!
//! The installation (sidevoice/sidevoice-connector#66, decision 3):
//!
//! | What | Where |
//! |---|---|
//! | `R/releases/<id>/` | one release per version: `bin/sidevoice-connector`, a copy of the installing binary, and `core/`, the core the package carries, staged and self-tested (`core_package.rs`) |
//! | `R/current` | a link to the selected release, switched by renaming a new link over it |
//! | `D/install.json` | `command`: `[R/current/bin/sidevoice-connector]`, what the service jobs, the desktop app and every agent registration run, never the package's own path (npm's cache is not stable); `releases`: `R` |
//!
//! `<id>` is the version; another build of a version already there (a nightly, a local build) is
//! `<version>+<the first 12 hex digits of its binary's digest>`.
//!
//! `install`, under the install lock: an installation made by an earlier installer is refused, with the one command
//! that removes it (decision 1: no upgrade in place); the release is staged unless it is there already; `current` is
//! switched to it and `install.json` written; then both jobs are defined and restarted (with a service manager), or
//! what runs on demand is stopped and the connector started from the new release (without one), and the pair must
//! answer within 60 s: the core's health, for a launch after the restart, and the connector's identity, naming this
//! release's binary, and its `node.status`. When it does not, `current` goes back to the release it named before,
//! that is restarted and verified, and the failed release is deleted; a first installation that does not answer is
//! left in place, said with the core's own failure. Releases other than the current one and the one before it are
//! pruned (exactly one previous), and the agents found are registered with the installation's command. Running it
//! again is the recovery.
//!
//! `uninstall`, under the install lock: both jobs unloaded and what runs on demand stopped (a refusal stops
//! everything here, with nothing deleted), the agents' registrations removed, then `R` and everything in `D` but
//! its permanent lock files and the stop, so that a launcher on its way finds the stop and does not serve.

use crate::agents::HostAgents;
use crate::messages::Keyed;
use crate::profile::Profile;
use crate::secure_fs::{atomic_json, digest_file, ensure_private_dir};
use crate::service::layout::Layout;
use crate::service::manager::Kind;
use crate::service::{launcher, status};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::time::{sleep, timeout, Duration, Instant};

/// The connector's program inside a package and inside a release.
pub const BINARY: &str = "bin/sidevoice-connector";
/// How long the pair has to answer after a restart.
const VERIFY_LIMIT: Duration = Duration::from_secs(60);
/// The lock files and the stop `uninstall` leaves in `D`.
const KEPT: &[&str] = &[
    "install.lock",
    "connector.lock",
    "agents.lock",
    "node-stopped.json",
];

/// The package being installed: its root (`bin/`, `connector.json`, `core/`), its binary and the version it is.
#[derive(Clone, Debug)]
pub struct Package {
    pub root: PathBuf,
    pub binary: PathBuf,
    pub version: String,
}

impl Package {
    /// The package this binary is part of.
    pub fn running() -> Result<Self> {
        let root = crate::core_package::package_root()?;
        Ok(Self {
            binary: std::env::current_exe()?.canonicalize()?,
            root,
            version: crate::identity::VERSION.to_owned(),
        })
    }
}

/// What restarts and verifies an installation, and unloads its jobs: this machine's service manager and processes,
/// or (tests) a stand-in.
#[derive(Clone)]
pub enum Runtime {
    Machine,
    #[cfg(test)]
    Fake(std::sync::Arc<std::sync::Mutex<tests::Fake>>),
}

fn keyed(key: &'static str, params: Value) -> anyhow::Error {
    Keyed::new(key, params).into()
}

fn failed(detail: impl std::fmt::Display) -> anyhow::Error {
    keyed("install.failed", json!({"detail": detail.to_string()}))
}

/// A failure as `{key, message, …}`: what a refusal carries about why the pair did not answer.
fn failure_value(failure: &crate::service::Failure) -> Value {
    failure.value()["error"].clone()
}

fn cause_text(failure: &Value) -> String {
    failure
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| failure.get("key").and_then(Value::as_str))
        .unwrap_or("no answer")
        .to_owned()
}

/// `R/current/bin/sidevoice-connector`: the installation's command.
pub fn stable_command(layout: &Layout) -> PathBuf {
    layout.current().join(BINARY)
}

fn releases_dir(layout: &Layout) -> PathBuf {
    layout.releases.join("releases")
}

/// The release `R/current` names, by its directory name, when it is one of `R/releases`.
fn current_release(layout: &Layout) -> Option<String> {
    let target = fs::read_link(layout.current()).ok()?;
    let mut parts = target.components();
    if parts.next()?.as_os_str() != "releases" {
        return None;
    }
    let id = parts.next()?.as_os_str().to_str()?.to_owned();
    (parts.next().is_none() && releases_dir(layout).join(&id).is_dir()).then_some(id)
}

/// `R/current` names `releases/<id>`: a new link renamed over it, then `R` synced.
fn point_current(layout: &Layout, id: &str) -> Result<()> {
    let temporary = layout
        .releases
        .join(format!(".current-{}.tmp", uuid::Uuid::new_v4().simple()));
    std::os::unix::fs::symlink(Path::new("releases").join(id), &temporary)?;
    if let Err(error) = fs::rename(&temporary, layout.current()) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    File::open(&layout.releases)?.sync_all()?;
    Ok(())
}

/// `D/install.json`: the installation's command and where its releases are.
fn record(layout: &Layout) -> Result<()> {
    ensure_private_dir(&layout.data)?;
    atomic_json(
        &layout.install_file(),
        &json!({"command": [stable_command(layout)], "releases": layout.releases}),
    )
}

/// What a dead installer left: partial releases and temporary links.
fn remove_leftovers(layout: &Layout) {
    let leftovers = |dir: &Path, prefix: &str| {
        fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
            .map(|entry| entry.path())
            .collect::<Vec<_>>()
    };
    for path in leftovers(&releases_dir(layout), ".") {
        let _ = fs::remove_dir_all(&path).or_else(|_| fs::remove_file(&path));
    }
    for path in leftovers(&layout.releases, ".current-") {
        let _ = fs::remove_file(path);
    }
}

fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 64
        && !version.starts_with('.')
        && version
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ".+-_".contains(ch))
}

/// A release that is all there: its binary and its core's program.
fn complete(release: &Path) -> bool {
    release.join(BINARY).is_file()
        && release
            .join(crate::service::layout::CORE_ENTRYPOINT)
            .is_file()
}

/// Whether the release's core is still the one `package` carries, file by file (`core_package::staged_intact`).
fn core_intact(release: &Path, package: &Package) -> bool {
    crate::core_package::packaged_as(&package.root, &package.version).is_ok_and(|core| {
        crate::core_package::staged_intact(
            &core,
            &release.join(crate::service::layout::CORE_DIRECTORY),
        )
        .is_ok()
    })
}

/// Which release this package is: `<version>`, unless another build holds that name; then
/// `<version>+<digest prefix>`. Whether it still has to be staged. A release of this build is reused only whole, its
/// core as its inventory says; a damaged one that is selected stays until its replacement, staged under the next
/// name, is in place.
fn release_id(layout: &Layout, package: &Package, digest: &str) -> Result<(String, bool)> {
    if !valid_version(&package.version) {
        return Err(failed(format!(
            "{:?} is not a version a release can be named after",
            package.version
        )));
    }
    let current = current_release(layout);
    let names = [
        package.version.clone(),
        format!("{}+{}", package.version, &digest[..12]),
    ];
    let same = |id: &str| {
        digest_file(&releases_dir(layout).join(id).join(BINARY))
            .ok()
            .as_deref()
            == Some(digest)
    };
    if let Some(whole) = names.iter().find(|id| {
        let release = releases_dir(layout).join(id);
        same(id) && complete(&release) && core_intact(&release, package)
    }) {
        return Ok((whole.clone(), false));
    }
    for id in names {
        let release = releases_dir(layout).join(&id);
        if fs::symlink_metadata(&release).is_err() {
            return Ok((id, true));
        }
        let same = same(&id);
        let selected = current.as_deref() == Some(id.as_str());
        if same && selected {
            continue;
        }
        if same || !selected {
            // Ours but incomplete, or another build that is not selected: staged again.
            fs::remove_dir_all(&release)?;
            return Ok((id, true));
        }
    }
    Err(failed(
        "the selected release is another build of this version; run `sidevoice uninstall`, then install again",
    ))
}

/// Copies `from` to `to` (created, 0700) and syncs it.
fn copy_executable(from: &Path, to: &Path) -> Result<()> {
    let mut source = File::open(from).with_context(|| format!("open {}", from.display()))?;
    let mut target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .custom_flags(libc::O_NOFOLLOW)
        .open(to)
        .with_context(|| format!("create {}", to.display()))?;
    io::copy(&mut source, &mut target)?;
    target.sync_all()?;
    Ok(())
}

/// The staged binary answers `--version --json` with the package's version.
async fn check_binary(binary: &Path, version: &str) -> Result<()> {
    let mut attempts = 0;
    let output = loop {
        let mut command = tokio::process::Command::new(binary);
        command
            .args(["--version", "--json"])
            .stdin(Stdio::null())
            .kill_on_drop(true);
        match timeout(Duration::from_secs(30), command.output()).await {
            Err(_) => return Err(failed("the connector did not say its version within 30 s")),
            // A program just written can be "busy" while another thread's fork still holds it open.
            Ok(Err(error)) if error.raw_os_error() == Some(libc::ETXTBSY) && attempts < 20 => {
                attempts += 1;
                sleep(Duration::from_millis(50)).await;
            }
            Ok(result) => break result.with_context(|| format!("run {}", binary.display()))?,
        }
    };
    let said: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    if !output.status.success() || said["version"] != version {
        return Err(failed(format!(
            "the staged connector says {} instead of version {version}",
            String::from_utf8_lossy(&output.stdout).trim()
        )));
    }
    Ok(())
}

/// Stages the package as `R/releases/<id>`: built beside it, then renamed into place, so a release is there whole
/// or not at all.
async fn stage(layout: &Layout, package: &Package, id: &str, digest: &str) -> Result<()> {
    let releases = releases_dir(layout);
    ensure_private_dir(&releases)?;
    let partial = releases.join(format!(".{id}.partial-{}", uuid::Uuid::new_v4().simple()));
    fs::DirBuilder::new().mode(0o700).create(&partial)?;
    let result = async {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(partial.join("bin"))?;
        let binary = partial.join(BINARY);
        copy_executable(&package.binary, &binary)?;
        crate::secure_fs::safe_executable(&binary)?;
        if digest_file(&binary)? != digest {
            return Err(failed("the connector changed while it was being copied"));
        }
        check_binary(&binary, &package.version).await?;
        let (root, release, version) = (
            package.root.clone(),
            partial.clone(),
            package.version.clone(),
        );
        tokio::task::spawn_blocking(move || {
            crate::core_package::install_as(&root, &release, &version)
        })
        .await
        .map_err(failed)??;
        fs::rename(&partial, releases.join(id))?;
        File::open(&releases)?.sync_all()?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_dir_all(&partial);
    }
    result
}

/// Every release but `keep` deleted.
fn prune(layout: &Layout, keep: &[&str]) -> Vec<String> {
    let mut pruned = Vec::new();
    for entry in fs::read_dir(releases_dir(layout))
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        if keep.contains(&name.as_str()) {
            continue;
        }
        if fs::remove_dir_all(entry.path()).is_ok() {
            pruned.push(name);
        }
    }
    pruned
}

/// The environment a connector started on demand gets: where this installation lives.
fn connector_environment(layout: &Layout) -> Vec<(String, PathBuf)> {
    let mut environment = vec![
        ("HOME".to_owned(), layout.home.clone()),
        ("SIDEVOICE_DATA_DIR".to_owned(), layout.data.clone()),
        ("XDG_DATA_HOME".to_owned(), layout.data_home.clone()),
        ("XDG_CONFIG_HOME".to_owned(), layout.config_home.clone()),
    ];
    for (name, dir) in &layout.agent_dirs {
        environment.push((name.clone(), dir.clone()));
    }
    environment
}

/// The connector on the socket is `release`'s binary.
async fn runs_release(layout: &Layout, binary: &Path) -> bool {
    let Some(identity) =
        launcher::ask_connector(layout, "identity", json!({}), Duration::from_secs(1)).await
    else {
        return false;
    };
    let expected = binary.canonicalize().ok();
    let running = identity
        .get("executable")
        .and_then(Value::as_str)
        .and_then(|path| Path::new(path).canonicalize().ok());
    expected.is_some() && running == expected
}

impl Runtime {
    /// The selection started: the person's stop cleared, then both jobs defined and restarted (a manager), or what
    /// runs on demand stopped (none). Which manager it was.
    async fn restart(&self, layout: &Layout) -> crate::service::Result<Kind> {
        match self {
            Runtime::Machine => {
                crate::service::clear_stop(layout)?;
                let _ = fs::remove_file(layout.core_failure());
                let kind = crate::service::manager::kind().await;
                if kind == Kind::None {
                    crate::service::stop_on_demand_or_fail(layout).await?;
                } else {
                    crate::service::define_and_start(layout, kind, true).await?;
                }
                Ok(kind)
            }
            #[cfg(test)]
            Runtime::Fake(fake) => {
                let selected = current_release(layout).unwrap_or_default();
                fake.lock().unwrap().restarts.push(selected);
                Ok(Kind::None)
            }
        }
    }

    /// The core serving for a launch other than `before` and the connector answering as `release`'s binary, with its
    /// `node.status`, within the limit; else why not.
    async fn verify(
        &self,
        layout: &Layout,
        kind: Kind,
        release: &Path,
        before: Option<String>,
    ) -> std::result::Result<(), Value> {
        match self {
            Runtime::Machine => {
                let binary = release.join(BINARY);
                if kind == Kind::None {
                    let fallback = vec![stable_command(layout).to_string_lossy().into_owned()];
                    launcher::ensure_connector(
                        layout,
                        fallback,
                        connector_environment(layout),
                        || runs_release(layout, &binary),
                    )
                    .await
                    .map_err(|failure| failure_value(&failure))?;
                }
                let deadline = Instant::now() + VERIFY_LIMIT;
                loop {
                    if let Some((ready, _)) =
                        status::serving(layout, Duration::from_millis(1500)).await
                    {
                        if Some(&ready.launch_id) != before.as_ref()
                            && runs_release(layout, &binary).await
                            && launcher::ask_connector(
                                layout,
                                "node.status",
                                json!({}),
                                Duration::from_secs(5),
                            )
                            .await
                            .is_some()
                        {
                            return Ok(());
                        }
                    }
                    let now = if kind == Kind::None {
                        // No job: what the core said about a failed start is the state.
                        match status::read_failure(layout, &crate::logfile::timestamp()) {
                            Some(failure) => json!({"state": "failed", "failure": failure}),
                            None => json!({"state": "starting"}),
                        }
                    } else {
                        status::status(layout, None).await
                    };
                    let state = now.get("state").and_then(Value::as_str).unwrap_or("");
                    if matches!(state, "failed" | "service-failed") && status::settled(&now) {
                        return Err(now
                            .get("failure")
                            .cloned()
                            .filter(Value::is_object)
                            .unwrap_or_else(|| json!({"key": state})));
                    }
                    if Instant::now() >= deadline {
                        return Err(now
                            .get("failure")
                            .cloned()
                            .filter(Value::is_object)
                            .unwrap_or_else(|| {
                                json!({"key": "ready.timeout",
                                    "message": crate::messages::message("ready.timeout", &Value::Null)})
                            }));
                    }
                    sleep(Duration::from_millis(250)).await;
                }
            }
            #[cfg(test)]
            Runtime::Fake(fake) => {
                let _ = (kind, before);
                let id = release
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if fake.lock().unwrap().failing.contains(&id) {
                    Err(json!({"key": "launch.exited", "message": "the core exited (test)"}))
                } else {
                    Ok(())
                }
            }
        }
    }

    /// The core's current launch, if one is ready.
    fn launch(&self, layout: &Layout) -> Option<String> {
        status::read_ready(layout).map(|ready| ready.launch_id)
    }

    /// Both jobs unloaded, what runs on demand stopped and both definitions deleted, the stop kept.
    async fn unload(&self, layout: &Layout) -> crate::service::Result<Value> {
        match self {
            Runtime::Machine => crate::service::uninstall_held(layout, true).await,
            #[cfg(test)]
            Runtime::Fake(fake) => {
                fake.lock().unwrap().unloaded = true;
                std::fs::write(layout.stop_marker(), b"{}")
                    .map_err(crate::service::Failure::plain)?;
                fs::set_permissions(
                    layout.stop_marker(),
                    std::os::unix::fs::PermissionsExt::from_mode(0o600),
                )
                .map_err(crate::service::Failure::plain)?;
                Ok(json!({"ok": true, "state": "absent", "service": "none"}))
            }
        }
    }
}

/// The selection restarted and verified: `Ok(kind)`, or why the pair did not answer.
async fn start_and_verify(
    runtime: &Runtime,
    layout: &Layout,
    id: &str,
) -> std::result::Result<Kind, Value> {
    let before = runtime.launch(layout);
    let kind = runtime
        .restart(layout)
        .await
        .map_err(|failure| failure_value(&failure))?;
    runtime
        .verify(layout, kind, &releases_dir(layout).join(id), before)
        .await?;
    Ok(kind)
}

/// What `install` and `uninstall` were asked.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Register with (or, uninstalling, remove from) the agents on this computer.
    pub agents: bool,
}

/// `install`: see the module's documentation. The answer is `install --json`'s.
pub async fn install(
    profile: &Profile,
    package: &Package,
    options: Options,
    runtime: &Runtime,
) -> Result<Value> {
    let layout = Layout::from_profile(profile);
    let lock = crate::service::install_lock(&layout).await?;
    ensure_private_dir(&layout.releases)?;
    remove_leftovers(&layout);
    let previous = current_release(&layout);
    let digest = digest_file(&package.binary)?;
    let (id, fresh) = release_id(&layout, package, &digest)?;
    if fresh {
        stage(&layout, package, &id, &digest).await?;
    }
    point_current(&layout, &id)?;
    record(&layout)?;
    let kind = match start_and_verify(runtime, &layout, &id).await {
        Ok(kind) => kind,
        Err(cause) => {
            let Some(back) = previous.clone().filter(|back| *back != id) else {
                return Err(keyed(
                    "install.verify-failed",
                    json!({"version": id, "cause": cause_text(&cause), "failure": cause}),
                ));
            };
            point_current(&layout, &back)?;
            let again = start_and_verify(runtime, &layout, &back).await;
            let _ = fs::remove_dir_all(releases_dir(&layout).join(&id));
            return Err(match again {
                Ok(_) => keyed(
                    "install.rolled-back",
                    json!({"version": id, "previous": back, "cause": cause_text(&cause), "failure": cause}),
                ),
                Err(back_cause) => keyed(
                    "install.rollback-failed",
                    json!({"version": id, "previous": back, "cause": cause_text(&cause), "failure": cause,
                        "back": cause_text(&back_cause)}),
                ),
            });
        }
    };
    let keep: Vec<&str> = [Some(id.as_str()), previous.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    let pruned = prune(&layout, &keep);
    let agents = if options.agents {
        register(profile).await
    } else {
        json!({})
    };
    drop(lock);
    let state = status::status(&layout, None).await;
    let action = match previous.as_deref() {
        None => "installed",
        Some(before) if before == id => "reinstalled",
        Some(_) => "updated",
    };
    let mut answer = json!({"ok": true, "action": action, "version": id, "previous": previous,
        "command": [stable_command(&layout)], "releases": layout.releases, "pruned": pruned,
        "service": kind.as_str(), "state": state.get("state").cloned().unwrap_or(Value::Null),
        "agents": agents, "paired": crate::pairing::previous_room(profile)});
    if kind == Kind::Systemd {
        answer["linger"] = crate::service::manager::linger().await;
    }
    answer["advice"] = json!(advice(profile, &answer));
    Ok(answer)
}

/// One agent's row of `agents.list`, its id and label.
fn agent_rows(listed: &Value) -> Vec<(String, String, Value)> {
    listed["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            Some((
                row["id"].as_str()?.to_owned(),
                row["label"].as_str().unwrap_or("?").to_owned(),
                row.clone(),
            ))
        })
        .collect()
}

fn error_message(answer: &Value) -> Option<(String, String)> {
    let error = answer.get("error")?;
    Some((
        error["key"]
            .as_str()
            .unwrap_or("agents.action-failed")
            .to_owned(),
        error["message"].as_str().unwrap_or_default().to_owned(),
    ))
}

/// Every agent found registered with the installation's command, never touching an entry Sidevoice did not write:
/// per agent, `{outcome, message}`.
async fn register(profile: &Profile) -> Value {
    let agents = match HostAgents::for_installer(profile.clone()) {
        Ok(agents) => agents,
        Err(error) => return json!({"error": error.to_string()}),
    };
    let listed = agents.handle("agents.list", json!({"rescan": true})).await;
    if let Some((key, message)) = error_message(&listed) {
        return json!({"error": {"key": key, "message": message}});
    }
    let mut outcomes = serde_json::Map::new();
    for (id, label, row) in agent_rows(&listed) {
        let manual = row
            .pointer("/instructions/command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let (outcome, message) = match row["registration"].as_str() {
            Some("connected") => ("registered", format!("{label} already uses Sidevoice.")),
            Some("not-connected") if row["connect"] == "auto" => {
                let answer = agents.handle("agents.connect", json!({"id": id})).await;
                match error_message(&answer) {
                    None => ("registered", format!("Registered Sidevoice with {label}.")),
                    Some((_, message)) => ("failed", message),
                }
            }
            Some("not-connected") => (
                "manual",
                format!(
                    "{label}: Sidevoice is not registered there; to register it:\n    {manual}"
                ),
            ),
            Some(other) => {
                let key = match other {
                    "foreign" => "agents.foreign",
                    "invalid" => "agents.invalid",
                    _ => "agents.registration-unknown",
                };
                let mut text = crate::messages::message(key, &json!({"agent": label}));
                if !manual.is_empty() {
                    text.push_str(&format!("\n    {manual}"));
                }
                (if other == "unknown" { "unknown" } else { other }, text)
            }
            None => continue,
        };
        outcomes.insert(id, json!({"outcome": outcome, "message": message}));
    }
    Value::Object(outcomes)
}

/// What uninstall does with an agent's row: `Some(true)` removes its entry, `Some(false)` leaves a foreign or unknown
/// one as it is, `None` has nothing to do. An entry of ours that is not the current one (disabled, an older release)
/// is removed too: once the release it names is deleted it would only be a broken server.
fn removal(row: &Value) -> Option<bool> {
    let owned = row["owned"] == Value::Bool(true);
    match row["registration"].as_str() {
        Some("not-connected") if !owned => None,
        Some("connected" | "not-connected") => Some(true),
        _ => Some(false),
    }
}

/// Every registration of ours removed: per agent, `{outcome, message}`.
async fn unregister(profile: &Profile) -> Value {
    let agents = match HostAgents::for_installer(profile.clone()) {
        Ok(agents) => agents,
        Err(error) => return json!({"error": error.to_string()}),
    };
    let listed = agents.handle("agents.list", json!({"rescan": true})).await;
    if let Some((key, message)) = error_message(&listed) {
        return json!({"error": {"key": key, "message": message}});
    }
    let mut outcomes = serde_json::Map::new();
    for (id, label, row) in agent_rows(&listed) {
        let Some(remove) = removal(&row) else {
            continue;
        };
        let (outcome, message) = match row["registration"].as_str() {
            _ if remove => {
                let answer = agents.handle("agents.disconnect", json!({"id": id})).await;
                match error_message(&answer) {
                    None => ("removed", format!("Removed Sidevoice from {label}.")),
                    Some((_, message)) => ("failed", message),
                }
            }
            Some("foreign") => (
                "foreign",
                format!("{label} has a Sidevoice entry that Sidevoice did not create; it was left unchanged."),
            ),
            _ => (
                "unknown",
                crate::messages::message("agents.registration-unknown", &json!({"agent": label})),
            ),
        };
        outcomes.insert(id, json!({"outcome": outcome, "message": message}));
    }
    Value::Object(outcomes)
}

/// What Cursor can and cannot do with a room, said at install so that nobody expects more.
const CURSOR_NOTES: &str = "Cursor: ask a chat to join the voice room; it speaks its replies there. The first time, \
Cursor asks to approve the new MCP server (cursor-agent: or run  cursor-agent mcp enable sidevoice ). What you say in \
the room reaches Cursor only by experimental routes, because Cursor offers none of its own:\n  - Cursor CLI: only a \
chat started with  cursor-agent persist  (needs tmux); the room types into its terminal.\n  - Cursor editor: a small \
Sidevoice card appears under the join call; while it stays open in that chat, the room sends to it. Several chats of \
a window can join, each with its own card.\nIf you type while the room sends, the two mix.";

/// Claude Code holds what other local processes send to a session that bypasses permission prompts.
fn inbound_warning(profile: &Profile) -> Option<String> {
    let settings = profile.claude.join("settings.json");
    let parsed: Value = serde_json::from_slice(&fs::read(&settings).ok()?).ok()?;
    if parsed
        .get("crossSessionInbound")
        .is_some_and(|value| !value.is_null())
        || parsed
            .pointer("/permissions/defaultMode")
            .and_then(Value::as_str)
            != Some("bypassPermissions")
    {
        return None;
    }
    Some(format!(
        "This machine runs Claude Code sessions in bypassPermissions, and those hold what the room sends instead of \
         delivering it: voice looks sent and never arrives. Either start a session with\n  --settings \
         '{{\"crossSessionInbound\":\"accept\"}}'\nor add \"crossSessionInbound\": \"accept\" to {}. That second one \
         lets any local process post into every Claude session on this machine, which is the safeguard it removes: \
         your call, not ours.",
        settings.display()
    ))
}

/// What is left for the person after an install.
fn advice(profile: &Profile, answer: &Value) -> Vec<String> {
    let mut next = Vec::new();
    if let Some(linger) = answer
        .get("linger")
        .filter(|linger| linger["enabled"] == false)
    {
        next.push(format!(
            "{}\n    {}",
            linger["reason"].as_str().unwrap_or_default(),
            linger["command"].as_str().unwrap_or_default()
        ));
    }
    if answer["paired"].is_null() {
        next.push(
            "This machine is not paired with a room yet. In a conversation, ask to join the voice room and give it \
             the room's address and the code the room shows for pairing a machine; or pair the Sidevoice app on \
             this computer with `sidevoice pair-device`."
                .to_owned(),
        );
    }
    let agents = answer["agents"].as_object();
    let has = |id: &str| agents.is_some_and(|agents| agents.contains_key(id));
    if has("claude") {
        next.push(
            "In a conversation, ask to join the voice room. Sessions already open need a restart before they see \
             the server."
                .to_owned(),
        );
        if let Some(warning) = inbound_warning(profile) {
            next.push(warning);
        }
    }
    if has("cursor") {
        next.push(CURSOR_NOTES.to_owned());
    }
    next
}

/// `install` as a person reads it.
pub fn human_install(answer: &Value) -> String {
    let mut done = Vec::new();
    let version = answer["version"].as_str().unwrap_or("?");
    done.push(match answer["action"].as_str() {
        Some("reinstalled") => format!("Sidevoice {version} is installed again."),
        Some("updated") => format!(
            "Sidevoice {version} is installed, in place of {}.",
            answer["previous"].as_str().unwrap_or("?")
        ),
        _ => format!("Sidevoice {version} is installed."),
    });
    done.push(format!(
        "It runs as {}.",
        answer["command"][0].as_str().unwrap_or("?")
    ));
    let state = answer["state"].as_str().unwrap_or("failed");
    done.push(crate::messages::message(
        &format!("service.state.{state}"),
        &json!({"service": answer["service"]}),
    ));
    if answer["service"] == "none" {
        done.push("With no service manager here, Sidevoice starts on demand when a conversation needs it.".into());
    }
    if let Some(agents) = answer["agents"].as_object() {
        for outcome in agents.values() {
            if let Some(message) = outcome["message"].as_str() {
                done.push(message.to_owned());
            }
        }
    }
    match answer["paired"].as_str() {
        Some(room) => done.push(format!("This machine is paired with {room}.")),
        None => done.push("This machine is not paired with any room yet.".into()),
    }
    let mut out = done
        .iter()
        .map(|line| format!("· {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let next: Vec<&str> = answer["advice"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if !next.is_empty() {
        out.push_str("\n\nLeft for you:");
        for line in next {
            out.push_str("\n\n");
            out.push_str(line);
        }
    }
    out
}

/// `uninstall`: see the module's documentation. The answer is `uninstall --json`'s.
pub async fn uninstall(profile: &Profile, options: Options, runtime: &Runtime) -> Result<Value> {
    let layout = Layout::from_profile(profile);
    let _lock = crate::service::install_lock(&layout).await?;
    let service = runtime.unload(&layout).await?;
    let agents = if options.agents {
        unregister(profile).await
    } else {
        json!({})
    };
    let paired = crate::pairing::previous_room(profile);
    let had_releases = fs::symlink_metadata(&layout.releases).is_ok();
    match fs::remove_dir_all(&layout.releases) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(failed(error)),
        _ => {}
    }
    let mut removed = Vec::new();
    if layout.data.is_dir() {
        let _agents_lock = crate::lock::wait_lock(
            &layout.data.join("agents.lock"),
            "uninstall",
            Duration::from_secs(30),
        )
        .await?
        .ok_or_else(|| failed("another agents request holds agents.lock"))?;
        for entry in fs::read_dir(&layout.data)?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if KEPT.contains(&name.as_str()) {
                continue;
            }
            let path = entry.path();
            let gone = if entry.file_type()?.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            gone.map_err(|error| failed(format!("{}: {error}", path.display())))?;
            removed.push(name);
        }
    }
    let mut advice = Vec::new();
    if let Some(room) = &paired {
        advice.push(format!(
            "The room at {room} still lists this machine as paired until you revoke it on the room's page."
        ));
    }
    advice
        .push("Conversations already open keep their Sidevoice server until they end.".to_owned());
    Ok(
        json!({"ok": true, "state": "absent", "service": service.get("service").cloned().unwrap_or(Value::Null),
        "note": service.get("note").cloned().unwrap_or(Value::Null), "agents": agents,
        "releases": had_releases.then(|| layout.releases.clone()), "data": layout.data, "removed": removed,
        "advice": advice}),
    )
}

/// `uninstall` as a person reads it.
pub fn human_uninstall(answer: &Value) -> String {
    let mut done = vec![format!(
        "Sidevoice no longer runs here ({}).",
        answer["service"].as_str().unwrap_or("none")
    )];
    if let Some(note) = answer["note"].as_str() {
        done.push(note.to_owned());
    }
    if let Some(agents) = answer["agents"].as_object() {
        for outcome in agents.values() {
            if let Some(message) = outcome["message"].as_str() {
                done.push(message.to_owned());
            }
        }
    }
    if let Some(releases) = answer["releases"].as_str() {
        done.push(format!("Removed {releases}."));
    }
    if answer["removed"]
        .as_array()
        .is_some_and(|removed| !removed.is_empty())
    {
        done.push(format!(
            "Removed what was in {} (credential, socket, outbox, logs, the core's data); its lock files and the stop \
             stay.",
            answer["data"].as_str().unwrap_or("?")
        ));
    }
    let mut out = done
        .iter()
        .map(|line| format!("· {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    out.push_str("\n\nLeft for you:");
    for line in answer["advice"].as_array().into_iter().flatten() {
        out.push_str("\n\n");
        out.push_str(line.as_str().unwrap_or_default());
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::core_package::tests::{core_archive, core_files};
    use crate::secure_fs::tests::Scratch;
    use sha2::{Digest, Sha256};
    use std::collections::BTreeSet;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    pub(crate) struct Fake {
        /// The releases whose pair does not answer.
        pub failing: BTreeSet<String>,
        /// The release `current` named at each restart.
        pub restarts: Vec<String>,
        pub unloaded: bool,
    }

    struct Fixture {
        scratch: Scratch,
        profile: Profile,
        fake: Arc<Mutex<Fake>>,
    }

    const NO_AGENTS: Options = Options { agents: false };

    impl Fixture {
        fn new(label: &str) -> Self {
            let scratch = Scratch::new(label);
            let profile = Profile::for_test(&scratch.0.join("profile"));
            Self {
                scratch,
                profile,
                fake: Arc::default(),
            }
        }

        fn layout(&self) -> Layout {
            Layout::from_profile(&self.profile)
        }

        fn runtime(&self) -> Runtime {
            Runtime::Fake(self.fake.clone())
        }

        /// A package of connector `version`: a binary that says its version (`build` tells builds apart) and a
        /// core that passes its self-test, or fails it.
        fn package(&self, version: &str, build: &str, core_passes: bool) -> Package {
            let root = self.scratch.0.join(format!(
                "package-{version}-{build}-{}",
                uuid::Uuid::new_v4().simple()
            ));
            fs::create_dir_all(root.join("bin")).unwrap();
            fs::create_dir_all(root.join("core")).unwrap();
            let binary = root.join(BINARY);
            fs::write(
                &binary,
                format!("#!/bin/sh\n# {build}\necho '{{\"ok\":true,\"version\":\"{version}\"}}'\n"),
            )
            .unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
            let archive = core_archive(&core_files(core_passes), &[]);
            let target = crate::identity::target().unwrap();
            let name = format!("core/sidevoice-core-0.2.0-{target}.tar.zst");
            fs::write(root.join(&name), &archive).unwrap();
            let inventory = json!({"version": version, "target": target,
                "core": {"version": "0.2.0", "archive": name, "size": archive.len(),
                    "source_sha": "0123456789abcdef0123456789abcdef01234567",
                    "sha256": hex::encode(Sha256::digest(&archive))}});
            fs::write(root.join("connector.json"), inventory.to_string()).unwrap();
            Package {
                root,
                binary,
                version: version.to_owned(),
            }
        }

        async fn install(&self, package: &Package) -> Result<Value> {
            install(&self.profile, package, NO_AGENTS, &self.runtime()).await
        }

        fn releases(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(releases_dir(&self.layout()))
                .unwrap()
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    fn key(error: &anyhow::Error) -> &'static str {
        crate::messages::keyed(error).key
    }

    #[tokio::test]
    async fn install_switches_current_upgrades_keeps_one_previous_rolls_back_and_uninstalls() {
        let fixture = Fixture::new("install");
        let layout = fixture.layout();

        // First install: the release, `current`, and the stable command recorded.
        let answer = fixture
            .install(&fixture.package("1.0.0", "a", true))
            .await
            .unwrap();
        assert_eq!(answer["action"], "installed", "{answer}");
        assert_eq!(answer["version"], "1.0.0");
        assert!(answer["previous"].is_null());
        assert_eq!(fixture.releases(), ["1.0.0"]);
        assert_eq!(current_release(&layout).as_deref(), Some("1.0.0"));
        assert_eq!(
            fs::read_link(layout.current()).unwrap(),
            Path::new("releases/1.0.0")
        );
        let release = releases_dir(&layout).join("1.0.0");
        assert!(complete(&release));
        assert!(release.join("core/native-core.json").is_file());
        let stable = layout.current().join("bin/sidevoice-connector");
        assert_eq!(answer["command"], json!([stable]));
        assert_eq!(
            layout.connector_command(),
            Some(vec![stable.to_string_lossy().into_owned()])
        );
        assert!(layout.installed());
        let mode = fs::metadata(release.join(BINARY))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);

        // The same build again: nothing staged, the selection restarted and verified (the recovery).
        let staged_at = fs::metadata(release.join(BINARY))
            .unwrap()
            .modified()
            .unwrap();
        let again = fixture
            .install(&fixture.package("1.0.0", "a", true))
            .await
            .unwrap();
        assert_eq!(again["action"], "reinstalled", "{again}");
        assert_eq!(
            fs::metadata(release.join(BINARY))
                .unwrap()
                .modified()
                .unwrap(),
            staged_at
        );

        // Upgrades: the one before is kept, any older one pruned.
        let second = fixture
            .install(&fixture.package("2.0.0", "a", true))
            .await
            .unwrap();
        assert_eq!(
            (second["action"].as_str(), second["previous"].as_str()),
            (Some("updated"), Some("1.0.0"))
        );
        assert_eq!(fixture.releases(), ["1.0.0", "2.0.0"]);
        let third = fixture
            .install(&fixture.package("3.0.0", "a", true))
            .await
            .unwrap();
        assert_eq!(third["pruned"], json!(["1.0.0"]));
        assert_eq!(fixture.releases(), ["2.0.0", "3.0.0"]);
        assert_eq!(current_release(&layout).as_deref(), Some("3.0.0"));

        // A release whose pair does not answer: back to the one before, restarted, and the failed one deleted.
        fixture.fake.lock().unwrap().failing.insert("4.0.0".into());
        let error = fixture
            .install(&fixture.package("4.0.0", "a", true))
            .await
            .unwrap_err();
        assert_eq!(key(&error), "install.rolled-back", "{error}");
        let keyed = crate::messages::keyed(&error);
        assert_eq!(keyed.params["previous"], "3.0.0");
        assert_eq!(keyed.params["failure"]["key"], "launch.exited");
        assert!(error.to_string().contains("back on 3.0.0"), "{error}");
        assert_eq!(current_release(&layout).as_deref(), Some("3.0.0"));
        assert_eq!(fixture.releases(), ["2.0.0", "3.0.0"]);
        let restarts = fixture.fake.lock().unwrap().restarts.clone();
        assert_eq!(
            restarts[restarts.len() - 2..],
            ["4.0.0".to_owned(), "3.0.0".to_owned()]
        );

        // Uninstall: the releases and the data go; the locks and the stop stay.
        crate::secure_fs::atomic_json(&layout.data.join("outbox.json"), &json!([])).unwrap();
        crate::lock::try_lock(&layout.data.join("agents.lock"), "test").unwrap();
        let removed = uninstall(&fixture.profile, NO_AGENTS, &fixture.runtime())
            .await
            .unwrap();
        assert_eq!(removed["state"], "absent", "{removed}");
        assert!(fixture.fake.lock().unwrap().unloaded);
        assert!(!layout.releases.exists());
        let mut left: Vec<String> = fs::read_dir(&layout.data)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert!(
            left.iter().all(|name| KEPT.contains(&name.as_str())),
            "{left:?}"
        );
        assert!(left.contains(&"node-stopped.json".to_owned()), "{left:?}");
        assert!(left.contains(&"install.lock".to_owned()), "{left:?}");
        assert!(!layout.installed());

        // And it installs again from nothing.
        fixture
            .install(&fixture.package("3.0.0", "a", true))
            .await
            .unwrap();
        assert_eq!(fixture.releases(), ["3.0.0"]);
    }

    #[tokio::test]
    async fn a_first_install_that_does_not_answer_stays_and_says_why() {
        let fixture = Fixture::new("first-fails");
        fixture.fake.lock().unwrap().failing.insert("1.0.0".into());
        let error = fixture
            .install(&fixture.package("1.0.0", "a", true))
            .await
            .unwrap_err();
        assert_eq!(key(&error), "install.verify-failed", "{error}");
        assert!(error.to_string().contains("the core exited"), "{error}");
        assert_eq!(current_release(&fixture.layout()).as_deref(), Some("1.0.0"));
    }

    #[tokio::test]
    async fn a_core_that_fails_its_self_test_is_not_installed() {
        let fixture = Fixture::new("self-test");
        fixture
            .install(&fixture.package("1.0.0", "a", true))
            .await
            .unwrap();
        let error = fixture
            .install(&fixture.package("2.0.0", "a", false))
            .await
            .unwrap_err();
        assert_eq!(key(&error), "core.self-test", "{error}");
        assert_eq!(fixture.releases(), ["1.0.0"], "nothing partial is left");
        assert_eq!(current_release(&fixture.layout()).as_deref(), Some("1.0.0"));
        assert_eq!(fixture.fake.lock().unwrap().restarts.len(), 1);
    }

    #[tokio::test]
    async fn another_build_of_a_version_is_a_release_of_its_own() {
        let fixture = Fixture::new("builds");
        fixture
            .install(&fixture.package("1.0.0", "a", true))
            .await
            .unwrap();
        let other = fixture.package("1.0.0", "b", true);
        let digest = digest_file(&other.binary).unwrap();
        let answer = fixture.install(&other).await.unwrap();
        let id = format!("1.0.0+{}", &digest[..12]);
        assert_eq!(answer["version"], id.as_str(), "{answer}");
        assert_eq!(answer["action"], "updated");
        assert_eq!(fixture.releases(), ["1.0.0".to_owned(), id]);
        assert!(fixture
            .install(&fixture.package("bad/version", "a", true))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn leftovers_of_a_dead_installer_are_removed() {
        let fixture = Fixture::new("leftovers");
        let layout = fixture.layout();
        fs::create_dir_all(releases_dir(&layout).join(".1.0.0.partial-x/core")).unwrap();
        fs::set_permissions(&layout.releases, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(releases_dir(&layout), fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink("releases/x", layout.releases.join(".current-x.tmp")).unwrap();
        fixture
            .install(&fixture.package("1.0.0", "a", true))
            .await
            .unwrap();
        assert_eq!(fixture.releases(), ["1.0.0"]);
        assert!(fs::symlink_metadata(layout.releases.join(".current-x.tmp")).is_err());
    }

    #[test]
    fn uninstall_removes_every_entry_of_ours_and_leaves_the_rest() {
        let row =
            |registration: &str, owned: bool| json!({"registration": registration, "owned": owned});
        assert_eq!(removal(&row("connected", true)), Some(true));
        assert_eq!(
            removal(&row("not-connected", true)),
            Some(true),
            "disabled or an older release, still ours"
        );
        assert_eq!(removal(&row("not-connected", false)), None, "nothing there");
        assert_eq!(removal(&row("foreign", false)), Some(false));
        assert_eq!(removal(&row("unknown", false)), Some(false));
    }

    #[tokio::test]
    async fn installing_again_repairs_a_release_whose_core_was_damaged() {
        let fixture = Fixture::new("repair");
        let layout = fixture.layout();
        let package = fixture.package("1.0.0", "a", true);
        fixture.install(&package).await.unwrap();
        let first = current_release(&layout).unwrap();
        let model = releases_dir(&layout)
            .join(&first)
            .join(crate::service::layout::CORE_DIRECTORY)
            .join("models/silero.onnx");
        fs::write(&model, b"damaged").unwrap();
        assert!(!core_intact(&releases_dir(&layout).join(&first), &package));

        // The same package again: staged anew beside the damaged release, which stayed selected until then.
        fixture.install(&package).await.unwrap();
        let repaired = current_release(&layout).unwrap();
        assert_ne!(
            repaired, first,
            "a fresh release, not the damaged one reused"
        );
        assert!(core_intact(
            &releases_dir(&layout).join(&repaired),
            &package
        ));

        // And once more: the repaired release is reused as it is.
        fixture.install(&package).await.unwrap();
        assert_eq!(current_release(&layout).unwrap(), repaired);
    }
}
