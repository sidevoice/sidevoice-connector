//! Cursor as a harness: the CLI's chats (tmux, transcript) and the editor's chats, reached through Cursor's Desktop
//! Bridge once it knows which chat a conversation is, their transcript found the same way.

use super::{
    bridge_running, composer_holding, composer_holding_for_delivery, deliver_editor,
    editor_transcript_chat, engine, log_bridge, sqlite3_found, transcript_path,
};
use crate::adapters::conversation::{Conversation, Room};
use crate::adapters::harness::{no_route, Harness, Route};
use crate::adapters::registration::Registration;
use crate::adapters::transcript::{self, Observed, TranscriptReader};
use crate::adapters::Identity;
use crate::profile::Profile;
use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::{json, Map, Value};
use std::any::Any;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tokio::time::Duration;

/// What an editor chat's delivery and status say until the Desktop Bridge knows which chat it is.
const WAITING: &str = "waiting for Cursor to register this chat";
/// What they say when Cursor's Desktop Bridge is not running at all.
const NO_BRIDGE: &str = "Cursor's Desktop Bridge is not running";
/// What they say when the chat cannot be looked up: Cursor's records are read with `sqlite3`.
const NO_SQLITE: &str =
    "sqlite3 is not installed, and Sidevoice needs it to find which Cursor chat this is";

/// Cursor.
#[derive(Default)]
pub struct Cursor;

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

    fn accepts(&self, delivery: &Value, thread: &str) -> Route {
        match delivery.get("kind").and_then(Value::as_str) {
            Some("cursor-tmux") => {
                Route::known(delivery.get("chat").and_then(Value::as_str) == Some(thread))
            }
            Some("cursor-bridge") => {
                Route::known(delivery.get("thread").and_then(Value::as_str) == Some(thread))
            }
            Some("none") => Route::Accepted,
            _ => crate::adapters::http::accepts(delivery, thread),
        }
    }

    fn conversation_state(&self) -> Box<dyn Any + Send + Sync> {
        Box::new(Chats::default())
    }

    fn deliver<'a>(
        &'a self,
        conversation: &'a Conversation,
        frame: &'a Value,
        _profile: &'a Profile,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            match conversation.route_kind() {
                Some("cursor-bridge") => Ok(deliver_to_editor(conversation, frame).await),
                Some("cursor-tmux") => super::deliver(&conversation.delivery, frame).await,
                Some("http") => crate::adapters::http::deliver(&conversation.delivery, frame).await,
                _ => Ok(no_route(&conversation.thread)),
            }
        })
    }

    fn status<'a>(
        &'a self,
        conversation: &'a Conversation,
    ) -> BoxFuture<'a, Option<Map<String, Value>>> {
        Box::pin(async move {
            if conversation.route_kind() != Some("cursor-bridge") {
                return None;
            }
            let mut fields = Map::new();
            fields.insert(
                "delivery_state".into(),
                bridge_state(route_state(bridge(conversation).is_some())),
            );
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

    fn hosts_several_chats(&self, client_caps: &Value) -> bool {
        super::is_editor(client_caps)
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

    fn joined_fields(&self, identity: &Identity, _registered: &Value) -> Map<String, Value> {
        client::joined_fields(identity)
    }

    fn status_fields(&self, identity: &Identity, listed: Option<&Value>) -> Map<String, Value> {
        client::status_fields(identity, listed)
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
            Some("cursor-bridge") => watch_editor(conversation, room),
            _ => {}
        }
    }
}

/// Where an editor chat's route stands: registered when the Desktop Bridge knows which chat it is, else why not
/// yet.
fn route_state(chat_known: bool) -> &'static str {
    state_of(chat_known, bridge_running(), sqlite3_found())
}

fn state_of(chat_known: bool, bridge_running: bool, sqlite3: bool) -> &'static str {
    if chat_known {
        "registered"
    } else if !bridge_running {
        NO_BRIDGE
    } else if !sqlite3 {
        NO_SQLITE
    } else {
        WAITING
    }
}

fn bridge_state(state: &str) -> Value {
    json!({"bridge_chat_known":state == "registered","state":state})
}

/// An editor chat, only by its own id through the Desktop Bridge. Until the bridge knows which chat it is, the
/// message is not sent and the answer says so.
async fn deliver_to_editor(conversation: &Conversation, frame: &Value) -> Value {
    let mut delivery = conversation.delivery.clone();
    let composer = match bridge(conversation) {
        Some(composer) => {
            log_bridge("lookup", "cached_chat", Some(&composer), None, None);
            Some(composer)
        }
        None => composer_holding_for_delivery(&delivery).await,
    };
    let Some(composer) = composer else {
        log_bridge(
            "delivery",
            "chat_unresolved",
            delivery.get("marker").and_then(Value::as_str),
            None,
            None,
        );
        let state = route_state(false);
        return json!({"status":"failed",
            "detail":format!("Not sent, {state}: voice reaches this Cursor chat only through its Desktop Bridge, once Cursor has recorded the chat's join call (up to ~30 s after it).")});
    };
    delivery["composer"] = json!(composer);
    set_bridge(conversation, composer);
    deliver_editor(&delivery, frame).await.unwrap_or_else(|| {
        json!({"status":"failed",
            "detail":"Not sent: Cursor's Desktop Bridge did not take the message (it may not be running)."})
    })
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

/// The hooks Cursor's MCP client is served by.
pub(super) mod client {
    use super::*;

    pub fn recognises(client: &Value) -> bool {
        super::super::is_cursor_client(client)
    }

    pub fn experimental_note(capability: &str, identity: &Identity) -> Option<&'static str> {
        match (capability, identity.delivery["kind"].as_str()) {
            ("deliver", Some("cursor-tmux")) => Some("Voice from the room reaches this conversation by an experimental route: it is pasted into this chat's terminal through tmux and sent with Enter. If the user is typing at the same moment, the two texts mix. Tell the user once."),
            ("deliver", Some("cursor-bridge")) => Some("Voice from the room reaches this chat by an experimental route: Cursor's Desktop Bridge submits it to this chat as if the user had typed it, once Cursor has recorded this join call (up to ~30 s). If the user is typing at the same moment, the two may mix. Tell the user once."),
            ("sessionIdentity", _) => Some("This conversation is identified by Sidevoice, not by an id Cursor gives: each chat of this window that joins gets a conversation of its own. Asking this same chat to join again gives it a second one; leave the old one with voice_disconnect and its conversation id."),
            _ => None,
        }
    }

    /// What Cursor's editor client is told when one of its chats joins: how voice reaches it, and the marker the
    /// Desktop Bridge finds this chat by in Cursor's own record of the call.
    pub fn joined_fields(identity: &Identity) -> Map<String, Value> {
        let mut fields = Map::new();
        if !identity.editor {
            return fields;
        }
        fields.insert("voice_in".into(), json!(format!("Voice reaches this chat through Cursor's Desktop Bridge, once Cursor has recorded this call (up to ~30 s): until then it says \"{WAITING}\" and nothing is sent. Other chats of this window can join too. Remember this chat's conversation id ({}): voice_status and voice_disconnect need it when several chats are joined.", identity.thread)));
        fields.insert("speak_first".into(), json!(format!("To speak before the user has said anything by voice, call voice_say with conversation \"{}\" and no session_id. To answer a voice message, use its session_id and revision as usual.", identity.thread)));
        if let Some(marker) = &identity.chat_marker {
            fields.insert("chat_marker".into(), json!(marker));
        }
        fields
    }

    /// Where an editor chat's route stands, for its client.
    pub fn status_fields(identity: &Identity, listed: Option<&Value>) -> Map<String, Value> {
        let mut fields = Map::new();
        if !identity.editor {
            return fields;
        }
        let known = listed
            .and_then(|item| item.pointer("/delivery_state/bridge_chat_known"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let state = listed
            .and_then(|item| item.pointer("/delivery_state/state"))
            .and_then(Value::as_str)
            .unwrap_or(if known { "registered" } else { WAITING });
        fields.insert(
            "bridge".into(),
            json!({"bridge_chat_known":known,"state":state}),
        );
        fields
    }

    pub fn adopted_identity(name: &str, result: &Value) -> (Identity, String) {
        let delivery = result
            .get("delivery")
            .cloned()
            .unwrap_or_else(|| json!({"kind":"cursor-bridge","thread":name}));
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
            chat_marker: result
                .pointer("/delivery/marker")
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use tokio::sync::Mutex as AsyncMutex;

    /// An editor chat as Cursor's client declares itself at `initialize`: MCP Apps support.
    fn editor_caps() -> Value {
        json!({"extensions":{"io.modelcontextprotocol/ui":{"mimeTypes":["text/html;profile=mcp-app"]}}})
    }

    fn conversation(identity: &Identity) -> Conversation {
        Conversation {
            client_ref: identity.thread.clone(),
            harness: identity.harness.clone(),
            thread: identity.thread.clone(),
            title: "chat".into(),
            delivery: identity.delivery.clone(),
            engine: AsyncMutex::new(None),
            route: identity.route.clone(),
            capabilities: identity.capabilities.clone(),
            experimental: json!([]),
            inbound: Value::Null,
            detachable: true,
            owner: AtomicU64::new(1),
            id: AsyncMutex::new("local-1".into()),
            serial: AsyncMutex::new(()),
            pending: AsyncMutex::new(VecDeque::new()),
            turns: AsyncMutex::new(HashMap::new()),
            input_context: AsyncMutex::new(HashMap::new()),
            read: AsyncMutex::new(VecDeque::new()),
            working: AsyncMutex::new(None),
            stopped: AtomicBool::new(false),
            state: Cursor.conversation_state(),
        }
    }

    /// Why an editor chat is not reached yet, in the order a person fixes it: the bridge, then sqlite3, then Cursor
    /// recording the join.
    #[test]
    fn an_unregistered_chat_says_what_it_waits_for() {
        assert_eq!(state_of(true, false, false), "registered");
        assert_eq!(state_of(false, false, true), NO_BRIDGE);
        assert_eq!(state_of(false, true, false), NO_SQLITE);
        assert_eq!(state_of(false, true, true), WAITING);
    }

    /// An editor chat joins on the Desktop Bridge route, with a marker of its own the bridge finds it by, which its
    /// join answer carries; no card is drawn or offered.
    #[test]
    fn an_editor_chat_joins_on_the_bridge_route_with_its_marker() {
        let identity =
            super::super::identity(&json!({"name":"cursor-vscode"}), &editor_caps()).unwrap();
        assert_eq!(identity.delivery["kind"], "cursor-bridge");
        assert_eq!(identity.route.as_deref(), Some("cursor-editor-bridge"));
        let marker = identity.chat_marker.clone().unwrap();
        assert_eq!(identity.delivery["marker"], marker.as_str());
        assert_eq!(
            Cursor.accepts(&identity.delivery, &identity.thread),
            Route::Accepted
        );
        let fields = client::joined_fields(&identity);
        assert_eq!(fields["chat_marker"], marker.as_str());
        assert!(fields["voice_in"].as_str().unwrap().contains(WAITING));
        assert!(!fields.contains_key("card") && !fields.contains_key("view_link"));
    }

    /// Until the Desktop Bridge knows which chat it is, nothing is sent and both the delivery and the status say so.
    #[tokio::test]
    async fn an_unregistered_editor_chat_is_waiting_not_reached_another_way() {
        let identity =
            super::super::identity(&json!({"name":"cursor-vscode"}), &editor_caps()).unwrap();
        let conversation = conversation(&identity);
        let frame =
            json!({"channel":"voice","session_id":"s","revision":1,"message_id":"m","text":"hola"});
        let answer = deliver_to_editor(&conversation, &frame).await;
        assert_eq!(answer["status"], "failed", "{answer}");
        // Only the fields the room takes in a delivery answer: a failure it keeps, with why in the detail.
        assert_eq!(
            answer.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["detail", "status"],
            "{answer}"
        );
        let state = route_state(false);
        assert!(
            answer["detail"].as_str().unwrap().contains(state),
            "{answer}"
        );

        let listed = Cursor.status(&conversation).await.unwrap();
        assert_eq!(listed["delivery_state"]["state"], state);
        let item = json!({"delivery_state": listed["delivery_state"]});
        let fields = client::status_fields(&identity, Some(&item));
        assert_eq!(fields["bridge"]["bridge_chat_known"], false);

        set_bridge(&conversation, "chat-1".into());
        let listed = Cursor.status(&conversation).await.unwrap();
        assert_eq!(listed["delivery_state"]["state"], "registered");
    }
}
