//! Sidevoice's entry in Codex (`codex mcp`, `config.toml`).

use super::Codex;
use crate::adapters::registration::{
    cli_step, looks_absent, read_profile_file, run_required, strings, AgentError, AgentHost,
    BenchSetup, CliError, Entry, Launch, Manual, Registration,
};
use crate::profile::Profile;
use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::Value;
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
    let text = match read_profile_file(&file, 1 << 20) {
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
        let mut add = strings(&["mcp", "add", "sidevoice"]);
        for (name, value) in setup.env {
            add.extend(["--env".to_owned(), format!("{name}={value}")]);
        }
        add.push("--".into());
        Some(format!(
            "Codex (log in once inside the profile, register once, then start it):\n  {}\n  {}\n  {}",
            setup.line("codex", &strings(&["login"]), None),
            setup.line("codex", &add, Some(setup.server)),
            setup.line("codex", &[], None),
        ))
    }

    fn config_root<'a>(&self, profile: &'a Profile) -> &'a Path {
        profile.agent_home(super::HOME.var)
    }

    fn binary_names(&self) -> &'static [&'static str] {
        &["codex"]
    }

    fn fallback_binaries(
        &self,
        roots: &crate::adapters::markers::Roots,
    ) -> Vec<std::path::PathBuf> {
        super::binary::fallbacks(roots)
    }

    // npm and Homebrew select their own installation prefix; neither is a stable Codex marker.
    fn install_markers(&self) -> &'static [crate::adapters::markers::Marker] {
        &[]
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

    fn manual(&self, host: &dyn AgentHost, registration: &str, binary: Option<&str>) -> Manual {
        let cli = binary.unwrap_or("codex");
        let profile = host.profile();
        // `codex mcp` writes `config.toml` in Codex's home: the file each step changes. A step names `CODEX_HOME` only
        // when the person's environment sets it; without it, Codex's default is the same home.
        let file = profile.agent_home(super::HOME.var).join("config.toml");
        let step = |args: &[String]| cli_step(profile, super::HOME.var, cli, args, &file);
        let remove = step(&strings(&REMOVE));
        Manual::by_cli(registration, step(&add_args(host)), remove.clone(), remove)
    }
}
