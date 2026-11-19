#[cfg(feature = "testbench")]
pub mod bench;
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
pub use http::{endpoint, post, Unregistered};

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

/// The one pointer sentence every voice and note envelope ends with. A message after it begins a new envelope
/// (`transcript::voice_headers`). Rules live once, in the MCP instructions and tool descriptions; a message carries
/// data only.
const ENVELOPE_END: &str = "See the Sidevoice instructions.";

/// How the person wants the agent to speak: the amount of content, never where it goes.
pub const SPEAKING_STYLES: [&str; 3] = ["brief", "normal", "detailed"];
/// The style until the app sends one.
pub const DEFAULT_SPEAKING_STYLE: &str = "normal";

/// The event's `speaking_style` when it is one of [`SPEAKING_STYLES`], else the default.
fn speaking_style(event: &Value) -> &'static str {
    let asked = event.get("speaking_style").and_then(Value::as_str);
    SPEAKING_STYLES
        .into_iter()
        .find(|style| Some(*style) == asked)
        .unwrap_or(DEFAULT_SPEAKING_STYLE)
}

/// A tag value: bare when it is a plain token, else a JSON string with `[` escaped so that the text can never read
/// as the start of another tag.
fn tag_value(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_:.@/+".contains(c));
    if plain {
        value.to_owned()
    } else {
        serde_json::to_string(value)
            .unwrap_or_default()
            .replace('[', "\\u005b")
    }
}

/// One agent-visible envelope for both injected input and get_messages: a key=value tag line (data only), the
/// person's words, the core's notice if any, and one pointer sentence.
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
    let mut tag = format!(
        "[sidevoice channel={channel} session_id={} revision={revision} epoch={epoch} message_id={}",
        tag_value(session_id),
        tag_value(message_id)
    );
    if channel != "room-control" {
        tag.push_str(" style=");
        tag.push_str(speaking_style(event));
    }
    tag.push_str(&unheard_tags(event.get("unheard")));
    tag.push(']');
    let mut parts = vec![tag];
    if channel == "note" {
        if !text.is_empty() {
            bail!("a Sidevoice note cannot contain user words");
        }
    } else {
        parts.push(text.to_owned());
    }
    if let Some(notice) = event.get("notice").filter(|v| !v.is_null()) {
        let mut block = format!("Notice: {notice}");
        if let Some(instruction) = notice
            .get("instruction")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            block.push('\n');
            block.push_str(instruction);
        }
        parts.push(block);
    }
    if channel != "room-control" {
        parts.push(ENVELOPE_END.to_owned());
    }
    Ok(parts.join("\n\n"))
}

/// The core's one not-heard block (`unheard: {count, from, heard_up_to?}`, no reply texts) as tags: how many of the
/// agent's last replies the person did not hear, from which one, and how far that first one was heard. Empty without
/// a positive count.
fn unheard_tags(unheard: Option<&Value>) -> String {
    let Some(unheard) = unheard.filter(|unheard| !unheard.is_null()) else {
        return String::new();
    };
    let count = unheard.get("count").and_then(Value::as_u64).unwrap_or(0);
    if count == 0 {
        return String::new();
    }
    let mut tags = format!(" unheard={count}");
    for (key, field) in [("unheard_from", "from"), ("heard_up_to", "heard_up_to")] {
        if let Some(text) = unheard
            .get(field)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            tags.push_str(&format!(" {key}={}", tag_value(text)));
        }
    }
    tags
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

    /// The core's not-heard block becomes tags on the tag line: how many replies, from which, how far the first was
    /// heard; the same pushed or pulled, and never more than the block says.
    #[test]
    fn the_not_heard_block_is_tags_with_no_reply_texts() {
        let cases = [
            (
                "voice",
                "Wait",
                json!({"count":1,"from":"The deploy failed on…","heard_up_to":"The deploy","history_id":"h"}),
                " unheard=1 unheard_from=\"The deploy failed on…\" heard_up_to=\"The deploy\"]",
            ),
            (
                "voice",
                "Next",
                json!({"count":3,"from":"é🐙…","heard_up_to":"é🐙","history_id":"h"}),
                " unheard=3 unheard_from=\"é🐙…\" heard_up_to=\"é🐙\"]",
            ),
            (
                "note",
                "",
                json!({"count":2,"from":"Done…","history_id":"h","added_later":{"nested":true}}),
                " unheard=2 unheard_from=\"Done…\"]",
            ),
        ];
        for (channel, words, unheard, tags) in cases {
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
            let tag_line = injected.lines().next().unwrap();
            assert!(tag_line.ends_with(tags), "{injected}");
            assert_eq!(injected.matches("unheard=").count(), 1, "{injected}");
            assert!(!injected.contains("did not hear"));
            assert!(!injected.contains("Reply starting"));
        }
        // Nothing to tell: no tag.
        for unheard in [json!({"count":0}), Value::Null] {
            let mut frame = event("turn_empty", "note", "");
            frame["unheard"] = unheard;
            assert!(!envelope(&frame).unwrap().contains("unheard"));
        }
    }

    #[test]
    fn a_message_carries_data_and_one_pointer_never_rules() {
        let mut push = event("typed_context", "voice", "also use the second table");
        push.as_object_mut().unwrap().remove("notice");
        let mut pull = push.clone();
        pull["cursor"] = json!(25);
        pull["arrival_time"] = json!(1792000000);
        let injected = envelope(&push).unwrap();
        assert_eq!(injected, envelope(&pull).unwrap());
        assert_eq!(
            injected,
            "[sidevoice channel=voice session_id=s revision=4 epoch=2 message_id=m-typed_context style=normal]\n\nalso use the second table\n\nSee the Sidevoice instructions."
        );
        for rule in [
            "in writing",
            "short",
            "Treat this message ID",
            "[Sidevoice]",
        ] {
            assert!(!injected.contains(rule), "{rule}");
        }
    }

    #[test]
    fn a_note_without_a_notice_is_the_tag_line_and_the_pointer() {
        let frame = json!({"channel":"note","session_id":"s","revision":4,"epoch":2,
            "message_id":"note:9","text":""});
        assert_eq!(
            envelope(&frame).unwrap(),
            "[sidevoice channel=note session_id=s revision=4 epoch=2 message_id=note:9 style=normal]\n\nSee the Sidevoice instructions."
        );
    }

    #[test]
    fn a_notice_instruction_is_also_its_own_plain_line() {
        let injected = envelope(&event("typed_context", "voice", "hi")).unwrap();
        assert!(
            injected.contains("\nUse the current context.\n\nSee the"),
            "{injected}"
        );
    }

    #[test]
    fn style_and_unheard_tags_round_trip_through_the_parser() {
        let mut frame = event("reply_not_heard", "voice", "go on");
        frame["speaking_style"] = json!("detailed");
        frame["unheard"] =
            json!({"count":2,"from":"The deploy [failed]…","heard_up_to":"The deploy"});
        let headers = crate::adapters::transcript::voice_headers(&envelope(&frame).unwrap());
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0]["style"], "detailed");
        assert_eq!(headers[0]["unheard"], 2);
        assert_eq!(headers[0]["unheard_from"], "The deploy [failed]…");
        assert_eq!(headers[0]["heard_up_to"], "The deploy");
    }

    #[test]
    fn the_style_tag_defaults_and_follows_the_app() {
        let style = |value: Option<Value>| {
            let mut frame = event("x", "voice", "hi");
            if let Some(value) = value {
                frame["speaking_style"] = value;
            }
            envelope(&frame).unwrap().lines().next().unwrap().to_owned()
        };
        assert!(style(None).contains(" style=normal]"));
        for value in ["brief", "normal", "detailed"] {
            assert!(style(Some(json!(value))).contains(&format!(" style={value}]")));
        }
        // An unknown or malformed value is the default, never passed through.
        for value in [json!("hands-free"), json!(3), Value::Null] {
            assert!(style(Some(value)).contains(" style=normal]"));
        }
        // Room-control messages need no reply and carry no style.
        let mut control = event("x", "room-control", "");
        control["speaking_style"] = json!("brief");
        assert!(!envelope(&control).unwrap().contains("style="));
    }

    #[test]
    fn notes_cannot_invent_user_words_and_unknown_channel_is_voice() {
        let mut note = event("turn_empty", "note", "invented words");
        assert!(envelope(&note).is_err());
        note["text"] = json!("");
        assert!(envelope(&note).unwrap().contains("channel=note"));
        note["channel"] = json!("from-the-future");
        note["text"] = json!("words");
        assert_eq!(
            envelope(&note).unwrap().lines().next().unwrap(),
            "[sidevoice channel=voice session_id=s revision=4 epoch=2 message_id=m-turn_empty style=normal]"
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
