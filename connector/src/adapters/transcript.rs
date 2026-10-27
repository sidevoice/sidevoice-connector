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

/// The longest a header can be. A real one is a handful of short fields; a candidate with no closing brace inside
/// this window is not one, so malformed input is scanned in time proportional to its length.
const HEADER_LIMIT: usize = 1024;

const HEADER_START: &str = "{\"channel\":";

/// Every header a delivered message starts with (`envelope`) in `text`, in order. A harness may coalesce several
/// messages into one transcript entry, so one entry can carry many.
///
/// The first header is the first valid one anywhere in the text. Each later one is accepted only where an envelope
/// can begin: straight after the previous envelope's closing sentence (`ENVELOPE_END`) or, when that one is a
/// room-control message (which has no closing sentence), straight after its blank line. A header quoted in a
/// message body, or in the unheard replies an envelope carries, is not a delivery and never reads one.
pub fn voice_headers(text: &str) -> Vec<Value> {
    let mut headers = Vec::new();
    let Some((first, mut position)) = first_header(text) else {
        return headers;
    };
    let mut room_control = is_room_control(&first);
    headers.push(first);
    while let Some((header, end)) = next_header(text, position, room_control) {
        room_control = is_room_control(&header);
        headers.push(header);
        position = end;
    }
    headers
}

fn is_room_control(header: &Value) -> bool {
    header.get("channel").and_then(Value::as_str) == Some("room-control")
}

/// The first valid header anywhere in `text`, and where it ends.
fn first_header(text: &str) -> Option<(Value, usize)> {
    let mut from = 0;
    while let Some(at) = text[from..].find(HEADER_START) {
        let start = from + at;
        if let Some((header, length)) = header_at(&text[start..]) {
            return Some((header, start + length));
        }
        from = start + 1;
    }
    None
}

/// The header of the envelope that follows the one ending at `from`, and where it ends.
fn next_header(text: &str, mut from: usize, room_control: bool) -> Option<(Value, usize)> {
    let boundary = if room_control {
        "\n\n"
    } else {
        super::ENVELOPE_END
    };
    loop {
        from += text[from..].find(boundary)? + boundary.len();
        let after = text[from..].trim_start_matches(['\r', '\n']);
        if let Some((header, length)) = header_at(after) {
            return Some((header, text.len() - after.len() + length));
        }
        if room_control {
            return None;
        }
    }
}

/// The header `text` starts with, and its length in bytes.
fn header_at(text: &str) -> Option<(Value, usize)> {
    if !text.starts_with(HEADER_START) {
        return None;
    }
    let mut window = text.len().min(HEADER_LIMIT);
    while !text.is_char_boundary(window) {
        window -= 1;
    }
    let end = text[..window].find('}')?;
    Some((valid_header(&text[..=end])?, end + 1))
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

    fn envelope(channel: &str, id: &str, words: &str) -> String {
        crate::adapters::envelope(
            &json!({"channel":channel,"session_id":"s","revision":1,"epoch":1,
            "message_id":id,"text":words}),
        )
        .unwrap()
    }

    fn ids(text: &str) -> Vec<String> {
        voice_headers(text)
            .iter()
            .map(|header| header["message_id"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn every_envelope_of_a_coalesced_entry_is_read_in_order() {
        let text = [
            envelope("voice", "m1", "one"),
            envelope("room-control", "c1", ""),
            envelope("voice", "m2", "two"),
            envelope("voice", "m3", "three"),
        ]
        .join("\n\n");
        assert_eq!(ids(&text), ["m1", "c1", "m2", "m3"]);
    }

    #[test]
    fn an_invalid_header_does_not_hide_the_next_one() {
        let text = format!(
            "{{\"channel\":\"voice\"}} words {}",
            envelope("voice", "m", "x")
        );
        assert_eq!(ids(&text), ["m"]);
    }

    #[test]
    fn a_header_quoted_in_a_body_is_not_a_delivery() {
        let quoted =
            r#"{"channel":"voice","session_id":"s","revision":1,"epoch":1,"message_id":"pending"}"#;
        let text = [
            envelope(
                "voice",
                "m1",
                &format!("look at this:\n\n{quoted}\n\nthanks"),
            ),
            envelope("voice", "m2", &format!("again {quoted}")),
        ]
        .join("\n\n");
        assert_eq!(ids(&text), ["m1", "m2"]);
    }

    #[test]
    fn a_header_quoted_in_an_unheard_reply_is_not_a_delivery() {
        let quoted =
            r#"{"channel":"voice","session_id":"s","revision":1,"epoch":1,"message_id":"pending"}"#;
        let with_reply = crate::adapters::envelope(&json!({"channel":"voice","session_id":"s",
            "revision":1,"epoch":1,"message_id":"m1","text":"hi",
            "unheard":{"count":1,"replies":[{"text":format!("I wrote\n\n{quoted}\n\nand more"),
                "heard_chars":0}]}}))
        .unwrap();
        let text = [with_reply, envelope("voice", "m2", "next")].join("\n\n");
        assert_eq!(ids(&text), ["m1", "m2"]);
    }

    #[test]
    fn a_run_of_malformed_candidates_is_scanned_in_linear_time() {
        let text = format!("{}}}", "{\"channel\":".repeat(80_000));
        let start = std::time::Instant::now();
        assert!(voice_headers(&text).is_empty());
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        let spaced = format!("{}{}", text, envelope("voice", "m", "x"));
        assert_eq!(ids(&spaced), ["m"]);
    }
}
