//! Registering Sidevoice with an agent: the trait a harness implements, and what the host-agent coordinator lends
//! it for one request.

use crate::messages::message;
use crate::profile::Profile;
use futures_util::future::BoxFuture;
use serde::Serialize;
use serde_json::{json, Value};
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
    /// OS registration identity for a desktop app, when this adapter has one.
    fn app_identity(&self) -> Option<crate::adapters::app::Identity> {
        None
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

/// How a person registers Sidevoice with an agent by hand (`connect`), updates an entry an older Sidevoice wrote
/// (`update`: removes ours, then adds the current one), removes it (`disconnect`), and replaces another "sidevoice"
/// entry with it (`replace`), each as steps in order.
#[derive(Serialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Manual {
    pub connect: Vec<ManualStep>,
    pub update: Vec<ManualStep>,
    pub disconnect: Vec<ManualStep>,
    pub replace: Vec<ManualStep>,
}

impl Manual {
    /// `steps` as the one step the older fields (`instructions`, `removal`) hold: commands joined with `&&`, each run
    /// once the one before succeeded, else the last step that gives a snippet (the one that adds), with its file.
    pub fn as_one(steps: &[ManualStep]) -> Option<ManualStep> {
        if !steps.is_empty() && steps.iter().all(|step| step.command.is_some()) {
            let command = steps
                .iter()
                .filter_map(|step| step.command.as_deref())
                .collect::<Vec<_>>()
                .join(" && ");
            return Some(ManualStep {
                command: Some(command),
                snippet: None,
                file: steps.last().and_then(|step| step.file.clone()),
            });
        }
        steps
            .iter()
            .rev()
            .find(|step| step.snippet.is_some())
            .or(steps.last())
            .cloned()
    }

    /// An agent whose CLI adds and removes the entry. Connecting adds ours, after removing ours first when it is
    /// `outdated` (the CLI refuses to add over it); updating removes ours, then adds it; replacing removes the other
    /// entry (`remove_other`), then adds ours.
    pub fn by_cli(
        registration: &str,
        add: ManualStep,
        remove: ManualStep,
        remove_other: ManualStep,
    ) -> Self {
        let update = vec![remove.clone(), add.clone()];
        let connect = if registration == "outdated" {
            update.clone()
        } else {
            vec![add.clone()]
        };
        Self {
            connect,
            update,
            disconnect: vec![remove],
            replace: vec![remove_other, add],
        }
    }

    /// An agent configured by a file: connecting adds `snippet` to `file`, disconnecting deletes the "sidevoice" entry
    /// under `under`, updating deletes it and adds `snippet` again, and replacing deletes the other one there first.
    pub fn by_file(file: &str, snippet: String, under: &str) -> Self {
        let step = |snippet: String| ManualStep {
            command: None,
            snippet: Some(snippet),
            file: Some(file.to_owned()),
        };
        let params = json!({"under": under});
        let add = step(snippet);
        let delete = step(message("agents.manual.delete-entry", &params));
        Self {
            connect: vec![add.clone()],
            update: vec![delete.clone(), add.clone()],
            disconnect: vec![delete],
            replace: vec![
                step(message("agents.manual.delete-other-entry", &params)),
                add,
            ],
        }
    }
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

/// `command args` for a POSIX shell, run with the agent's home `variable` as the connector runs its CLI: named when the
/// person's environment sets it, else left to the agent's own default, as in the person's shell. A step done by hand
/// then changes the same configuration as the connector would.
pub fn shell_command_in(
    profile: &Profile,
    variable: &str,
    command: &str,
    args: &[String],
) -> String {
    if !profile.agent_home_set(variable) {
        return shell_command(command, args);
    }
    format!(
        "{variable}={} {}",
        shell_quote(&profile.agent_home(variable).to_string_lossy()),
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

/// A file in an agent's home, read only when it is the person's own (a regular file, writable by nobody else) and at
/// most `limit` bytes. A link is followed wherever it leads (a dotfiles manager's, say), and the file it leads to is
/// judged. A FIFO or a device is refused without waiting on it.
pub fn read_profile_file(path: &Path, limit: u64) -> anyhow::Result<String> {
    use anyhow::Context;
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let regular = |metadata: &std::fs::Metadata| {
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o022 == 0
            && metadata.len() <= limit
    };
    if !regular(&std::fs::metadata(path)?) {
        anyhow::bail!("unsafe profile file");
    }
    // Opened without waiting (a FIFO put there since would wait for a writer), then judged again as opened.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
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

    /// The older single-step fields: commands run in order, each once the one before succeeded; file steps keep the
    /// snippet that adds.
    #[test]
    fn a_sequence_as_one_step_does_the_whole_operation() {
        let command = |text: &str| ManualStep {
            command: Some(text.into()),
            snippet: None,
            file: Some("/f".into()),
        };
        assert_eq!(
            Manual::as_one(&[command("remove"), command("add")]),
            Some(ManualStep {
                command: Some("remove && add".into()),
                snippet: None,
                file: Some("/f".into()),
            })
        );
        let file = Manual::by_file("/mcp.json", "{\"add\":1}".into(), "mcpServers");
        assert_eq!(Manual::as_one(&file.update), file.connect.first().cloned());
        assert_eq!(
            Manual::as_one(&file.disconnect),
            file.disconnect.first().cloned()
        );
        assert_eq!(Manual::as_one(&[]), None);
    }
}
