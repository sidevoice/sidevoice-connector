//! Following a conversation's JSONL transcript as the harness appends it, and what the harness reads in each line.

use super::conversation::{report_working, Conversation, IdentityFailures, Reporter};
use crate::platform::fs::{FileId, Files, Fs};
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::time::Duration;

/// What one transcript line says about the conversation.
pub enum Observed {
    /// The engine it runs on: `{model, effort, thinking}`.
    Engine(Value),
    /// Whether it is working.
    Working(bool),
    /// A delivered message's header, now in the transcript: it was read.
    Read(Value),
}

/// How a harness reads its transcript.
pub trait TranscriptReader: Send + 'static {
    /// Where the transcript is, once the harness has written it.
    fn path(&self) -> Option<PathBuf>;
    /// Whether this transcript is still the conversation's; following ends when it is not.
    fn current(&self, _conversation: &Conversation) -> bool {
        true
    }
    /// Whether what it says of working and engine may be reported yet.
    fn authenticated(&self, _conversation: &Conversation) -> bool {
        true
    }
    /// Whether the harness may rewrite the file in place, so a changed tail means reading it again from the top.
    fn rewritten_in_place(&self) -> bool {
        false
    }
    /// What to report before reading, each pass.
    fn begin(&mut self, _authenticated: bool) -> Vec<Observed> {
        Vec::new()
    }
    /// What one line says.
    fn item(&mut self, item: &Value, authenticated: bool) -> Vec<Observed>;
}

/// Follows the conversation's transcript until it stops: waits for the file, then reads it as it grows.
pub fn follow<R: TranscriptReader>(
    conversation: Arc<Conversation>,
    reporter: Arc<dyn Reporter>,
    reader: R,
) {
    tokio::spawn(async move {
        while !conversation.stopped.load(Ordering::Relaxed) {
            if !reader.current(&conversation) {
                return;
            }
            if let Some(path) = reader.path() {
                follow_file(&*reporter, &conversation, path, reader).await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    });
}

async fn follow_file<R: TranscriptReader>(
    reporter: &dyn Reporter,
    conversation: &Conversation,
    path: PathBuf,
    mut reader: R,
) {
    let mut file_id: Option<FileId> = None;
    let mut offset = 0u64;
    let mut partial = Vec::new();
    let mut tail = Vec::new();
    let mut working = None;
    let mut catching_up = true;
    let mut announced_initial = false;
    let mut oversized = false;
    let rewritten_in_place = reader.rewritten_in_place();
    let mut identity_failures = IdentityFailures::default();
    loop {
        if conversation.stopped.load(Ordering::Relaxed) {
            break;
        }
        if !reader.current(conversation) {
            break;
        }
        let authenticated = reader.authenticated(conversation);
        for observed in reader.begin(authenticated) {
            report(
                reporter,
                conversation,
                observed,
                &mut working,
                catching_up,
                authenticated,
            )
            .await;
        }
        let Ok(mut input) = fs::File::open(&path) else {
            tokio::time::sleep(Duration::from_millis(400)).await;
            continue;
        };
        let Ok(meta) = input.metadata() else {
            continue;
        };
        // Which file was opened, from the handle read below: a different one after a rotation or a replacement. When
        // the OS cannot tell, nothing is read this round, and after a few in a row the transcript is given up.
        let current = match Files::identity(&input) {
            Ok(current) => {
                identity_failures.read();
                current
            }
            Err(error) => {
                if identity_failures.failed(&error) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(400)).await;
                continue;
            }
        };
        if file_id.as_ref() != Some(&current) || meta.len() < offset {
            file_id = Some(current);
            offset = 0;
            partial.clear();
            tail.clear();
            working = None;
            catching_up = true;
            announced_initial = false;
            oversized = false;
        }
        if rewritten_in_place && !tail.is_empty() && offset >= tail.len() as u64 {
            let mut current_tail = vec![0; tail.len()];
            if input
                .seek(SeekFrom::Start(offset - tail.len() as u64))
                .is_ok()
                && input.read_exact(&mut current_tail).is_ok()
                && current_tail != tail
            {
                offset = 0;
                partial.clear();
                tail.clear();
                working = None;
                catching_up = true;
                announced_initial = false;
            }
        }
        let before = offset;
        if input.seek(SeekFrom::Start(offset)).is_ok() {
            let mut bytes = [0u8; 65536];
            let limit = bytes.len().min(meta.len().saturating_sub(offset) as usize);
            if let Ok(count) = input.read(&mut bytes[..limit]) {
                offset += count as u64;
                if rewritten_in_place && count > 0 {
                    tail.extend_from_slice(&bytes[..count]);
                    if tail.len() > 64 {
                        tail.drain(..tail.len() - 64);
                    }
                }
                for segment in bytes[..count].split_inclusive(|byte| *byte == b'\n') {
                    if !oversized {
                        if partial.len() + segment.len() > 1 << 20 {
                            partial.clear();
                            oversized = true;
                        } else {
                            partial.extend_from_slice(segment);
                        }
                    }
                    if segment.last() == Some(&b'\n') {
                        if !oversized {
                            if let Ok(item) = serde_json::from_slice::<Value>(&partial) {
                                for observed in reader.item(&item, authenticated) {
                                    report(
                                        reporter,
                                        conversation,
                                        observed,
                                        &mut working,
                                        catching_up,
                                        authenticated,
                                    )
                                    .await;
                                }
                            }
                        }
                        partial.clear();
                        oversized = false;
                    }
                }
            }
        }
        if catching_up && offset >= meta.len() {
            catching_up = false;
        }
        if !catching_up && authenticated && !announced_initial {
            if let Some(state) = working {
                report_working(reporter, conversation, state).await;
            }
            announced_initial = true;
        }
        // More of the file waits: read on at once. One 64 KiB read per poll took minutes to reach the end of a
        // large transcript, and a message delivered meanwhile stayed unconfirmed until then.
        if offset > before && offset < meta.len() {
            continue;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

/// Reports one observation. Working is only remembered while catching up or before the transcript is authenticated;
/// the latest is announced once both are past.
async fn report(
    reporter: &dyn Reporter,
    conversation: &Conversation,
    observed: Observed,
    working: &mut Option<bool>,
    catching_up: bool,
    authenticated: bool,
) {
    match observed {
        Observed::Engine(engine) => reporter.engine(conversation, engine).await,
        Observed::Working(state) => {
            *working = Some(state);
            if !catching_up && authenticated {
                report_working(reporter, conversation, state).await;
            }
        }
        Observed::Read(header) => {
            let Some(message_id) = header.get("message_id").and_then(Value::as_str) else {
                return;
            };
            let session_id = header
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let revision = header.get("revision").and_then(Value::as_i64).unwrap_or(0);
            reporter
                .read(
                    conversation,
                    message_id,
                    json!(session_id),
                    json!(revision),
                    Value::Null,
                )
                .await;
        }
    }
}

/// The text of a user's line in a transcript: Claude Code's `user` entries and queued prompts (`claude`), or a
/// `role: user` entry (Cursor).
pub fn user_text(item: &Value, claude: bool) -> Option<String> {
    if claude
        && item.get("type") == Some(&json!("attachment"))
        && item.pointer("/attachment/type") == Some(&json!("queued_command"))
    {
        return item
            .pointer("/attachment/prompt")
            .and_then(Value::as_str)
            .map(str::to_owned);
    }
    let is_user = if claude {
        item.get("type") == Some(&json!("user"))
            && item.pointer("/message/role") == Some(&json!("user"))
    } else {
        item.get("role") == Some(&json!("user"))
    };
    if !is_user {
        return None;
    }
    let content = item
        .pointer("/message/content")
        .or_else(|| item.get("content"))?;
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    let parts = content.as_array()?;
    let texts = parts
        .iter()
        .filter_map(|part| {
            (part.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| part.get("text").and_then(Value::as_str))
                .flatten()
        })
        .collect::<Vec<_>>();
    (!texts.is_empty()).then(|| texts.join("\n"))
}

/// Every header a delivered message starts with (`envelope`) in `text`, in order. A harness may coalesce several
/// messages into one transcript entry, so one entry can carry many.
pub fn voice_headers(text: &str) -> Vec<Value> {
    let mut headers = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{\"channel\":") {
        let tail = &rest[start..];
        let Some(end) = tail.find('}') else {
            break;
        };
        match valid_header(&tail[..=end]) {
            Some(header) => {
                headers.push(header);
                rest = &tail[end + 1..];
            }
            None => rest = &tail[1..],
        }
    }
    headers
}

fn valid_header(text: &str) -> Option<Value> {
    let header: Value = serde_json::from_str(text).ok()?;
    if !matches!(
        header.get("channel").and_then(Value::as_str),
        Some("voice" | "room-control" | "note")
    ) || header
        .get("session_id")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
        || header
            .get("message_id")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || header
            .get("revision")
            .and_then(Value::as_i64)
            .is_none_or(|revision| revision < 0)
    {
        return None;
    }
    Some(header)
}

#[cfg(test)]
mod tests {
    use super::voice_headers;
    use serde_json::json;

    #[test]
    fn a_note_is_read_back_by_its_header_like_a_voice_message() {
        let note = crate::adapters::envelope(
            &json!({"channel":"note","session_id":"s","revision":2,"epoch":1,
            "message_id":"note:1","text":"","unheard":{"count":1,"replies":[]}}),
        )
        .unwrap();
        let headers = voice_headers(&note);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0]["message_id"], "note:1");
        assert_eq!(headers[0]["channel"], "note");
    }

    #[test]
    fn every_envelope_of_a_coalesced_entry_is_read_in_order() {
        let envelope = |channel: &str, id: &str, words: &str| {
            let header =
                json!({"channel":channel,"session_id":"s","revision":1,"epoch":1,"message_id":id});
            format!("{header}\n{words}\n[Sidevoice] trailer\n")
        };
        let text = [
            envelope("voice", "m1", "one"),
            envelope("room-control", "c1", ""),
            envelope("voice", "m2", "two"),
            envelope("voice", "m3", "three"),
        ]
        .concat();
        let ids = voice_headers(&text)
            .iter()
            .map(|header| header["message_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["m1", "c1", "m2", "m3"]);
    }

    #[test]
    fn an_invalid_header_does_not_hide_the_next_one() {
        let text = "{\"channel\":\"voice\"} words {\"channel\":\"voice\",\"session_id\":\"s\",\"revision\":0,\"message_id\":\"m\"}";
        assert_eq!(voice_headers(text).len(), 1);
    }
}
