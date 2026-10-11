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
    pub engine: Mutex<Option<Value>>,
    pub route: Option<String>,
    pub capabilities: Value,
    pub experimental: Value,
    pub inbound: Value,
    pub detachable: bool,
    pub owner: AtomicU64,
    pub id: Mutex<String>,
    pub serial: Mutex<()>,
    pub pending: Mutex<VecDeque<String>>,
    pub turns: Mutex<HashMap<String, String>>,
    pub input_context: Mutex<HashMap<String, (String, i64)>>,
    pub read: Mutex<VecDeque<String>>,
    pub working: Mutex<Option<bool>>,
    pub stopped: AtomicBool,
    pub state: Box<dyn Any + Send + Sync>,
}

impl Conversation {
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
