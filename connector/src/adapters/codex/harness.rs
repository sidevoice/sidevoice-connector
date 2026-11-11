//! Codex as a harness: a thread is named in the tool call's metadata, reached by steering its running turn when Codex
//! allows it and else through its queue, and followed through its rollout.

use super::rollout;
use crate::adapters::harness::{Agent, CallContext, Conversation, Harness, Sent, State};
use crate::adapters::watch::{Watch, Watches};
use crate::profile::Profile;
use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::sync::Arc;

/// Codex's threads, as the connector follows them.
pub struct Threads {
    profile: Profile,
    watches: Watches,
}

impl Threads {
    pub fn new(profile: Profile) -> Self {
        Self {
            profile,
            watches: Watches::default(),
        }
    }

    fn watch(&self, thread: &str) -> Arc<Watch> {
        self.watches.get(thread, |watch| {
            rollout::follow(
                watch.clone(),
                self.profile.agent_home(super::HOME.var).to_owned(),
                thread.to_owned(),
            )
        })
    }
}

impl Harness for Threads {
    fn id(&self) -> &'static str {
        "codex"
    }

    /// Codex names the thread in each tool call's metadata (where depends on its version), or in the environment of
    /// an MCP server it started for one thread.
    fn identify(&self, call: &CallContext) -> Result<Option<Conversation>> {
        let meta = call.meta;
        let turn = meta.get("x-codex-turn-metadata").and_then(|value| {
            if let Some(text) = value.as_str() {
                serde_json::from_str::<Value>(text).ok()
            } else {
                Some(value.clone())
            }
        });
        let thread = [
            meta.get("openai/threadId"),
            meta.get("openai/thread_id"),
            meta.get("codexThreadId"),
            meta.get("codex_thread_id"),
            turn.as_ref().and_then(|value| value.get("thread_id")),
            meta.get("session_id"),
            meta.get("thread_id"),
        ]
        .into_iter()
        .flatten()
        .find_map(|value| {
            value
                .as_str()
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        })
        .or_else(|| call.var("CODEX_THREAD_ID"));
        Ok(thread.map(|id| Conversation {
            harness: self.id().into(),
            id,
            agent: Agent::default(),
        }))
    }

    fn send<'a>(
        &'a self,
        conversation: &'a Conversation,
        message: &'a Value,
    ) -> BoxFuture<'a, Sent> {
        Box::pin(super::send(&conversation.id, message, &self.profile))
    }

    fn state<'a>(&'a self, conversation: &'a Conversation) -> BoxFuture<'a, State> {
        Box::pin(async move { self.watch(&conversation.id).state() })
    }

    fn received<'a>(
        &'a self,
        conversation: &'a Conversation,
        ids: &'a [String],
    ) -> BoxFuture<'a, Vec<String>> {
        Box::pin(async move { self.watch(&conversation.id).received(ids) })
    }
}
