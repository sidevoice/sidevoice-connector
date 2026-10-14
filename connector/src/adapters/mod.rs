pub mod claude;
pub mod codex;
mod conversation;
pub mod cursor;
mod harness;
pub mod health;
mod http;
pub mod registration;
mod transcript;

pub use conversation::{Conversation, Room};
pub use harness::{Harness, Registry, Route};

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

/// One agent-visible envelope for both injected input and get_messages. The structured facts are kept in full;
/// prose is only guidance, never a substitute for the core's evidence.
pub fn envelope(event: &Value) -> Result<String> {
    let channel = match event.get("channel").and_then(Value::as_str) {
        Some("note") => "note",
        Some("room-control") => "room-control",
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
    let text = event
        .get("text")
        .and_then(Value::as_str)
        .context("text required")?;
    let header = json!({"channel":channel,"session_id":session_id,"revision":revision,
        "epoch":epoch,"message_id":message_id});
    let mut body = format!("{}\n\n", header);
    if channel == "note" {
        if !text.is_empty() {
            bail!("a Sidevoice note cannot contain user words");
        }
        body.push_str(
            "[Sidevoice] A note from the room; the person sent no words in this message.",
        );
    } else {
        body.push_str(text);
    }
    if channel != "room-control" {
        let verb = if channel == "note" {
            "If a reply is appropriate, use voice_say"
        } else {
            "Reply with voice_say"
        };
        body.push_str(&format!("\n\n[Sidevoice] {verb} (session_id {}, revision {revision}, epoch {epoch}). Treat this message ID as stable: a redelivery is not new input.", serde_json::to_string(session_id)?));
    }
    if let Some(notice) = event.get("notice").filter(|v| !v.is_null()) {
        body.push_str("\n[Sidevoice] Notice: ");
        body.push_str(&notice.to_string());
        if let Some(instruction) = notice.get("instruction").and_then(Value::as_str) {
            body.push_str("\n[Sidevoice] ");
            body.push_str(instruction);
        }
    }
    if let Some(unheard) = event.get("unheard").filter(|v| !v.is_null()) {
        body.push_str("\n[Sidevoice] Complete unheard evidence: ");
        body.push_str(&unheard.to_string());
        if let Some(replies) = unheard.get("replies").and_then(Value::as_array) {
            for reply in replies {
                if let Some(full) = reply.get("text").and_then(Value::as_str) {
                    let (heard, remainder) = match reply.get("heard_chars").and_then(Value::as_u64)
                    {
                        Some(count) => {
                            let split = usize::try_from(count).unwrap_or(usize::MAX);
                            (
                                Some(full.chars().take(split).collect::<String>()),
                                Some(full.chars().skip(split).collect::<String>()),
                            )
                        }
                        None => (None, None),
                    };
                    body.push_str(&format!("\n[Sidevoice] Reply {}: heard prefix {}; unheard remainder {}; full text {}.",
                        reply.get("utterance_id").unwrap_or(&Value::Null),
                        serde_json::to_string(&heard)?, serde_json::to_string(&remainder)?,
                        serde_json::to_string(full)?));
                }
            }
        }
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::envelope;
    use super::unidentified;
    use serde_json::{json, Value};

    fn event(kind: &str, channel: &str, text: &str) -> serde_json::Value {
        json!({"channel":channel,"session_id":"s","revision":4,"epoch":2,
            "message_id":format!("m-{kind}"),"text":text,
            "notice":{"kind":kind,"instruction":"Use the current context.",
                "related_message_id":"m-original"}})
    }

    #[test]
    fn t16_notice_matrix_has_identical_inject_and_pull_meaning() {
        // The eleven scenarios are the producer matrix in core's flow.rs. The transport extras
        // differ; every agent-visible fact and instruction is rendered by this one formatter.
        for scenario in [
            "cut_words",
            "cut_empty_early",
            "cut_empty_late",
            "unplayed_words",
            "unplayed_empty_early",
            "unplayed_empty_late",
            "refused_empty",
            "return",
            "skip",
            "stale",
            "full_evidence",
        ] {
            let (kind, channel, words) = match scenario {
                "cut_words" => ("reply_interrupted", "voice", "Wait"),
                "cut_empty_early" | "cut_empty_late" => ("reply_interrupted", "note", ""),
                "unplayed_words" => ("reply_not_played", "voice", "Next"),
                "unplayed_empty_early" | "unplayed_empty_late" => ("reply_not_played", "note", ""),
                "refused_empty" => ("turn_empty", "note", ""),
                "return" => ("user_returned", "note", ""),
                "skip" => ("reply_skipped", "note", ""),
                "stale" | "full_evidence" => ("reply_not_played", "note", ""),
                _ => unreachable!(),
            };
            let mut push = event(kind, channel, words);
            push["message_id"] = json!(format!("m-{scenario}"));
            let long = "é🐙".to_owned() + &"z".repeat(300);
            let evidence = |n: usize| {
                json!({"utterance_id":format!("u{n}"),
                "history_id":format!("h{n}"),"session_id":"s","revision":3,"epoch":1,
                "text":long.clone(),"fate":if scenario.starts_with("cut") {"interrupted"} else {"unplayed"},
                "reason":if scenario == "stale" {"newer_turn"} else {"user_interrupted"},
                "cut":scenario.starts_with("cut"),"heard_chars":if scenario.starts_with("cut") {json!(2)} else {Value::Null}})
            };
            if scenario == "skip" {
                push["notice"]["skipped"] =
                    json!([{"utterance_id":"u0","history_id":"h0","fate":"skipped"}]);
            } else if scenario != "refused_empty" {
                let count = if scenario == "full_evidence" { 8 } else { 1 };
                push["unheard"] = json!({"count":count,
                    "replies":(0..count).map(evidence).collect::<Vec<_>>()});
            }
            let mut pull = push.clone();
            pull["cursor"] = json!(25);
            pull["arrival_time"] = json!(1792000000);
            let injected = envelope(&push).unwrap();
            assert_eq!(injected, envelope(&pull).unwrap(), "{scenario}");
            assert!(injected.contains(&format!("m-{scenario}")));
            assert!(injected.contains("m-original"), "related ID: {scenario}");
            if scenario != "refused_empty" && scenario != "skip" {
                assert!(
                    injected.contains(&"z".repeat(300)),
                    "full text clipped: {scenario}"
                );
            }
            if scenario.starts_with("cut") {
                assert!(
                    injected.contains("heard prefix \"é🐙\""),
                    "scalar split: {scenario}"
                );
            }
            if channel == "note" {
                assert!(injected.contains("person sent no words"), "{scenario}");
            }
        }
    }

    #[test]
    fn t14_later_hearing_correction_retains_its_link_and_instruction() {
        let mut correction = event("reply_heard", "note", "");
        correction["unheard"] = Value::Null;
        correction["notice"]["corrects_message_id"] = json!("m-earlier");
        correction["notice"]["heard"] = json!([{"utterance_id":"u","history_id":"h",
            "fate":"heard","heard_chars":304}]);
        correction["notice"]["instruction"] =
            json!("Do not resend solely because of the earlier notice.");
        let body = envelope(&correction).unwrap();
        assert!(body.contains("m-earlier"));
        assert!(body.contains("Do not resend solely"));
        assert!(body.contains("If a reply is appropriate"));
        assert!(!body.contains("Complete unheard evidence"));
    }

    #[test]
    fn t04b_t05_nine_reply_evidence_keeps_both_batches_in_publication_order() {
        let mut delivered = Vec::new();
        for batch in [0..8, 8..9] {
            let mut note = event("reply_not_played", "note", "");
            note["message_id"] = json!(format!("batch-{}", batch.start));
            note["unheard"] = json!({"count":9,"replies":batch.map(|n| json!({
                "utterance_id":format!("u{n}"),"history_id":format!("h{n}"),
                "text":format!("reply {n}"),"heard_chars":null,"fate":"unplayed"
            })).collect::<Vec<_>>()});
            delivered.push(envelope(&note).unwrap());
        }
        let first = &delivered[0];
        let second = &delivered[1];
        for n in 0..8 {
            assert!(first.contains(&format!("reply {n}")));
        }
        assert!(second.contains("reply 8"));
        assert!(first.contains("batch-0") && second.contains("batch-8"));
        assert!(first.contains("m-original") && second.contains("m-original"));
    }

    #[test]
    fn t01_t02_t12_t14_shared_core_fixture_keeps_scalar_and_fate_meaning() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/conversation-flow.json"))
                .unwrap();
        let original = fixture["text"].as_str().unwrap();
        for (index, case) in fixture["playback"].as_array().unwrap().iter().enumerate() {
            let mut frame = event("reply_interrupted", "voice", "Current words");
            frame["unheard"] = json!({"count":1,"replies":[{"utterance_id":format!("u{index}"),
                "text":original,"heard_chars":case["history"]["heard_chars"],
                "fate":case["history"]["status"],"reason":case["history"]["audio_reason"]}]});
            let rendered = envelope(&frame).unwrap();
            assert!(rendered.contains(original));
            if case["history"]["heard_chars"] == 2 {
                assert!(rendered.contains("heard prefix \"é🐙\""));
            }
        }
    }

    #[test]
    fn notes_cannot_invent_user_words_and_unknown_channel_is_voice() {
        let mut note = event("turn_empty", "note", "invented words");
        assert!(envelope(&note).is_err());
        note["text"] = json!("");
        assert!(envelope(&note).unwrap().contains("person sent no words"));
        note["channel"] = json!("from-the-future");
        note["text"] = json!("words");
        assert_eq!(
            envelope(&note).unwrap().lines().next().unwrap(),
            r#"{"channel":"voice","epoch":2,"message_id":"m-turn_empty","revision":4,"session_id":"s"}"#
        );
    }

    #[test]
    fn a_call_tied_to_no_conversation_names_the_agent_and_the_agents_sidevoice_joins() {
        let said = |client: serde_json::Value| unidentified(Some(&client));
        let codex = said(json!({"name": "codex-mcp-client", "version": "0.160.0"}));
        assert!(
            codex.contains("Codex 0.160.0 did not say which of its conversations is calling"),
            "{codex}"
        );
        assert!(said(json!({"name": "claude-code"})).contains("Claude Code did not say"));
        assert!(said(json!({"name": "cursor-vscode"})).contains("Cursor did not say"));
        let other = said(json!({"name": "zed"}));
        assert!(
            other.contains("zed is not an agent Sidevoice joins"),
            "{other}"
        );
        assert!(unidentified(None).contains("the agent did not say who it is"));
        for text in [codex, other] {
            assert!(text.ends_with("Sidevoice joins conversations of Claude Code, Codex (the CLI and the desktop app) and Cursor."), "{text}");
            assert!(!text.contains("SIDEVOICE_"), "{text}");
        }
    }
}
