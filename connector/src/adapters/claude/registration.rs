//! Sidevoice's entry in Claude Code (`claude mcp`, user scope).

use super::Claude;
use crate::adapters::registration::{
    looks_absent, run_required, shell_command_in, strings, AgentError, AgentHost, BenchSetup,
    CliError, Manual, ManualStep, Registration,
};
use crate::profile::Profile;
use futures_util::future::BoxFuture;
use serde_json::Value;
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
        match fs::symlink_metadata(host.profile().agent_home(super::HOME.var)) {
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok("absent".into()),
            Err(_) => return Ok("invalid".into()),
            Ok(_) => {}
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

    fn manual(&self, host: &dyn AgentHost, registration: &str, binary: Option<&str>) -> Manual {
        let cli = binary.unwrap_or("claude");
        let profile = host.profile();
        // `claude mcp` writes Claude Code's user configuration: the file each step changes. A step names
        // `CLAUDE_CONFIG_DIR` only when the person's environment sets it, as the connector runs the CLI.
        let file = super::user_config(profile);
        let step = |args: &[String]| ManualStep {
            command: Some(shell_command_in(profile, super::HOME.var, cli, args)),
            snippet: None,
            file: Some(file.to_string_lossy().into_owned()),
        };
        // Ours is always in the user scope. Another entry may be in any scope, a project's or a local one relative to
        // the directory the person runs it in: without `--scope`, Claude Code removes it from the scope it is in.
        Manual::by_cli(
            registration,
            step(&add_args(host)),
            step(&strings(&REMOVE)),
            step(&strings(&["mcp", "remove", "sidevoice"])),
        )
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
