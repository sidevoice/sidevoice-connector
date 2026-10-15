//! Sidevoice's entry in Cursor (`mcp.json`, written directly: Cursor has no CLI for it).

use super::Cursor;
use crate::adapters::registration::{
    mcp_servers_json, AgentError, AgentHost, Entry, Launch, Manual, Registration,
};
use crate::profile::Profile;
use crate::secure_fs::validate_user_directory;
use anyhow::{Context, Result};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// The `command` and `args` of an `mcp.json` entry.
fn entry_command(entry: &Value) -> (&str, Vec<String>) {
    let command = entry.get("command").and_then(Value::as_str).unwrap_or("");
    let args = entry
        .get("args")
        .and_then(Value::as_array)
        .and_then(|values| {
            values
                .iter()
                .map(|value| value.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
        })
        .unwrap_or_default();
    (command, args)
}

fn registration_state(host: &dyn AgentHost) -> Result<Entry> {
    let profile = host.profile();
    profile.validate_existing_private()?;
    let file = super::config_home(profile).join("mcp.json");
    let root = profile.agent_root(super::config_home(profile))?;
    let Some((mut config, _mode, _target)) = read_cursor_config(&file, &root)? else {
        return Ok(Entry::bare("not-connected"));
    };
    let current = config
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .and_then(|servers| servers.get("sidevoice"))
        .cloned();
    let Some(current) = current else {
        return Ok(Entry::bare("not-connected"));
    };
    let (command, args) = entry_command(&current);
    let env = cursor_env(&current);
    let state = if !host.owns(command, &args) {
        "foreign"
    } else if command == host.command() && args == host.args() && env == host.mcp_env() {
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
            // Cursor's stdio entries name no working directory.
            cwd: None,
        },
    ))
}

fn connect_entry(host: &dyn AgentHost) -> Result<(), AgentError> {
    let profile = host.profile();
    profile
        .validate_for_agent(super::config_home(profile))
        .map_err(|_| AgentError::Key("agents.invalid"))?;
    let file = super::config_home(profile).join("mcp.json");
    let root = profile
        .agent_root(super::config_home(profile))
        .map_err(|_| AgentError::Key("agents.invalid"))?;
    let mut config = match read_cursor_config(&file, &root) {
        Ok(Some((config, _, _))) => config,
        Ok(None) => json!({}),
        Err(_) => return Err(AgentError::Key("agents.invalid")),
    };
    if !config.is_object() {
        return Err(AgentError::Key("agents.invalid"));
    }
    let servers = config
        .get("mcpServers")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(current) = servers.get("sidevoice") {
        let (command, args) = entry_command(current);
        if !host.owns(command, &args) {
            return Err(AgentError::Key("agents.foreign"));
        }
        if command == host.command() && args == host.args() && cursor_env(current) == host.mcp_env()
        {
            return Ok(());
        }
    }
    let mut servers = servers;
    let mut entry = servers
        .get("sidevoice")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    entry.insert("command".into(), json!(host.command()));
    entry.insert("args".into(), json!(host.args()));
    entry.insert("env".into(), json!(host.mcp_env()));
    servers.insert("sidevoice".into(), Value::Object(entry));
    config["mcpServers"] = Value::Object(servers);
    write_cursor_config(profile, &file, &config).map_err(|_| AgentError::Key("agents.invalid"))
}

fn disconnect_entry(host: &dyn AgentHost) -> Result<(), AgentError> {
    let profile = host.profile();
    profile
        .validate_for_agent(super::config_home(profile))
        .map_err(|_| AgentError::Key("agents.invalid"))?;
    let file = super::config_home(profile).join("mcp.json");
    let root = profile
        .agent_root(super::config_home(profile))
        .map_err(|_| AgentError::Key("agents.invalid"))?;
    let Some((mut config, _, _)) =
        read_cursor_config(&file, &root).map_err(|_| AgentError::Key("agents.invalid"))?
    else {
        return Ok(());
    };
    let Some(servers) = config.get_mut("mcpServers").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    let Some(current) = servers.get("sidevoice") else {
        return Ok(());
    };
    let (command, args) = entry_command(current);
    if !host.owns(command, &args) {
        return Err(AgentError::Key("agents.foreign"));
    }
    servers.remove("sidevoice");
    write_cursor_config(profile, &file, &config).map_err(|_| AgentError::Key("agents.invalid"))
}

/// The environment a Cursor entry gives its server.
fn cursor_env(entry: &Value) -> BTreeMap<String, String> {
    entry
        .get("env")
        .and_then(Value::as_object)
        .map(|env| {
            env.iter()
                .map(|(key, value)| (key.clone(), value.as_str().unwrap_or("").to_owned()))
                .collect()
        })
        .unwrap_or_default()
}

fn read_cursor_config(file: &Path, root: &Path) -> Result<Option<(Value, u32, PathBuf)>> {
    let link_metadata = match fs::symlink_metadata(file) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let target = if link_metadata.file_type().is_symlink() {
        file.canonicalize()?
    } else {
        file.to_path_buf()
    };
    let metadata = fs::symlink_metadata(&target)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
        || metadata.len() > (1 << 20)
        || !target.canonicalize()?.starts_with(root)
    {
        anyhow::bail!("unsafe Cursor profile file");
    }
    let text = fs::read_to_string(&target)?;
    let config: Value = serde_json::from_str(&text)?;
    if !config.is_object() {
        anyhow::bail!("Cursor configuration is not an object");
    }
    Ok(Some((
        config,
        metadata.permissions().mode() & 0o777,
        target,
    )))
}

fn write_cursor_config(profile: &Profile, file: &Path, config: &Value) -> Result<()> {
    profile.validate_for_agent(super::config_home(profile))?;
    let root = profile.agent_root(super::config_home(profile))?;
    let (target, mode) = match read_cursor_config(file, &root)? {
        Some((_, mode, target)) => (target, mode),
        None => (file.to_path_buf(), 0o600),
    };
    let parent = target.parent().context("Cursor config parent")?;
    validate_user_directory(parent)?;
    if !parent.canonicalize()?.starts_with(&root) {
        anyhow::bail!("Cursor config escaped its directory");
    }
    let temporary = parent.join(format!(".mcp.json.{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        profile.validate_for_agent(super::config_home(profile))?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        serde_json::to_writer_pretty(&mut output, config)?;
        use std::io::Write;
        output.write_all(b"\n")?;
        output.sync_all()?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))?;
        profile.validate_for_agent(super::config_home(profile))?;
        fs::rename(&temporary, &target)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// What Cursor can and cannot do with a room, said at install so that nobody expects more.
const INSTALL_NOTES: &str = "Cursor: ask a chat to join the voice room; it speaks its replies there. The first time, \
Cursor asks to approve the new MCP server (cursor-agent: or run  cursor-agent mcp enable sidevoice ). What you say in \
the room reaches Cursor only by experimental routes, because Cursor offers none of its own:\n  - Cursor CLI: only a \
chat started with  cursor-agent persist  (needs tmux); the room types into its terminal.\n  - Cursor editor: through \
Cursor's Desktop Bridge, once Cursor has recorded the chat's join call (up to ~30 s); until then nothing is sent. \
Several chats of a window can join.\nIf you type while the room sends, the two mix.";

impl Registration for Cursor {
    fn after_install(&self, _profile: &Profile) -> Vec<String> {
        vec![INSTALL_NOTES.to_owned()]
    }

    fn config_root<'a>(&self, profile: &'a Profile) -> &'a Path {
        super::config_home(profile)
    }

    fn binary_names(&self) -> &'static [&'static str] {
        &["cursor-agent", "cursor"]
    }

    fn install_markers(&self) -> &'static [crate::adapters::markers::Marker] {
        use crate::adapters::markers::{Marker, Root};
        &[
            Marker {
                os: "macos",
                kind: "app_bundle",
                root: Root::Absolute,
                path: "/Applications/Cursor.app",
            },
            Marker {
                os: "macos",
                kind: "app_bundle",
                root: Root::Home,
                path: "Applications/Cursor.app",
            },
            Marker {
                os: "windows",
                kind: "install_dir",
                root: Root::LocalAppData,
                path: "Programs/cursor",
            },
            Marker {
                os: "windows",
                kind: "install_dir",
                root: Root::ProgramFiles,
                path: "cursor",
            },
            Marker {
                os: "linux",
                kind: "install_dir",
                root: Root::Absolute,
                path: "/usr/share/cursor",
            },
            Marker {
                os: "linux",
                kind: "install_dir",
                root: Root::Home,
                path: ".local/share/cursor",
            },
        ]
    }

    fn connects_without_binary(&self) -> bool {
        true
    }

    fn state<'a>(
        &'a self,
        host: &'a dyn AgentHost,
        _binary: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Entry, AgentError>> {
        Box::pin(
            async move { Ok(registration_state(host).unwrap_or_else(|_| Entry::bare("invalid"))) },
        )
    }

    fn connect<'a>(
        &'a self,
        host: &'a dyn AgentHost,
        _binary: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), AgentError>> {
        Box::pin(async move {
            host.live()?;
            connect_entry(host)
        })
    }

    fn disconnect<'a>(
        &'a self,
        host: &'a dyn AgentHost,
        _binary: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), AgentError>> {
        Box::pin(async move {
            host.live()?;
            disconnect_entry(host)
        })
    }

    fn manual(&self, host: &dyn AgentHost, _registration: &str, _binary: Option<&str>) -> Manual {
        Manual::by_file(
            &super::config_home(host.profile())
                .join("mcp.json")
                .to_string_lossy(),
            mcp_servers_json(host.command(), host.args(), host.mcp_env()).unwrap_or_default(),
            "mcpServers",
        )
    }
}
