//! The harness adapter: everything the connector knows about one agent, behind one trait, found by its id.

use super::conversation::{Conversation, Room};
use super::registration::Registration;
use crate::profile::Profile;
use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::{json, Map, Value};
use std::any::Any;
use std::sync::{Arc, Weak};

/// One harness: how its conversations are reached and followed, and how Sidevoice is registered with it.
pub trait Harness: Send + Sync {
    /// The id conversations and the agent list name it by (`claude`, `codex`, …).
    fn id(&self) -> &'static str;

    /// What its conversations can do when the client declares nothing.
    fn capabilities(&self) -> Value;

    /// Registering Sidevoice with the agent itself; None when there is nothing to register with.
    fn registration(&self) -> Option<&dyn Registration> {
        None
    }

    /// Whether voice reaches its conversation `thread` by `delivery`.
    fn accepts(&self, delivery: &Value, thread: &str) -> bool;

    /// The harness's own state for one conversation, kept in `Conversation::state`.
    fn conversation_state(&self) -> Box<dyn Any + Send + Sync> {
        Box::new(())
    }

    /// Starts what the harness keeps for all its conversations on this machine, once the room exists.
    fn attach(&self, _profile: &Profile, _room: Weak<dyn Room>) -> Result<()> {
        Ok(())
    }

    /// Readies a conversation's route as it joins; what it returns goes back to the façade as `prepared`.
    fn prepare<'a>(
        &'a self,
        _conversation: &'a Conversation,
    ) -> BoxFuture<'a, Result<Option<Value>>> {
        Box::pin(async { Ok(None) })
    }

    /// Puts a message from the room into the conversation.
    fn deliver<'a>(
        &'a self,
        conversation: &'a Conversation,
        frame: &'a Value,
        profile: &'a Profile,
    ) -> BoxFuture<'a, Result<Value>>;

    /// Releases what `prepare` held, when the conversation leaves.
    fn close<'a>(&'a self, _conversation: &'a Conversation) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    /// Fields about its route that `status` lists beside the conversation.
    fn status<'a>(
        &'a self,
        _conversation: &'a Conversation,
    ) -> BoxFuture<'a, Option<Map<String, Value>>> {
        Box::pin(async { None })
    }

    /// Whether a detached conversation may be taken by another façade: any of this harness (`None`), or the one
    /// named `client_ref`.
    fn adopts(&self, _client_ref: Option<&str>) -> bool {
        false
    }

    /// Follows what the harness records about the conversation (transcript, rollout, session state), reporting it
    /// to the room until the conversation stops.
    fn watch(&self, _conversation: Arc<Conversation>, _room: Arc<dyn Room>) {}
}

/// Unsupported delivery, as a delivery result.
pub fn no_route(thread: &str) -> Value {
    json!({"status":"unsupported","detail":format!("{} has no delivery route", thread)})
}

/// The harnesses, by id. A conversation of an id none claims is served by the fallback (the generic HTTP route).
pub struct Registry {
    harnesses: Vec<Arc<dyn Harness>>,
    fallback: Arc<dyn Harness>,
}

impl Registry {
    pub fn new(harnesses: Vec<Arc<dyn Harness>>) -> Self {
        Self {
            harnesses,
            fallback: Arc::new(super::http::Http),
        }
    }

    /// The harnesses Sidevoice ships.
    pub fn standard() -> Self {
        Self::new(vec![
            Arc::new(super::claude::Claude),
            Arc::new(super::codex::Codex),
            Arc::new(super::cursor::Cursor::default()),
        ])
    }

    /// The harness of `id`, or the fallback.
    pub fn get(&self, id: &str) -> &Arc<dyn Harness> {
        self.find(id).unwrap_or(&self.fallback)
    }

    /// The harness of `id`, if one claims it.
    pub fn find(&self, id: &str) -> Option<&Arc<dyn Harness>> {
        self.harnesses.iter().find(|harness| harness.id() == id)
    }

    /// Every harness, in the order the agent list shows them.
    pub fn all(&self) -> impl Iterator<Item = &Arc<dyn Harness>> {
        self.harnesses.iter().chain(std::iter::once(&self.fallback))
    }

    /// The harnesses with an agent to register with, in the order the agent list shows them.
    pub fn agents(&self) -> impl Iterator<Item = (&'static str, &dyn Registration)> {
        self.harnesses.iter().filter_map(|harness| {
            harness
                .registration()
                .map(|registration| (harness.id(), registration))
        })
    }

    /// The registration of the agent `id`.
    pub fn agent(&self, id: &str) -> Option<(&'static str, &dyn Registration)> {
        self.agents().find(|(agent, _)| *agent == id)
    }
}
