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

/// How the agent starts the server its entry names: the command, its arguments, the environment the entry gives it and
/// the working directory it names, if any.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Launch {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
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
    /// How to start this agent against the test bench's profile, when it can be: printed when the bench starts.
    fn bench_steps(&self, _setup: &BenchSetup) -> Option<String> {
        None
    }

    /// What is left for the person after an install that registered Sidevoice with this agent.
    fn after_install(&self, _profile: &Profile) -> Vec<String> {
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

/// The test bench's profile, as an agent is pointed at it: what [`Registration::bench_steps`] writes its steps with.
pub struct BenchSetup<'a> {
    /// The script to source before starting the agent.
    pub env_script: &'a str,
    /// An `mcpServers` configuration file naming the installation.
    pub mcp_config: &'a str,
    /// The installation's MCP server command, quoted for a shell.
    pub server: &'a str,
    /// The environment the server runs with.
    pub env: &'a BTreeMap<String, String>,
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

/// A file in an agent's home, read only when it is the person's own (a regular file, writable by nobody else) and at
/// most `limit` bytes. With `root`, it must be the file itself, not a link, inside `root`. Without, a link is followed
/// wherever it leads (a dotfiles manager's, say), and the file it leads to is judged. A FIFO or a device is refused
/// without waiting on it.
pub fn read_profile_file(path: &Path, root: Option<&Path>, limit: u64) -> anyhow::Result<String> {
    use anyhow::Context;
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let regular = |metadata: &std::fs::Metadata| {
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o022 == 0
            && metadata.len() <= limit
    };
    let mut flags = libc::O_NONBLOCK | libc::O_NOCTTY;
    if let Some(root) = root {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink()
            || !regular(&metadata)
            || !path.canonicalize()?.starts_with(root)
        {
            anyhow::bail!("unsafe profile file");
        }
        flags |= libc::O_NOFOLLOW;
    } else if !regular(&std::fs::metadata(path)?) {
        anyhow::bail!("unsafe profile file");
    }
    // Opened without waiting (a FIFO put there since would wait for a writer), then judged again as opened.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)
        .context("open profile file")?;
    if !regular(&file.metadata()?) {
        anyhow::bail!("unsafe profile file");
    }
    let mut text = String::new();
    file.take(limit)
        .read_to_string(&mut text)
        .context("read profile file")?;
    Ok(text)
}

/// What the test bench asks the connector about the agents (`bench-agents`, a separate binary that cannot read the
/// registry itself):
/// - `{"root": R}`: each declared agent home, below `R/agents/`, and each agent's CLI as this `PATH` finds it
///   (`SIDEVOICE_<ID>_BIN`), as `{"homes": {VAR: path}, "binaries": {VAR: path}}`;
/// - `{"setup": {env_script, mcp_config, server, env}}`: how to start each agent against the bench, `{"steps": […]}`.
pub fn bench_agents(request: &Value) -> Value {
    let registry = super::Registry::standard();
    if let Some(root) = request.get("root").and_then(Value::as_str) {
        let homes: serde_json::Map<String, Value> = registry
            .homes()
            .iter()
            .map(|home| {
                let dir = Path::new(root)
                    .join("agents")
                    .join(home.var.to_ascii_lowercase());
                (
                    home.var.to_owned(),
                    Value::from(dir.to_string_lossy().into_owned()),
                )
            })
            .collect();
        let path = std::env::var("PATH").unwrap_or_default();
        let binaries: serde_json::Map<String, Value> = registry
            .agents()
            .filter_map(|(id, registration)| {
                let found = registration.binary_names().iter().find_map(|name| {
                    path.split(':')
                        .filter(|dir| !dir.is_empty())
                        .map(|dir| Path::new(dir).join(name))
                        .find(|candidate| executable(candidate))
                })?;
                Some((
                    format!("SIDEVOICE_{}_BIN", id.to_ascii_uppercase()),
                    Value::from(found.to_string_lossy().into_owned()),
                ))
            })
            .collect();
        return serde_json::json!({"homes": homes, "binaries": binaries});
    }
    let setup = request.get("setup").cloned().unwrap_or_default();
    let text = |key: &str| {
        setup
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let env: BTreeMap<String, String> = setup
        .get("env")
        .and_then(Value::as_object)
        .map(|env| {
            env.iter()
                .map(|(name, value)| (name.clone(), value.as_str().unwrap_or("").to_owned()))
                .collect()
        })
        .unwrap_or_default();
    let (env_script, mcp_config, server) = (text("env_script"), text("mcp_config"), text("server"));
    let bench = BenchSetup {
        env_script: &env_script,
        mcp_config: &mcp_config,
        server: &server,
        env: &env,
    };
    let steps: Vec<String> = registry
        .agents()
        .filter_map(|(_, registration)| registration.bench_steps(&bench))
        .collect();
    serde_json::json!({"steps": steps})
}

/// A file anyone may run.
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// The bench's CLI lookup passes over a file of an agent's CLI name that cannot be run, as a shell would.
    #[test]
    fn a_cli_that_cannot_be_run_is_passed_over() {
        let scratch = crate::secure_fs::tests::Scratch::new("bench-cli");
        let (first, second) = (scratch.0.join("first"), scratch.0.join("second"));
        let name = super::super::Registry::standard()
            .agents()
            .next()
            .map(|(_, registration)| registration.binary_names()[0])
            .unwrap();
        for (dir, mode) in [(&first, 0o644), (&second, 0o755)] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join(name), "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(mode))
                .unwrap();
        }
        assert!(!executable(&first.join(name)));
        assert!(executable(&second.join(name)));
    }
}
