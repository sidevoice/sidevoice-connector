//! Sidevoice's entry in Codex (`codex mcp`, `config.toml`).

use super::Codex;
use crate::adapters::registration::{
    looks_absent, read_profile_file, run_required, shell_command, strings, AgentError, AgentHost,
    BenchSetup, CliError, Entry, Launch, Registration,
};
use crate::messages::message;
use crate::profile::Profile;
use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

const REMOVE: [&str; 3] = ["mcp", "remove", "sidevoice"];

impl Codex {
    async fn registration_state(
        &self,
        host: &dyn AgentHost,
        binary: Option<&str>,
    ) -> Result<Entry, AgentError> {
        let profile = host.profile();
        let guard = file_guard(host);
        if guard == "foreign" || guard == "invalid" {
            return Ok(Entry::bare(&guard));
        }
        if matches!(
            fs::symlink_metadata(profile.agent_home(super::HOME.var)),
            Err(ref error) if error.kind() == ErrorKind::NotFound
        ) {
            return Ok(Entry::bare("not-connected"));
        }
        let Some(binary) = binary else {
            return Ok(Entry::bare("unknown"));
        };
        let output = match host
            .run(binary, strings(&["mcp", "get", "sidevoice", "--json"]))
            .await
        {
            Ok(output) => output,
            Err(CliError::Cancelled) => return Err(AgentError::Cancelled),
            Err(_) => return Ok(Entry::bare("unknown")),
        };
        if !output.status.success() {
            let detail = format!("{}\n{}", output.stderr, output.stdout);
            if guard == "not-connected" && looks_absent(&detail) {
                return Ok(Entry::bare("not-connected"));
            }
            return Ok(Entry::bare("unknown"));
        }
        let entry: Value = match serde_json::from_str(output.stdout.trim()) {
            Ok(value) => value,
            Err(_) => return Ok(Entry::bare("unknown")),
        };
        let transport = entry.get("transport");
        let command = transport
            .and_then(|value| value.get("command"))
            .and_then(Value::as_str);
        let args = transport
            .and_then(|value| value.get("args"))
            .and_then(Value::as_array)
            .and_then(|values| {
                values
                    .iter()
                    .map(|value| value.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
            });
        if entry.get("name").and_then(Value::as_str) != Some("sidevoice")
            || transport
                .and_then(|value| value.get("type"))
                .and_then(Value::as_str)
                != Some("stdio")
            || command.is_none()
            || args.is_none()
        {
            return Ok(Entry::bare("invalid"));
        }
        let command = command.unwrap_or_default();
        let args = args.unwrap_or_default();
        let enabled = entry.get("enabled").and_then(Value::as_bool) != Some(false);
        let env = transport
            .and_then(|value| value.get("env"))
            .and_then(Value::as_object)
            .map(|env| {
                env.iter()
                    .map(|(key, value)| (key.clone(), value.as_str().unwrap_or("").to_owned()))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        let state = if !host.owns(command, &args) {
            "foreign"
        } else if enabled
            && command == host.command()
            && args == host.args()
            && env == host.mcp_env()
        {
            "connected"
        } else {
            "outdated"
        };
        Ok(Entry::with(
            state,
            Launch {
                command: command.to_owned(),
                args,
                env,
                // Codex starts the server in it.
                cwd: transport
                    .and_then(|value| value.get("cwd"))
                    .and_then(Value::as_str)
                    .map(PathBuf::from),
            },
        ))
    }

    async fn connect_entry(
        &self,
        host: &dyn AgentHost,
        binary: Option<&str>,
    ) -> Result<(), AgentError> {
        let binary = binary.ok_or(AgentError::Key("agents.manual-required"))?;
        let state = self
            .registration_state(host, Some(binary))
            .await?
            .registration;
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
        let again = self
            .registration_state(host, Some(binary))
            .await?
            .registration;
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
        if file_guard(host) == "ours" {
            run_required(host, binary, &strings(&REMOVE)).await?;
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
            // Ours, current or not (disabled, an older release, another environment): removed.
            "connected" | "outdated" => run_required(host, binary, &strings(&REMOVE)).await,
            "foreign" => Err(AgentError::Key("agents.foreign")),
            "invalid" => Err(AgentError::Key("agents.invalid")),
            _ => Err(AgentError::Key("agents.registration-unknown")),
        }
    }
}

/// What `config.toml` says of the `sidevoice` entry, read without the CLI: `ours`, `foreign`, `not-connected` or
/// `invalid`.
fn file_guard(host: &dyn AgentHost) -> String {
    let profile = host.profile();
    if profile.validate_existing_private().is_err() {
        return "invalid".into();
    }
    match fs::symlink_metadata(profile.agent_home(super::HOME.var)) {
        Err(error) if error.kind() == ErrorKind::NotFound => return "not-connected".into(),
        Err(_) => return "invalid".into(),
        Ok(_) => {}
    }
    if profile
        .validate_for_agent(profile.agent_home(super::HOME.var))
        .is_err()
    {
        return "invalid".into();
    }
    let file = profile.agent_home(super::HOME.var).join("config.toml");
    match fs::symlink_metadata(&file) {
        Err(error) if error.kind() == ErrorKind::NotFound => return "not-connected".into(),
        Err(_) => return "invalid".into(),
        Ok(_) => {}
    }
    let root = match profile.agent_root(profile.agent_home(super::HOME.var)) {
        Ok(root) => root,
        Err(_) => return "invalid".into(),
    };
    let text = match read_profile_file(&file, &root, 1 << 20) {
        Ok(text) => text,
        Err(_) => return "invalid".into(),
    };
    let config: toml::Value = match toml::from_str(&text) {
        Ok(value) => value,
        Err(_) => return "invalid".into(),
    };
    let Some(servers) = config.get("mcp_servers") else {
        return "not-connected".into();
    };
    let Some(servers) = servers.as_table() else {
        return "invalid".into();
    };
    let Some(entry) = servers.get("sidevoice") else {
        return "not-connected".into();
    };
    let Some(entry) = entry.as_table() else {
        return "invalid".into();
    };
    let Some(command) = entry.get("command").and_then(toml::Value::as_str) else {
        return "invalid".into();
    };
    let Some(args) = entry.get("args").and_then(toml::Value::as_array) else {
        return "invalid".into();
    };
    let Some(args) = args
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
    else {
        return "invalid".into();
    };
    if host.owns(command, &args) {
        "ours"
    } else {
        "foreign"
    }
    .into()
}

/// `mcp add` for this installation: its command, and the environment it runs with.
fn add_args(host: &dyn AgentHost) -> Vec<String> {
    let mut args = strings(&["mcp", "add", "sidevoice"]);
    for (key, value) in host.mcp_env() {
        args.push("--env".into());
        args.push(format!("{key}={value}"));
    }
    args.push("--".into());
    args.push(host.command().to_owned());
    args.extend(host.args().iter().cloned());
    args
}

impl Registration for Codex {
    fn bench_steps(&self, setup: &BenchSetup) -> Option<String> {
        let env: String = setup
            .env
            .iter()
            .map(|(name, value)| {
                format!(" --env {}", shell_command(&format!("{name}={value}"), &[]))
            })
            .collect();
        let script = setup.env_script;
        Some(format!(
            "Codex (log in once inside the profile, register once, then start it):\n  . {script} && codex login\n  . {script} && codex mcp add sidevoice{env} -- {}\n  . {script} && codex",
            setup.server
        ))
    }

    fn config_root<'a>(&self, profile: &'a Profile) -> &'a Path {
        profile.agent_home(super::HOME.var)
    }

    fn binary_names(&self) -> &'static [&'static str] {
        &["codex"]
    }

    fn known_paths(&self, home: &Path) -> Vec<PathBuf> {
        vec![
            home.join(".local/bin/codex"),
            PathBuf::from("/usr/local/bin/codex"),
            PathBuf::from("/opt/homebrew/bin/codex"),
            home.join("AppData/Local/Programs/Codex/codex.exe"),
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
        registration: &str,
        binary: Option<&str>,
    ) -> Value {
        if registration == "foreign" {
            let cli = binary.unwrap_or("codex");
            let replace_message = message(
                "agents.manual.codex.replace-existing",
                &json!({
                    "remove":shell_command(cli, &strings(&REMOVE)),
                    "add":shell_command(cli, &add_args(host)),
                }),
            );
            return json!({
                "command":replace_message,
                "file":Value::Null,
                "snippet":Value::Null,
            });
        }
        let command = shell_command(host.command(), host.args());
        let mut snippet = format!(
            "[mcp_servers.sidevoice]\ncommand = {}\nargs = {}\n\n[mcp_servers.sidevoice.env]",
            serde_json::to_string(host.command()).unwrap_or_else(|_| "\"\"".into()),
            serde_json::to_string(host.args()).unwrap_or_else(|_| "[]".into())
        );
        for (key, value) in host.mcp_env() {
            snippet.push_str(&format!(
                "\n{key} = {}",
                serde_json::to_string(&value).unwrap_or_else(|_| "\"\"".into())
            ));
        }
        json!({
            "command":command,
            "file":host.profile().agent_home(super::HOME.var).join("config.toml").to_string_lossy(),
            "snippet":snippet,
        })
    }
}
