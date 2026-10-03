use super::{envelope, Identity};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

fn config_dir() -> PathBuf {
    env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".claude"))
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
