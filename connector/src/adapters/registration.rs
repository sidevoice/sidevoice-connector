//! Registering Sidevoice with an agent: the trait a harness implements, and what the host-agent coordinator lends
//! it for one request.

use crate::profile::Profile;
use futures_util::future::BoxFuture;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

/// Why a registration step stopped: the request was cancelled or ran out of time, or a keyed failure about the agent.
#[derive(Debug)]
pub enum AgentError {
    Cancelled,
    Busy,
    Key(&'static str),
}

/// What an agent's CLI printed and how it exited.
pub struct CliOutput {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

/// Why an agent's CLI gave no output: the request was cancelled, or it could not start, finish or be read in time.
#[derive(Debug)]
pub enum CliError {
    Cancelled,
    Failed,
}

/// How the agent starts the server its entry names: the command, its arguments and the environment the entry gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// What the agent's configuration says of Sidevoice's entry: the registration's state and, when the entry could be
/// read, how the agent starts it.
#[derive(Clone, Debug)]
pub struct Entry {
    pub registration: String,
    pub launch: Option<Launch>,
}

impl Entry {
    /// A state with no entry to start.
    pub fn bare(registration: &str) -> Self {
        Self {
            registration: registration.to_owned(),
            launch: None,
        }
    }

    pub fn with(registration: &str, launch: Launch) -> Self {
        Self {
            registration: registration.to_owned(),
            launch: Some(launch),
        }
    }
}

/// What the coordinator lends a registration for one request on one agent.
pub trait AgentHost: Send + Sync {
    fn profile(&self) -> &Profile;
    /// The command this installation registers, and its arguments.
    fn command(&self) -> &str;
    fn args(&self) -> &[String];
    /// Whether an entry running `command args` is this installation's (any of its releases).
    fn owns(&self, command: &str, args: &[String]) -> bool;
    /// The environment a registration hands the server.
    fn mcp_env(&self) -> BTreeMap<String, String>;
    /// Runs the agent's CLI with its home, under the request's cancellation and deadline.
    fn run<'a>(
        &'a self,
        binary: &'a str,
        args: Vec<String>,
    ) -> BoxFuture<'a, Result<CliOutput, CliError>>;
    /// Fails when the request was cancelled or ran out of time.
    fn live(&self) -> Result<(), AgentError>;
}

/// Registering Sidevoice with an agent, reading that registration back and removing it, touching only an entry
/// Sidevoice wrote.
pub trait Registration: Send + Sync {
    /// The agent's home.
    fn config_root<'a>(&self, profile: &'a Profile) -> &'a Path;
    /// Its CLI's names, as found on PATH.
    fn binary_names(&self) -> &'static [&'static str];
    /// Where its CLI is installed when PATH does not reach it.
    fn known_paths(&self, home: &Path) -> Vec<PathBuf>;
    /// Evidence, beyond its home and its CLI, that the agent is on this computer: `(kind, path)`.
    fn evidence(&self) -> Vec<(&'static str, PathBuf)> {
        Vec::new()
    }
    /// Whether Sidevoice registers with it without its CLI.
    fn connects_without_binary(&self) -> bool {
        false
    }
    /// The registration's state: `connected`, `outdated` (ours, not current), `not-connected`, `foreign`, `invalid`
    /// or `unknown`; with the entry's launch, as the agent would run it, whenever its configuration names one.
    fn state<'a>(
        &'a self,
        host: &'a dyn AgentHost,
        binary: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Entry, AgentError>>;
    fn connect<'a>(
        &'a self,
        host: &'a dyn AgentHost,
        binary: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), AgentError>>;
    fn disconnect<'a>(
        &'a self,
        host: &'a dyn AgentHost,
        binary: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), AgentError>>;
    /// How a person registers it by hand: `{command, file, snippet}`.
    fn instructions(&self, host: &dyn AgentHost, registration: &str, binary: Option<&str>)
        -> Value;
}

/// Runs the agent's CLI and requires it to succeed.
pub async fn run_required(
    host: &dyn AgentHost,
    binary: &str,
    args: &[String],
) -> Result<(), AgentError> {
    let output = match host.run(binary, args.to_vec()).await {
        Ok(output) => output,
        Err(CliError::Cancelled) => return Err(AgentError::Cancelled),
        Err(CliError::Failed) => return Err(AgentError::Key("agents.action-failed")),
    };
    host.live()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(AgentError::Key("agents.action-failed"))
    }
}

/// Owned strings of `args`.
pub fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|value| (*value).to_owned()).collect()
}

/// Whether a CLI's answer says the entry does not exist.
pub fn looks_absent(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "no such server",
        "no such mcp server",
        "no mcp server",
        "no such mcp entry",
        "not found",
        "does not exist",
        "no entry",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// `command args`, each quoted for a POSIX shell.
pub fn shell_command(command: &str, args: &[String]) -> String {
    std::iter::once(command)
        .chain(args.iter().map(String::as_str))
        .map(|value| format!("'{}'", value.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Serialize)]
struct CustomConfig<'a> {
    #[serde(rename = "mcpServers")]
    mcp_servers: CustomServers<'a>,
}

#[derive(Serialize)]
struct CustomServers<'a> {
    sidevoice: CustomServer<'a>,
}

#[derive(Serialize)]
struct CustomServer<'a> {
    command: &'a str,
    args: &'a [String],
    env: BTreeMap<String, String>,
}

/// The `mcpServers` entry for Sidevoice, as JSON configuration files write it.
pub fn mcp_servers_json(
    command: &str,
    args: &[String],
    env: BTreeMap<String, String>,
) -> serde_json::Result<String> {
    serde_json::to_string_pretty(&CustomConfig {
        mcp_servers: CustomServers {
            sidevoice: CustomServer { command, args, env },
        },
    })
}
