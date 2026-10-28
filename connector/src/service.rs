//! `sidevoice service install | uninstall | start | stop | restart | status | login on|off|status [--json]`: Sidevoice
//! at login, as two jobs of this user's service manager (`platform::service`: launchd on macOS, systemd's user manager
//! on Linux, Task Scheduler on Windows), and nothing of ours supervising either; and, where there is no manager,
//! Sidevoice on demand (`launcher`).
//!
//! Whether the jobs start at the next login is the person's (`service login`) and is kept by the manager alone,
//! never stored: a new service starts at login; `start`, `restart`, `install` over defined jobs and an update leave
//! it as it is; and turning it on or off neither starts nor stops anything.
//!
//! A stop is a person's: `D/node-stopped.json` is written before the manager is asked, and while it is there the
//! launcher starts nothing; `service start` clears it, and so does the connector job starting at the next login.
//! Every command that changes something holds the install lock. Linux keeps a user's services only while that user
//! has a session; running them without one is `loginctl enable-linger`, a system setting the machine's owner
//! enables: printed, with the reason, and never run.
//!
//! What the node is doing is never stored (`status`): read fresh from the manager, the core's failure report and
//! its health, the same answer for `service status --json` and for a connector's `node.status`.

pub mod background;
pub mod commands;
pub mod definition;
pub mod jobs;
pub mod launcher;
pub mod layout;
pub mod status;

use crate::lock::Lock;
use crate::platform::service::Service;
use crate::profile::Profile;
use jobs::Job;
use layout::Layout;
use serde_json::{json, Value};
use std::fmt::{Display, Formatter};
use std::fs;
use std::io;
use std::path::PathBuf;
use tokio::time::Duration;

const LOCK_WAIT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Install,
    Start,
    Stop,
    Restart,
    Status,
    Uninstall,
    /// `service login on|off|status`: `Some(on)` sets it, `None` reads it.
    Login(Option<bool>),
}

/// A refusal: a stable key, its parameters, and (for a core that failed) the failure as the core described it.
#[derive(Debug)]
pub struct Failure {
    pub key: String,
    params: Value,
    failure: Option<Value>,
}

impl Failure {
    pub fn keyed(key: &str, params: Value) -> Self {
        Self::keyed_owned(key.to_owned(), params)
    }

    pub fn keyed_owned(key: String, params: Value) -> Self {
        Self {
            key,
            params,
            failure: None,
        }
    }

    fn described(key: String, failure: Value) -> Self {
        Self {
            params: json!({"detail": failure.get("detail").cloned().unwrap_or(Value::Null)}),
            key,
            failure: Some(failure),
        }
    }

    pub fn plain(error: impl Display) -> Self {
        let detail: String = error
            .to_string()
            .chars()
            .filter(|ch| !ch.is_control())
            .take(240)
            .collect();
        Self::keyed("service.failed", json!({"detail": detail}))
    }

    pub fn message(&self) -> String {
        match self
            .failure
            .as_ref()
            .and_then(|failure| failure.get("message"))
            .and_then(Value::as_str)
        {
            Some(message) => message.to_owned(),
            None => crate::messages::message(&self.key, &self.params),
        }
    }

    /// `{ok: false, error: {key, params, message}}`, as every `--json` failure.
    pub fn value(&self) -> Value {
        json!({"ok": false, "error": {"key": self.key, "params": self.params, "message": self.message()}})
    }
}

impl Display for Failure {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for Failure {}

pub type Result<T> = std::result::Result<T, Failure>;

/// `D`, created private when missing.
fn data_dir(layout: &Layout) -> Result<()> {
    crate::private_dir::ensure(&layout.data).map_err(Failure::plain)
}

/// The install lock, waited for up to 30 s.
pub async fn install_lock(layout: &Layout) -> Result<Lock> {
    data_dir(layout)?;
    crate::lock::wait_lock(&layout.install_lock(), "service", LOCK_WAIT)
        .await
        .map_err(Failure::plain)?
        .ok_or_else(|| {
            Failure::keyed(
                "service.busy",
                json!({"detail": "another install or service command"}),
            )
        })
}

pub fn clear_stop(layout: &Layout) -> Result<()> {
    match fs::remove_file(layout.stop_marker()) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(Failure::plain(error)),
        _ => Ok(()),
    }
}

/// Both definitions written for the selected installation and both jobs started; with `restart`, restarted even
/// when their definitions did not change, so that both run what `R/current.json` names now. When no job was defined,
/// anything running on demand is stopped first, and both start at login; otherwise that stays as it was. The caller
/// holds the install lock.
pub async fn define_and_start(layout: &Layout, restart: bool) -> Result<()> {
    let definitions = definition::definitions(layout)?;
    let service = jobs::manager(layout);
    let had = jobs::installed(layout).await;
    let login = jobs::keeps_login(&service, had.as_ref()).await;
    let mut changed = Vec::new();
    for (job, definition) in &definitions {
        match service.define(definition).await {
            Ok(true) => changed.push(*job),
            Ok(false) => {}
            Err(refusal) => return Err(jobs::failure(&service, refusal).await),
        }
    }
    if had.is_none() {
        stop_on_demand_or_fail(layout).await?;
    }
    jobs::start(layout, &service, &changed, restart, login, &Job::ALL).await
}

/// What runs outside a manager stopped (`launcher::stop_on_demand`), or the refusal naming what is still running.
pub async fn stop_on_demand_or_fail(layout: &Layout) -> Result<launcher::Teardown> {
    let down = launcher::stop_on_demand(layout).await;
    if !down.left.is_empty() {
        return Err(Failure::keyed(
            "service.unload-failed",
            json!({"detail": format!("still running: {}", down.left.join(", "))}),
        ));
    }
    Ok(down)
}

/// `node.status` for a connector answering it itself.
pub async fn node_status(profile: &Profile) -> Value {
    status::status(&Layout::from_profile(profile), Some(true)).await
}

/// A connector started by its manager at login clears a person's stop, unless a service command (which holds the
/// install lock) is under way: then the stop being written now is the one that holds. Whether it may serve.
pub async fn managed_start_clears_stop(profile: &Profile) -> anyhow::Result<bool> {
    let layout = Layout::from_profile(profile);
    if !status::stopped(&layout) {
        return Ok(true);
    }
    match crate::lock::wait_lock(&layout.install_lock(), "login", Duration::from_secs(2)).await? {
        Some(_lock) => {
            clear_stop(&layout)?;
            Ok(true)
        }
        None => Ok(!status::stopped(&layout)),
    }
}

/// The supervisor a connector keeps over a core it starts (none while a core job is defined).
pub fn core_supervisor(profile: &Profile) -> launcher::CoreSupervisor {
    launcher::CoreSupervisor::new(Layout::from_profile(profile))
}

/// Get this machine's connector answering for an MCP server.
pub async fn ensure_connector(profile: &Profile) -> anyhow::Result<()> {
    let layout = Layout::from_profile(profile);
    let fallback = vec![std::env::current_exe()?.to_string_lossy().into_owned()];
    let environment: Vec<(String, PathBuf)> = profile
        .variables()
        .into_iter()
        .map(|(name, path)| (name.to_owned(), path.to_path_buf()))
        .collect();
    launcher::ensure_connector(&layout, fallback, environment, || verify_connector(profile))
        .await
        .map_err(anyhow::Error::new)
}

/// The connector on the socket answers.
async fn verify_connector(profile: &Profile) -> bool {
    let layout = Layout::from_profile(profile);
    launcher::ask_connector(&layout, "identity", json!({}), Duration::from_secs(1))
        .await
        .is_some()
}
