//! `install` and `uninstall`: this package on this computer, and in front of the agents on it.
//!
//! The installation (sidevoice/sidevoice-connector#66, decision 3):
//!
//! | What | Where |
//! |---|---|
//! | `R/releases/<id>/` | one release per version: `bin/sidevoice-connector`, a copy of the installing binary (on Windows also `bin/sidevoice-background.exe`, the windowless program the service tasks run), and `core/`, the core the package carries, staged and checked against its inventory (`core_package.rs`) |
//! | `R/current.json` | the pointer record `{"release": "<id>"}`, written whole over the old one: the selected release |
//! | `D/install.json` | `command`: `[R/releases/<id>/bin/sidevoice-connector]` of the selected release, rewritten at each switch, what the service jobs, the desktop app and every agent registration run, never the package's own path (npm's cache is not stable); `releases`: `R` |
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
//! pruned (exactly one previous). Running it again is the recovery.
//!
//! The agents (unless `--no-agents`): installing connects none. An agent gets Sidevoice only when the person connects
//! it: Connect in the Sidevoice app (`agents.connect`), `sidevoice agents connect <id>`, or their answer to the offer
//! below. `install` lists the agents found (a rescan) and reports each one in its answer: one that uses Sidevoice as
//! it is; one without Sidevoice as found, with the command that connects it (`command`) when the connector can do it
//! itself, else the steps by hand. The one entry it writes is an entry an older Sidevoice wrote (`outdated`: disabled,
//! naming an older release, or without this installation's environment), written again with the installation's
//! command: the person already chose to connect that agent, and the entry would stop working once the release it
//! names is pruned, so keeping it working across an update is not a new consent.
//!
//! At a terminal (standard input and output both terminals, no `--json`, no `--no-prompt`, not under CI), the command
//! line then offers to connect the agents found without Sidevoice that the connector can connect itself: all, some,
//! or none, none being the default (Enter, or the end of input). Those chosen are connected as `agents connect`
//! connects them. The offer comes after the install lock is released, so a person thinking does not hold up another
//! install. With `--json` (the desktop app reads it) or without a terminal nothing is asked and no agent is connected.
//!
//! `uninstall`, under the install lock: both jobs unloaded and what runs on demand stopped (a refusal stops
//! everything here, with nothing deleted), the agents' registrations removed, then `R` and everything in `D` but
//! its permanent lock files and the stop, so that a launcher on its way finds the stop and does not serve. A `D`
//! that is not the standard place and holds nothing but those leftovers, with no installation record, proves no
//! installation: `uninstall` answers that nothing is installed there and changes nothing at all.

use crate::agents::HostAgents;
use crate::messages::Keyed;
use crate::platform::data_dir;
use crate::platform::env::{Env, Environment};
use crate::platform::fs::{Create, Files, Fs};
use crate::platform::process::{Process, Processes};
use crate::platform::service::{Service, Services};
use crate::platform::target;
use crate::platform::uninstall::{Ownership, Removal, Remover, Roots};
use crate::profile::Profile;
use crate::service::jobs::{Job, Kind};
use crate::service::layout::Layout;
use crate::service::{launcher, status};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::time::{sleep, timeout, timeout_at, Duration, Instant};

/// The connector's program inside a package and inside a release (`bin/sidevoice-connector`, `.exe` on Windows).
pub fn binary() -> String {
    format!("bin/{}", target::exe_name("sidevoice-connector"))
}
/// How long the pair has to answer after a restart.
const VERIFY_LIMIT: Duration = Duration::from_secs(60);
/// How long to wait between looks at the pair.
const POLL_PAUSE: Duration = Duration::from_millis(250);
/// How long the service's own account of the job may take once the wait is over.
const DIAGNOSIS_LIMIT: Duration = Duration::from_secs(10);

/// What one look at the pair found.
enum Poll {
    /// The pair answers.
    Ready,
    /// A failure that waiting will not change.
    Failed(Value),
    /// Not yet, with the node's state as it was seen.
    Waiting(Value),
}

/// How a wait ended without the pair answering.
enum Wait {
    Failed(Value),
    /// The limit passed; the last state seen, if any.
    TimedOut(Option<Value>),
}

/// `poll` until it is ready or fails, for at most `limit` in all: a look that is itself slow (a manager command has a
/// 30 s deadline of its own) is cut off at the limit too, so the wait never outlasts it.
async fn wait_until<F, Fut>(limit: Duration, pause: Duration, mut poll: F) -> Result<(), Wait>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Poll>,
{
    let deadline = Instant::now() + limit;
    let mut last = None;
    loop {
        match timeout_at(deadline, poll()).await {
            Ok(Poll::Ready) => return Ok(()),
            Ok(Poll::Failed(failure)) => return Err(Wait::Failed(failure)),
            Ok(Poll::Waiting(state)) => last = Some(state),
            Err(_) => return Err(Wait::TimedOut(last)),
        }
        if Instant::now() >= deadline {
            return Err(Wait::TimedOut(last));
        }
        sleep(pause).await;
    }
}

/// What the install waited for, and where to look when it did not come.
struct Waited {
    /// The core's ready file.
    ready: PathBuf,
    /// Where the service appends the core's output. Its lines are never copied into an error: they may hold
    /// credentials, so the error names the file.
    output: PathBuf,
}

/// The failure of a wait that reached `limit`: what was awaited, the last state seen and which component was still
/// not ready, the service's own account of the job (`Diagnosis`), where the core's output is, and what to run next.
fn timeout_failure(limit: Duration, waited: &Waited, service: &str, last: Option<&Value>) -> Value {
    let seen = match last {
        None => " No look at the service finished.".to_owned(),
        Some(last) => {
            let text = |pointer: &str| last.pointer(pointer).and_then(Value::as_str);
            format!(
                " Last seen: state {}{}; still waiting for {}.",
                text("/state").unwrap_or("unknown"),
                text("/failure/message")
                    .map(|message| format!(" ({message})"))
                    .unwrap_or_default(),
                text("/waiting_for").unwrap_or("the pair"),
            )
        }
    };
    json!({"key": "ready.timeout", "step": "ready",
        "message": crate::messages::message("install.not-ready", &json!({
            "seconds": limit.as_secs(), "ready": waited.ready, "service": service,
            "seen": seen, "output": waited.output})),
        "service": service, "output_log": waited.output})
}

/// What `uninstall` leaves in `D` while it runs: the lock files, the stop, and `Models` (named by `sidevoice-platform`); the
/// lock files go last when nothing else is left, preserving the stop (`Removal::remove_data_dir`). The shared models
/// are the engine's, never this connector's to remove; they live in the OS standard place (`data_dir::models_dir`), which
/// is `D` itself when `D` is not overridden, so they are kept here so that an un-overridden `D` never loses them (when `D`
/// is overridden they are not under it and this entry matches nothing).
const KEPT: &[&str] = &[
    "install.lock",
    "connector.lock",
    "agents.lock",
    "node-stopped.json",
    data_dir::MODELS,
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

/// `R/releases/<id>/bin/sidevoice-connector`: the installation's command while `id` is selected. It names the
/// release itself, so `select` writes it again at each switch, into `install.json` and so into the service jobs.
fn release_command(layout: &Layout, id: &str) -> PathBuf {
    layout.releases_dir().join(id).join(binary())
}

/// `R/current.json` names release `id`, written whole (`platform::fs`), then `D/install.json` names its command.
/// `D` is made first, so that failing to make it never leaves the pointer switched and the command not.
fn select(layout: &Layout, id: &str) -> Result<()> {
    Files::ensure_private_dir(&layout.data)?;
    Files::write_json(&layout.current_record(), &json!({"release": id}))?;
    Files::write_json(
        &layout.install_file(),
        &json!({"command": [release_command(layout, id)], "releases": layout.releases}),
    )?;
    Ok(())
}

/// What a dead installer left: partial releases and the temporary file of a pointer record being written.
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
    for path in leftovers(&layout.releases_dir(), ".") {
        let _ = fs::remove_dir_all(&path).or_else(|_| fs::remove_file(&path));
    }
    for path in leftovers(&layout.releases, ".current.json.") {
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

/// The program beside the connector that runs the service jobs without a window (`bin/sidevoice-background.exe`),
/// where the OS's service manager needs one (`Service::BACKGROUND`).
fn background() -> Option<String> {
    Services::BACKGROUND.map(|stem| format!("bin/{}", target::exe_name(stem)))
}

/// A release that is all there: its binary, its background program where there is one, and its core's program.
fn complete(release: &Path) -> bool {
    release.join(binary()).is_file()
        && background().is_none_or(|program| release.join(program).is_file())
        && release
            .join(crate::service::layout::core_entrypoint())
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
    let current = layout.selected_id();
    let names = [
        package.version.clone(),
        format!("{}+{}", package.version, &digest[..12]),
    ];
    let same = |id: &str| {
        digest_file(&layout.releases_dir().join(id).join(binary()))
            .ok()
            .as_deref()
            == Some(digest)
    };
    if let Some(whole) = names.iter().find(|id| {
        let release = layout.releases_dir().join(id);
        same(id) && complete(&release) && core_intact(&release, package)
    }) {
        return Ok((whole.clone(), false));
    }
    for id in names {
        let release = layout.releases_dir().join(&id);
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
        "the selected release is another build of this version; run `npx sidevoice uninstall`, then install again",
    ))
}

/// The SHA-256 of a file, in lower-case hex.
fn digest_file(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    std::io::copy(&mut File::open(path)?, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

/// Copies `from` to `to` (created, 0700) and syncs it.
fn copy_executable(from: &Path, to: &Path) -> Result<()> {
    let mut source = File::open(from).with_context(|| format!("open {}", from.display()))?;
    let mut target = Files::open_private(to, Create::NewProgram)
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
            Ok(Err(error)) if Processes::executable_is_busy(&error) && attempts < 20 => {
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
    let releases = layout.releases_dir();
    Files::ensure_private_dir(&releases)?;
    let partial = releases.join(format!(".{id}.partial-{}", uuid::Uuid::new_v4().simple()));
    Files::create_private_dir(&partial)?;
    let result = async {
        Files::create_private_dir(&partial.join("bin"))?;
        let binary = partial.join(binary());
        copy_executable(&package.binary, &binary)?;
        Files::check_private(&Files::open_read(&binary)?)
            .with_context(|| format!("unsafe private file {}", binary.display()))?;
        which::which(&binary).context("the staged connector cannot be run")?;
        if digest_file(&binary)? != digest {
            return Err(failed("the connector changed while it was being copied"));
        }
        check_binary(&binary, &package.version).await?;
        if let Some(program) = background() {
            // Beside the connector it was started from, as the package carries them.
            let from = package
                .binary
                .with_file_name(Path::new(&program).file_name().unwrap_or_default());
            let staged = partial.join(&program);
            copy_executable(&from, &staged)?;
            Files::check_private(&Files::open_read(&staged)?)
                .with_context(|| format!("unsafe private file {}", staged.display()))?;
        }
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
        Files::sync_dir(&releases)?;
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
    for entry in fs::read_dir(layout.releases_dir())
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
        (Environment::HOME.to_owned(), layout.home.clone()),
        (data_dir::VARIABLE.to_owned(), layout.data.clone()),
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
                let kind = crate::service::jobs::kind().await;
                if kind == Kind::None {
                    crate::service::stop_on_demand_or_fail(layout).await?;
                } else {
                    crate::service::define_and_start(layout, true).await?;
                }
                Ok(kind)
            }
            #[cfg(test)]
            Runtime::Fake(fake) => {
                let selected = layout.selected_id().unwrap_or_default();
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
                let binary = release.join(binary());
                if kind == Kind::None {
                    let fallback = vec![binary.to_string_lossy().into_owned()];
                    launcher::ensure_connector(
                        layout,
                        fallback,
                        connector_environment(layout),
                        || runs_release(layout, &binary),
                    )
                    .await
                    .map_err(|failure| failure_value(&failure))?;
                }
                let layout_ref = layout;
                let binary_ref = &binary;
                let before_ref = before.as_ref();
                let poll = || async move {
                    let mut waiting = "the core (no ready file with a health answer)";
                    if let Some((ready, _)) =
                        status::serving(layout_ref, Duration::from_millis(1500)).await
                    {
                        if Some(&ready.launch_id) == before_ref {
                            waiting = "a new core launch (the previous one still answers)";
                        } else if !runs_release(layout_ref, binary_ref).await {
                            waiting = "the connector (not answering as this release)";
                        } else if launcher::ask_connector(
                            layout_ref,
                            "node.status",
                            json!({}),
                            Duration::from_secs(5),
                        )
                        .await
                        .is_none()
                        {
                            waiting = "the connector (node.status unanswered)";
                        } else {
                            return Poll::Ready;
                        }
                    }
                    let mut now = if kind == Kind::None {
                        // No job: what the core said about a failed start is the state.
                        match status::read_failure(layout_ref, &crate::logfile::timestamp()) {
                            Some(failure) => json!({"state": "failed", "failure": failure}),
                            None => json!({"state": "starting"}),
                        }
                    } else {
                        status::status(layout_ref, None).await
                    };
                    let state = now.get("state").and_then(Value::as_str).unwrap_or("");
                    if matches!(state, "failed" | "service-failed") && status::settled(&now) {
                        return Poll::Failed(
                            now.get("failure")
                                .cloned()
                                .filter(Value::is_object)
                                .unwrap_or_else(|| json!({"key": state})),
                        );
                    }
                    now["waiting_for"] = json!(waiting);
                    Poll::Waiting(now)
                };
                match wait_until(VERIFY_LIMIT, POLL_PAUSE, poll).await {
                    Ok(()) => Ok(()),
                    Err(Wait::Failed(failure)) => Err(failure),
                    Err(Wait::TimedOut(last)) => {
                        // The diagnosis is bounded on its own: the error and the rollback come whatever it does.
                        let service = match kind {
                            Kind::Manager => timeout(
                                DIAGNOSIS_LIMIT,
                                crate::service::jobs::manager(layout).diagnose(Job::Core.id()),
                            )
                            .await
                            .map(|diagnosis| diagnosis.to_string())
                            .unwrap_or_else(|_| "diagnosis unavailable (timed out)".to_owned()),
                            Kind::None => {
                                "No service manager runs it here: it is started on demand"
                                    .to_owned()
                            }
                        };
                        Err(timeout_failure(
                            VERIFY_LIMIT,
                            &Waited {
                                ready: layout.core_ready(),
                                output: layout.core_output(),
                            },
                            &service,
                            last.as_ref(),
                        ))
                    }
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
            Runtime::Machine => crate::service::commands::uninstall_held(layout, true).await,
            #[cfg(test)]
            Runtime::Fake(fake) => {
                fake.lock().unwrap().unloaded = true;
                Files::write_private(&layout.stop_marker(), b"{}")
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
        .verify(layout, kind, &layout.releases_dir().join(id), before)
        .await?;
    Ok(kind)
}

/// What `install` and `uninstall` were asked.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Installing: list the agents on this computer and write again the entries an older Sidevoice wrote (never
    /// connecting a new one); uninstalling: remove Sidevoice's entries from them.
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
    Files::ensure_private_dir(&layout.releases)?;
    remove_leftovers(&layout);
    let previous = layout.selected_id();
    let digest = digest_file(&package.binary)?;
    let (id, fresh) = release_id(&layout, package, &digest)?;
    if fresh {
        stage(&layout, package, &id, &digest).await?;
    }
    select(&layout, &id)?;
    let kind = match start_and_verify(runtime, &layout, &id).await {
        Ok(kind) => kind,
        Err(cause) => {
            let Some(back) = previous.clone().filter(|back| *back != id) else {
                return Err(keyed(
                    "install.verify-failed",
                    json!({"version": id, "cause": cause_text(&cause), "failure": cause}),
                ));
            };
            select(&layout, &back)?;
            let again = start_and_verify(runtime, &layout, &back).await;
            let _ = fs::remove_dir_all(layout.releases_dir().join(&id));
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
        refresh(profile).await
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
        "command": [release_command(&layout, &id)], "releases": layout.releases, "pruned": pruned,
        "service": kind.as_str(), "state": state.get("state").cloned().unwrap_or(Value::Null),
        "agents": agents});
    if kind == Kind::Manager {
        let service = crate::service::jobs::manager(&layout);
        if let Some(linger) = crate::service::jobs::linger(&service).await {
            answer["linger"] = linger;
        }
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

/// What `install` says for the agents instead of one entry each: the listing's failure, or a scan of its own that did
/// not happen (the login PATH was not captured, or other scans kept overtaking it), said as such: never an empty list,
/// nor what an earlier or another scan recorded, as if this install had found it.
fn listing_error(listed: &Value) -> Option<Value> {
    if let Some((key, message)) = error_message(listed) {
        return Some(json!({"error": {"key": key, "message": message}}));
    }
    if listed["scan_failed"] == true {
        let key = "agents.scan-failed";
        let message = crate::messages::message(key, &json!({"agent": "your agents"}));
        return Some(json!({"error": {"key": key, "message": message}}));
    }
    None
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

/// The command a person runs to connect Sidevoice with one agent.
pub fn connect_command(id: &str) -> String {
    format!("npx sidevoice agents connect {id}")
}

/// How the answer names an agent.
fn agent_label(id: &str) -> String {
    crate::messages::message(&format!("harness.{id}"), &json!({}))
}

/// The agents found, as installing leaves them: no agent connected, and only an entry an older Sidevoice wrote
/// (`outdated`) written again with the installation's command (the module's documentation says why); never an entry
/// Sidevoice did not write. Per agent, `{outcome, message}`, and `command` for one the connector connects when the
/// person asks: `outcome` is `registered` (it uses Sidevoice: as it was, or its entry written again),
/// `not-connected` (found without Sidevoice), `manual` (its configuration needs a step by hand), `failed`, `foreign`,
/// `invalid` or `unknown`.
async fn refresh(profile: &Profile) -> Value {
    let agents = match HostAgents::for_installer(profile.clone()) {
        Ok(agents) => agents,
        Err(error) => return json!({"error": error.to_string()}),
    };
    let listed = agents.handle("agents.list", json!({"rescan": true})).await;
    if let Some(error) = listing_error(&listed) {
        return error;
    }
    let mut outcomes = serde_json::Map::new();
    for (id, label, row) in agent_rows(&listed) {
        // Another "sidevoice" entry is replaced by hand; anything else is connected by hand.
        let manual = manual_text(
            &row,
            if row["registration"] == "foreign" {
                "replace"
            } else {
                "connect"
            },
        );
        let auto = row["connect"] == "auto";
        let (outcome, message) = match row["registration"].as_str() {
            Some("connected") => ("registered", format!("{label} already uses Sidevoice.")),
            // The person connected it with an older Sidevoice: kept working with this one.
            Some("outdated") if auto => {
                let answer = agents.handle("agents.connect", json!({"id": id})).await;
                match error_message(&answer) {
                    None => (
                        "registered",
                        format!("{label}'s Sidevoice entry, written by an older Sidevoice, now runs this one."),
                    ),
                    Some((_, message)) => ("failed", message),
                }
            }
            Some("outdated") => (
                "manual",
                format!("{label}: an older Sidevoice registered it; to register this one:{manual}"),
            ),
            // Found, and left as it is: the person connects it.
            Some("not-connected") if auto => {
                let command = connect_command(&id);
                let message = format!(
                    "{label} does not use Sidevoice; to connect it, use the Sidevoice app or run `{command}`."
                );
                outcomes.insert(
                    id,
                    json!({"outcome": "not-connected", "message": message, "command": command}),
                );
                continue;
            }
            Some("not-connected") => (
                "manual",
                format!("{label} does not use Sidevoice; to connect it:{manual}"),
            ),
            Some(other) => {
                let key = match other {
                    "foreign" => "agents.foreign",
                    "invalid" => "agents.invalid",
                    _ => "agents.registration-unknown",
                };
                let mut text = crate::messages::message(key, &json!({"agent": label}));
                text.push_str(&manual);
                (if other == "unknown" { "unknown" } else { other }, text)
            }
            None => continue,
        };
        outcomes.insert(id, json!({"outcome": outcome, "message": message}));
    }
    Value::Object(outcomes)
}

/// The steps of an agent row's `manual.<kind>`, each line on its own indented line: a command, or a snippet and the
/// file it goes in.
fn manual_text(row: &Value, kind: &str) -> String {
    let steps = row
        .pointer(&format!("/manual/{kind}"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|step| {
            if let Some(command) = step["command"].as_str() {
                return Some(command.to_owned());
            }
            let snippet = step["snippet"].as_str()?;
            Some(match step["file"].as_str() {
                Some(file) => format!("In {file}:\n{snippet}"),
                None => snippet.to_owned(),
            })
        })
        .collect::<Vec<_>>()
        .join("\n");
    steps.lines().map(|line| format!("\n    {line}")).collect()
}

/// What uninstall does with an agent's row: `Some(true)` removes its entry, `Some(false)` leaves a foreign or unknown
/// one as it is, `None` has nothing to do. An entry of ours that is not the current one (`outdated`: disabled, an
/// older release) is removed too: once the release it names is deleted it would only be a broken server.
fn removal(row: &Value) -> Option<bool> {
    match row["registration"].as_str() {
        Some("not-connected") => None,
        Some("connected" | "outdated") => Some(true),
        _ => Some(false),
    }
}

/// Diagnose an entry uninstall cannot remove without advising deletion of an invalid or unclassified entry.
fn unremoved(row: &Value, label: &str) -> (&'static str, String) {
    match row["registration"].as_str() {
            Some("foreign") => (
                "foreign",
                format!("{label} has a Sidevoice entry that Sidevoice did not create; it was left unchanged."),
            ),
            Some("invalid") => (
                "invalid",
                crate::messages::message("agents.invalid", &json!({"agent": label})),
            ),
            Some("unknown") if row.pointer("/can/disconnect") == Some(&Value::Bool(false)) => (
                "unknown",
                crate::messages::message("agents.left-behind", &json!({"agent": label})),
            ),
            _ => (
                "unknown",
                crate::messages::message("agents.registration-unknown", &json!({"agent": label})),
            ),
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
            _ => unremoved(&row, &label),
        };
        outcomes.insert(id, json!({"outcome": outcome, "message": message}));
    }
    Value::Object(outcomes)
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
    next.push(
        "To pair the Sidevoice app with this machine, run `npx sidevoice pair-device` (or ask an agent for a device code)."
            .to_owned(),
    );
    // What to do next with an agent that uses Sidevoice; one without it has its own line in the answer.
    for (id, registration) in crate::adapters::Registry::standard().agents() {
        if answer["agents"][id]["outcome"] == "registered" {
            next.extend(registration.after_install(profile));
        }
    }
    next
}

/// Whether `install` offers, after installing, to connect the agents it found: only to a person at a terminal
/// (`terminal`: its standard input and output both are), never with `--json` (a program reads the answer: the desktop
/// app), `--no-prompt`, or under CI (`CI` set to anything but nothing, `0` or `false`).
pub fn may_offer(
    json: bool,
    no_prompt: bool,
    terminal: bool,
    ci: Option<&std::ffi::OsStr>,
) -> bool {
    let ci = ci.is_some_and(|value| !matches!(value.to_str(), Some("" | "0" | "false")));
    !json && !no_prompt && terminal && !ci
}

/// The agents `install` found without Sidevoice that the connector connects itself, as `(id, label)`: what it offers.
pub fn offerable(answer: &Value) -> Vec<(String, String)> {
    answer["agents"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, agent)| agent["outcome"] == "not-connected" && agent["command"].is_string())
        .map(|(id, _)| (id.clone(), agent_label(id)))
        .collect()
}

/// The person's choice among `offered`, asked on `output` and read from `input`: Enter, `none` (`n`, `no`) or the
/// end of input chooses none; `all` (`a`) all of them; numbers or ids, separated by spaces or commas, those. An
/// answer not understood is asked again.
pub fn ask(
    offered: &[(String, String)],
    input: &mut impl io::BufRead,
    output: &mut impl io::Write,
) -> io::Result<Vec<String>> {
    writeln!(
        output,
        "\nSidevoice is not connected to these agents found here:"
    )?;
    for (number, (_, label)) in offered.iter().enumerate() {
        writeln!(output, "  {}. {label}", number + 1)?;
    }
    loop {
        write!(
            output,
            "Connect it to all of them, some (their numbers, such as 1 2), or none? [none] "
        )?;
        output.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            writeln!(output)?;
            return Ok(Vec::new());
        }
        match choice(offered, &line) {
            Some(chosen) => return Ok(chosen),
            None => writeln!(
                output,
                "Answer all, none (or just Enter), or numbers from 1 to {}.",
                offered.len()
            )?,
        }
    }
}

/// One answer to [`ask`]: the ids chosen, or `None` when it is not understood.
fn choice(offered: &[(String, String)], answer: &str) -> Option<Vec<String>> {
    let answer = answer.trim().to_lowercase();
    match answer.as_str() {
        "" | "n" | "no" | "none" => return Some(Vec::new()),
        "a" | "all" => return Some(offered.iter().map(|(id, _)| id.clone()).collect()),
        _ => {}
    }
    let mut chosen: Vec<String> = Vec::new();
    for word in answer
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|word| !word.is_empty())
    {
        let (id, _) = match word.parse::<usize>() {
            Ok(number) => offered.get(number.checked_sub(1)?)?,
            Err(_) => offered.iter().find(|(id, _)| id == word)?,
        };
        if !chosen.contains(id) {
            chosen.push(id.clone());
        }
    }
    Some(chosen)
}

/// The agents the person chose after an install connected, as `agents connect` connects them, and said in `answer`:
/// each one's outcome (`registered` or `failed`), and the advice again.
pub async fn connect_chosen(profile: &Profile, answer: &mut Value, chosen: &[String]) {
    if chosen.is_empty() {
        return;
    }
    let agents = HostAgents::for_installer(profile.clone());
    for id in chosen {
        let label = agent_label(id);
        let outcome = match &agents {
            Ok(agents) => {
                let connected = agents.handle("agents.connect", json!({"id": id})).await;
                match error_message(&connected) {
                    None => json!({"outcome": "registered",
                        "message": format!("Connected {label} to Sidevoice.")}),
                    Some((_, message)) => json!({"outcome": "failed", "message": message}),
                }
            }
            Err(error) => json!({"outcome": "failed", "message": error.to_string()}),
        };
        answer["agents"][id.as_str()] = outcome;
    }
    answer["advice"] = json!(advice(profile, answer));
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
    uninstall_with_hooks(profile, options, runtime, |guard| guard.preflight(), |_| {}).await
}

/// The two roots `uninstall` may remove, as this installation names them.
fn removal_roots(layout: &Layout) -> Roots {
    Roots {
        standard: data_dir::standard(),
        data: layout.data.clone(),
        releases: layout.releases.clone(),
        kept: KEPT,
        // The profile's home and the OS's: the same on Unix, two places on Windows (`USERPROFILE`, the Known Folder).
        homes: std::iter::once(layout.home.clone())
            .chain(dirs::home_dir())
            .collect(),
    }
}

async fn uninstall_with_hooks(
    profile: &Profile,
    options: Options,
    runtime: &Runtime,
    preflight: impl Fn(&Remover) -> std::result::Result<bool, &'static str>,
    before_claim: impl Fn(&OsStr),
) -> Result<Value> {
    let layout = Layout::from_profile(profile);
    let guard = Remover::new(removal_roots(&layout)).map_err(failed)?;
    let found = guard.ownership().map_err(failed)?;
    if found == Ownership::Leftovers {
        // Nothing proves an installation of ours there: nothing is written, stopped or deleted.
        return Ok(json!({"ok": true, "state": "absent", "installed": false,
            "safe_uninstall": crate::identity::SAFE_UNINSTALL, "data": layout.data, "removed": [],
            "advice": []}));
    }
    let had_releases = preflight(&guard).map_err(failed)?;
    guard.preflight().map_err(failed)?;
    let install_lock = guard.lock_file("install.lock").map_err(failed)?;
    let install_held = crate::lock::wait_lock_file(
        install_lock,
        &layout.data.join("install.lock"),
        Duration::from_secs(30),
    )
    .await
    .map_err(failed)?
    .ok_or_else(|| failed("another install or service command holds install.lock"))?;
    // What `D` holds may have changed while the lock was awaited: still ours, or (missing at the start) only the
    // lock this uninstall just made there.
    match (found, guard.ownership().map_err(failed)?) {
        (_, Ownership::Ours) | (Ownership::Absent, Ownership::Leftovers) => {}
        _ => {
            return Err(failed(
                "the data directory changed while uninstall waited for install.lock",
            ))
        }
    }
    guard.preflight().map_err(failed)?;
    let service = runtime.unload(&layout).await?;
    let agents = if options.agents {
        unregister(profile).await
    } else {
        json!({})
    };
    let agents_lock = guard.lock_file("agents.lock").map_err(failed)?;
    let agents_held = crate::lock::wait_lock_file(
        agents_lock,
        &layout.data.join("agents.lock"),
        Duration::from_secs(30),
    )
    .await?
    .ok_or_else(|| failed("another agents request holds agents.lock"))?;
    guard.stage_recovery().map_err(failed)?;
    let removed = guard.remove_data_contents(&before_claim).map_err(failed)?;
    guard.remove_releases().map_err(failed)?;
    guard.finish_claim().map_err(failed)?;
    guard.finish_recovery().map_err(failed)?;
    let data_dir = match guard.remove_data_dir(&|_| {}) {
        Ok(left) => json!({"removed": left.removed, "kept": left.kept, "held": left.held}),
        // Everything else is already gone: this is said, and is not a failure of the uninstall.
        Err(reason) => json!({"removed": false, "kept": [], "held": [], "error": reason}),
    };
    drop(agents_held);
    drop(install_held);
    let advice =
        vec!["Conversations already open keep their Sidevoice server until they end.".to_owned()];
    Ok(
        json!({"ok": true, "state": "absent", "safe_uninstall": crate::identity::SAFE_UNINSTALL,
        "service": service.get("service").cloned().unwrap_or(Value::Null),
        "note": service.get("note").cloned().unwrap_or(Value::Null), "agents": agents,
        "releases": had_releases.then(|| layout.releases.clone()), "data": layout.data, "removed": removed, "data_dir": data_dir,
        "advice": advice}),
    )
}

/// What became of the data directory itself, as a person reads it.
fn data_dir_line(answer: &Value) -> String {
    let path = answer["data"].as_str().unwrap_or("?");
    let names = |key: &str| -> String {
        answer["data_dir"][key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    };
    if answer["data_dir"]["removed"] == true {
        format!("Removed {path}.")
    } else if let Some(error) = answer["data_dir"]["error"].as_str() {
        format!("{path} was left as it is: {error}.")
    } else if !names("held").is_empty() {
        format!(
            "{path} was left: something still holds {}. Close what uses it and remove the folder.",
            names("held")
        )
    } else {
        format!(
            "{path} was left because it also holds {}, which is not Sidevoice's to remove.",
            names("kept")
        )
    }
}

/// `uninstall` as a person reads it.
pub fn human_uninstall(answer: &Value) -> String {
    if answer["installed"] == false {
        return format!(
            "· Nothing of Sidevoice is installed in {}; nothing was changed.",
            answer["data"].as_str().unwrap_or("?")
        );
    }
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
            "Removed what was in {} (credential, socket, outbox, logs, the core's data).",
            answer["data"].as_str().unwrap_or("?")
        ));
    }
    done.push(data_dir_line(answer));
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
    use crate::platform::fs::TestFiles;
    use sha2::{Digest, Sha256};
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// An install whose own scan did not happen says so, by key, whether the listing is empty or holds what an earlier or
    /// another scan recorded; a listing that worked is no error.
    #[test]
    fn an_agent_scan_that_looked_at_nothing_is_said_not_listed_as_empty() {
        let failed = listing_error(&json!({"agents": [], "scan_failed": true})).unwrap();
        assert_eq!(failed["error"]["key"], "agents.scan-failed", "{failed}");
        assert!(failed["error"]["message"]
            .as_str()
            .is_some_and(|text| !text.is_empty()));
        let kept =
            json!({"agents": [{"id": "some-agent", "label": "Some agent"}], "scan_failed": true});
        assert_eq!(
            listing_error(&kept).unwrap()["error"]["key"],
            "agents.scan-failed"
        );
        assert!(listing_error(&json!({"agents": [], "scan_failed": false})).is_none());
    }

    #[derive(Default)]
    pub struct Fake {
        /// The releases whose pair does not answer.
        pub failing: BTreeSet<String>,
        /// The release `current` named at each restart.
        pub restarts: Vec<String>,
        pub unloaded: bool,
    }

    struct Fixture {
        scratch: tempfile::TempDir,
        profile: Profile,
        fake: Arc<Mutex<Fake>>,
    }

    const NO_AGENTS: Options = Options { agents: false };
    const AGENTS: Options = Options { agents: true };

    impl Fixture {
        fn new(label: &str) -> Self {
            let scratch = crate::test_support::temp_dir(label);
            let homes = crate::adapters::Registry::standard().homes();
            let original = Profile::for_test(&scratch.path().join("profile"), &homes);
            let variables = original.variables();
            let profile = Profile::from_vars(
                |name| {
                    // Where an uninstall may remove: a directory of the OS's name below the home.
                    if name == data_dir::VARIABLE {
                        Some(
                            original
                                .home
                                .join(data_dir::standard().unwrap().file_name().unwrap())
                                .into_os_string(),
                        )
                    } else {
                        variables
                            .iter()
                            .find(|(key, _)| *key == name)
                            .map(|(_, value)| value.as_os_str().to_owned())
                    }
                },
                &homes,
            )
            .unwrap();
            Files::ensure_private_dir(&profile.data.join("core")).unwrap();
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
        /// core.
        fn package(&self, version: &str, build: &str) -> Package {
            let root = self.scratch.path().join(format!(
                "package-{version}-{build}-{}",
                uuid::Uuid::new_v4().simple()
            ));
            fs::create_dir_all(root.join("bin")).unwrap();
            fs::create_dir_all(root.join("core")).unwrap();
            let binary = root.join(binary());
            // `build` is part of the stand-in, so two builds are two files.
            crate::test_support::stand_in::make(
                &binary,
                &json!({"play": "script", "build": build,
                    "print": format!("{}\n", json!({"ok": true, "version": version}))}),
            );
            if let Some(program) = background() {
                fs::write(root.join(program), b"background").unwrap();
            }
            let archive = core_archive(&core_files(), &[]);
            let target = target::build_target().map(|target| target.name).unwrap();
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
            let mut names: Vec<String> = fs::read_dir(self.layout().releases_dir())
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

        // First install: the release, `current`, and its command recorded.
        let answer = fixture
            .install(&fixture.package("1.0.0", "a"))
            .await
            .unwrap();
        assert_eq!(answer["action"], "installed", "{answer}");
        assert_eq!(answer["version"], "1.0.0");
        assert!(answer["previous"].is_null());
        assert_eq!(fixture.releases(), ["1.0.0"]);
        assert_eq!(layout.selected_id().as_deref(), Some("1.0.0"));
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(layout.current_record()).unwrap()).unwrap(),
            json!({"release": "1.0.0"})
        );
        let release = layout.releases_dir().join("1.0.0");
        assert!(complete(&release));
        assert!(release.join("core/native-core.json").is_file());
        let stable = release.join(binary());
        assert_eq!(answer["command"], json!([stable]));
        assert_eq!(
            layout.connector_command(),
            Some(vec![stable.to_string_lossy().into_owned()])
        );
        assert!(layout.installed());
        // The executable bit is how Unix marks a program; Windows has none.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(release.join(binary()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }

        // The same build again: nothing staged, the selection restarted and verified (the recovery).
        let staged_at = fs::metadata(release.join(binary()))
            .unwrap()
            .modified()
            .unwrap();
        let again = fixture
            .install(&fixture.package("1.0.0", "a"))
            .await
            .unwrap();
        assert_eq!(again["action"], "reinstalled", "{again}");
        assert_eq!(
            fs::metadata(release.join(binary()))
                .unwrap()
                .modified()
                .unwrap(),
            staged_at
        );

        // Upgrades: the one before is kept, any older one pruned.
        let second = fixture
            .install(&fixture.package("2.0.0", "a"))
            .await
            .unwrap();
        assert_eq!(
            (second["action"].as_str(), second["previous"].as_str()),
            (Some("updated"), Some("1.0.0"))
        );
        assert_eq!(fixture.releases(), ["1.0.0", "2.0.0"]);
        let third = fixture
            .install(&fixture.package("3.0.0", "a"))
            .await
            .unwrap();
        assert_eq!(third["pruned"], json!(["1.0.0"]));
        assert_eq!(fixture.releases(), ["2.0.0", "3.0.0"]);
        assert_eq!(layout.selected_id().as_deref(), Some("3.0.0"));

        // A release whose pair does not answer: back to the one before, restarted, and the failed one deleted.
        fixture.fake.lock().unwrap().failing.insert("4.0.0".into());
        let error = fixture
            .install(&fixture.package("4.0.0", "a"))
            .await
            .unwrap_err();
        assert_eq!(key(&error), "install.rolled-back", "{error}");
        let keyed = crate::messages::keyed(&error);
        assert_eq!(keyed.params["previous"], "3.0.0");
        assert_eq!(keyed.params["failure"]["key"], "launch.exited");
        assert!(error.to_string().contains("back on 3.0.0"), "{error}");
        assert_eq!(layout.selected_id().as_deref(), Some("3.0.0"));
        assert_eq!(fixture.releases(), ["2.0.0", "3.0.0"]);
        let restarts = fixture.fake.lock().unwrap().restarts.clone();
        assert_eq!(
            restarts[restarts.len() - 2..],
            ["4.0.0".to_owned(), "3.0.0".to_owned()]
        );

        // Uninstall removes releases and owned data, preserving the stop.
        Files::write_json(&layout.data.join("outbox.json"), &json!([])).unwrap();
        crate::lock::try_lock(&layout.data.join("agents.lock"), "test").unwrap();
        // The shared models are another program's: if they are in `D` (un-overridden), they and what is in them survive.
        let models = layout.data.join(data_dir::MODELS).join("other-program");
        fs::create_dir_all(&models).unwrap();
        fs::write(models.join("sentinel"), "theirs").unwrap();
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
        // The models are not ours, so they and `D` stay (and our files beside them), and it is said.
        assert_eq!(
            left,
            [
                data_dir::MODELS,
                "agents.lock",
                "install.lock",
                "node-stopped.json"
            ],
            "{left:?}"
        );
        assert_eq!(
            removed["data_dir"],
            json!({"removed": false, "kept": [data_dir::MODELS], "held": []}),
            "{removed}"
        );
        assert!(human_uninstall(&removed).contains(data_dir::MODELS));
        assert_eq!(
            fs::read_to_string(models.join("sentinel")).unwrap(),
            "theirs"
        );
        assert!(!layout.installed());

        // And it installs again from nothing.
        fixture
            .install(&fixture.package("3.0.0", "a"))
            .await
            .unwrap();
        assert_eq!(fixture.releases(), ["3.0.0"]);
    }

    #[tokio::test]
    async fn a_pair_that_never_answers_ends_the_wait_at_its_limit_with_the_last_state_seen() {
        let started = Instant::now();
        let ended = wait_until(
            Duration::from_millis(80),
            Duration::from_millis(5),
            || async { Poll::Waiting(json!({"state": "backoff"})) },
        )
        .await;
        let Err(Wait::TimedOut(Some(last))) = ended else {
            panic!("the wait must time out with a state seen");
        };
        assert_eq!(last["state"], "backoff");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_look_that_itself_never_returns_is_cut_off_at_the_limit() {
        let ended = wait_until(Duration::from_millis(50), Duration::from_millis(5), || {
            std::future::pending::<Poll>()
        })
        .await;
        assert!(matches!(ended, Err(Wait::TimedOut(None))));
    }

    #[tokio::test]
    async fn a_wait_ends_at_once_when_the_pair_answers_or_fails_for_good() {
        let ready = wait_until(Duration::from_secs(5), Duration::from_millis(5), || async {
            Poll::Ready
        })
        .await;
        assert!(ready.is_ok());
        let failed = wait_until(Duration::from_secs(5), Duration::from_millis(5), || async {
            Poll::Failed(json!({"key": "bind.port-in-use"}))
        })
        .await;
        assert!(matches!(failed, Err(Wait::Failed(f)) if f["key"] == "bind.port-in-use"));
    }

    #[test]
    fn a_timeout_names_what_was_awaited_the_service_the_output_and_the_next_command() {
        let waited = Waited {
            ready: PathBuf::from("/d/core/core.json"),
            output: PathBuf::from("/d/core.stderr.log"),
        };
        let last = json!({"state": "failed", "waiting_for": "the connector (node.status unanswered)",
            "failure": {"message": "The core exited (2)."}});
        let failure = timeout_failure(
            Duration::from_secs(60),
            &waited,
            "task-scheduler job sidevoice-core-a: loaded, not running, state Ready, last exit 2",
            Some(&last),
        );
        assert_eq!(failure["key"], "ready.timeout");
        let text = failure["message"].as_str().unwrap();
        for part in [
            "within 60 s",
            "/d/core/core.json",
            "Last seen: state failed (The core exited (2).); still waiting for the connector (node.status unanswered).",
            "task-scheduler job sidevoice-core-a: loaded, not running, state Ready, last exit 2",
            "/d/core.stderr.log",
            "Next: run `npx sidevoice service status`.",
        ] {
            assert!(text.contains(part), "{part} missing in {text}");
        }
        // A state with no failure still says what was seen; no look at all says that.
        let starting = json!({"state": "starting", "waiting_for": "the core (no ready file with a health answer)"});
        let text = timeout_failure(Duration::from_secs(60), &waited, "x", Some(&starting))
            ["message"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            text.contains("state starting; still waiting for the core"),
            "{text}"
        );
        let text = timeout_failure(Duration::from_secs(60), &waited, "x", None)["message"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(text.contains("No look at the service finished."), "{text}");
    }

    #[tokio::test]
    async fn a_first_install_that_does_not_answer_stays_and_says_why() {
        let fixture = Fixture::new("first-fails");
        fixture.fake.lock().unwrap().failing.insert("1.0.0".into());
        let error = fixture
            .install(&fixture.package("1.0.0", "a"))
            .await
            .unwrap_err();
        assert_eq!(key(&error), "install.verify-failed", "{error}");
        assert!(error.to_string().contains("the core exited"), "{error}");
        assert_eq!(fixture.layout().selected_id().as_deref(), Some("1.0.0"));
    }

    #[tokio::test]
    async fn another_build_of_a_version_is_a_release_of_its_own() {
        let fixture = Fixture::new("builds");
        fixture
            .install(&fixture.package("1.0.0", "a"))
            .await
            .unwrap();
        let other = fixture.package("1.0.0", "b");
        let digest = digest_file(&other.binary).unwrap();
        let answer = fixture.install(&other).await.unwrap();
        let id = format!("1.0.0+{}", &digest[..12]);
        assert_eq!(answer["version"], id.as_str(), "{answer}");
        assert_eq!(answer["action"], "updated");
        assert_eq!(fixture.releases(), ["1.0.0".to_owned(), id]);
        assert!(fixture
            .install(&fixture.package("bad/version", "a"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn leftovers_of_a_dead_installer_are_removed() {
        let fixture = Fixture::new("leftovers");
        let layout = fixture.layout();
        fs::create_dir_all(layout.releases_dir().join(".1.0.0.partial-x/core")).unwrap();
        Files::shut_others_out(&layout.releases);
        Files::shut_others_out(&layout.releases_dir());
        fs::write(layout.releases.join(".current.json.x"), b"{}").unwrap();
        fixture
            .install(&fixture.package("1.0.0", "a"))
            .await
            .unwrap();
        assert_eq!(fixture.releases(), ["1.0.0"]);
        assert!(fs::symlink_metadata(layout.releases.join(".current.json.x")).is_err());
    }

    #[test]
    fn uninstall_removes_every_entry_of_ours_and_leaves_the_rest() {
        let row =
            |registration: &str, owned: bool| json!({"registration": registration, "owned": owned});
        assert_eq!(removal(&row("connected", true)), Some(true));
        assert_eq!(
            removal(&row("outdated", true)),
            Some(true),
            "disabled or an older release, still ours"
        );
        assert_eq!(removal(&row("not-connected", false)), None, "nothing there");
        assert_eq!(removal(&row("foreign", false)), Some(false));
        assert_eq!(removal(&row("invalid", false)), Some(false));
        assert_eq!(removal(&row("unknown", false)), Some(false));
    }

    #[test]
    fn uninstall_preserves_the_invalid_config_diagnosis() {
        for disconnect in [false, true] {
            let row = json!({"registration": "invalid", "can": {"disconnect": disconnect}});
            let (outcome, message) = unremoved(&row, "Agent");
            assert_eq!(outcome, "invalid");
            assert_eq!(
                message,
                crate::messages::message("agents.invalid", &json!({"agent": "Agent"}))
            );
        }
        let row =
            |disconnect| json!({"registration": "unknown", "can": {"disconnect": disconnect}});
        assert_eq!(
            unremoved(&row(false), "Agent").1,
            crate::messages::message("agents.left-behind", &json!({"agent": "Agent"}))
        );
        assert_eq!(
            unremoved(&row(true), "Agent").1,
            crate::messages::message("agents.registration-unknown", &json!({"agent": "Agent"}))
        );
    }

    /// An agent of the registry the connector connects by itself, without its CLI: connected in any profile of the tests
    /// (its configuration directory is there) once a stand-in of its program says it is installed.
    fn agent_without_cli() -> &'static str {
        crate::adapters::Registry::standard()
            .agents()
            .find(|(_, registration)| registration.connects_without_binary())
            .map(|(id, _)| id)
            .expect("an agent the connector connects without its CLI")
    }

    /// The agent's registration as a scan finds it, judged against the installation.
    async fn registration(profile: &Profile, id: &str) -> Value {
        let agents = HostAgents::for_installer(profile.clone()).unwrap();
        let listed = agents.handle("agents.list", json!({"rescan": true})).await;
        agent_rows(&listed)
            .into_iter()
            .find(|(found, _, _)| found == id)
            .map(|(_, _, row)| row["registration"].clone())
            .unwrap_or_default()
    }

    /// Whether `answer`'s advice includes what to do next with the agent `id` once it uses Sidevoice.
    fn advised(profile: &Profile, answer: &Value, id: &str) -> bool {
        let registry = crate::adapters::Registry::standard();
        let (_, agent) = registry.agents().find(|(found, _)| *found == id).unwrap();
        let notes = agent.after_install(profile);
        assert!(!notes.is_empty(), "{id} has advice after an install");
        let advice = answer["advice"].to_string();
        notes
            .iter()
            .all(|note| advice.contains(&json!(note).to_string()))
    }

    #[tokio::test]
    async fn installing_connects_no_agent_and_keeps_an_entry_an_older_sidevoice_wrote_working() {
        // Every scan resolves every agent: in turn with the agents' tests, which point them at stand-ins.
        let _turn = crate::agents::tests::agent_env().await;
        let fixture = Fixture::new("agents");
        let profile = &fixture.profile;
        let id = agent_without_cli();
        let program = fixture
            .scratch
            .path()
            .join(target::exe_name("agent-program"));
        crate::test_support::stand_in::Script::default().make(&program);
        let _installed = crate::agents::tests::EnvGuard::set(
            &format!("SIDEVOICE_{}_BIN", id.to_ascii_uppercase()),
            program.as_os_str(),
        );
        let label = agent_label(id);
        let with_agents = |version: &'static str| {
            let package = fixture.package(version, "a");
            let runtime = fixture.runtime();
            async move { install(profile, &package, AGENTS, &runtime).await.unwrap() }
        };

        // Before any installation, the person connected the agent with a Sidevoice run by its own path: an entry of
        // Sidevoice's that is not the installation's command. Installing writes it again with that command: the
        // person chose that agent already.
        let by_path = HostAgents::new(profile.clone()).unwrap();
        let connected = by_path.handle("agents.connect", json!({"id": id})).await;
        assert!(connected.get("error").is_none(), "{connected}");
        let first = with_agents("1.0.0").await;
        assert_eq!(first["agents"][id]["outcome"], "registered", "{first}");
        assert_eq!(registration(profile, id).await, "connected");
        assert!(advised(profile, &first, id), "{first}");

        // Its current entry is left as it is.
        let second = with_agents("2.0.0").await;
        assert_eq!(second["agents"][id]["outcome"], "registered", "{second}");
        assert_eq!(registration(profile, id).await, "connected");

        // Without Sidevoice (the person removed it), installing leaves it so and says how to connect it. No agent
        // found is connected.
        let installer = HostAgents::for_installer(profile.clone()).unwrap();
        let removed = installer
            .handle("agents.disconnect", json!({"id": id}))
            .await;
        assert!(removed.get("error").is_none(), "{removed}");
        let mut third = with_agents("2.0.0").await;
        let agent = &third["agents"][id];
        assert_eq!(agent["outcome"], "not-connected", "{third}");
        let command = format!("npx sidevoice agents connect {id}");
        assert_eq!(agent["command"], command.as_str());
        for (found, agent) in third["agents"].as_object().unwrap() {
            assert_ne!(agent["outcome"], "registered", "{found}: {third}");
        }
        assert_eq!(registration(profile, id).await, "not-connected");
        let said = human_install(&third);
        assert!(
            said.contains(&format!(
                "{label} does not use Sidevoice; to connect it, use the Sidevoice app or run `{command}`."
            )),
            "{said}"
        );
        assert!(!advised(profile, &third, id), "{third}");
        assert!(offerable(&third).contains(&(id.to_owned(), label.clone())));

        // At a terminal the person chooses it: connected as `agents connect` connects it.
        let mut asked = Vec::new();
        let chosen = ask(
            &offerable(&third),
            &mut format!("{id}\n").as_bytes(),
            &mut asked,
        )
        .unwrap();
        assert_eq!(chosen, [id]);
        connect_chosen(profile, &mut third, &chosen).await;
        assert_eq!(third["agents"][id]["outcome"], "registered", "{third}");
        assert_eq!(registration(profile, id).await, "connected");
        assert!(advised(profile, &third, id), "{third}");
    }

    #[test]
    fn the_offer_is_made_only_to_a_person_at_a_terminal() {
        let ci = |value: &str| Some(std::ffi::OsString::from(value));
        assert!(may_offer(false, false, true, None));
        for unset in ["", "0", "false"] {
            assert!(
                may_offer(false, false, true, ci(unset).as_deref()),
                "CI={unset:?}"
            );
        }
        assert!(
            !may_offer(true, false, true, None),
            "--json: a program reads the answer"
        );
        assert!(!may_offer(false, true, true, None), "--no-prompt");
        assert!(!may_offer(false, false, false, None), "no terminal");
        for set in ["true", "1"] {
            assert!(
                !may_offer(false, false, true, ci(set).as_deref()),
                "CI={set:?}"
            );
        }
    }

    #[test]
    fn the_offer_chooses_none_unless_told_all_or_which() {
        let offered = [("first", "First Agent"), ("second", "Second Agent")]
            .map(|(id, label)| (id.to_owned(), label.to_owned()));
        let answer = |input: &str| {
            let mut said = Vec::new();
            let chosen = ask(&offered, &mut input.as_bytes(), &mut said).unwrap();
            (chosen, String::from_utf8(said).unwrap())
        };
        let (chosen, said) = answer("\n");
        assert!(chosen.is_empty(), "Enter is none");
        assert!(
            said.contains("1. First Agent")
                && said.contains("2. Second Agent")
                && said.contains("[none]"),
            "{said}"
        );
        for none in ["", "none\n", "N\n", " no \n"] {
            assert!(answer(none).0.is_empty(), "{none:?}");
        }
        for all in ["all\n", "A\n"] {
            assert_eq!(answer(all).0, ["first", "second"], "{all:?}");
        }
        assert_eq!(answer("2\n").0, ["second"]);
        assert_eq!(answer("2, 1 2\n").0, ["second", "first"]);
        assert_eq!(answer("second\n").0, ["second"]);
        // An answer not understood chooses nothing and is asked again.
        let (chosen, said) = answer("3\nyes\n1\n");
        assert_eq!(chosen, ["first"]);
        assert_eq!(said.matches("[none]").count(), 3, "{said}");
        assert!(
            answer("0\n").0.is_empty(),
            "not understood, then the end of input"
        );
    }

    #[tokio::test]
    async fn installing_again_repairs_a_release_whose_core_was_damaged() {
        let fixture = Fixture::new("repair");
        let layout = fixture.layout();
        let package = fixture.package("1.0.0", "a");
        fixture.install(&package).await.unwrap();
        let first = layout.selected_id().unwrap();
        let notice = layout
            .releases_dir()
            .join(&first)
            .join(crate::service::layout::CORE_DIRECTORY)
            .join("notices/LICENSE");
        fs::write(&notice, b"damaged").unwrap();
        assert!(!core_intact(&layout.releases_dir().join(&first), &package));

        // The same package again: staged anew beside the damaged release, which stayed selected until then.
        fixture.install(&package).await.unwrap();
        let repaired = layout.selected_id().unwrap();
        assert_ne!(
            repaired, first,
            "a fresh release, not the damaged one reused"
        );
        assert!(core_intact(
            &layout.releases_dir().join(&repaired),
            &package
        ));

        // And once more: the repaired release is reused as it is.
        fixture.install(&package).await.unwrap();
        assert_eq!(layout.selected_id().unwrap(), repaired);
    }

    // A preflight refusal (here the one a mount boundary in `D` gives; the boundary itself is tested in
    // `platform::uninstall`) stops the uninstall before anything is written, the install lock included.
    #[tokio::test]
    async fn a_preflight_refusal_stops_before_install_lock_is_rewritten() {
        let fixture = Fixture::new("mounted-uninstall-data");
        let layout = fixture.layout();
        // An installation of ours in this scratch `D`, so the refusal tested is the preflight's.
        fs::write(
            layout.data.join("install.json"),
            json!({"releases": layout.releases}).to_string(),
        )
        .unwrap();
        let lock = layout.install_lock();
        fs::write(&lock, b"sentinel lock contents").unwrap();
        Files::shut_others_out(&lock);
        let error = uninstall_with_hooks(
            &fixture.profile,
            NO_AGENTS,
            &fixture.runtime(),
            |_| Err("uninstall mount or device boundary"),
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("mount or device boundary"),
            "{error}"
        );
        assert_eq!(fs::read(&lock).unwrap(), b"sentinel lock contents");
    }

    #[tokio::test]
    async fn uninstall_refuses_hard_linked_install_lock_without_touching_sentinel() {
        let fixture = Fixture::new("hard-linked-uninstall-lock");
        let outside = fixture.scratch.path().join("outside-sentinel");
        fs::write(&outside, b"outside contents").unwrap();
        Files::shut_others_out(&outside);
        fs::hard_link(&outside, fixture.layout().install_lock()).unwrap();
        assert!(uninstall(&fixture.profile, NO_AGENTS, &fixture.runtime())
            .await
            .is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"outside contents");
    }

    #[tokio::test]
    async fn uninstall_refuses_hard_linked_agents_lock_without_touching_sentinel() {
        let fixture = Fixture::new("hard-linked-uninstall-agents-lock");
        let outside = fixture.scratch.path().join("outside-agents-sentinel");
        fs::write(&outside, b"outside contents").unwrap();
        Files::shut_others_out(&outside);
        fs::hard_link(&outside, fixture.layout().data.join("agents.lock")).unwrap();
        assert!(uninstall(&fixture.profile, NO_AGENTS, &fixture.runtime())
            .await
            .is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"outside contents");
    }

    #[tokio::test]
    async fn failed_claim_keeps_recovery_cli_and_retry_clears_residual_owned_data() {
        let fixture = Fixture::new("uninstall-retry");
        let package = fixture.package("1.0.0", "a");
        fixture.install(&package).await.unwrap();
        let layout = fixture.layout();
        let victim = layout.data.join("victim");
        let moved = fixture.scratch.path().join("outside-victim");
        fs::create_dir(&victim).unwrap();
        fs::write(victim.join("keep"), b"outside contents").unwrap();
        let swapped = AtomicBool::new(false);
        let hook = |name: &OsStr| {
            if name == OsStr::new("victim") && !swapped.swap(true, Ordering::SeqCst) {
                fs::rename(&victim, &moved).unwrap();
                Files::symlink(&moved, &victim);
            }
        };
        assert!(uninstall_with_hooks(
            &fixture.profile,
            NO_AGENTS,
            &fixture.runtime(),
            |guard| guard.preflight(),
            hook,
        )
        .await
        .is_err());
        assert!(
            layout.releases.exists(),
            "the installed release is still a retry route"
        );
        let record = layout.install_record().unwrap();
        let recovery = PathBuf::from(record["command"][0].as_str().unwrap());
        assert!(
            recovery.is_file(),
            "the desktop can run the recorded recovery CLI"
        );
        let recovery_probe = std::process::Command::new(&recovery)
            .arg("--list")
            .output()
            .unwrap();
        assert!(
            recovery_probe.status.success(),
            "the staged binary must execute"
        );
        assert_eq!(fs::read(moved.join("keep")).unwrap(), b"outside contents");

        let answer = uninstall(&fixture.profile, NO_AGENTS, &fixture.runtime())
            .await
            .unwrap();
        assert_eq!(answer["state"], "absent");
        assert!(!layout.releases.exists());
        assert!(!layout.install_file().exists());
        assert_eq!(fs::read(moved.join("keep")).unwrap(), b"outside contents");
    }
}
