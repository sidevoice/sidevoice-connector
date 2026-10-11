//! Codex as a harness: voice through its queue, the rollout followed.

use crate::adapters::conversation::{Conversation, Room};
use crate::adapters::harness::{no_route, Harness};
use crate::adapters::registration::Registration;
use crate::profile::Profile;
use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::sync::Arc;

pub struct Codex;

impl Harness for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn capabilities(&self) -> Value {
        super::capabilities()
    }

    fn registration(&self) -> Option<&dyn Registration> {
        Some(self)
    }

    fn accepts(&self, delivery: &Value, thread: &str) -> bool {
        match delivery.get("kind").and_then(Value::as_str) {
            Some("codex-queue") => delivery.get("thread").and_then(Value::as_str) == Some(thread),
            _ => crate::adapters::http::accepts(delivery, thread),
        }
    }

    fn deliver<'a>(
        &'a self,
        conversation: &'a Conversation,
        frame: &'a Value,
        profile: &'a Profile,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            match conversation.route_kind() {
                Some("codex-queue") => super::deliver(&conversation.delivery, frame, profile).await,
                Some("http") => crate::adapters::http::deliver(&conversation.delivery, frame).await,
                _ => Ok(no_route(&conversation.thread)),
            }
        })
    }

    fn watch(&self, conversation: Arc<Conversation>, room: Arc<dyn Room>) {
        let follows = match conversation.route_kind() {
            Some("codex-queue") => true,
            Some("http") => conversation.capabilities["working"] == "supported",
            _ => false,
        };
        if follows {
            super::rollout::follow(conversation, room);
        }
    }
}
