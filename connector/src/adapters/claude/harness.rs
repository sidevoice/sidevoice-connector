//! Claude Code as a harness: voice through its messaging socket, the transcript and session record followed.

use super::{assistant_engine, assistant_model, session_engine, transcript_path, working_state};
use crate::adapters::conversation::{report_working, Conversation, Room};
use crate::adapters::harness::{no_route, Harness, Route};
use crate::adapters::registration::Registration;
use crate::adapters::transcript::{self, Observed, TranscriptReader};
use crate::profile::Profile;
use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::time::Duration;

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

    fn inject<'a>(
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

    fn watch(&self, conversation: Arc<Conversation>, room: Arc<dyn Room>) {
        if conversation.route_kind() != Some("claude-uds") {
            return;
        }
        transcript::follow(
            conversation.clone(),
            room.clone(),
            Reader {
                session: conversation.thread.clone(),
            },
        );
        watch_working(conversation, room);
    }
}

/// Working state, from the session record Claude Code keeps.
fn watch_working(conversation: Arc<Conversation>, room: Arc<dyn Room>) {
    tokio::spawn(async move {
        let mut last = None;
        while !conversation.stopped.load(Ordering::Relaxed) {
            let current = working_state(&conversation.thread);
            if let Some(working) = current {
                if current != last {
                    report_working(&*room, &conversation, working).await;
                    last = current;
                }
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    });
}

/// A session's transcript: the engine of each assistant entry, and the messages read.
struct Reader {
    session: String,
}

impl TranscriptReader for Reader {
    fn path(&self) -> Option<PathBuf> {
        transcript_path(&self.session)
    }

    fn item(&mut self, item: &Value, _authenticated: bool) -> Vec<Observed> {
        let mut observed = Vec::new();
        if assistant_model(item).is_some() {
            let launch = session_engine(&self.session).unwrap_or_else(|| json!({}));
            if let Some(engine) = assistant_engine(item, &launch) {
                observed.push(Observed::Engine(engine));
            }
        }
        if let Some(header) =
            transcript::user_text(item, true).and_then(|text| transcript::voice_header(&text))
        {
            observed.push(Observed::Read(header));
        }
        observed
    }
}
