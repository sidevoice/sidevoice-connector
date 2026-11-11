pub mod claude;
pub mod codex;
pub mod cursor;
mod harness;
pub mod health;
mod http;
pub(crate) mod markers;
pub mod registration;
mod transcript;
mod watch;

pub use harness::{
    Agent, CallContext, Conversation, Harness, Inbound, Limit, Model, Registry, Sent, State, Status,
};
pub use http::{endpoint, post};

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
        if bytes > crate::frame::MESSAGE_LIMIT || (exposed > 1 && bytes > PULL_PAGE_TARGET_BYTES) {
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

/// The sentence every voice and note envelope ends with. A message after it begins a new envelope (`transcript::voice_headers`).
const ENVELOPE_END: &str = " Treat this message ID as stable: a redelivery is not new input.";

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
    if channel == "note" {
        if !text.is_empty() {
            bail!("a Sidevoice note cannot contain user words");
        }
        body.push_str(
            "[Sidevoice] A note from Sidevoice; the person sent no words in this message.",
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
    if let Some(block) = unheard_block(event.get("unheard"))? {
        body.push_str("\n\n");
        body.push_str(&block);
    }
    if channel != "room-control" {
        let say = format!(
            "voice_say (session_id {}, revision {revision}, epoch {epoch})",
            serde_json::to_string(session_id)?
        );
        let kind = event
            .get("notice")
            .and_then(|notice| notice.get("kind"))
            .and_then(Value::as_str);
        body.push_str("\n\n");
        body.push_str(&reply_rule(channel, kind, &say));
        body.push_str(ENVELOPE_END);
    }
    Ok(body)
}

/// The core's one not-heard block (`unheard: {count, from, heard_up_to?}`, no reply texts) as one line for the agent:
/// how many of its last replies the person did not hear, from which one, and how far that first one was heard. None
/// without a positive count.
fn unheard_block(unheard: Option<&Value>) -> Result<Option<String>> {
    let Some(unheard) = unheard.filter(|unheard| !unheard.is_null()) else {
        return Ok(None);
    };
    let count = unheard.get("count").and_then(Value::as_u64).unwrap_or(0);
    if count == 0 {
        return Ok(None);
    }
    let from = unheard
        .get("from")
        .and_then(Value::as_str)
        .filter(|from| !from.is_empty());
    let heard = unheard
        .get("heard_up_to")
        .and_then(Value::as_str)
        .filter(|heard| !heard.is_empty());
    let quote = |text: &str| serde_json::to_string(text);
    let mut line = if count == 1 {
        match (from, heard) {
            (Some(from), Some(heard)) => format!(
                "[Sidevoice] The person did not hear the end of your last reply, which began {}: they heard up to {}",
                quote(from)?,
                quote(heard)?
            ),
            (Some(from), None) => format!(
                "[Sidevoice] The person did not hear your last reply, which began {}",
                quote(from)?
            ),
            (None, _) => "[Sidevoice] The person did not hear your last reply".to_owned(),
        }
    } else {
        let mut line = format!("[Sidevoice] The person did not hear your last {count} replies");
        if let Some(from) = from {
            line.push_str(&format!(", the first of which began {}", quote(from)?));
            if let Some(heard) = heard {
                line.push_str(&format!(" (they heard that one up to {})", quote(heard)?));
            }
        }
        line
    };
    line.push('.');
    Ok(Some(line))
}

/// When and whether to answer. Only the content of the person's message decides whether; a note
/// says when the agent can speak again, never whether it should.
fn reply_rule(channel: &str, kind: Option<&str>, say: &str) -> String {
    match (channel, kind) {
        ("note", Some("turn_empty" | "user_returned")) => format!(
            "[Sidevoice] This note tells you when to speak again, not whether: the person can hear you now. If you still owe them an answer (a refused or unheard reply, or a result), give it now with {say}, with what is new and relevant at this moment, in fresh words, without saying it was missed."
        ),
        ("note", _) => format!(
            "[Sidevoice] This note tells you when to speak again, not whether: what the person asked decides that. Use {say} for your next spoken reply."
        ),
        _ => format!(
            "[Sidevoice] Only its content decides whether to answer by voice: if it asks or needs something, answer with {say}, short, with the details in writing."
        ),
    }
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

    /// The core's one not-heard block becomes one line: how many replies, from which, how far the first was heard; the
    /// same pushed or pulled, after the notice, and never more than the block says.
    #[test]
    fn the_not_heard_block_is_one_line_with_no_reply_texts() {
        let cases = [
            (
                "voice",
                "Wait",
                json!({"count":1,"from":"The deploy failed on…","heard_up_to":"The deploy","history_id":"h"}),
                "[Sidevoice] The person did not hear the end of your last reply, which began \"The deploy failed on…\": they heard up to \"The deploy\".",
            ),
            (
                "voice",
                "Next",
                json!({"count":3,"from":"é🐙…","heard_up_to":"é🐙","history_id":"h"}),
                "[Sidevoice] The person did not hear your last 3 replies, the first of which began \"é🐙…\" (they heard that one up to \"é🐙\").",
            ),
            (
                "note",
                "",
                json!({"count":2,"from":"Done…","history_id":"h","added_later":{"nested":true}}),
                "[Sidevoice] The person did not hear your last 2 replies, the first of which began \"Done…\".",
            ),
        ];
        for (channel, words, unheard, line) in cases {
            let kind = if channel == "note" {
                "user_returned"
            } else {
                "reply_not_heard"
            };
            let mut push = event(kind, channel, words);
            push["unheard"] = unheard;
            let mut pull = push.clone();
            pull["cursor"] = json!(25);
            pull["arrival_time"] = json!(1792000000);
            let injected = envelope(&push).unwrap();
            assert_eq!(injected, envelope(&pull).unwrap());
            assert!(injected.contains(line), "{injected}");
            assert_eq!(injected.matches("did not hear").count(), 1, "{injected}");
            assert!(injected.find("Notice:").unwrap() < injected.find(line).unwrap());
            assert!(injected.find(line).unwrap() < injected.find("voice_say (session_id").unwrap());
            assert!(!injected.contains("Reply starting"));
        }
        // Nothing to tell: no line.
        for unheard in [json!({"count":0}), Value::Null] {
            let mut frame = event("turn_empty", "note", "");
            frame["unheard"] = unheard;
            assert!(!envelope(&frame).unwrap().contains("did not hear"));
        }
    }

    #[test]
    fn typed_context_keeps_its_epoch_and_its_content_decides_the_reply() {
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
        assert!(injected.contains(
            "Only its content decides whether to answer by voice: if it asks or needs something, answer with voice_say (session_id \"s\", revision 4, epoch 2), short, with the details in writing."
        ));
        assert!(!injected.contains("If a reply is appropriate"));
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
