//! Sidevoice's entry in Claude Code (`claude mcp`, user scope).

use super::Claude;
use crate::adapters::registration::{
    looks_absent, run_required, shell_command, strings, AgentError, AgentHost, CliError,
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

impl Claude {
    async fn entry_state(
        &self,
        host: &dyn AgentHost,
        binary: Option<&str>,
    ) -> Result<String, AgentError> {
        match fs::symlink_metadata(&host.profile().claude) {
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok("absent".into()),
            Err(_) => return Ok("invalid".into()),
            Ok(_) => {}
        }
        // Settings Claude Code cannot parse are left alone: its CLI is not even asked.
        if settings_unparsable(&super::user_config(host.profile())) {
            return Ok("invalid".into());
        }
        let Some(binary) = binary else {
            return Ok("unknown".into());
        };
        let output = match host
            .run(binary, strings(&["mcp", "get", "sidevoice"]))
            .await
        {
            Ok(output) => output,
            Err(CliError::Cancelled) => return Err(AgentError::Cancelled),
            Err(_) => return Ok("unknown".into()),
        };
        if !output.status.success() {
            let detail = format!("{}\n{}", output.stdout, output.stderr);
            if (output.status.code() == Some(1) && detail.trim().is_empty())
                || looks_absent(&detail)
            {
                return Ok("absent".into());
            }
            return Ok("unknown".into());
        }
        let detail = format!("{}\n{}", output.stdout, output.stderr);
        let field = |name: &str| -> Option<String> {
            detail.lines().find_map(|line| {
                let (key, value) = line.trim().split_once(':')?;
                key.eq_ignore_ascii_case(name)
                    .then(|| value.trim().to_owned())
            })
        };
        let Some(command) = field("Command").filter(|value| !value.is_empty()) else {
            return Ok(if looks_absent(&detail) {
                "absent"
            } else {
                "unknown"
            }
            .into());
        };
        let args = field("Args")
            .map(|value| {
                value
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let user_scope =
            field("Scope").is_some_and(|value| value.to_ascii_lowercase().contains("user"));
        if !user_scope {
            return Ok("foreign".into());
        }
        // The block under `Environment:`, one `KEY=value` per line, indented below the field.
        let env = detail
            .lines()
            .skip_while(|line| !line.trim().eq_ignore_ascii_case("Environment:"))
            .skip(1)
            .take_while(|line| line.starts_with("    ") && line.contains('='))
            .filter_map(|line| line.trim().split_once('='))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect::<BTreeMap<_, _>>();
        if host.owns(&command, &args) {
            Ok(
                if command == host.command() && args == host.args() && env == host.mcp_env() {
                    "connected"
                } else {
                    "owned-old"
                }
                .into(),
            )
        } else {
            Ok("foreign".into())
        }
    }

    async fn registration_state(
        &self,
        host: &dyn AgentHost,
        binary: Option<&str>,
    ) -> Result<String, AgentError> {
        let state = self.entry_state(host, binary).await?;
        Ok(match state.as_str() {
            "absent" => "not-connected".into(),
            "owned-old" => "outdated".into(),
            _ => state,
        })
    }

    async fn connect_entry(
        &self,
        host: &dyn AgentHost,
        binary: Option<&str>,
    ) -> Result<(), AgentError> {
        let binary = binary.ok_or(AgentError::Key("agents.manual-required"))?;
        let state = self.entry_state(host, Some(binary)).await?;
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
            let again = self.entry_state(host, Some(binary)).await?;
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
        let state = self.registration_state(host, Some(binary)).await?;
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

/// Whether `file` is there and is not JSON. One too large to read here is not judged: the CLI says.
fn settings_unparsable(file: &Path) -> bool {
    const LIMIT: u64 = 64 << 20;
    match fs::metadata(file) {
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(_) => true,
        Ok(metadata) if metadata.len() > LIMIT => false,
        Ok(_) => match fs::read(file) {
            Ok(bytes) => serde_json::from_slice::<Value>(&bytes).is_err(),
            Err(_) => true,
        },
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
    ) -> BoxFuture<'a, Result<String, AgentError>> {
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
        json!({
            "command":command.clone(),
            "file":super::user_config(host.profile()).to_string_lossy(),
            "snippet":command,
        })
    }
}
