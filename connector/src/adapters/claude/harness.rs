//! Claude Code as a harness: a session is found by the environment Claude Code starts its MCP server with, reached
//! through the inbox its session record names, and followed through that record and its transcript.

use super::{
    assistant_engine, assistant_model, inbound, session_engine, transcript_path, working_state,
};
use crate::adapters::harness::{CallContext, Conversation, Harness, Sent, State};
use crate::adapters::transcript::{self, Observed, TranscriptReader};
use crate::adapters::watch::{Watch, Watches};
use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::time::Duration;

/// How long the record may say idle while the transcript holds a turn open before the turn is taken as dead (an
/// interrupted or crashed one leaves no closing entry).
const OPEN_TURN_MS: u64 = 30_000;
/// How long not working must hold before it is reported: the record flickers idle between tool calls.
const IDLE_DEBOUNCE: Duration = Duration::from_secs(2);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(1, |d| d.as_millis() as u64)
}

/// When the last entry of a session's open turn was written, in epoch milliseconds; 0 when none is open.
#[derive(Default)]
pub struct OpenTurn(AtomicU64);

/// Claude Code's sessions, as the connector follows them.
#[derive(Default)]
pub struct Sessions {
    watches: Watches<OpenTurn>,
}

impl Sessions {
    fn watch(&self, session: &str) -> Arc<Watch<OpenTurn>> {
        self.watches.get(session, |watch| {
            watch.set_inbound(inbound(session));
            if let Some(engine) = session_engine(session) {
                watch.set_engine(engine);
            }
            transcript::follow(
                watch.clone(),
                Reader {
                    session: session.to_owned(),
                    watch: watch.clone(),
                },
            );
            follow_working(watch.clone(), session.to_owned());
        })
    }
}

impl Harness for Sessions {
    fn id(&self) -> &'static str {
        "claude"
    }

    /// Claude Code names the session, and that it has an inbox for messages, in its MCP server's environment.
    fn identify(&self, call: &CallContext) -> Result<Option<Conversation>> {
        let (Some(id), Some(_)) = (
            call.var("CLAUDE_CODE_SESSION_ID"),
            call.var("CLAUDE_CODE_MESSAGING_SOCKET"),
        ) else {
            return Ok(None);
        };
        Ok(Some(Conversation {
            harness: self.id().into(),
            id,
        }))
    }

    fn send<'a>(
        &'a self,
        conversation: &'a Conversation,
        message: &'a Value,
    ) -> BoxFuture<'a, Sent> {
        Box::pin(super::send(&conversation.id, message))
    }

    fn state<'a>(&'a self, conversation: &'a Conversation) -> BoxFuture<'a, State> {
        Box::pin(async move { self.watch(&conversation.id).state() })
    }

    fn received<'a>(
        &'a self,
        conversation: &'a Conversation,
        ids: &'a [String],
    ) -> BoxFuture<'a, Vec<String>> {
        Box::pin(async move { self.watch(&conversation.id).received(ids) })
    }
}

/// Working state: the session record says busy, or the transcript holds a turn open (a message in, no final answer
/// yet) and was written to lately. The record alone reads idle between a message arriving and the turn starting, and
/// between tool calls. Working at once, idle once it has held for [`IDLE_DEBOUNCE`].
fn follow_working(watch: Arc<Watch<OpenTurn>>, session: String) {
    tokio::spawn(async move {
        let mut idle_since = Instant::now();
        while !watch.stopped() {
            let open = watch.extra.0.load(Ordering::Relaxed);
            let working = working_state(&session) == Some(true)
                || (open > 0 && now_ms().saturating_sub(open) < OPEN_TURN_MS);
            if working {
                idle_since = Instant::now();
            }
            watch.set_working(working || idle_since.elapsed() < IDLE_DEBOUNCE);
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    });
}

/// What a transcript entry says of the turn: `Some(true)` a turn is open, `Some(false)` it ended.
fn turn_signal(item: &Value) -> Option<bool> {
    if item.get("isSidechain") == Some(&json!(true)) {
        return None;
    }
    match item.get("type").and_then(Value::as_str)? {
        "assistant" => match item
            .pointer("/message/stop_reason")
            .and_then(Value::as_str)?
        {
            "end_turn" | "stop_sequence" | "max_tokens" | "refusal" => Some(false),
            _ => Some(true),
        },
        "user" => {
            let interrupted = transcript::user_text(item, true)
                .is_some_and(|text| text.starts_with("[Request interrupted by user"));
            Some(!interrupted)
        }
        "attachment" => (item.pointer("/attachment/type")? == "queued_command").then_some(true),
        _ => None,
    }
}

/// A session's transcript: the engine of each assistant entry, the open turn, and the messages read.
struct Reader {
    session: String,
    watch: Arc<Watch<OpenTurn>>,
}

impl TranscriptReader for Reader {
    fn path(&self) -> Option<PathBuf> {
        transcript_path(&self.session)
    }

    fn item(&mut self, item: &Value) -> Vec<Observed> {
        let mut observed = Vec::new();
        match turn_signal(item) {
            Some(true) => self.watch.extra.0.store(now_ms(), Ordering::Relaxed),
            Some(false) => self.watch.extra.0.store(0, Ordering::Relaxed),
            None => {}
        }
        if assistant_model(item).is_some() {
            let launch = session_engine(&self.session).unwrap_or_else(|| json!({}));
            if let Some(engine) = assistant_engine(item, &launch) {
                observed.push(Observed::Engine(engine));
            }
        }
        if let Some(text) = transcript::user_text(item, true) {
            observed.extend(
                transcript::voice_headers(&text)
                    .into_iter()
                    .map(Observed::Read),
            );
        }
        observed
    }
}

#[cfg(test)]
mod tests {
    use super::turn_signal;
    use serde_json::json;

    #[test]
    fn a_turn_is_open_from_the_message_to_the_final_answer() {
        let user = json!({"type":"user","message":{"role":"user","content":"hi"}});
        let tool = json!({"type":"assistant","message":{"stop_reason":"tool_use"}});
        let end = json!({"type":"assistant","message":{"stop_reason":"end_turn"}});
        let side =
            json!({"type":"assistant","isSidechain":true,"message":{"stop_reason":"end_turn"}});
        let stop = json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}});
        let queued =
            json!({"type":"attachment","attachment":{"type":"queued_command","prompt":"x"}});
        assert_eq!(turn_signal(&user), Some(true));
        assert_eq!(turn_signal(&tool), Some(true));
        assert_eq!(turn_signal(&queued), Some(true));
        assert_eq!(turn_signal(&end), Some(false));
        assert_eq!(turn_signal(&stop), Some(false));
        assert_eq!(turn_signal(&side), None);
    }
}
