//! Cursor as a harness: the CLI's chats (tmux, transcript) and the editor's chats, reached through Cursor's Desktop
//! Bridge once it knows which chat a conversation is, their transcript found the same way.

use super::engine::editor_engine_in;
use super::{
    bridge_instances, bridge_running, composer_holding, composer_holding_for_delivery,
    deliver_editor, editor_transcript_chat, engine, log_bridge, log_correlation, transcript_path,
    BridgeInstance,
};
use crate::adapters::conversation::{Conversation, Reporter};
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
use tokio::sync::Notify;
use tokio::time::Duration;

/// What an editor chat's delivery and status say until the Desktop Bridge knows which chat it is.
const WAITING: &str = "waiting for Cursor to register this chat";
/// What they say when Cursor's Desktop Bridge is not running at all.
const NO_BRIDGE: &str = "Cursor's Desktop Bridge is not running";
/// Cursor.
#[derive(Default)]
pub struct Cursor;

/// Which Cursor chat a conversation is: the one the Desktop Bridge holds (`bridge`, which authenticates it), and the
/// one whose transcript is followed.
#[derive(Default)]
struct Chats {
    bridge: Mutex<Option<String>>,
    transcript: Mutex<Option<String>>,
    bridge_changed: Notify,
}

fn chats(conversation: &Conversation) -> Option<&Chats> {
    conversation.state.downcast_ref::<Chats>()
}

fn bridge(conversation: &Conversation) -> Option<String> {
    chats(conversation).and_then(|chats| chats.bridge.lock().ok()?.clone())
}

fn set_bridge(conversation: &Conversation, chat: String) {
    if let Some(chats) = chats(conversation) {
        if let Ok(mut bridge) = chats.bridge.lock() {
            *bridge = Some(chat);
            chats.bridge_changed.notify_one();
        }
    }
}

/// Wait for the next discovery scan, unless delivery already identified this chat.
async fn wait_for_bridge_or_scan(conversation: &Conversation, scan_ms: u64) {
    // notify_one retains a permit if delivery identifies the chat between this check and wait.
    let changed = chats(conversation)
        .expect("Cursor conversation state")
        .bridge_changed
        .notified();
    if bridge(conversation).is_some() {
        return;
    }
    tokio::select! {
        _ = changed => {},
        _ = tokio::time::sleep(Duration::from_millis(scan_ms)) => {},
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

    fn watch(&self, conversation: Arc<Conversation>, reporter: Arc<dyn Reporter>) {
        match conversation.route_kind() {
            Some("cursor-tmux" | "none") => {
                let chat = conversation.thread.clone();
                watch_engine(conversation.clone(), reporter.clone(), false);
                transcript::follow(conversation, reporter, Reader::new(chat, false));
            }
            Some("cursor-bridge") => {
                watch_engine(conversation.clone(), reporter.clone(), true);
                watch_editor(conversation, reporter);
            }
            _ => {}
        }
    }
}

fn watch_engine(conversation: Arc<Conversation>, reporter: Arc<dyn Reporter>, editor: bool) {
    tokio::spawn(async move {
        let mut last = None;
        while !conversation.stopped.load(Ordering::Relaxed) {
            let chat = if editor {
                bridge(&conversation)
                    .filter(|chat| followed(&conversation).as_deref() == Some(chat))
            } else {
                Some(conversation.thread.clone())
            };
            if let Some(chat) = chat {
                if let Some(found) = engine(&chat).await {
                    let still_current = !editor
                        || (bridge(&conversation).as_deref() == Some(chat.as_str())
                            && followed(&conversation).as_deref() == Some(chat.as_str()));
                    if still_current && last.as_ref() != Some(&found) {
                        reporter.engine(&conversation, found.clone()).await;
                        last = Some(found);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
}

/// Where an editor chat's route stands: registered when the Desktop Bridge knows which chat it is, else why not
/// yet.
fn route_state(chat_known: bool) -> &'static str {
    state_of(chat_known, bridge_running())
}

fn state_of(chat_known: bool, bridge_running: bool) -> &'static str {
    if chat_known {
        "registered"
    } else if !bridge_running {
        NO_BRIDGE
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
    log_bridge(
        "delivery",
        "attempt",
        delivery.get("marker").and_then(Value::as_str),
        None,
        None,
    );
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
    let result = deliver_editor(&delivery, frame).await.unwrap_or_else(|| {
        json!({"status":"failed",
            "detail":"Not sent: Cursor's Desktop Bridge did not take the message (it may not be running)."})
    });
    let outcome = match result.get("status").and_then(Value::as_str) {
        Some("accepted") => "accepted",
        Some("unknown") => "unconfirmed",
        _ => "bridge_failed",
    };
    log_bridge(
        "delivery",
        outcome,
        delivery.get("marker").and_then(Value::as_str),
        None,
        None,
    );
    result
}

/// Finds which Cursor chat an editor conversation is (by the Desktop Bridge, or by a delivered message showing up
/// in a transcript), then follows that chat's transcript.
fn watch_editor(conversation: Arc<Conversation>, reporter: Arc<dyn Reporter>) {
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
                    transcript::follow(
                        binding.clone(),
                        reporter.clone(),
                        Reader::new(chat.clone(), true),
                    );
                }
                follow_editor_engine(binding.clone(), reporter.clone(), chat, bridge_instances());
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
            for other in reporter.conversations().await {
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
                        reporter.clone(),
                        Reader::new(chat.clone(), true),
                    );
                }
                if pending_message_match {
                    log_correlation(
                        "mapping",
                        "transcript_message",
                        binding.delivery.get("marker").and_then(Value::as_str),
                        &chat,
                        None,
                    );
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
            wait_for_bridge_or_scan(binding, scan_ms).await;
        }
    });
}

/// Observe authenticated editor metadata independently of transcript creation and replay.
fn follow_editor_engine(
    conversation: Arc<Conversation>,
    reporter: Arc<dyn Reporter>,
    chat: String,
    instances: Vec<BridgeInstance>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if conversation.stopped.load(Ordering::Relaxed)
                || bridge(&conversation).as_deref() != Some(chat.as_str())
                || followed(&conversation).as_deref() != Some(chat.as_str())
            {
                return;
            }
            if let Some(engine) = editor_engine_in(&chat, &instances).await {
                // The mapping may change while SQLite answers. Never attribute its model to a
                // chat that is no longer authenticated for this conversation.
                if bridge(&conversation).as_deref() == Some(chat.as_str())
                    && followed(&conversation).as_deref() == Some(chat.as_str())
                {
                    reporter.engine(&conversation, engine).await;
                }
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    })
}

/// A Cursor chat's transcript: working from its turns, the engine once the chat is authenticated, and the messages
/// read. An editor chat's is only followed while it is the conversation's, and only authenticated once the Desktop
/// Bridge names the same chat.
struct Reader {
    chat: String,
    editor: bool,
}

impl Reader {
    fn new(chat: String, editor: bool) -> Self {
        Self { chat, editor }
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

    fn item(&mut self, item: &Value, _authenticated: bool) -> Vec<Observed> {
        let mut observed = Vec::new();
        let text = transcript::user_text(item, false);
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
            ("deliver", Some("cursor-tmux")) => Some("Voice reaches this conversation by an experimental route: it is pasted into this chat's terminal through tmux and sent with Enter. If the user is typing at the same moment, the two texts mix. Tell the user once."),
            ("deliver", Some("cursor-bridge")) => Some("Voice reaches this chat by an experimental route: Cursor's Desktop Bridge submits it to this chat as if the user had typed it, once Cursor has recorded this join call (up to ~30 s). If the user is typing at the same moment, the two may mix. Tell the user once."),
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
    use crate::platform::fs::tests::Scratch;
    use std::collections::{HashMap, VecDeque};
    use std::fs;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use tokio::sync::Mutex as AsyncMutex;

    struct RecordingReporter {
        profile: Profile,
        engines: AsyncMutex<Vec<Value>>,
    }

    impl Reporter for RecordingReporter {
        fn profile(&self) -> &Profile {
            &self.profile
        }
        fn engine<'a>(
            &'a self,
            _conversation: &'a Conversation,
            engine: Value,
        ) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                self.engines.lock().await.push(engine);
            })
        }
        fn working<'a>(
            &'a self,
            _conversation: &'a Conversation,
            _event: Value,
        ) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }
        fn read<'a>(
            &'a self,
            _conversation: &'a Conversation,
            _message_id: &'a str,
            _session_id: Value,
            _revision: Value,
            _turn_id: Value,
        ) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }
        fn conversations(&self) -> BoxFuture<'_, Vec<Arc<Conversation>>> {
            Box::pin(async { Vec::new() })
        }
    }

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
            initial_reply_context: AsyncMutex::new(Value::Null),
            engine: AsyncMutex::new(None),
            route: identity.route.clone(),
            capabilities: identity.capabilities.clone(),
            experimental: json!([]),
            inbound: Value::Null,
            detachable: true,
            owner: AtomicU64::new(1),
            id: AsyncMutex::new("local-1".into()),
            serial: AsyncMutex::new(()),
            publication: AsyncMutex::new(()),
            pending: AsyncMutex::new(VecDeque::new()),
            turns: AsyncMutex::new(HashMap::new()),
            input_context: AsyncMutex::new(HashMap::new()),
            recent_inputs: AsyncMutex::new(VecDeque::new()),
            read: AsyncMutex::new(VecDeque::new()),
            deliveries: AsyncMutex::new(VecDeque::new()),
            working: AsyncMutex::new(None),
            stopped: AtomicBool::new(false),
            state: Cursor.conversation_state(),
        }
    }

    /// A delivery can identify the editor chat while the periodic transcript scan sleeps.
    /// Its retained wakeup must let the watcher attach without waiting for that scan.
    #[tokio::test]
    async fn bridge_identification_wakes_working_monitor() {
        let identity =
            super::super::identity(&json!({"name":"cursor-vscode"}), &editor_caps()).unwrap();
        let conversation = Arc::new(conversation(&identity));
        let waiter = tokio::spawn({
            let conversation = conversation.clone();
            async move { wait_for_bridge_or_scan(&conversation, 2_000).await }
        });
        tokio::task::yield_now().await;
        set_bridge(&conversation, "chat-1".into());
        tokio::time::timeout(Duration::from_millis(500), waiter)
            .await
            .expect("delivery must wake the monitor without a scan interval")
            .unwrap();
        assert_eq!(bridge(&conversation).as_deref(), Some("chat-1"));

        // A bridge set just before the wait is also observed without sleeping.
        tokio::time::timeout(
            Duration::from_millis(500),
            wait_for_bridge_or_scan(&conversation, 2_000),
        )
        .await
        .expect("a known bridge must not wait for another scan");
    }

    #[test]
    fn transcript_fixture_reports_busy_until_turn_ends() {
        let mut reader = Reader::new("chat-1".into(), true);
        let fixture = [
            r#"{"role":"user","message":{"content":"hello"}}"#,
            r#"{"role":"assistant","message":{"content":"working"}}"#,
            r#"{"type":"turn_ended"}"#,
        ];
        let events = fixture
            .iter()
            .map(|line| {
                reader
                    .item(&serde_json::from_str::<Value>(line).unwrap(), false)
                    .into_iter()
                    .filter_map(|event| match event {
                        Observed::Working(value) => Some(value),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(events, vec![vec![true], vec![], vec![false]]);
    }

    /// Why an editor chat is not reached yet: the bridge, then Cursor recording the join.
    #[test]
    fn an_unregistered_chat_says_what_it_waits_for() {
        assert_eq!(state_of(true, false), "registered");
        assert_eq!(state_of(false, false), NO_BRIDGE);
        assert_eq!(state_of(false, true), WAITING);
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
        let frame = json!({"channel":"voice","session_id":"s","revision":1,"epoch":0,"message_id":"m","text":"hola"});
        let answer = deliver_to_editor(&conversation, &frame).await;
        assert_eq!(answer["status"], "failed", "{answer}");
        // Only the fields the core takes in a delivery answer: a failure it keeps, with why in the detail.
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

    #[tokio::test]
    async fn authenticated_editor_reports_engine_without_a_transcript_and_retries_missing_metadata()
    {
        let scratch = Scratch::new("cursor-engine-before-transcript");
        let user_data = scratch.0.join("Cursor");
        let storage = user_data.join("User/globalStorage");
        fs::create_dir_all(&storage).unwrap();
        let database = storage.join("state.vscdb");
        super::super::state_db::fixture(
            &database,
            "CREATE TABLE cursorDiskKV(key TEXT, value TEXT);",
        );
        let identity =
            super::super::identity(&json!({"name":"cursor-vscode"}), &editor_caps()).unwrap();
        let conversation = Arc::new(conversation(&identity));
        let chat = "chat-model".to_owned();
        set_followed(&conversation, chat.clone());
        let profile = Profile::from_vars(
            |key| (key == "HOME").then(|| scratch.0.as_os_str().to_os_string()),
            &[],
        )
        .unwrap();
        let reporter = Arc::new(RecordingReporter {
            profile,
            engines: AsyncMutex::new(Vec::new()),
        });
        let instance = BridgeInstance {
            socket: PathBuf::new(),
            token: String::new(),
            user_data,
            created_at: 0,
        };
        // A known transcript path is deliberately absent throughout this test.
        let unauthenticated = follow_editor_engine(
            conversation.clone(),
            reporter.clone(),
            chat.clone(),
            vec![instance.clone()],
        );
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            reporter.engines.lock().await.is_empty(),
            "unbound chat reported an engine"
        );
        unauthenticated.await.unwrap();
        set_bridge(&conversation, "chat-model".into());
        let watcher =
            follow_editor_engine(conversation.clone(), reporter.clone(), chat, vec![instance]);
        tokio::time::sleep(Duration::from_millis(450)).await;
        assert!(
            reporter.engines.lock().await.is_empty(),
            "missing metadata was guessed"
        );
        super::super::state_db::fixture(&database, "INSERT INTO cursorDiskKV VALUES ('composerData:chat-model', '{\"modelConfig\":{\"modelName\":\"gpt-5\",\"effort\":\"high\"}}');");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !reporter.engines.lock().await.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("authenticated metadata was not reported without a transcript");
        assert_eq!(
            reporter.engines.lock().await.as_slice(),
            &[json!({"model":"gpt-5","effort":"high","thinking":null})]
        );
        conversation.stopped.store(true, Ordering::Relaxed);
        tokio::time::timeout(Duration::from_secs(1), watcher)
            .await
            .unwrap()
            .unwrap();
    }

    /// Records Working from the real Cursor and Codex followers with populated histories.
    /// The initial catch-up delay remains after Cursor bridge discovery is woken.
    struct HistoryReporter {
        profile: Profile,
        events: AsyncMutex<Vec<Value>>,
    }
    impl Reporter for HistoryReporter {
        fn profile(&self) -> &Profile {
            &self.profile
        }
        fn engine<'a>(&'a self, _: &'a Conversation, _: Value) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }
        fn working<'a>(&'a self, _: &'a Conversation, event: Value) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                self.events.lock().await.push(event);
            })
        }
        fn read<'a>(
            &'a self,
            _: &'a Conversation,
            _: &'a str,
            _: Value,
            _: Value,
            _: Value,
        ) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }
        fn conversations(&self) -> BoxFuture<'_, Vec<Arc<Conversation>>> {
            Box::pin(async { vec![] })
        }
    }
    async fn wait_for_caught_up_working(reporter: &HistoryReporter) {
        tokio::time::timeout(Duration::from_secs(6), async {
            while reporter.events.lock().await.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn populated_cursor_and_codex_history_delays_initial_working() {
        use std::{fs, io::Write};
        let root = std::env::temp_dir().join(format!("sv-history-watch-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let previous_data = std::env::var_os("CURSOR_DATA_DIR");
        std::env::set_var("CURSOR_DATA_DIR", &root);
        for large in [false, true] {
            let chat = if large {
                "history-large-chat"
            } else {
                "history-small-chat"
            };
            let path = root.join(format!(
                "projects/project/agent-transcripts/{chat}/{chat}.jsonl"
            ));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let history = format!(
                "{}\n",
                json!({"role":"assistant","message":{"content":"x".repeat(1024)}})
            );
            let mut content = if large {
                history.repeat(500)
            } else {
                String::new()
            };
            content.push_str("{\"role\":\"user\",\"message\":{\"content\":\"hello\"}}\n");
            fs::write(&path, &content).unwrap();
            let identity =
                super::super::identity(&json!({"name":"cursor-vscode"}), &editor_caps()).unwrap();
            let binding = Arc::new(conversation(&identity));
            let reporter = Arc::new(HistoryReporter {
                profile: Profile::for_test(&root.join(chat), &[]),
                events: AsyncMutex::new(vec![]),
            });
            watch_editor(binding.clone(), reporter.clone());
            tokio::time::sleep(Duration::from_millis(50)).await;
            let start = tokio::time::Instant::now();
            set_bridge(&binding, chat.to_owned());
            wait_for_caught_up_working(&reporter).await;
            let elapsed = start.elapsed();
            println!(
                "History Cursor large={large} bytes={} first_working_ms={}",
                content.len(),
                elapsed.as_millis()
            );
            assert_eq!(
                reporter.events.lock().await.as_slice(),
                &[json!({"working":true,"turn_id":null})]
            );
            assert!(elapsed < Duration::from_secs(2));
            for _ in 0..10 {
                set_bridge(&binding, chat.to_owned());
            }
            let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(file, "{{\"type\":\"turn_ended\"}}").unwrap();
            tokio::time::sleep(Duration::from_millis(850)).await;
            assert_eq!(
                reporter.events.lock().await.as_slice(),
                &[
                    json!({"working":true,"turn_id":null}),
                    json!({"working":false,"turn_id":null})
                ]
            );
            binding.stopped.store(true, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(450)).await;
        }
        let profile = Profile::for_test(&root.join("codex"), &[crate::adapters::codex::HOME]);
        let path = profile
            .agent_home("CODEX_HOME")
            .join("sessions/2026/10/15/rollout-history-codex.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let history = format!(
            "{}\n",
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"x".repeat(1024)}]}})
        );
        let mut content = history.repeat(500);
        content.push_str(
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"t1\"}}\n",
        );
        fs::write(&path, &content).unwrap();
        let identity =
            super::super::identity(&json!({"name":"cursor-vscode"}), &editor_caps()).unwrap();
        let mut binding = conversation(&identity);
        binding.thread = "history-codex".into();
        binding.delivery = json!({"kind":"codex-queue"});
        let binding = Arc::new(binding);
        let reporter = Arc::new(HistoryReporter {
            profile,
            events: AsyncMutex::new(vec![]),
        });
        let start = tokio::time::Instant::now();
        crate::adapters::codex::Codex.watch(binding.clone(), reporter.clone());
        wait_for_caught_up_working(&reporter).await;
        println!(
            "History Codex bytes={} first_working_ms={}",
            content.len(),
            start.elapsed().as_millis()
        );
        assert!(start.elapsed() >= Duration::from_secs(2));
        assert_eq!(
            reporter.events.lock().await.as_slice(),
            &[json!({"working":true,"turn_id":"t1"})]
        );
        binding.stopped.store(true, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(450)).await;
        if let Some(value) = previous_data {
            std::env::set_var("CURSOR_DATA_DIR", value);
        } else {
            std::env::remove_var("CURSOR_DATA_DIR");
        }
        fs::remove_dir_all(root).unwrap();
    }
}
