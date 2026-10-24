//! Registering Sidevoice with an agent: the trait a harness implements, and what the host-agent coordinator lends
//! it for one request.

use crate::messages::message;
use crate::platform::env::{Env, Environment};
use crate::platform::fs::{Files, Fs};
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

/// How the agent starts the server its entry .find_map(|name| Environment::find_program(name, &path))?s: the command, its arguments, the environment the entry gives it and
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

/// One agent-owned readiness check, with a stable code and English fallback when it is not OK.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadinessCheck {
    Ok,
    NeedsAttention(&'static str, &'static str),
    Unsupported(&'static str, &'static str),
}

pub fn aggregate_readiness(checks: &[ReadinessCheck]) -> Option<Value> {
    if checks.is_empty() {
        return None;
    }
    let state = if checks
        .iter()
        .any(|check| matches!(check, ReadinessCheck::Unsupported(_, _)))
    {
        "unsupported"
    } else if checks
        .iter()
        .any(|check| matches!(check, ReadinessCheck::NeedsAttention(_, _)))
    {
        "needs_attention"
    } else {
        "ready"
    };
    let reasons = checks
        .iter()
        .filter_map(|check| match check {
            ReadinessCheck::Ok => None,
            ReadinessCheck::NeedsAttention(code, message)
            | ReadinessCheck::Unsupported(code, message) => {
                Some(json!({"code":code,"message":message}))
            }
        })
        .collect::<Vec<_>>();
    Some(json!({"state":state,"reasons":reasons}))
}

/// Attach only nonempty agent checks; an absent field means ready.
pub fn with_readiness(mut row: Value, checks: Vec<ReadinessCheck>) -> Value {
    if let Some(value) = aggregate_readiness(&checks) {
        row["readiness"] = value;
    }
    row
}

/// Registering Sidevoice with an agent, reading its state, and removing only an entry Sidevoice wrote.
pub trait Registration: Send + Sync {
    /// The agent's home.
    fn config_root<'a>(&self, profile: &'a Profile) -> &'a Path;
    /// Its CLI's names, as found on PATH.
    fn binary_names(&self) -> &'static [&'static str];
    /// Where its CLI may be when it is not on `PATH`, most preferred first. Looked at only after `PATH`, by file
    /// metadata; the first that can be run is the one used, as one found on `PATH` would be.
    fn fallback_binaries(
        &self,
        _roots: &crate::adapters::markers::Roots,
    ) -> Vec<std::path::PathBuf> {
        Vec::new()
    }
    /// The npm package and native program layout, when this harness publishes one.
    /// Shared resolution reads package-manager shims and follows Node dependency lookup.
    fn npm_package(&self) -> Option<crate::agents::npm::NativePackage> {
        None
    }
    /// Fixed, passive filesystem markers for this agent. CLI names above use the captured login PATH.
    fn install_markers(&self) -> &'static [crate::adapters::markers::Marker] {
        &[]
    }
    /// Checks specific to this agent; an empty list needs no readiness field.
    fn readiness_checks(&self, _profile: &Profile) -> Vec<ReadinessCheck> {
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
    /// `steps` as the one step the older fields (`instructions`, `removal`) hold: commands joined as this OS's shell
    /// runs each once the one before succeeded ([`Env::in_sequence`]), else the last step that gives a snippet (the one that adds), with its file.
    pub fn as_one(steps: &[ManualStep]) -> Option<ManualStep> {
        if !steps.is_empty() && steps.iter().all(|step| step.command.is_some()) {
            let commands: Vec<&str> = steps
                .iter()
                .filter_map(|step| step.command.as_deref())
                .collect();
            let command = Environment::in_sequence(&commands);
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

impl BenchSetup<'_> {
    /// One line that sources [`Self::env_script`] (`. 'script'`, read alike by a POSIX shell and PowerShell), then
    /// runs `command args`, then `tail` (shell text as given, e.g. [`Self::server`]) after it: in sequence
    /// ([`Env::in_sequence`]), the command only once the script was read.
    pub fn line(&self, command: &str, args: &[String], tail: Option<&str>) -> String {
        let source = format!(". {}", Environment::quote(self.env_script));
        let mut run = shell_command(command, args);
        if let Some(tail) = tail {
            run.push(' ');
            run.push_str(tail);
        }
        Environment::in_sequence(&[&source, &run])
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

/// `command args`, each quoted for the shell a person types into on this OS (a POSIX shell; PowerShell on Windows).
pub fn shell_command(command: &str, args: &[String]) -> String {
    let words = std::iter::once(command)
        .chain(args.iter().map(String::as_str))
        .map(Environment::quote)
        .collect::<Vec<_>>()
        .join(" ");
    format!("{}{words}", Environment::CALL)
}

/// [`shell_command`], or `None` when this OS's shell cannot be trusted to read it as written ([`Env::hand_safe`]:
/// on Windows, a batch shim or an unknown target with `& | < > ^ % !` or `"` in the program or an argument).
pub fn hand_command(command: &str, args: &[String]) -> Option<String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    Environment::hand_safe(command, &words).then(|| shell_command(command, args))
}

/// [`hand_command`] run with the agent's home `variable` as the connector runs its CLI: named when the person's
/// environment sets it, else left to the agent's own default, as in the person's shell. A step done by hand then
/// changes the same configuration as the connector would.
pub fn hand_command_in(
    profile: &Profile,
    variable: &str,
    command: &str,
    args: &[String],
) -> Option<String> {
    let line = hand_command(command, args)?;
    if !profile.agent_home_set(variable) {
        return Some(line);
    }
    let home = Environment::quote(&profile.agent_home(variable).to_string_lossy());
    Some(format!("{}{line}", Environment::set(variable, &home)))
}

/// The step that runs `command args` by hand ([`hand_command_in`]) and changes `file`; when no line can be made
/// safely, a note saying so in place of the command.
pub fn cli_step(
    profile: &Profile,
    variable: &str,
    command: &str,
    args: &[String],
    file: &Path,
) -> ManualStep {
    let file = Some(file.to_string_lossy().into_owned());
    match hand_command_in(profile, variable, command, args) {
        Some(line) => ManualStep {
            command: Some(line),
            snippet: None,
            file,
        },
        None => ManualStep {
            command: None,
            snippet: Some(message(
                "agents.manual.unsafe-command",
                &json!({"program": command}),
            )),
            file,
        },
    }
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
    let bytes = Files::read_trusted(&path.canonicalize()?, limit)
        .context("unsafe profile file")?
        .context("missing profile file")?;
    String::from_utf8(bytes).context("profile file is not UTF-8")
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
        let path = std::env::var_os("PATH").unwrap_or_default();
        let binaries: serde_json::Map<String, Value> = registry
            .agents()
            .filter_map(|(id, registration)| {
                let found = registration
                    .binary_names()
                    .iter()
                    .find_map(|name| Environment::find_program(name, &path))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The bench's lines source its script, then run the agent, in sequence as this OS's shell reads them.
    #[test]
    fn bench_steps_source_the_profile_then_run_the_agent_in_sequence() {
        let answer = bench_agents(
            &json!({"setup": {"env_script": "/b/env.sh", "mcp_config": "/b/mcp.json",
            "server": "'/r/sidevoice' mcp", "env": {"SIDEVOICE_DATA_DIR": "/d"}}}),
        );
        let steps: Vec<&str> = answer["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|step| step.as_str().unwrap())
            .collect();
        let source = format!(". {}", Environment::quote("/b/env.sh"));
        let line = |run: String| Environment::in_sequence(&[&source, &run]);
        let claude = line(shell_command(
            "claude",
            &strings(&["--mcp-config", "/b/mcp.json", "--strict-mcp-config"]),
        ));
        let codex_add = line(format!(
            "{} '/r/sidevoice' mcp",
            shell_command(
                "codex",
                &strings(&[
                    "mcp",
                    "add",
                    "sidevoice",
                    "--env",
                    "SIDEVOICE_DATA_DIR=/d",
                    "--"
                ])
            )
        ));
        for expected in [claude, codex_add, line(shell_command("codex", &[]))] {
            assert!(
                steps
                    .iter()
                    .any(|step| step.lines().any(|text| text.trim() == expected)),
                "{expected} in {steps:?}"
            );
        }
    }
    /// The shell reads each word as given, except where a batch shim would be re-parsed by `cmd.exe`: there no line
    /// is made when any word has `& | < > ^ % !` or `"`.
    #[test]
    fn a_line_to_paste_is_made_only_where_the_shell_reads_it_as_written() {
        let args = |words: &[&str]| strings(words);
        let quoted = if cfg!(windows) {
            "& 'claude' 'mcp' 'add'"
        } else {
            "'claude' 'mcp' 'add'"
        };
        assert_eq!(
            hand_command("claude", &args(&["mcp", "add"])).as_deref(),
            Some(quoted)
        );
        for metacharacter in ["&", "|", "<", ">", "^", "%", "!", "\""] {
            let path = format!("C:\\R{metacharacter}D\\sidevoice");
            // A batch shim, or a name that may be one: refused on Windows.
            let expected = |line: String| (!cfg!(windows)).then_some(line);
            assert_eq!(
                hand_command("claude.cmd", &args(&["--", &path])),
                expected(shell_command("claude.cmd", &args(&["--", &path]))),
                "{metacharacter} in an argument of a .cmd"
            );
            assert_eq!(
                hand_command(&format!("C:\\R{metacharacter}D\\claude.bat"), &[]),
                expected(shell_command(
                    &format!("C:\\R{metacharacter}D\\claude.bat"),
                    &[]
                )),
                "{metacharacter} in the path of a .bat"
            );
            assert_eq!(
                hand_command("claude", &args(&[&path])),
                expected(shell_command("claude", &args(&[&path]))),
                "{metacharacter} in an argument of an unknown target"
            );
            // An executable is started directly: PowerShell's quoting is enough (but for `"`, see `Env::quote`).
            if metacharacter != "\"" {
                assert!(hand_command("C:\\bin\\claude.exe", &args(&[&path])).is_some());
            }
        }
        // Spaces and quotes of the typographic kind are not cmd.exe syntax.
        assert!(hand_command("claude.cmd", &args(&["C:\\Program Files\\x", "it's"])).is_some());
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
                command: Some(Environment::in_sequence(&["remove", "add"])),
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
