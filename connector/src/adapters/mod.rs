pub mod claude;
pub mod codex;
mod conversation;
pub mod cursor;
mod harness;
pub mod health;
mod http;
pub(crate) mod markers;
pub mod registration;
mod transcript;

pub use conversation::{Conversation, Room};
pub use harness::{Harness, Registry, Route};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

pub(crate) const PULL_POSITION_FIELD: &str = "cursor";
const PULL_PAGE_TARGET_BYTES: usize = 1 << 20;

/// Render a core page before exposing its claims. Include complete evidence in the measured IPC
/// response and return an ordered prefix when a whole page expands beyond the usual page budget.
pub(crate) fn prepare_pull_page(result: &mut Value, after: u64) -> Result<()> {
    let messages = result
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .context("pull messages missing")?;
    for message in messages.iter_mut() {
        message["session_id"]
            .as_str()
            .context("pull session_id missing")?;
        message["revision"]
            .as_i64()
            .context("pull revision missing")?;
        message["epoch"].as_u64().context("pull epoch missing")?;
        message["message_id"]
            .as_str()
            .context("pull message_id missing")?;
        message[PULL_POSITION_FIELD]
            .as_u64()
            .context("pull cursor missing")?;
        message["envelope"] = json!(envelope(message)?);
    }
    let all = std::mem::take(messages);
    let total = all.len();
    let core_more = result["more"] == true;
    for message in all {
        result["messages"].as_array_mut().unwrap().push(message);
        let exposed = result["messages"].as_array().unwrap().len();
        result[PULL_POSITION_FIELD] = result["messages"][exposed - 1][PULL_POSITION_FIELD].clone();
        result["more"] = json!(core_more || exposed < total);
        let bytes =
            serde_json::to_vec(&json!({"id":u64::MAX,"ok":true,"result":result}))?.len() + 1;
        if bytes > crate::IPC_FRAME_LIMIT || (exposed > 1 && bytes > PULL_PAGE_TARGET_BYTES) {
            result["messages"].as_array_mut().unwrap().pop();
            break;
        }
    }
    let exposed = result["messages"].as_array().unwrap();
    if total > 0 && exposed.is_empty() {
        bail!("one complete pull message exceeds the IPC frame limit");
    }
    let position = exposed.last().map_or_else(
        || json!(after),
        |message| message[PULL_POSITION_FIELD].clone(),
    );
    let more = core_more || exposed.len() < total;
    result[PULL_POSITION_FIELD] = position;
    result["more"] = json!(more);
    Ok(())
}

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
    /// A string unique to this join that the harness may find in its own record of the call.
    pub chat_marker: Option<String>,
    pub deliver_note: Option<String>,
    pub watch_note: Option<String>,
}

impl Identity {
    fn new(harness: &str, thread: String, delivery: Value) -> Self {
        let capabilities = advertised_capabilities(harness);
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
            chat_marker: None,
            deliver_note: None,
            watch_note: None,
        }
    }
}

pub fn advertised_capabilities(harness: &str) -> Value {
    Registry::standard().get(harness).capabilities()
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
    bail!("{}", unidentified(client))
}

/// The agents whose conversations Sidevoice joins.
pub const SUPPORTED: &str = "Claude Code, Codex (the CLI and the desktop app) and Cursor";

/// Why a call could not be tied to a conversation, naming the agent that made it (from its MCP client info), and
/// which agents Sidevoice joins.
pub fn unidentified(client: Option<&Value>) -> String {
    let name = client
        .and_then(|client| client.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let version = client
        .and_then(|client| client.get("version"))
        .and_then(Value::as_str)
        .filter(|version| !version.is_empty());
    let agent = if name.contains("claude") {
        Some("Claude Code")
    } else if name.contains("codex") {
        Some("Codex")
    } else if client.is_some_and(cursor::is_cursor_client) {
        Some("Cursor")
    } else {
        None
    };
    let detected = match (agent, version) {
        (Some(agent), Some(version)) => {
            format!("{agent} {version} did not say which of its conversations is calling")
        }
        (Some(agent), None) => format!("{agent} did not say which of its conversations is calling"),
        (None, _) if !name.is_empty() => format!("{name} is not an agent Sidevoice joins"),
        (None, _) => "the agent did not say who it is".to_owned(),
    };
    format!("Cannot tell which conversation this is: {detected}. Sidevoice joins conversations of {SUPPORTED}.")
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
        // Cursor's database is read asynchronously by its watcher after join.
        Some("cursor-tmux" | "none") => None,
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
            Some("cursor-tmux" | "cursor-bridge")
        )
    {
        names.push("deliver".into());
    }
    names.retain(|name| capabilities[name] == "supported");
    names.sort();
    names.dedup();
    names
}

/// One agent-visible envelope for both injected input and get_messages.
pub fn envelope(event: &Value) -> Result<String> {
    let channel = match event.get("channel").and_then(Value::as_str) {
        Some("note") => "note",
        _ => "voice",
    };
    let session_id = event
        .get("session_id")
        .and_then(Value::as_str)
        .context("session_id required")?;
    let revision = event
        .get("revision")
        .and_then(Value::as_i64)
        .context("revision required")?;
    let epoch = event
        .get("epoch")
        .and_then(Value::as_u64)
        .context("epoch required")?;
    let message_id = event
        .get("message_id")
        .and_then(Value::as_str)
        .context("message_id required")?;
    let text = event.get("text").and_then(Value::as_str).unwrap_or("");
    let hearing_changes = event
        .get("hearing_changes")
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or_else(|| {
            // Older Core versions report #152/#153 evidence in `unheard`. Preserve the
            // compact physical fact without copying the original text into every input.
            let changes = event
                .pointer("/unheard/replies")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|reply| {
                    let utterance_id = reply.get("utterance_id")?.as_str()?;
                    let heard_chars = reply.get("heard_chars").and_then(Value::as_u64);
                    let hearing = match (reply.get("fate").and_then(Value::as_str), heard_chars) {
                        (Some("heard"), Some(_)) => "complete",
                        (Some("partial"), Some(_)) => "partial",
                        (Some("not_heard"), Some(0)) => "not_played",
                        _ => "unknown",
                    };
                    Some(json!({"utterance_id":utterance_id,"attempt_id":reply.get("attempt_id"),
                        "hearing":hearing,"heard_chars":if hearing == "unknown" {Value::Null} else {json!(heard_chars)}}))
                })
                .collect::<Vec<_>>();
            json!(changes)
        });
    let mut header = json!({"channel":channel,"session_id":session_id,"revision":revision,
        "epoch":epoch,"message_id":message_id,"hearing_changes":hearing_changes});
    for key in [
        "playback_outcomes",
        "corrects_message_id",
        "related_message_id",
    ] {
        if let Some(value) = event.get(key).filter(|v| !v.is_null()) {
            header[key] = value.clone();
        }
    }
    if channel == "note" {
        if !text.is_empty() {
            bail!("a Sidevoice note cannot contain user words");
        }
        header["event"] = event
            .get("event")
            .or_else(|| event.pointer("/notice/kind"))
            .filter(|v| v.is_string())
            .cloned()
            .context("note event required")?;
        return Ok(header.to_string());
    }
    let origin = match event.get("origin").and_then(Value::as_str) {
        Some("typed_room") => "typed_room",
        _ => "spoken_room",
    };
    header["origin"] = json!(origin);
    if origin == "spoken_room" {
        header["playback_at_turn_start"] = event
            .get("playback_at_turn_start")
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or_else(
                || json!({"knowledge":"unknown","last_played":null,"recent":[],"truncated":false}),
            );
        if let Some(value) = event
            .get("not_played_at_turn_start")
            .filter(|v| !v.is_null())
        {
            header["not_played_at_turn_start"] = value.clone();
        }
        if let Some(value) = event
            .get("not_played_at_turn_start_truncated")
            .filter(|v| !v.is_null())
        {
            header["not_played_at_turn_start_truncated"] = value.clone();
        }
    }
    Ok(format!("{header}\n\n{text}"))
}

#[cfg(test)]
mod tests {
    use super::{envelope, unidentified};
    use serde_json::{json, Value};

    fn spoken() -> Value {
        json!({"channel":"voice","origin":"spoken_room","message_id":"m-42",
            "session_id":"s-7","revision":3,"epoch":8,
            "playback_at_turn_start":{"knowledge":"known",
                "last_played":{"utterance_id":"17","hearing":"complete","heard_chars":32},
                "recent":[{"utterance_id":"17","hearing":"complete","heard_chars":32}],
                "truncated":false},
            "not_played_at_turn_start":[{"utterance_id":"18","hearing":"not_played","heard_chars":0}],
            "hearing_changes":[],"text":"Sí, adelante. 🐙\nExact next line"})
    }

    #[test]
    fn spoken_words_are_exact_after_one_json_header() {
        let frame = spoken();
        let rendered = envelope(&frame).unwrap();
        let (header, words) = rendered.split_once("\n\n").unwrap();
        let header: Value = serde_json::from_str(header).unwrap();
        assert_eq!(words, frame["text"].as_str().unwrap());
        assert_eq!(
            header["playback_at_turn_start"],
            frame["playback_at_turn_start"]
        );
        assert_eq!(
            header["not_played_at_turn_start"],
            frame["not_played_at_turn_start"]
        );
        assert_eq!(header["origin"], "spoken_room");
        assert!(header.get("text").is_none());
        assert!(header.get("type").is_none());
        assert!(!rendered.contains("[Sidevoice]"));
    }

    #[test]
    fn pull_and_push_render_the_same_logical_event() {
        let push = spoken();
        let mut pull = push.clone();
        pull["cursor"] = json!(25);
        pull["arrival_time"] = json!(1792000000);
        assert_eq!(envelope(&push).unwrap(), envelope(&pull).unwrap());
    }

    #[test]
    fn typed_room_has_no_acoustic_claim_or_spoken_epoch_change() {
        let mut frame = spoken();
        frame["origin"] = json!("typed_room");
        frame["epoch"] = json!(7);
        let rendered = envelope(&frame).unwrap();
        let header: Value = serde_json::from_str(rendered.lines().next().unwrap()).unwrap();
        assert_eq!(header["epoch"], 7);
        assert_eq!(header["origin"], "typed_room");
        assert!(header.get("playback_at_turn_start").is_none());
        assert!(header.get("not_played_at_turn_start").is_none());
    }

    #[test]
    fn notes_are_json_only_and_cannot_carry_user_words() {
        let frame = json!({"channel":"note","event":"user_returned","message_id":"m-43",
            "session_id":"s-7","revision":3,"epoch":9,
            "hearing_changes":[{"utterance_id":"u-19","attempt_id":null,
                "hearing":"not_played","heard_chars":0}]});
        let rendered = envelope(&frame).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&rendered).unwrap()["event"],
            "user_returned"
        );
        assert!(!rendered.contains("\n\n"));
        let mut bad = frame.clone();
        bad["text"] = json!("invented words");
        assert!(envelope(&bad).is_err());
    }

    #[test]
    fn unknown_evidence_stays_unknown() {
        let mut frame = spoken();
        frame
            .as_object_mut()
            .unwrap()
            .remove("playback_at_turn_start");
        let header: Value =
            serde_json::from_str(envelope(&frame).unwrap().lines().next().unwrap()).unwrap();
        assert_eq!(header["playback_at_turn_start"]["knowledge"], "unknown");
        assert!(header["playback_at_turn_start"]["last_played"].is_null());
    }

    #[test]
    fn unknown_channel_is_read_as_voice() {
        let mut frame = spoken();
        frame["channel"] = json!("future-channel");
        let header: Value =
            serde_json::from_str(envelope(&frame).unwrap().lines().next().unwrap()).unwrap();
        assert_eq!(header["channel"], "voice");
    }

    #[test]
    fn a_call_tied_to_no_conversation_names_the_agent_and_the_agents_sidevoice_joins() {
        let said = |client: serde_json::Value| unidentified(Some(&client));
        let codex = said(json!({"name": "codex-mcp-client", "version": "0.160.0"}));
        assert!(codex.contains("Codex 0.160.0 did not say which of its conversations is calling"));
        assert!(said(json!({"name": "claude-code"})).contains("Claude Code did not say"));
        assert!(said(json!({"name": "cursor-vscode"})).contains("Cursor did not say"));
        let other = said(json!({"name": "zed"}));
        assert!(other.contains("zed is not an agent Sidevoice joins"));
        assert!(unidentified(None).contains("the agent did not say who it is"));
    }
}
