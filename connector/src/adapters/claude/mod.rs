mod harness;
mod registration;

pub use harness::Claude;

/// Claude Code's home: its data (projects, sessions) and, when the variable is set, its user configuration. Passed
/// on only when the person's environment sets it: Claude Code keeps its user configuration, MCP servers included,
/// in `~/.claude.json` without it and in `$CLAUDE_CONFIG_DIR/.claude.json` with it.
pub const HOME: crate::profile::AgentHome = crate::profile::AgentHome {
    var: "CLAUDE_CONFIG_DIR",
    default: crate::profile::HomeDefault::Home(".claude"),
    always_passed: false,
    config: true,
    check_parent: false,
};

use super::{envelope, Identity};
use crate::profile::Profile;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

pub fn capabilities() -> Value {
    json!({"deliver":"supported","inspectInbound":"supported","working":"supported","endOfTurn":"supported","sessionIdentity":"supported"})
}

fn config_dir() -> PathBuf {
    env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".claude"))
}

/// The file Claude Code keeps its user configuration in, user-scope MCP servers included, resolved as its CLI resolves
/// it: `$CLAUDE_CONFIG_DIR/.claude.json` when the person's environment sets the variable, else `~/.claude.json`.
pub(crate) fn user_config(profile: &Profile) -> PathBuf {
    if profile.agent_home_set(HOME.var) {
        profile.agent_home(HOME.var).join(".claude.json")
    } else {
        profile.home.join(".claude.json")
    }
}

/// The largest `.claude.json` read: it also holds Claude Code's per-project state.
const USER_CONFIG_LIMIT: u64 = 64 << 20;

/// Claude Code's user configuration cannot be parsed or cannot be read safely: it is left alone, whatever else is
/// there.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Unreadable;

/// Reads [`user_config`] with the guarded reader, before anything else is judged: a regular file of the person's,
/// writable by nobody else, and JSON. A link is followed wherever it leads (dotfiles managers link it), and the file
/// it leads to is judged; one that leads nowhere is unreadable. `None` when there is no such file.
pub(crate) fn read_user_config(profile: &Profile) -> Result<Option<serde_json::Value>, Unreadable> {
    let file = user_config(profile);
    match fs::symlink_metadata(&file) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(Unreadable),
        Ok(_) => {}
    }
    let text =
        super::registration::read_profile_file(&file, USER_CONFIG_LIMIT).map_err(|_| Unreadable)?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|_| Unreadable)
}

pub fn identity() -> Result<Option<Identity>> {
    let Some(thread) = env::var("CLAUDE_CODE_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    let Some(socket) = env::var("CLAUDE_CODE_MESSAGING_SOCKET")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    let mut identity = Identity::new(
        "claude",
        thread,
        json!({
            "kind":"claude-uds",
            "socket":socket,
            "token":env::var("CLAUDE_CODE_MESSAGING_TOKEN").unwrap_or_default()
        }),
    );
    identity.inbound = Some(inspect_inbound(&identity.thread)?);
    Ok(Some(identity))
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn session_record(session_id: &str) -> Option<Value> {
    let registry = config_dir().join("sessions");
    for entry in fs::read_dir(registry).ok()?.flatten() {
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            let Some(record) = read_json(&entry.path()) else {
                continue;
            };
            if record.get("sessionId").and_then(Value::as_str) == Some(session_id) {
                return Some(record);
            }
        }
    }
    None
}

pub fn working_state(session_id: &str) -> Option<bool> {
    match session_record(session_id)?.get("status")?.as_str()? {
        "busy" => Some(true),
        "idle" | "ready" | "waiting" => Some(false),
        _ => None,
    }
}

pub fn session_engine(session_id: &str) -> Option<Value> {
    let pid = session_record(session_id)
        .as_ref()
        .and_then(|record| record.get("pid"))
        .and_then(Value::as_i64);
    let args = launch_args(pid);
    if args.is_empty() {
        return None;
    }
    let model = flag(&args, "--model");
    let effort = flag(&args, "--effort");
    let thinking = flag(&args, "--thinking");
    if model.is_none() && effort.is_none() {
        return None;
    }
    Some(json!({"model":model,"effort":effort,"thinking":thinking}))
}

pub fn assistant_model(entry: &Value) -> Option<&str> {
    (entry.get("type") == Some(&json!("assistant")))
        .then(|| entry.pointer("/message/model").and_then(Value::as_str))
        .flatten()
        .filter(|model| !model.is_empty() && !is_placeholder_model(model))
}

/// Claude Code records its own synthetic turns (a rate-limit error, for instance) under a bracketed
/// placeholder such as `<synthetic>`. That names no engine, so the last real model stays.
fn is_placeholder_model(model: &str) -> bool {
    model.starts_with('<') && model.ends_with('>')
}

/// The engine an assistant transcript entry records: its model and the effort of that turn,
/// falling back to the launch settings for an effort the entry lacks and for thinking.
pub fn assistant_engine(entry: &Value, launch: &Value) -> Option<Value> {
    let model = assistant_model(entry)?;
    let effort = entry
        .get("effort")
        .filter(|effort| effort.as_str().is_some_and(|effort| !effort.is_empty()))
        .or_else(|| launch.get("effort"))
        .cloned()
        .unwrap_or(Value::Null);
    let thinking = launch.get("thinking").cloned().unwrap_or(Value::Null);
    Some(json!({"model":model,"effort":effort,"thinking":thinking}))
}

pub fn transcript_path(session_id: &str) -> Option<PathBuf> {
    let projects = config_dir().join("projects");
    for entry in fs::read_dir(projects).ok()?.flatten() {
        let path = entry.path().join(format!("{session_id}.jsonl"));
        if path.exists() {
            return Some(path);
        }
    }
    None
}

fn launch_args(pid: Option<i64>) -> String {
    let Some(pid) = pid else { return String::new() };
    Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_default()
}

fn flag(args: &str, name: &str) -> Option<String> {
    for (index, _) in args.match_indices(name) {
        let end = index + name.len();
        if end != args.len()
            && !args.as_bytes()[end].is_ascii_whitespace()
            && args.as_bytes()[end] != b'='
        {
            continue;
        }
        let mut value = end;
        if args.as_bytes().get(value) == Some(&b'=') {
            value += 1;
        }
        while args
            .as_bytes()
            .get(value)
            .is_some_and(u8::is_ascii_whitespace)
        {
            value += 1;
        }
        if args.as_bytes().get(value) == Some(&b'\'') || args.as_bytes().get(value) == Some(&b'"') {
            let quote = args.as_bytes()[value] as char;
            value += 1;
            let finish = args[value..].find(quote)? + value;
            return Some(args[value..finish].to_owned());
        }
        let finish = args[value..]
            .find(char::is_whitespace)
            .unwrap_or(args.len() - value)
            + value;
        return Some(args[value..finish].to_owned());
    }
    None
}

fn settings_from_flags(args: &str) -> Option<Value> {
    let value = flag(args, "--settings")?;
    if value.trim_start().starts_with('{') {
        serde_json::from_str(&value).ok()
    } else {
        read_json(Path::new(&value))
    }
}

pub fn inspect_inbound(session_id: &str) -> Result<Value> {
    let record = session_record(session_id);
    let pid = record
        .as_ref()
        .and_then(|v| v.get("pid"))
        .and_then(Value::as_i64);
    let args = launch_args(pid);
    let user = read_json(&config_dir().join("settings.json")).unwrap_or_else(|| json!({}));
    let flagged = settings_from_flags(&args).unwrap_or_else(|| json!({}));
    let mode = flag(&args, "--permission-mode")
        .or_else(|| {
            user.pointer("/permissions/defaultMode")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "default".into());
    let inbound = flagged
        .get("crossSessionInbound")
        .or_else(|| user.get("crossSessionInbound"))
        .cloned();
    let bypassing = mode == "bypassPermissions";
    let ok = !bypassing || inbound.as_ref().and_then(Value::as_str) == Some("accept");
    let confidence = if pid.is_some() {
        "read from the session launch flags and settings"
    } else {
        "settings only; the session process was not found"
    };
    if ok {
        return Ok(
            json!({"ok":true,"mode":mode,"crossSessionInbound":inbound,"confidence":confidence}),
        );
    }
    let refuses = inbound.as_ref().and_then(Value::as_str) == Some("refuse");
    let reason = if refuses {
        "This session refuses messages from other local processes (crossSessionInbound is \"refuse\")."
    } else {
        "This session bypasses permission prompts, so Claude Code holds messages from other local processes for the user to approve instead of delivering them, and it sends no receipt to say so. Voice will appear to be sent and nothing will arrive."
    };
    let remedy = if refuses {
        "Change crossSessionInbound from \"refuse\" to \"accept\", or start the session in a permission mode that prompts.".to_owned()
    } else {
        format!("Two ways out. Per session: start it with --settings '{{\"crossSessionInbound\":\"accept\"}}'. For every session on this machine: add \"crossSessionInbound\": \"accept\" to {} — that takes effect immediately, releases any messages already held, and also lets any other local process post into all your sessions, which is the safeguard it removes. Or run the conversation in a prompting mode such as --permission-mode auto.", config_dir().join("settings.json").display())
    };
    Ok(
        json!({"ok":false,"mode":mode,"crossSessionInbound":inbound,"reason":reason,"remedy":remedy,"confidence":confidence}),
    )
}

pub async fn deliver(delivery: &Value, event: &Value) -> Result<Value> {
    let path = delivery
        .get("socket")
        .and_then(Value::as_str)
        .context("Claude inbox socket missing")?;
    let token = delivery.get("token").and_then(Value::as_str).unwrap_or("");
    let mut socket = UnixStream::connect(path)
        .await
        .context("Claude inbox refused the connection")?;
    let content = envelope(event)?;
    socket
        .write_all(json!({"type":"auth","token":token}).to_string().as_bytes())
        .await?;
    socket.write_all(b"\n").await?;
    socket
        .write_all(
            json!({"type":"user","message":{"role":"user","content":content}})
                .to_string()
                .as_bytes(),
        )
        .await?;
    socket.write_all(b"\n").await?;
    let mut peer = Vec::new();
    match tokio::time::timeout(Duration::from_millis(1500), socket.read_to_end(&mut peer)).await {
        Err(_) => Ok(
            json!({"status":"unknown","detail":"Claude Code inbox has no acknowledgement; transcript read will confirm receipt"}),
        ),
        Ok(Ok(_)) => Ok(
            json!({"status":"rejected","detail":"Claude Code inbox closed before confirming delivery"}),
        ),
        Ok(Err(error)) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude Code keeps its user configuration in `~/.claude.json` unless `CLAUDE_CONFIG_DIR` is set (in
    /// `$CLAUDE_CONFIG_DIR/.claude.json` then): its home reaches a child only when the person's environment sets it.
    #[test]
    fn claude_config_dir_is_passed_on_only_when_the_environment_sets_it() {
        use crate::profile::Profile;
        let scratch = crate::platform::fs::tests::Scratch::new("claude-home");
        let home = scratch.0.to_string_lossy().into_owned();
        let homes = crate::adapters::Registry::standard().homes();
        let unset =
            Profile::from_vars(|key| (key == "HOME").then(|| home.clone().into()), &homes).unwrap();
        assert_eq!(
            unset.agent_home(HOME.var),
            scratch.0.join(".claude"),
            "its data stays in ~/.claude"
        );
        assert!(!unset
            .variables()
            .iter()
            .any(|(key, _)| *key == "CLAUDE_CONFIG_DIR"));

        let named = scratch.0.join("elsewhere");
        let set = Profile::from_vars(
            |key| match key {
                "HOME" => Some(home.clone().into()),
                "CLAUDE_CONFIG_DIR" => Some(named.clone().into_os_string()),
                _ => None,
            },
            &homes,
        )
        .unwrap();
        assert!(set
            .variables()
            .iter()
            .any(|(key, path)| *key == "CLAUDE_CONFIG_DIR" && *path == named));
    }

    #[test]
    fn assistant_engine_takes_the_turn_effort() {
        let entry =
            json!({"type":"assistant","effort":"high","message":{"model":"claude-opus-5-5"}});
        let launch = json!({"effort":"low","thinking":"enabled"});
        assert_eq!(
            assistant_engine(&entry, &launch),
            Some(json!({"model":"claude-opus-5-5","effort":"high","thinking":"enabled"}))
        );
    }

    #[test]
    fn assistant_engine_falls_back_to_the_launch_effort() {
        let entry = json!({"type":"assistant","message":{"model":"claude-opus-5-5"}});
        assert_eq!(
            assistant_engine(&entry, &json!({"effort":"low"})),
            Some(json!({"model":"claude-opus-5-5","effort":"low","thinking":null}))
        );
        assert_eq!(
            assistant_engine(&entry, &json!({})),
            Some(json!({"model":"claude-opus-5-5","effort":null,"thinking":null}))
        );
    }

    #[test]
    fn a_synthetic_placeholder_model_is_not_an_engine() {
        let synthetic = json!({"type":"assistant","message":{"model":"<synthetic>"}});
        assert_eq!(assistant_model(&synthetic), None);
        assert_eq!(
            assistant_engine(&synthetic, &json!({"effort":"medium"})),
            None
        );
        let real = json!({"type":"assistant","message":{"model":"claude-opus-5-5"}});
        assert_eq!(assistant_model(&real), Some("claude-opus-5-5"));
    }

    #[test]
    fn assistant_engine_ignores_other_entries() {
        let entry = json!({"type":"user","effort":"high","message":{"model":"claude-opus-5-5"}});
        assert_eq!(assistant_engine(&entry, &json!({})), None);
    }
}
