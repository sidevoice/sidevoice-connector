//! Sidevoice's entry in Claude Code: written with `claude mcp`, user scope; read back from `.claude.json`.

use super::Claude;
use crate::adapters::registration::{
    read_profile_file, run_required, shell_command, strings, AgentError, AgentHost, Entry, Launch,
    Registration,
};
use crate::profile::Profile;
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

const REMOVE: [&str; 5] = ["mcp", "remove", "--scope", "user", "sidevoice"];
/// The largest `.claude.json` read: it also holds Claude Code's per-project state.
const SETTINGS_LIMIT: u64 = 64 << 20;

impl Claude {
    /// The `sidevoice` entry of Claude Code's user scope, read from its configuration file and never through `claude
    /// mcp get`, which starts the server it names: a foreign command must not run, and ours would start the daemon.
    async fn entry_state(
        &self,
        host: &dyn AgentHost,
        binary: Option<&str>,
    ) -> Result<Entry, AgentError> {
        let profile = host.profile();
        match fs::symlink_metadata(&profile.claude) {
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Entry::bare("absent")),
            Err(_) => return Ok(Entry::bare("invalid")),
            Ok(_) => {}
        }
        // Registering goes through its CLI: without one, what the file says cannot be acted on.
        if binary.is_none() {
            return Ok(Entry::bare("unknown"));
        }
        let file = settings_file(profile);
        match fs::symlink_metadata(&file) {
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Entry::bare("absent")),
            Err(_) => return Ok(Entry::bare("invalid")),
            Ok(_) => {}
        }
        let Ok(root) = profile.agent_root(&profile.claude) else {
            return Ok(Entry::bare("invalid"));
        };
        let Ok(text) = read_profile_file(&file, &root, SETTINGS_LIMIT) else {
            return Ok(Entry::bare("invalid"));
        };
        let Ok(config) = serde_json::from_str::<Value>(&text) else {
            return Ok(Entry::bare("invalid"));
        };
        let Some(config) = config.as_object() else {
            return Ok(Entry::bare("invalid"));
        };
        let Some(servers) = config.get("mcpServers") else {
            return Ok(Entry::bare("absent"));
        };
        let Some(entry) = servers
            .as_object()
            .and_then(|servers| servers.get("sidevoice"))
        else {
            return Ok(Entry::bare(if servers.is_object() {
                "absent"
            } else {
                "invalid"
            }));
        };
        // Another transport (`http`, `sse`) or no command: not an entry Sidevoice writes.
        let stdio = entry
            .get("type")
            .and_then(Value::as_str)
            .is_none_or(|kind| kind == "stdio");
        let Some(command) = entry
            .get("command")
            .and_then(Value::as_str)
            .filter(|command| stdio && !command.is_empty())
            .map(str::to_owned)
        else {
            return Ok(Entry::bare("foreign"));
        };
        let Some(args) = strings_of(entry.get("args")) else {
            return Ok(Entry::bare("invalid"));
        };
        let env = entry
            .get("env")
            .and_then(Value::as_object)
            .map(|env| {
                env.iter()
                    .map(|(key, value)| (key.clone(), value.as_str().unwrap_or("").to_owned()))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        let state = if !host.owns(&command, &args) {
            "foreign"
        } else if command == host.command() && args == host.args() && env == host.mcp_env() {
            "connected"
        } else {
            "owned-old"
        };
        Ok(Entry::with(
            state,
            Launch {
                command,
                args,
                env,
                cwd: None,
            },
        ))
    }

    async fn registration_state(
        &self,
        host: &dyn AgentHost,
        binary: Option<&str>,
    ) -> Result<Entry, AgentError> {
        let mut entry = self.entry_state(host, binary).await?;
        match entry.registration.as_str() {
            "absent" => entry.registration = "not-connected".into(),
            "owned-old" => entry.registration = "outdated".into(),
            _ => {}
        }
        Ok(entry)
    }

    async fn connect_entry(
        &self,
        host: &dyn AgentHost,
        binary: Option<&str>,
    ) -> Result<(), AgentError> {
        let binary = binary.ok_or(AgentError::Key("agents.manual-required"))?;
        let state = self.entry_state(host, Some(binary)).await?.registration;
        if state == "connected" {
            return Ok(());
        }
        if state == "foreign" {
            return Err(AgentError::Key("agents.foreign"));
        }
        if state == "invalid" {
            return Err(AgentError::Key("agents.invalid"));
        }
        if state == "unknown" {
            return Err(AgentError::Key("agents.registration-unknown"));
        }
        host.live()?;
        if state == "absent" || state == "owned-old" {
            // A second probe just before mutation closes the gap with an external CLI writer.
            let again = self.entry_state(host, Some(binary)).await?.registration;
            if again == "foreign" {
                return Err(AgentError::Key("agents.foreign"));
            }
            if again == "invalid" {
                return Err(AgentError::Key("agents.invalid"));
            }
            if again == "unknown" {
                return Err(AgentError::Key("agents.registration-unknown"));
            }
            if again == "connected" {
                return Ok(());
            }
            if again == "owned-old" {
                run_required(host, binary, &strings(&REMOVE)).await?;
            }
        }
        run_required(host, binary, &add_args(host)).await
    }

    async fn disconnect_entry(
        &self,
        host: &dyn AgentHost,
        binary: Option<&str>,
    ) -> Result<(), AgentError> {
        let binary = binary.ok_or(AgentError::Key("agents.registration-unknown"))?;
        let state = self
            .registration_state(host, Some(binary))
            .await?
            .registration;
        match state.as_str() {
            "not-connected" => Ok(()),
            // Ours, current or not (an older release, another environment): removed.
            "connected" | "outdated" => run_required(host, binary, &strings(&REMOVE)).await,
            "foreign" => Err(AgentError::Key("agents.foreign")),
            "invalid" => Err(AgentError::Key("agents.invalid")),
            _ => Err(AgentError::Key("agents.registration-unknown")),
        }
    }
}

/// `mcp add` for this installation: its command, and the environment it runs with. `-e` takes every value up
/// to the next option, so the server's name comes before it.
fn add_args(host: &dyn AgentHost) -> Vec<String> {
    let mut args = strings(&["mcp", "add", "--scope", "user", "sidevoice"]);
    for (key, value) in host.mcp_env() {
        args.push("-e".into());
        args.push(format!("{key}={value}"));
    }
    args.push("--".into());
    args.push(host.command().to_owned());
    args.extend(host.args().iter().cloned());
    args
}

impl Registration for Claude {
    fn config_root<'a>(&self, profile: &'a Profile) -> &'a Path {
        &profile.claude
    }

    fn binary_names(&self) -> &'static [&'static str] {
        &["claude"]
    }

    fn known_paths(&self, home: &Path) -> Vec<PathBuf> {
        vec![
            home.join(".claude/local/claude"),
            home.join(".local/bin/claude"),
            PathBuf::from("/usr/local/bin/claude"),
            PathBuf::from("/opt/homebrew/bin/claude"),
        ]
    }

    fn state<'a>(
        &'a self,
        host: &'a dyn AgentHost,
        binary: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Entry, AgentError>> {
        Box::pin(self.registration_state(host, binary))
    }

    fn connect<'a>(
        &'a self,
        host: &'a dyn AgentHost,
        binary: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), AgentError>> {
        Box::pin(self.connect_entry(host, binary))
    }

    fn disconnect<'a>(
        &'a self,
        host: &'a dyn AgentHost,
        binary: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), AgentError>> {
        Box::pin(self.disconnect_entry(host, binary))
    }

    fn instructions(
        &self,
        host: &dyn AgentHost,
        _registration: &str,
        binary: Option<&str>,
    ) -> Value {
        let command = shell_command(binary.unwrap_or("claude"), &add_args(host));
        json!({"command":command.clone(),"file":Value::Null,"snippet":command})
    }
}

/// The file Claude Code keeps its user-scope MCP servers in: `.claude.json` in its configuration directory, which the
/// connector names to its CLI (`CLAUDE_CONFIG_DIR`).
fn settings_file(profile: &Profile) -> PathBuf {
    profile.claude.join(".claude.json")
}

/// The strings of a JSON array; none at all is no arguments, anything but strings is not an entry.
fn strings_of(value: Option<&Value>) -> Option<Vec<String>> {
    match value {
        None => Some(Vec::new()),
        Some(value) => value
            .as_array()?
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect(),
    }
}
