//! Cursor as a harness: the CLI's chats (tmux, transcript) and the editor's chats (the Sidevoice card, its Desktop
//! Bridge, the transcript found by what was delivered to it).

use super::app::{AppNotice, CursorApps};
use super::{composer_holding, deliver_editor, editor_transcript_chat, engine, transcript_path};
use crate::adapters::conversation::{Conversation, Room};
use crate::adapters::envelope;
use crate::adapters::harness::{no_route, Harness, View};
use crate::adapters::registration::Registration;
use crate::adapters::transcript::{self, Observed, TranscriptReader};
use crate::adapters::Identity;
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

    fn homes(&self) -> &'static [crate::profile::AgentHome] {
        &super::HOMES
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

    fn private_id_prefixes(&self) -> &'static [&'static str] {
        &["cursor-editor-"]
    }

    fn recognises_client(&self, client: &Value) -> bool {
        client::recognises(client)
    }

    fn view(&self, client_caps: &Value) -> Option<View> {
        client::view(client_caps)
    }

    fn refine_identity<'a>(
        &'a self,
        identity: &'a mut Identity,
        title: &'a str,
    ) -> BoxFuture<'a, ()> {
        Box::pin(super::refine_identity(identity, title))
    }

    fn experimental_note(&self, capability: &str, identity: &Identity) -> Option<&'static str> {
        client::experimental_note(capability, identity)
    }

    fn joined_fields(
        &self,
        identity: &Identity,
        registered: &Value,
        views: bool,
    ) -> Map<String, Value> {
        Cursor::card_fields(identity, registered, views)
    }

    fn status_fields(
        &self,
        identity: &Identity,
        listed: Option<&Value>,
        view_reads: u64,
    ) -> Map<String, Value> {
        client::status_fields(identity, listed, view_reads)
    }

    fn adopted_identity(&self, name: &str, adopted: &Value) -> Option<(Identity, String)> {
        Some(client::adopted_identity(name, adopted))
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

const VIEW_URI: &str = "ui://sidevoice/voice-link";

/// The card a Cursor editor chat draws under its join call (an MCP App), through which voice reaches it.
fn card() -> View {
    View {
        uri: VIEW_URI,
        name: "Sidevoice voice link",
        mime: "text/html;profile=mcp-app",
        html: super::app::resource_html,
        meta: || json!({"ui":{"csp":{"connectDomains":["http://127.0.0.1:*"]},"prefersBorder":true}}),
    }
}

impl Cursor {
    /// What Cursor's client is told about the card when a conversation joins.
    fn card_fields(identity: &Identity, registered: &Value, views: bool) -> Map<String, Value> {
        let mut fields = Map::new();
        let prepared = registered.pointer("/prepared/port").is_some();
        if identity.editor {
            fields.insert("view".into(), json!(format!("Voice reaches this chat through the small Sidevoice card drawn under this call: it must stay open in this chat. Other chats of this window can join too, each with its own card. Remember this chat's conversation id ({}): voice_status and voice_disconnect need it when several chats are joined.", identity.thread)));
            fields.insert("speak_first".into(), json!(format!("To speak before the user has said anything by voice, call voice_say with conversation \"{}\" and no session_id. To answer a voice message, use its session_id and revision as usual.", identity.thread)));
        }
        let bridge = identity.route.as_deref() == Some("cursor-editor-bridge");
        fields.insert("card".into(), json!({"requested":identity.editor && prepared,
            "bridge":bridge,
            "note":if bridge {"Cursor's Desktop Bridge is on: once Cursor has recorded this call (up to ~30 s), voice reaches this chat by its own id, on screen or not. Until then, and whenever the bridge cannot, it comes through the card under this call."}
                else if identity.editor && prepared {"A Sidevoice card should appear under this call within seconds. If none does, Cursor did not draw it: tell the user, and that ~/.sidevoice/mcp.log says whether Cursor read the card (a \"resources/read\" line) — without the card, what they say in the room does not reach this chat."}
                else if identity.editor {"The connector could not open the card's loopback bridge, so no card will work: tell the user to look at ~/.sidevoice/connector.log."}
                else if !views {"This Cursor did not declare MCP Apps support in initialize, so it draws no card: what the user says in the room cannot reach this chat. ~/.sidevoice/mcp.log shows what it declared."}
                else {"Cursor CLI: no card; voice reaches the chat only under cursor-agent persist."}}));
        if identity.delivery["kind"] == "cursor-app" && prepared {
            fields.insert(
                "view_link".into(),
                json!({"conversation":identity.thread,"port":registered["prepared"]["port"],"key":identity.view_key}),
            );
        }
        fields
    }
}

/// The hooks Cursor's MCP client is served by.
pub(super) mod client {
    use super::*;

    pub fn recognises(client: &Value) -> bool {
        super::super::is_cursor_client(client)
    }

    pub fn view(client_caps: &Value) -> Option<View> {
        super::super::draws_views(client_caps).then(card)
    }

    pub fn experimental_note(capability: &str, identity: &Identity) -> Option<&'static str> {
        match (capability, identity.delivery["kind"].as_str()) {
            ("deliver", Some("cursor-tmux")) => Some("Voice from the room reaches this conversation by an experimental route: it is pasted into this chat's terminal through tmux and sent with Enter. If the user is typing at the same moment, the two texts mix. Tell the user once."),
            ("deliver", Some("cursor-app")) => Some("Voice from the room reaches this chat by an experimental route: the small Sidevoice card under this call submits it as if the user had typed it; it must stay open in this chat. If the user is typing at the same moment, the two may mix. Tell the user once."),
            ("sessionIdentity", _) => Some("This conversation is identified by the Sidevoice card drawn in it, not by an id Cursor gives: each chat of this window that joins gets a card and a conversation of its own. Asking this same chat to join again gives it a second one; leave the old one with voice_disconnect and its conversation id."),
            _ => None,
        }
    }

    pub fn status_fields(
        identity: &Identity,
        listed: Option<&Value>,
        reads: u64,
    ) -> Map<String, Value> {
        let mut fields = Map::new();
        if !identity.editor {
            return fields;
        }
        let state = listed
            .and_then(|item| item.get("delivery_state"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        fields.insert("card".into(), json!({"html_read_by_cursor":reads,"card_connected":state.get("card_connected"),
            "card_last_seen_ms_ago":state.get("card_last_seen_ms_ago"),"bridge_chat_known":state.get("bridge_chat_known"),
            "note":if state.get("card_connected") == Some(&json!(true)) {"The card in this chat is connected."}
                else if reads > 0 {"Cursor read the card but it has not connected to this machine's connector: it may not be on screen, or it failed to load."}
                else {"Cursor never read the card, so it drew none: tell the user; ~/.sidevoice/mcp.log has what Cursor declared."}}));
        if let Some(port) = listed
            .and_then(|item| item.get("cursor_app_port"))
            .and_then(Value::as_u64)
        {
            fields.insert(
                "view_link".into(),
                json!({"conversation":identity.thread,"port":port,"key":identity.view_key}),
            );
        }
        fields
    }

    pub fn adopted_identity(name: &str, result: &Value) -> (Identity, String) {
        let delivery = result
            .get("delivery")
            .cloned()
            .unwrap_or_else(|| json!({"kind":"cursor-app","thread":name}));
        let identity = Identity {
            harness: result
                .get("harness")
                .and_then(Value::as_str)
                .unwrap_or("cursor")
                .to_owned(),
            thread: name.to_owned(),
            delivery,
            capability_overrides: json!({}),
            route: result
                .get("route")
                .and_then(Value::as_str)
                .map(str::to_owned),
            capabilities: result
                .get("capabilities")
                .cloned()
                .unwrap_or_else(super::super::capabilities),
            experimental: result
                .get("experimental")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            inbound: result
                .get("inbound")
                .filter(|value| !value.is_null())
                .cloned(),
            editor: true,
            detachable: true,
            view_key: result
                .pointer("/delivery/key")
                .and_then(Value::as_str)
                .map(str::to_owned),
            deliver_note: None,
            watch_note: None,
        };
        let title = result
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Cursor conversation")
            .to_owned();
        (identity, title)
    }
}
