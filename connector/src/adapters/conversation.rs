//! A joined conversation as the connector holds it, and what a harness reports about one while following it.

use crate::profile::Profile;
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use tokio::sync::Mutex;

/// One joined conversation. `state` is its harness's own (`Harness::conversation_state`).
pub struct Conversation {
    pub client_ref: String,
    pub harness: String,
    pub thread: String,
    pub title: String,
    pub delivery: Value,
    pub initial_reply_context: Mutex<Value>,
    pub engine: Mutex<Option<Value>>,
    pub route: Option<String>,
    pub capabilities: Value,
    pub experimental: Value,
    pub inbound: Value,
    pub detachable: bool,
    pub owner: AtomicU64,
    pub id: Mutex<String>,
    pub serial: Mutex<()>,
    pub publication: Mutex<()>,
    pub pending: Mutex<VecDeque<String>>,
    pub turns: Mutex<HashMap<String, (String, u64)>>,
    pub input_context: Mutex<HashMap<String, (String, i64)>>,
    pub recent_inputs: Mutex<VecDeque<(String, String)>>,
    pub read: Mutex<VecDeque<String>>,
    pub working: Mutex<Option<bool>>,
    pub stopped: AtomicBool,
    pub state: Box<dyn Any + Send + Sync>,
}

impl Conversation {
    /// Keep recent reply routing without retaining a long-lived pull binding's entire journal.
    pub async fn remember_input(&self, id: &str, session: &str, revision: i64, epoch: u64) {
        let key = format!("{session}:{revision}");
        let mut recent = self.recent_inputs.lock().await;
        recent.retain(|(seen, _)| seen != id);
        recent.push_back((id.to_owned(), key.clone()));
        let mut turns = self.turns.lock().await;
        let mut context = self.input_context.lock().await;
        turns.insert(key, (id.to_owned(), epoch));
        context.insert(id.to_owned(), (session.to_owned(), revision));
        while recent.len() > 512 {
            if let Some((old_id, old_key)) = recent.pop_front() {
                context.remove(&old_id);
                if turns
                    .get(&old_key)
                    .is_some_and(|(current, _)| current == &old_id)
                {
                    turns.remove(&old_key);
                }
            }
        }
    }

    pub async fn forget_input(&self, id: &str, session: &str, revision: i64) {
        let key = format!("{session}:{revision}");
        self.recent_inputs
            .lock()
            .await
            .retain(|(seen, _)| seen != id);
        let mut turns = self.turns.lock().await;
        if turns.get(&key).is_some_and(|(current, _)| current == id) {
            turns.remove(&key);
        }
        self.input_context.lock().await.remove(id);
    }

    /// The kind of route voice reaches it by (`delivery.kind`).
    pub fn route_kind(&self) -> Option<&str> {
        self.delivery.get("kind").and_then(Value::as_str)
    }
}

/// Where a harness reports what it observes about a conversation: the connector's link to the room.
pub trait Room: Send + Sync {
    fn profile(&self) -> &Profile;
    /// The engine it runs on: `{model, effort, thinking}`, reported when it changes.
    fn engine<'a>(&'a self, conversation: &'a Conversation, engine: Value) -> BoxFuture<'a, ()>;
    /// Whether it is working: `{working, turn_id}`.
    fn working<'a>(&'a self, conversation: &'a Conversation, event: Value) -> BoxFuture<'a, ()>;
    /// That a message delivered to it was read.
    fn read<'a>(
        &'a self,
        conversation: &'a Conversation,
        message_id: &'a str,
        session_id: Value,
        revision: Value,
        turn_id: Value,
    ) -> BoxFuture<'a, ()>;
    /// Every conversation joined on this machine.
    fn conversations(&self) -> BoxFuture<'_, Vec<Arc<Conversation>>>;
}

/// Working state without a turn id.
pub async fn report_working(room: &dyn Room, conversation: &Conversation, working: bool) {
    room.working(
        conversation,
        json!({"working":working,"turn_id":Value::Null}),
    )
    .await;
}
