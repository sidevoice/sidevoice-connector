pub mod claude;
mod codex;
pub mod cursor;
mod http;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::Path;

#[derive(Clone, Debug)]
pub struct Identity {
    pub harness: String,
    pub thread: String,
    pub delivery: Value,
    pub capability_overrides: Value,
    pub route: Option<String>,
    pub capabilities: Value,
    pub experimental: Vec<String>,
    pub inbound: Option<Value>,
    pub editor: bool,
    pub detachable: bool,
    pub view_key: Option<String>,
    pub deliver_note: Option<String>,
    pub watch_note: Option<String>,
}

impl Identity {
    fn new(harness: &str, thread: String, delivery: Value) -> Self {
        let capabilities = match harness {
            "claude" => {
                json!({"deliver":"supported","inspectInbound":"supported","working":"supported","endOfTurn":"supported","sessionIdentity":"supported"})
            }
            "codex" => codex::capabilities(),
            "cursor" => cursor::capabilities(),
            _ => http::capabilities(),
        };
        Self {
            harness: harness.to_owned(),
            thread,
            delivery,
            capability_overrides: json!({}),
            route: None,
            capabilities,
            experimental: Vec::new(),
            inbound: None,
            editor: false,
            detachable: false,
            view_key: None,
            deliver_note: None,
            watch_note: None,
        }
    }
}

pub fn advertised_capabilities(harness: &str) -> Value {
    match harness {
        "claude" => {
            json!({"deliver":"supported","inspectInbound":"supported","working":"supported","endOfTurn":"supported","sessionIdentity":"supported"})
        }
        "codex" => codex::capabilities(),
        "cursor" => cursor::capabilities(),
        _ => http::capabilities(),
    }
}

pub fn identify(
    meta: &Value,
    client: Option<&Value>,
    client_capabilities: &Value,
) -> Result<Identity> {
    if client.is_some_and(cursor::is_cursor_client) {
        if let Some(identity) = http::identity()? {
            return Ok(identity);
        }
        return cursor::identity(client.unwrap(), client_capabilities);
    }
    if let Some(identity) = claude::identity()? {
        return Ok(identity);
    }
    if let Some(identity) = codex::identity(meta)? {
        return Ok(identity);
    }
    if let Some(identity) = http::identity()? {
        return Ok(identity);
    }
    bail!("Cannot tell which conversation this is: not launched by Claude Code, Codex or the Cursor CLI, and no SIDEVOICE_THREAD/SIDEVOICE_DELIVERY_URL set")
}

pub fn inspect_inbound(identity: &Identity) -> Result<Option<Value>> {
    match identity.delivery.get("kind").and_then(Value::as_str) {
        Some("claude-uds") => Ok(Some(claude::inspect_inbound(&identity.thread)?)),
        _ => Ok(None),
    }
}

pub fn engine(identity: &Identity) -> Option<Value> {
    match identity.delivery.get("kind").and_then(Value::as_str) {
        Some("claude-uds") => claude::session_engine(&identity.thread),
        Some("codex-queue") | Some("http") if identity.harness == "codex" => {
            let model = std::env::var("CODEX_MODEL")
                .ok()
                .filter(|value| !value.is_empty())?;
            Some(
                json!({"model":model,"effort":std::env::var("CODEX_REASONING_EFFORT").ok(),"thinking":Value::Null}),
            )
        }
        Some("cursor-tmux" | "none") => cursor::engine(&identity.thread),
        _ => None,
    }
}

pub fn conversation_capabilities(identity: &Identity) -> Value {
    let mut capabilities = identity.capabilities.clone();
    if let Some(overrides) = identity.capability_overrides.as_object() {
        for (name, state) in overrides {
            if capabilities.get(name) == Some(&json!("supported")) && state == "unsupported" {
                capabilities[name] = state.clone();
            }
        }
    }
    capabilities
}

pub fn experimental(identity: &Identity, capabilities: &Value) -> Vec<String> {
    let mut names = identity.experimental.clone();
    if identity.harness == "cursor"
        && matches!(
            identity.delivery["kind"].as_str(),
            Some("cursor-tmux" | "cursor-app")
        )
    {
        names.push("deliver".into());
    }
    names.retain(|name| capabilities[name] == "supported");
    names.sort();
    names.dedup();
    names
}

pub fn envelope(event: &Value) -> Result<String> {
    let channel = event
        .get("channel")
        .and_then(Value::as_str)
        .unwrap_or("voice");
    #[derive(serde::Serialize)]
    struct Header<'a> {
        channel: &'a str,
        session_id: &'a str,
        revision: i64,
        message_id: &'a str,
    }
    let header = Header {
        channel: if channel == "room-control" {
            "room-control"
        } else {
            "voice"
        },
        session_id: event
            .get("session_id")
            .and_then(Value::as_str)
            .context("session_id required")?,
        revision: event
            .get("revision")
            .and_then(Value::as_i64)
            .context("revision required")?,
        message_id: event
            .get("message_id")
            .and_then(Value::as_str)
            .context("message_id required")?,
    };
    let text = event
        .get("text")
        .and_then(Value::as_str)
        .context("text required")?;
    let header_text = serde_json::to_string(&header)?;
    let mut body = format!("{}\n\n{}", header_text, text);
    if header.channel != "room-control" {
        body.push_str(&format!(
            "\n\n[Sidevoice] Voice from the room: acknowledge with voice_say (session_id \"{}\", revision {}) before any other tool, then work and reply by voice, as the sidevoice server's instructions say.",
            header.session_id, header.revision
        ));
    }
    Ok(body)
}

pub async fn deliver(
    delivery: &Value,
    thread: &str,
    codex_home: &Path,
    event: &Value,
) -> Result<Value> {
    match delivery.get("kind").and_then(Value::as_str).unwrap_or("") {
        "claude-uds" => claude::deliver(delivery, event).await,
        "codex-queue" => codex::deliver(delivery, event, codex_home).await,
        "http" => http::deliver(delivery, event).await,
        "cursor-tmux" => cursor::deliver(delivery, event).await,
        "cursor-app" => bail!("Cursor editor card delivery is not prepared"),
        _ => {
            Ok(json!({"status":"unsupported","detail":format!("{} has no delivery route", thread)}))
        }
    }
}
