//! A joined conversation as the daemon holds it: who it is, which façade owns it, what was handed to it, and what was
//! last told about it.

use crate::adapters::{Agent, Conversation, State};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicU64;
use tokio::sync::Mutex;

/// How many recent deliveries a conversation keeps for diagnostics.
pub const DELIVERIES_KEPT: usize = 20;

/// One joined conversation.
pub struct Binding {
    pub client_ref: String,
    pub harness: String,
    pub thread: String,
    /// Where the agent runs, as its MCP server found it at join.
    pub agent: Agent,
    pub title: String,
    /// The HTTP endpoint the agent named for its messages, used instead of its harness.
    pub endpoint: Option<String>,
    pub initial_reply_context: Mutex<Value>,
    pub engine: Mutex<Option<Value>>,
    /// How it is, as last polled.
    pub state: Mutex<State>,
    /// Kept, ownerless, when its façade goes away: another façade of the same client may take it by its id.
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
    /// How the last few deliveries to it ended (`{message_id, channel, at, result}`), for diagnostics. Never text.
    pub deliveries: Mutex<VecDeque<Value>>,
    pub working: Mutex<Option<bool>>,
}

impl Binding {
    /// The conversation, as its harness names it.
    pub fn conversation(&self) -> Conversation {
        Conversation {
            harness: self.harness.clone(),
            id: self.thread.clone(),
            agent: self.agent.clone(),
        }
    }

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

    /// Keep how a delivery ended: the last [`DELIVERIES_KEPT`] of them.
    pub async fn record_delivery(&self, record: Value) {
        let mut deliveries = self.deliveries.lock().await;
        deliveries.push_back(record);
        while deliveries.len() > DELIVERIES_KEPT {
            deliveries.pop_front();
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
}
