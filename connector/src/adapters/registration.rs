//! Registering Sidevoice with an agent: the trait a harness implements, and what the host-agent coordinator lends
//! it for one request.

use crate::messages::message;
use crate::profile::Profile;
use futures_util::future::BoxFuture;
use serde::Serialize;
use serde_json::json;
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
    /// or `unknown`.
    fn state<'a>(
        &'a self,
        host: &'a dyn AgentHost,
        binary: Option<&'a str>,
    ) -> BoxFuture<'a, Result<String, AgentError>>;
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
    /// How a person does each operation by hand, as ordered steps, for the registration `state` reported.
    fn manual(&self, host: &dyn AgentHost, registration: &str, binary: Option<&str>) -> Manual;
}

/// One step done by hand: a command to run, or a snippet for a file. `file` is the file the step changes, when
/// known.
#[derive(Serialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct ManualStep {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// How a person registers Sidevoice with an agent by hand (`connect`), removes it (`disconnect`), and replaces another
/// "sidevoice" entry with it (`replace`), each as steps in order.
#[derive(Serialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Manual {
    pub connect: Vec<ManualStep>,
    pub disconnect: Vec<ManualStep>,
    pub replace: Vec<ManualStep>,
}

impl Manual {
    /// An agent whose CLI adds and removes the entry. Connecting adds ours, after removing ours first when it is
    /// `outdated` (the CLI refuses to add over it); replacing removes the other entry (`remove_other`), then adds ours.
    pub fn by_cli(
        registration: &str,
        add: ManualStep,
        remove: ManualStep,
        remove_other: ManualStep,
    ) -> Self {
        let connect = if registration == "outdated" {
            vec![remove.clone(), add.clone()]
        } else {
            vec![add.clone()]
        };
        Self {
            connect,
            disconnect: vec![remove],
            replace: vec![remove_other, add],
        }
    }

    /// An agent configured by a file: connecting adds `snippet` to `file`, disconnecting deletes the "sidevoice" entry
    /// under `under`, and replacing deletes the other one there first.
    pub fn by_file(file: &str, snippet: String, under: &str) -> Self {
        let step = |snippet: String| ManualStep {
            command: None,
            snippet: Some(snippet),
            file: Some(file.to_owned()),
        };
        let params = json!({"under": under});
        let add = step(snippet);
        Self {
            connect: vec![add.clone()],
            disconnect: vec![step(message("agents.manual.delete-entry", &params))],
            replace: vec![
                step(message("agents.manual.delete-other-entry", &params)),
                add,
            ],
        }
    }
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

/// `value` quoted for a POSIX shell.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// `command args`, each quoted for a POSIX shell.
pub fn shell_command(command: &str, args: &[String]) -> String {
    std::iter::once(command)
        .chain(args.iter().map(String::as_str))
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

/// `command args` for a POSIX shell, run with the agent's home in `variable`, as the connector runs its CLI: a step
/// done by hand then changes the same profile as the connector would.
pub fn shell_command_in(variable: &str, home: &Path, command: &str, args: &[String]) -> String {
    format!(
        "{variable}={} {}",
        shell_quote(&home.to_string_lossy()),
        shell_command(command, args)
    )
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
