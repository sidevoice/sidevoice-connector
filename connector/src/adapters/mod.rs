pub mod claude;
mod codex;
pub mod cursor;
mod http;

use crate::profile::Profile;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

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

/// What the agent reads for a delivery: the JSON header, then
/// - `voice`: the user's words and the `[Sidevoice]` lines (how to answer, and what the user did
///   not hear, when core says);
/// - `note`: the room's note that the user came back without saying anything, and what they did
///   not hear; it has no words of the user's;
/// - `room-control`: the text as it is.
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
        channel: match channel {
            "room-control" | "note" => channel,
            _ => "voice",
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
    let header_text = serde_json::to_string(&header)?;
    let unheard = unheard(event);
    if header.channel == "note" {
        let (count, list) = unheard.unwrap_or_default();
        return Ok(format!(
            "{header_text}\n\n[Sidevoice] A note from the room, not the user: the user came back and has not said anything yet. They did not hear {count} of your replies{list}\nTell them by voice, briefly, where things stand: what still matters from those replies and what is pending on them, without repeating them as they were. Reply with voice_say (session_id \"{}\", revision {}).",
            header.session_id, header.revision
        ));
    }
    let text = event
        .get("text")
        .and_then(Value::as_str)
        .context("text required")?;
    let mut body = format!("{}\n\n{}", header_text, text);
    if header.channel != "room-control" {
        body.push_str(&format!(
            "\n\n[Sidevoice] Voice from the room: acknowledge with voice_say (session_id \"{}\", revision {}) before any other tool, then work and reply by voice, as the sidevoice server's instructions say.",
            header.session_id, header.revision
        ));
        if let Some((count, list)) = unheard {
            body.push_str(&format!(
                "\n[Sidevoice] The user did not hear {count} of your earlier replies{list}\nDo not repeat them as they were: answer what the user says now and fold in only what still matters."
            ));
        }
    }
    Ok(body)
}

/// What the user did not hear, as core sends it (`unheard: {count, replies: [{text, truncated,
/// cut, heard_chars?}]}`, already bounded): the count, and the replies it shows as lines after a
/// colon, oldest first. None when there is nothing.
fn unheard(event: &Value) -> Option<(u64, String)> {
    let unheard = event.get("unheard")?;
    let count = unheard
        .get("count")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)?;
    let replies = unheard
        .get("replies")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut list = String::new();
    if !replies.is_empty() {
        let shown = replies.len() as u64;
        list.push_str(&if shown < count {
            format!(" (the latest {shown}, oldest first):")
        } else {
            " (oldest first):".to_owned()
        });
    } else {
        list.push('.');
    }
    for reply in replies {
        let mut text = reply
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if reply.get("truncated").and_then(Value::as_bool) == Some(true) {
            text.push('…');
        }
        let cut = reply.get("cut").and_then(Value::as_bool) == Some(true);
        let how = match reply.get("heard_chars").and_then(Value::as_u64) {
            Some(heard) if cut => {
                format!("cut off while playing: only its first {heard} characters were heard")
            }
            _ if cut => "cut off while playing: only its start was heard".to_owned(),
            _ => "never played".to_owned(),
        };
        let quoted = serde_json::to_string(&text).unwrap_or_default();
        list.push_str(&format!("\n- {quoted} ({how})"));
    }
    Some((count, list))
}

pub async fn deliver(
    delivery: &Value,
    thread: &str,
    profile: &Profile,
    event: &Value,
) -> Result<Value> {
    match delivery.get("kind").and_then(Value::as_str).unwrap_or("") {
        "claude-uds" => claude::deliver(delivery, event).await,
        "codex-queue" => codex::deliver(delivery, event, profile).await,
        "http" => http::deliver(delivery, event).await,
        "cursor-tmux" => cursor::deliver(delivery, event).await,
        "cursor-app" => bail!("Cursor editor card delivery is not prepared"),
        _ => {
            Ok(json!({"status":"unsupported","detail":format!("{} has no delivery route", thread)}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::envelope;
    use serde_json::json;

    const HEADER: &str = r#"{"channel":"voice","session_id":"s","revision":4,"message_id":"m"}"#;

    fn voice() -> serde_json::Value {
        json!({"channel":"voice","session_id":"s","revision":4,"message_id":"m","text":"And the tests?"})
    }

    #[test]
    fn a_voice_message_without_unheard_replies_is_as_before() {
        let body = envelope(&voice()).unwrap();
        assert!(body.starts_with(&format!(
            "{HEADER}\n\nAnd the tests?\n\n[Sidevoice] Voice from the room:"
        )));
        assert!(!body.contains("did not hear"));
    }

    #[test]
    fn a_voice_message_lists_what_the_user_did_not_hear() {
        let mut event = voice();
        event["unheard"] = json!({"count":5,"replies":[
            {"text":"The build is \"green\".","truncated":false,"cut":true},
            {"text":"Second part","truncated":true,"cut":false}]});
        let body = envelope(&event).unwrap();
        let trailer = body
            .split("\n\n[Sidevoice] Voice from the room:")
            .nth(1)
            .unwrap();
        assert!(
            trailer.ends_with(concat!(
                "\n[Sidevoice] The user did not hear 5 of your earlier replies (the latest 2, oldest first):",
                "\n- \"The build is \\\"green\\\".\" (cut off while playing: only its start was heard)",
                "\n- \"Second part…\" (never played)",
                "\nDo not repeat them as they were: answer what the user says now and fold in only what still matters."
            )),
            "{trailer}"
        );
        // The header the read receipts look for comes first, once.
        assert_eq!(body.matches("{\"channel\":").count(), 1);
    }

    #[test]
    fn a_cut_reply_says_how_far_it_was_heard_when_core_knows() {
        let mut event = voice();
        event["unheard"] = json!({"count":1,"replies":[
            {"text":"The build is green.","truncated":false,"cut":true,"heard_chars":9}]});
        let body = envelope(&event).unwrap();
        assert!(
            body.contains("\n- \"The build is green.\" (cut off while playing: only its first 9 characters were heard)"),
            "{body}"
        );
    }

    #[test]
    fn a_note_has_no_user_words_and_says_how_to_answer() {
        let event = json!({"channel":"note","session_id":"s","revision":7,"message_id":"note:1","text":"",
            "unheard":{"count":1,"replies":[{"text":"Done.","truncated":false,"cut":false}]}});
        let body = envelope(&event).unwrap();
        assert_eq!(
            body,
            concat!(
                r#"{"channel":"note","session_id":"s","revision":7,"message_id":"note:1"}"#,
                "\n\n[Sidevoice] A note from the room, not the user: the user came back and has not said anything yet.",
                " They did not hear 1 of your replies (oldest first):\n- \"Done.\" (never played)",
                "\nTell them by voice, briefly, where things stand: what still matters from those replies and what is",
                " pending on them, without repeating them as they were. Reply with voice_say (session_id \"s\", revision 7)."
            )
        );
    }

    #[test]
    fn room_control_stays_as_it_is() {
        let event = json!({"channel":"room-control","session_id":"s","revision":1,"message_id":"m","text":"x",
            "unheard":{"count":1,"replies":[]}});
        assert_eq!(
            envelope(&event).unwrap(),
            r#"{"channel":"room-control","session_id":"s","revision":1,"message_id":"m"}"#
                .to_owned()
                + "\n\nx"
        );
    }
}
