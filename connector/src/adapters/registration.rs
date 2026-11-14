//! Registering Sidevoice with an agent: the trait each harness's registration implements, the state it reports, what
//! the host-agent coordinator lends it for one request, and the plain helpers several harnesses share.

use super::markers::{self, Marker, Roots};
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

/// How the agent starts the server its entry names: the command, its arguments, the environment the entry gives it and
/// the working directory it names, if any.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Launch {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
}

/// The environment and the arguments can hold a credential URL: a `Debug` of a launch shows only how many of each.
impl std::fmt::Debug for Launch {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Launch")
            .field("command", &self.command)
            .field("args", &format_args!("<{} args>", self.args.len()))
            .field("env", &format_args!("<{} variables>", self.env.len()))
            .finish()
    }
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
    /// The agent's CLI as the coordinator finds it now, from the agent's `programs`: on the person's login PATH, then
    /// where the agent keeps it outside it.
    fn find_program(&self, programs: &Programs) -> Option<String>;
    /// The agent's CLI as the last look at its state found it: what connecting and disconnecting run.
    fn program(&self) -> Option<&str>;
    /// Runs the agent's CLI in `cwd` (its home), under the request's cancellation and deadline.
    fn run<'a>(
        &'a self,
        cwd: &'a Path,
        binary: &'a str,
        args: Vec<String>,
    ) -> BoxFuture<'a, Result<CliOutput, CliError>>;
    /// Fails when the request was cancelled or ran out of time.
    fn live(&self) -> Result<(), AgentError>;
}

/// Where an agent's CLI is: its names on PATH, where it may be outside PATH (most preferred first, looked at only after
/// PATH, the first that can be run taken), and the npm package whose launcher stands for a native program.
pub struct Programs {
    pub names: &'static [&'static str],
    pub fallbacks: fn(&Roots) -> Vec<PathBuf>,
    pub npm: Option<crate::agents::npm::NativePackage>,
}

/// No places outside PATH.
pub fn no_fallbacks(_: &Roots) -> Vec<PathBuf> {
    Vec::new()
}

/// Registering Sidevoice with an agent, reading its state, and removing only an entry Sidevoice wrote.
pub trait Registration: Send + Sync {
    /// Registers Sidevoice with the agent: writes its entry, or writes again one an older Sidevoice wrote.
    fn connect<'a>(&'a self, host: &'a dyn AgentHost) -> BoxFuture<'a, Result<(), AgentError>>;
    /// Removes Sidevoice's entry, current or not; never one Sidevoice did not write.
    fn disconnect<'a>(&'a self, host: &'a dyn AgentHost) -> BoxFuture<'a, Result<(), AgentError>>;
    /// How a person does each operation by hand, as ordered steps, for the registration `registered` reports.
    fn manual(&self, host: &dyn AgentHost, registered: Registered) -> Manual;
    /// The agent on this machine: whether it is installed, whether Sidevoice is registered with it, and what needs
    /// the person's attention.
    fn state<'a>(
        &'a self,
        host: &'a dyn AgentHost,
    ) -> BoxFuture<'a, Result<RegistrationState, AgentError>>;
}

/// An agent as Sidevoice finds it on this machine.
#[derive(Clone, Debug)]
pub struct RegistrationState {
    /// How the agent was found installed (`cli`, or a marker of its own such as `app_bundle`); empty when it was not.
    pub installed: Vec<&'static str>,
    pub registered: Registered,
    /// Whether Sidevoice registers with it by itself (its CLI was found, or it edits the agent's file); else only by
    /// hand.
    pub automatic: bool,
    /// What the person should know or do; any but a note means not ready.
    pub warnings: Vec<Warning>,
    /// How the agent starts Sidevoice's entry, when its configuration names one: what a health check starts.
    pub launch: Option<Launch>,
    /// The agent's CLI, as found.
    pub program: Option<String>,
}

impl RegistrationState {
    /// Whether nothing stops it from working: no warning but notes.
    pub fn ready(&self) -> bool {
        self.warnings
            .iter()
            .all(|warning| warning.level == Level::Note)
    }
}

/// Whether Sidevoice is registered with the agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Registered {
    /// No entry of Sidevoice's.
    Absent,
    /// Sidevoice's entry, running this installation.
    Registered,
    /// An entry Sidevoice wrote that is not the current one (disabled, an older release, another environment).
    Outdated,
    /// Another "sidevoice" entry, not Sidevoice's.
    Foreign,
    /// The agent's configuration cannot be read as it should.
    Invalid,
    /// It could not be told.
    Unknown,
}

impl Registered {
    /// As the agent list names it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "not-connected",
            Self::Registered => "connected",
            Self::Outdated => "outdated",
            Self::Foreign => "foreign",
            Self::Invalid => "invalid",
            Self::Unknown => "unknown",
        }
    }

    /// From [`Registered::as_str`]; anything else is `Unknown`.
    pub fn from_name(name: &str) -> Self {
        match name {
            "not-connected" => Self::Absent,
            "connected" => Self::Registered,
            "outdated" => Self::Outdated,
            "foreign" => Self::Foreign,
            "invalid" => Self::Invalid,
            _ => Self::Unknown,
        }
    }
}

/// Something about an agent the person should know, with a stable code and English text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    pub level: Level,
    pub code: &'static str,
    pub message: String,
}

/// How much a [`Warning`] matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// To know; it works.
    Note,
    /// It needs the person's attention to work.
    NeedsAttention,
    /// It cannot work here.
    Unsupported,
}

/// The agent list's `readiness` from `warnings`: none without any; else `ready`, `needs_attention` or `unsupported`,
/// with each reason (notes are none).
pub fn readiness(warnings: &[Warning]) -> Option<Value> {
    if warnings.is_empty() {
        return None;
    }
    let reasons: Vec<&Warning> = warnings
        .iter()
        .filter(|warning| warning.level != Level::Note)
        .collect();
    let state = if reasons.is_empty() {
        "ready"
    } else if reasons
        .iter()
        .any(|warning| warning.level == Level::Unsupported)
    {
        "unsupported"
    } else {
        "needs_attention"
    };
    Some(
        json!({"state":state,"reasons":reasons.iter().map(|warning| json!({"code":warning.code,"message":warning.message})).collect::<Vec<_>>()}),
    )
}

/// How the agent was found installed: `cli` when its CLI was found, then each of its `markers` present here.
pub fn installed(
    profile: &Profile,
    program: Option<&str>,
    markers: &'static [Marker],
) -> Vec<&'static str> {
    program
        .map(|_| "cli")
        .into_iter()
        .chain(markers::matching_kinds(
            markers,
            std::env::consts::OS,
            &Roots::process(&profile.home),
        ))
        .collect()
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

/// Runs the agent's CLI and requires it to succeed.
pub async fn run_required(
    host: &dyn AgentHost,
    cwd: &Path,
    binary: &str,
    args: &[String],
) -> Result<(), AgentError> {
    let output = match host.run(cwd, binary, args.to_vec()).await {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_launch_never_prints_its_environment() {
        let launch = Launch {
            command: "sidevoice".into(),
            env: [(
                "SIDEVOICE_DELIVERY_URL".to_owned(),
                "https://u:canary-pass@x/".to_owned(),
            )]
            .into(),
            ..Launch::default()
        };
        let shown = format!("{launch:?}");
        assert!(
            shown.contains("<1 variables>")
                && !shown.contains("canary-pass")
                && !shown.contains("SIDEVOICE_DELIVERY_URL"),
            "{shown}"
        );
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
