//! Cursor as a harness: the CLI's chats (tmux, transcript) and the editor's chats (the Sidevoice card, its Desktop
//! Bridge, the transcript found by what was delivered to it).

use super::app::{AppNotice, CursorApps};
use super::{composer_holding, deliver_editor, editor_transcript_chat, engine, transcript_path};
use crate::adapters::conversation::{Conversation, Room};
use crate::adapters::envelope;
use crate::adapters::harness::{no_route, Harness};
use crate::adapters::registration::Registration;
use crate::adapters::transcript::{self, Observed, TranscriptReader};
use crate::profile::Profile;
use anyhow::{Context, Result};
use futures_util::future::BoxFuture;
use serde_json::{json, Map, Value};
use std::any::Any;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::sync::mpsc;
use tokio::time::Duration;

/// Cursor, with the card bridge its editor chats are reached through once the room exists (`attach`).
#[derive(Default)]
pub struct Cursor {
    apps: OnceLock<Arc<CursorApps>>,
}

/// Which Cursor chat a conversation is: the one the Desktop Bridge holds (`bridge`, which authenticates it), and the
/// one whose transcript is followed.
#[derive(Default)]
struct Chats {
    bridge: Mutex<Option<String>>,
    transcript: Mutex<Option<String>>,
}

fn chats(conversation: &Conversation) -> Option<&Chats> {
    conversation.state.downcast_ref::<Chats>()
}

fn bridge(conversation: &Conversation) -> Option<String> {
    chats(conversation).and_then(|chats| chats.bridge.lock().ok()?.clone())
}

fn set_bridge(conversation: &Conversation, chat: String) {
    if let Some(mut bridge) = chats(conversation).and_then(|chats| chats.bridge.lock().ok()) {
        *bridge = Some(chat);
    }
}

fn followed(conversation: &Conversation) -> Option<String> {
    chats(conversation).and_then(|chats| chats.transcript.lock().ok()?.clone())
}

fn set_followed(conversation: &Conversation, chat: String) {
    if let Some(mut transcript) = chats(conversation).and_then(|chats| chats.transcript.lock().ok())
    {
        *transcript = Some(chat);
    }
}

impl Cursor {
    fn apps(&self) -> Result<&Arc<CursorApps>> {
        self.apps.get().context("Cursor card bridge is not running")
    }
}

impl Harness for Cursor {
    fn id(&self) -> &'static str {
        "cursor"
    }

    fn capabilities(&self) -> Value {
        super::capabilities()
    }

    fn registration(&self) -> Option<&dyn Registration> {
        Some(self)
    }

    fn accepts(&self, delivery: &Value, thread: &str) -> bool {
        match delivery.get("kind").and_then(Value::as_str) {
            Some("cursor-tmux") => delivery.get("chat").and_then(Value::as_str) == Some(thread),
            Some("cursor-app") => delivery.get("thread").and_then(Value::as_str) == Some(thread),
            Some("none") => true,
            _ => crate::adapters::http::accepts(delivery, thread),
        }
    }

    fn conversation_state(&self) -> Box<dyn Any + Send + Sync> {
        Box::new(Chats::default())
    }

    fn attach(&self, profile: &Profile, room: Weak<dyn Room>) -> Result<()> {
        let (notices, mut received) = mpsc::unbounded_channel();
        let apps = CursorApps::new(notices, profile.data.join("cursor-card-port.json"))?;
        let _ = self.apps.set(apps);
        tokio::spawn(async move {
            while let Some(notice) = received.recv().await {
                let Some(room) = room.upgrade() else {
                    return;
                };
                answered(&*room, notice).await;
            }
        });
        Ok(())
    }

    fn prepare<'a>(
        &'a self,
        conversation: &'a Conversation,
    ) -> BoxFuture<'a, Result<Option<Value>>> {
        Box::pin(async move {
            if conversation.route_kind() != Some("cursor-app") {
                return Ok(None);
            }
            let key = conversation
                .delivery
                .get("key")
                .and_then(Value::as_str)
                .context("Cursor card key missing")?;
            let port = self.apps()?.open(&conversation.thread, key).await?;
            Ok(Some(json!({"port":port})))
        })
    }

    fn deliver<'a>(
        &'a self,
        conversation: &'a Conversation,
        frame: &'a Value,
        _profile: &'a Profile,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            match conversation.route_kind() {
                Some("cursor-app") => self.deliver_to_editor(conversation, frame).await,
                Some("cursor-tmux") => super::deliver(&conversation.delivery, frame).await,
                Some("http") => crate::adapters::http::deliver(&conversation.delivery, frame).await,
                _ => Ok(no_route(&conversation.thread)),
            }
        })
    }

    fn close<'a>(&'a self, conversation: &'a Conversation) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Some(apps) = self.apps.get() {
                apps.close(&conversation.thread).await;
            }
        })
    }

    fn status<'a>(
        &'a self,
        conversation: &'a Conversation,
    ) -> BoxFuture<'a, Option<Map<String, Value>>> {
        Box::pin(async move {
            if conversation.route_kind() != Some("cursor-app") {
                return None;
            }
            let apps = self.apps.get()?;
            let mut state = apps.status(&conversation.thread).await;
            state["bridge_chat_known"] = json!(bridge(conversation).is_some());
            let mut fields = Map::new();
            fields.insert("delivery_state".into(), state);
            fields.insert("cursor_app_port".into(), json!(apps.port().await));
            Some(fields)
        })
    }

    fn adopts(&self, client_ref: Option<&str>) -> bool {
        client_ref.is_none_or(|client_ref| client_ref.starts_with("cursor-editor-"))
    }

    fn watch(&self, conversation: Arc<Conversation>, room: Arc<dyn Room>) {
        match conversation.route_kind() {
            Some("cursor-tmux" | "none") => {
                let chat = conversation.thread.clone();
                transcript::follow(conversation, room, Reader::new(chat, false));
            }
            Some("cursor-app") => watch_editor(conversation, room),
            _ => {}
        }
    }
}

impl Cursor {
    /// An editor chat: by its own id through the Desktop Bridge when it is known, else through the card.
    async fn deliver_to_editor(&self, conversation: &Conversation, frame: &Value) -> Result<Value> {
        let mut delivery = conversation.delivery.clone();
        let composer = bridge(conversation).or_else(|| {
            delivery
                .get("composer")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
        let composer = match composer {
            Some(composer) => Some(composer),
            None => composer_holding(&delivery).await,
        };
        if let Some(composer) = composer {
            delivery["composer"] = json!(composer);
            set_bridge(conversation, composer);
            if let Some(result) = deliver_editor(&delivery, frame).await {
                return Ok(result);
            }
        }
        let message_id = frame
            .get("message_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let text = frame.get("text").and_then(Value::as_str).unwrap_or("");
        self.apps()?
            .deliver(
                &conversation.thread,
                message_id,
                envelope(frame).unwrap_or_else(|_| text.to_owned()),
            )
            .await
    }
}

/// The card's answer that it submitted a message: the message was read.
async fn answered(room: &dyn Room, notice: AppNotice) {
    let (thread, message_id, answered) = match notice {
        AppNotice::Dispatched { thread, message_id } => (thread, message_id, None),
        AppNotice::Answered {
            thread,
            message_id,
            ok,
        } => (thread, message_id, Some(ok)),
    };
    let conversation = room
        .conversations()
        .await
        .into_iter()
        .find(|conversation| conversation.client_ref == thread);
    let Some(conversation) = conversation else {
        return;
    };
    if answered.is_none() || answered == Some(false) {
        return;
    }
    let context = conversation
        .input_context
        .lock()
        .await
        .get(&message_id)
        .cloned();
    let Some((session_id, revision)) = context else {
        return;
    };
    room.read(
        &conversation,
        &message_id,
        json!(session_id),
        json!(revision),
        Value::Null,
    )
    .await;
}

/// Finds which Cursor chat an editor conversation is (by the Desktop Bridge, or by a delivered message showing up
/// in a transcript), then follows that chat's transcript.
fn watch_editor(conversation: Arc<Conversation>, room: Arc<dyn Room>) {
    tokio::spawn(async move {
        let binding = &conversation;
        let lookup_at = std::env::var("SIDEVOICE_CURSOR_LOOKUP_AT")
            .ok()
            .map(|value| {
                value
                    .split(',')
                    .filter_map(|part| part.trim().parse::<u64>().ok())
                    .map(Duration::from_millis)
                    .collect::<Vec<_>>()
            })
            .filter(|times| !times.is_empty())
            .unwrap_or_else(|| {
                [3_000, 15_000, 35_000, 70_000]
                    .into_iter()
                    .map(Duration::from_millis)
                    .collect()
            });
        let started = tokio::time::Instant::now();
        let mut lookup_index = 0;
        let mut last_bridge_lookup: Option<tokio::time::Instant> = None;
        loop {
            if binding.stopped.load(Ordering::Relaxed) {
                return;
            }
            if let Some(chat) = bridge(binding) {
                if followed(binding).as_deref() != Some(chat.as_str()) {
                    set_followed(binding, chat.clone());
                    transcript::follow(binding.clone(), room.clone(), Reader::new(chat, true));
                }
                return;
            }

            let expected = binding
                .input_context
                .lock()
                .await
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            let mut excluded = HashSet::new();
            for other in room.conversations().await {
                if other.client_ref == binding.client_ref {
                    continue;
                }
                if let Some(chat) = bridge(&other) {
                    excluded.insert(chat);
                }
                if let Some(chat) = followed(&other) {
                    excluded.insert(chat);
                }
            }
            if let Some((chat, pending_message_match)) =
                editor_transcript_chat(&binding.delivery, &expected, &excluded)
            {
                let observed = followed(binding);
                if observed.as_deref() != Some(chat.as_str())
                    && (observed.is_none() || pending_message_match)
                {
                    set_followed(binding, chat.clone());
                    transcript::follow(
                        binding.clone(),
                        room.clone(),
                        Reader::new(chat.clone(), true),
                    );
                }
                if pending_message_match {
                    set_bridge(binding, chat);
                    continue;
                }
            }

            let lookup_due = if lookup_index < lookup_at.len() {
                started.elapsed() >= lookup_at[lookup_index]
            } else {
                last_bridge_lookup.is_some_and(|last| last.elapsed() >= Duration::from_secs(5))
            };
            if lookup_due {
                if lookup_index < lookup_at.len() {
                    lookup_index += 1;
                }
                last_bridge_lookup = Some(tokio::time::Instant::now());
                if let Some(chat) = composer_holding(&binding.delivery).await {
                    set_bridge(binding, chat);
                    continue;
                }
            }
            let scan_ms = std::env::var("SIDEVOICE_CURSOR_SCAN_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(2_000)
                .max(10);
            tokio::time::sleep(Duration::from_millis(scan_ms)).await;
        }
    });
}

/// A Cursor chat's transcript: working from its turns, the engine once the chat is authenticated, and the messages
/// read. An editor chat's is only followed while it is the conversation's, and only authenticated once the Desktop
/// Bridge names the same chat.
struct Reader {
    chat: String,
    editor: bool,
    engine_checked: bool,
}

impl Reader {
    fn new(chat: String, editor: bool) -> Self {
        Self {
            chat,
            editor,
            engine_checked: false,
        }
    }
}

impl TranscriptReader for Reader {
    fn path(&self) -> Option<PathBuf> {
        transcript_path(&self.chat)
    }

    fn current(&self, conversation: &Conversation) -> bool {
        !self.editor || followed(conversation).as_deref() == Some(self.chat.as_str())
    }

    fn authenticated(&self, conversation: &Conversation) -> bool {
        !self.editor || bridge(conversation).as_deref() == Some(self.chat.as_str())
    }

    fn rewritten_in_place(&self) -> bool {
        true
    }

    fn begin(&mut self, authenticated: bool) -> Vec<Observed> {
        if !authenticated || self.engine_checked {
            return Vec::new();
        }
        self.engine_checked = true;
        engine(&self.chat)
            .map(Observed::Engine)
            .into_iter()
            .collect()
    }

    fn item(&mut self, item: &Value, authenticated: bool) -> Vec<Observed> {
        let mut observed = Vec::new();
        let text = transcript::user_text(item, false);
        if authenticated && text.is_some() {
            if let Some(engine) = engine(&self.chat) {
                observed.push(Observed::Engine(engine));
            }
        }
        if item.get("type") == Some(&json!("turn_ended")) {
            observed.push(Observed::Working(false));
        } else if item.get("role") == Some(&json!("user")) {
            observed.push(Observed::Working(true));
        }
        if let Some(header) = text.as_deref().and_then(transcript::voice_header) {
            observed.push(Observed::Read(header));
        }
        observed
    }
}
