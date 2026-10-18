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
        if let Some(identity) =
            http::identity()?.filter(|identity| identity.delivery["kind"] == "http")
        {
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
    let replies = event
        .get("unheard")
        .and_then(|unheard| unheard.get("replies"))
        .and_then(Value::as_array)
        .filter(|replies| !replies.is_empty());
    if replies.is_some() {
        let destination = if channel == "note" { "note" } else { "message" };
        body.push_str(&format!(
            "[Sidevoice] Part of your last reply wasn't heard — see the end of this {destination}.\n\n"
        ));
    }
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
    if let Some(notice) = event.get("notice").filter(|v| !v.is_null()) {
        body.push_str("\n\n[Sidevoice] Notice: ");
        body.push_str(&notice.to_string());
        if let Some(instruction) = notice.get("instruction").and_then(Value::as_str) {
            body.push_str("\n[Sidevoice] ");
            body.push_str(instruction);
        }
    }
    if let Some(replies) = replies {
        if channel == "note" {
            body.push_str("\n\n[Sidevoice] The person didn't hear the end of your last reply and sent no new words. Tell them now what they missed that still matters, in fresh words.\n");
        } else {
            body.push_str("\n\n[Sidevoice] The person did not hear the end of your last reply.\n");
            body.push_str("In this reply, tell them what they missed that still matters, in fresh words (don't repeat what they heard; drop what is now obsolete), together with what they ask now. Skip it only if they changed topic or said to.\n");
        }
        for reply in replies {
            let full = reply.get("text").and_then(Value::as_str).unwrap_or("");
            body.push_str("\n[Sidevoice] Reply starting ");
            body.push_str(&serde_json::to_string(&reply_opening(full))?);
            if let Some(id) = reply.get("utterance_id").and_then(Value::as_str) {
                body.push_str(" (utterance ");
                body.push_str(&serde_json::to_string(id)?);
                body.push(')');
            }
            body.push_str(":\n");
            match reply.get("heard_chars").and_then(Value::as_u64) {
                Some(0) => body.push_str("Nothing of it was heard.\n"),
                Some(count) => {
                    let heard = full
                        .chars()
                        .take(usize::try_from(count).unwrap_or(usize::MAX))
                        .collect::<String>();
                    body.push_str("Heard up to: ");
                    body.push_str(&serde_json::to_string(&heard_tail(&heard))?);
                    body.push_str("\nEverything after that was not heard.\n");
                }
                None => body.push_str("How much was heard is unknown.\n"),
            }
        }
    }
    if channel != "room-control" {
        let separator = if replies.is_some() { "\n" } else { "\n\n" };
        body.push_str(&format!("{separator}[Sidevoice] If a reply is appropriate, use voice_say (session_id {}, revision {revision}, epoch {epoch}). Treat this message ID as stable: a redelivery is not new input.", serde_json::to_string(session_id)?));
    }
    Ok(body)
}

fn heard_tail(heard: &str) -> String {
    const LIMIT: usize = 200;
    let chars = heard.chars().collect::<Vec<_>>();
    if chars.len() <= LIMIT {
        return heard.to_owned();
    }
    let boundary = chars
        .iter()
        .enumerate()
        .skip(chars.len() - LIMIT)
        .find(|(index, ch)| *index <= chars.len() - LIMIT + 40 && ch.is_whitespace());
    let start = boundary.map_or(chars.len() - LIMIT, |(index, _)| index + 1);
    format!("…{}", chars[start..].iter().collect::<String>())
}

fn reply_opening(full: &str) -> String {
    const LIMIT: usize = 60;
    let chars = full.trim().chars().collect::<Vec<_>>();
    if chars.len() <= 1 {
        return "…".to_owned();
    }
    // Always leave part of the reply out, even when the whole reply is short.
    let limit = LIMIT.min(chars.len() - 1);
    let end = chars[..limit]
        .iter()
        .rposition(|ch| ch.is_whitespace())
        .filter(|index| *index > 0)
        .unwrap_or(limit);
    format!("{}…", chars[..end].iter().collect::<String>())
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
        let fixture: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/conversation-flow.json"))
                .unwrap();
        let original = fixture["text"].as_str().unwrap();
        let partial_extent = fixture["hearing"][1]["history"]["heard_chars"].clone();
        for scenario in [
            "partial_words",
            "partial_empty_early",
            "partial_empty_late",
            "not_heard_words",
            "not_heard_empty_early",
            "not_heard_empty_late",
            "refused_empty",
            "return",
            "local_stop",
            "stale",
            "full_evidence",
        ] {
            let (kind, channel, words) = match scenario {
                "partial_words" => ("reply_not_heard", "voice", "Wait"),
                "partial_empty_early" | "partial_empty_late" => ("reply_not_heard", "note", ""),
                "not_heard_words" => ("reply_not_heard", "voice", "Next"),
                "not_heard_empty_early" | "not_heard_empty_late" => ("reply_not_heard", "note", ""),
                "refused_empty" => ("turn_empty", "note", ""),
                "return" => ("user_returned", "note", ""),
                "local_stop" | "stale" | "full_evidence" => ("reply_not_heard", "note", ""),
                _ => unreachable!(),
            };
            let mut push = event(kind, channel, words);
            push["message_id"] = json!(format!("m-{scenario}"));
            let long = original.to_owned() + &"z".repeat(300);
            let evidence = |n: usize| {
                json!({"utterance_id":format!("u{n}"),
                "history_id":format!("h{n}"),"session_id":"s","revision":3,"epoch":1,
                "text":long.clone(),"fate":if scenario.starts_with("partial") || scenario == "local_stop" {"partial"} else {"not_heard"},
                "heard_chars":if scenario.starts_with("partial") || scenario == "local_stop" {partial_extent.clone()} else {json!(0)}})
            };
            if scenario != "refused_empty" {
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
            if scenario != "refused_empty" {
                assert!(
                    injected.contains(if channel == "note" {
                        "The person didn't hear the end of your last reply and sent no new words. Tell them now what they missed that still matters, in fresh words."
                    } else {
                        "The person did not hear the end of your last reply."
                    }),
                    "{scenario}"
                );
                assert!(injected.contains("Reply starting \"é🐙ab"), "{scenario}");
                assert!(!injected.contains(&"z".repeat(300)), "{scenario}");
                assert!(
                    !injected.contains("Complete unheard evidence"),
                    "{scenario}"
                );
                assert!(
                    injected
                        .find(if channel == "note" {
                            "The person didn't hear the end"
                        } else {
                            "The person did not hear the end"
                        })
                        .unwrap()
                        < injected
                            .find("If a reply is appropriate, use voice_say")
                            .unwrap(),
                    "{scenario}"
                );
                let preface = "Part of your last reply wasn't heard — see the end of this";
                assert!(
                    injected.find(preface).unwrap() < injected.find("Notice:").unwrap(),
                    "{scenario}"
                );
                if scenario == "full_evidence" {
                    let positions = (0..8)
                        .map(|n| injected.find(&format!("(utterance \"u{n}\")")).unwrap())
                        .collect::<Vec<_>>();
                    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
                }
            } else {
                assert!(!injected.contains("Part of your last reply wasn't heard"));
            }
            if scenario.starts_with("partial") || scenario == "local_stop" {
                assert!(
                    injected.contains("Heard up to: \"é🐙\""),
                    "scalar split: {scenario}"
                );
            } else if scenario != "refused_empty" {
                assert!(injected.contains("Nothing of it was heard."), "{scenario}");
            }
            if channel == "note" {
                assert!(injected.contains("person sent no words"), "{scenario}");
                assert!(!injected.contains("In your next reply"), "{scenario}");
            } else {
                assert!(
                    injected.contains("\n\nWait\n\n") || injected.contains("\n\nNext\n\n"),
                    "{scenario}"
                );
            }
        }
    }

    #[test]
    fn typed_context_keeps_its_epoch_and_leaves_the_reply_to_the_agent() {
        let mut push = event("typed_context", "voice", "also use the second table");
        push.as_object_mut().unwrap().remove("notice");
        let mut pull = push.clone();
        pull["cursor"] = json!(25);
        pull["arrival_time"] = json!(1792000000);
        let injected = envelope(&push).unwrap();
        assert_eq!(injected, envelope(&pull).unwrap());
        let header: Value = serde_json::from_str(injected.lines().next().unwrap()).unwrap();
        assert_eq!(header["epoch"], 2);
        assert!(injected.contains("\n\nalso use the second table\n\n"));
        assert!(injected.contains("If a reply is appropriate, use voice_say"));
        assert!(injected.contains("epoch 2"));
        assert!(!injected.contains("Reply with voice_say"));
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
        assert!(!body.contains("Part of your last reply wasn't heard"));
        assert!(!body.contains("The person did not hear the end"));

        correction["unheard"] = json!({"count":0,"replies":[]});
        let empty = envelope(&correction).unwrap();
        assert!(!empty.contains("Part of your last reply wasn't heard"));
        assert!(!empty.contains("The person did not hear the end"));
    }

    #[test]
    fn t04b_t05_nine_reply_evidence_keeps_both_batches_in_publication_order() {
        let mut delivered = Vec::new();
        for batch in [0..8, 8..9] {
            let mut note = event("reply_not_heard", "note", "");
            note["message_id"] = json!(format!("batch-{}", batch.start));
            note["unheard"] = json!({"count":9,"replies":batch.map(|n| json!({
                "utterance_id":format!("u{n}"),"history_id":format!("h{n}"),
                "text":format!("reply {n}"),"heard_chars":0,"fate":"not_heard"
            })).collect::<Vec<_>>()});
            delivered.push(envelope(&note).unwrap());
        }
        let first = &delivered[0];
        let second = &delivered[1];
        for n in 0..8 {
            assert!(first.contains(&format!("Reply starting \"reply…\" (utterance \"u{n}\")")));
            assert!(!first.contains(&format!("Reply starting \"reply {n}\"")));
        }
        assert!(second.contains("Reply starting \"reply…\" (utterance \"u8\")"));
        assert!(first.contains("batch-0") && second.contains("batch-8"));
        assert!(first.contains("m-original") && second.contains("m-original"));
    }

    #[test]
    fn t01_t02_t12_t14_shared_core_fixture_keeps_scalar_and_fate_meaning() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/conversation-flow.json"))
                .unwrap();
        let original = fixture["text"].as_str().unwrap();
        for (index, case) in fixture["hearing"].as_array().unwrap().iter().enumerate() {
            if case["history"]["hearing"] == "heard" {
                continue;
            }
            let mut frame = event("reply_not_heard", "voice", "Current words");
            frame["unheard"] = json!({"count":1,"replies":[{"utterance_id":format!("u{index}"),
                "text":original,"heard_chars":case["history"]["heard_chars"],
                "fate":case["history"]["hearing"]}]});
            let rendered = envelope(&frame).unwrap();
            if case["history"]["heard_chars"] == 2 {
                assert!(rendered.contains("Heard up to: \"é🐙\""));
                assert!(rendered.contains("Everything after that was not heard."));
            } else if case["history"]["heard_chars"] == 0 {
                assert!(rendered.contains("Nothing of it was heard."));
            }
            assert!(rendered.contains("Reply starting \"é🐙a…\""));
        }
    }

    #[test]
    fn long_multibyte_heard_prefix_uses_a_bounded_word_boundary_tail() {
        let heard = format!("{}é🐙 final sentence.", "Earlier sentence. ".repeat(20));
        let full = format!("{heard} UNHEARD_SECRET_END");
        let mut frame = event("reply_not_heard", "voice", "What now?");
        frame["unheard"] = json!({"count":1,"replies":[{
            "text":full,"heard_chars":heard.chars().count(),"fate":"partial"
        }]});
        let rendered = envelope(&frame).unwrap();
        let tail = super::heard_tail(&heard);
        assert!(tail.starts_with('…'));
        assert!(tail.chars().count() <= 201);
        assert!(tail.ends_with("é🐙 final sentence."));
        assert!(rendered.contains(&format!(
            "Heard up to: {}",
            serde_json::to_string(&tail).unwrap()
        )));
        assert!(!rendered.contains("UNHEARD_SECRET_END"));
        assert!(!rendered.contains(&heard));
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
