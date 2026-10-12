//! Sidevoice's entry in Claude Code: written with `claude mcp`, user scope; read back from its user configuration
//! (`adapters::claude::user_config`).

use super::Claude;
use crate::adapters::registration::{
    read_profile_file, run_required, shell_command, strings, AgentError, AgentHost, BenchSetup,
    Entry, Launch, Registration,
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
        match fs::symlink_metadata(profile.agent_home(super::HOME.var)) {
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Entry::bare("absent")),
            Err(_) => return Ok(Entry::bare("invalid")),
            Ok(_) => {}
        }
        // Registering goes through its CLI: without one, what the file says cannot be acted on.
        if binary.is_none() {
            return Ok(Entry::bare("unknown"));
        }
        let file = super::user_config(profile);
        match fs::symlink_metadata(&file) {
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Entry::bare("absent")),
            Err(_) => return Ok(Entry::bare("invalid")),
            Ok(_) => {}
        }
        // The file stays inside the directory Claude Code reads it from: its configuration directory when the
        // person's environment names one, else their home.
        let root = if profile.agent_home_set(super::HOME.var) {
            profile.agent_root(profile.agent_home(super::HOME.var))
        } else {
            profile
                .validate_existing_private()
                .and_then(|()| Ok(profile.home.canonicalize()?))
        };
        let Ok(root) = root else {
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
    fn bench_steps(&self, setup: &BenchSetup) -> Option<String> {
        Some(format!(
            "Claude Code (log in once inside the profile with /login):\n  . {} && claude --mcp-config {} --strict-mcp-config",
            setup.env_script, setup.mcp_config
        ))
    }

    fn after_install(&self, profile: &Profile) -> Vec<String> {
        let mut notes = vec!["In a conversation, ask to join the voice room. Sessions already open need a restart before they \
             see the server."
            .to_owned()];
        notes.extend(inbound_warning(profile));
        notes
    }

    fn config_root<'a>(&self, profile: &'a Profile) -> &'a Path {
        profile.agent_home(super::HOME.var)
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

/// Claude Code holds what other local processes send to a session that bypasses permission prompts.
fn inbound_warning(profile: &Profile) -> Option<String> {
    let settings = profile.agent_home(super::HOME.var).join("settings.json");
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
