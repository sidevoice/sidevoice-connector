//! Claude Code as a harness: voice through its messaging socket, the transcript and session record followed.

use super::{assistant_engine, assistant_model, session_engine, transcript_path, working_state};
use crate::adapters::conversation::{report_working, Conversation, Reporter};
use crate::adapters::harness::{no_route, Harness, Route};
use crate::adapters::registration::Registration;
use crate::adapters::transcript::{self, Observed, TranscriptReader};
use crate::profile::Profile;
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

pub struct Claude;

impl Harness for Claude {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn capabilities(&self) -> Value {
        super::capabilities()
    }

    fn homes(&self) -> &'static [crate::profile::AgentHome] {
        std::slice::from_ref(&super::HOME)
    }

    fn registration(&self) -> Option<&dyn Registration> {
        Some(self)
    }

    fn accepts(&self, delivery: &Value, thread: &str) -> Route {
        match delivery.get("kind").and_then(Value::as_str) {
            Some("claude-uds") => {
                Route::known(delivery.get("socket").and_then(Value::as_str).is_some())
            }
            _ => crate::adapters::http::accepts(delivery, thread),
        }
    }

    fn deliver<'a>(
        &'a self,
        conversation: &'a Conversation,
        frame: &'a Value,
        _profile: &'a Profile,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            match conversation.route_kind() {
                Some("claude-uds") => super::deliver(&conversation.delivery, frame).await,
                Some("http") => crate::adapters::http::deliver(&conversation.delivery, frame).await,
                _ => Ok(no_route(&conversation.thread)),
            }
        })
    }

    fn watch(&self, conversation: Arc<Conversation>, reporter: Arc<dyn Reporter>) {
        if conversation.route_kind() != Some("claude-uds") {
            return;
        }
        let turn = Arc::new(AtomicU64::new(0));
        transcript::follow(
            conversation.clone(),
            reporter.clone(),
            Reader {
                session: conversation.thread.clone(),
                turn: turn.clone(),
            },
        );
        watch_working(conversation, reporter, turn);
    }
}

/// Working state: the session record says busy, or the transcript holds a turn open (a message in, no final answer
/// yet) and was written to lately. The record alone reads idle between a message arriving and the turn starting, and
/// between tool calls. Reported working at once, idle once it has held for [`IDLE_DEBOUNCE`].
fn watch_working(
    conversation: Arc<Conversation>,
    reporter: Arc<dyn Reporter>,
    turn: Arc<AtomicU64>,
) {
    tokio::spawn(async move {
        let mut reported = None;
        let mut idle_since = Instant::now();
        while !conversation.stopped.load(Ordering::Relaxed) {
            let open = turn.load(Ordering::Relaxed);
            let working = working_state(&conversation.thread) == Some(true)
                || (open > 0 && now_ms().saturating_sub(open) < OPEN_TURN_MS);
            if working {
                idle_since = Instant::now();
            }
            let state = working || idle_since.elapsed() < IDLE_DEBOUNCE;
            if reported != Some(state) {
                report_working(&*reporter, &conversation, state).await;
                reported = Some(state);
            }
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

/// A session's transcript: the engine of each assistant entry, and the messages read.
struct Reader {
    session: String,
    /// When the last entry of the open turn was written, in epoch milliseconds; 0 when none is open.
    turn: Arc<AtomicU64>,
}

impl TranscriptReader for Reader {
    fn path(&self) -> Option<PathBuf> {
        transcript_path(&self.session)
    }

    fn item(&mut self, item: &Value, _authenticated: bool) -> Vec<Observed> {
        let mut observed = Vec::new();
        match turn_signal(item) {
            Some(true) => self.turn.store(now_ms(), Ordering::Relaxed),
            Some(false) => self.turn.store(0, Ordering::Relaxed),
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
