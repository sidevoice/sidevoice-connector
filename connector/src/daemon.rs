use crate::adapters::{Agent, Conversation, Inbound, Limit, Registry, Sent, State, Status};
use crate::agents::HostAgents;
use crate::frame;
use crate::link::{Incoming, Link};
use crate::log_safe::{error_code, ClientId};
use crate::platform::fs::{Files, Fs};
use crate::platform::ipc::Stream;
use crate::profile::Profile;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::time::Duration;
use tokio::time::Instant;

mod binding;
#[cfg(test)]
#[path = "daemon/tests.rs"]
mod bridge_tests;
#[cfg(test)]
mod contract_tests;
mod files;
mod record;
mod run;

use binding::Binding;

use files::{
    prepare_conversation_guard, read_conversation_state, read_outbox, write_conversation_guard,
    CONVERSATION_STATE_MAX_BYTES, GUARD_BLOCK_RESUME, GUARD_CLEAR,
};
use record::Record;
pub use run::run;

/// How long `pair_device` waits for the link to the core (the code itself has 10 s more).
const PAIR_DEVICE_LINK_WAIT: Duration = Duration::from_secs(15);
/// What was handed to the harnesses, kept across a restart (see `record.rs`).
const DELIVERED_FILE: &str = "delivered.json";
/// How often each joined conversation's harness is asked how it is and what it has received.
const POLL: Duration = Duration::from_millis(500);
const RESUME_REASON_NONE: u8 = 0;
const RESUME_REASON_HISTORY: u8 = 1;
const RESUME_REASON_STORAGE: u8 = 2;

/// An optional field of a message, `null` read as absent: a sender of another version may write either.
fn present<'a>(params: &'a Value, key: &str) -> Option<&'a Value> {
    params.get(key).filter(|value| !value.is_null())
}

pub struct Daemon {
    profile: Profile,
    link: Arc<Link>,
    bindings: Mutex<HashMap<String, Arc<Binding>>>,
    outbox: Mutex<Vec<Value>>,
    /// The per-id record of what was handed to each conversation's harness, by thread, kept across a restart: `accepted`
    /// (handed over: the agent's inbox took it, or a write may have reached it) or `received` (the agent has it). This is
    /// what makes the core's at-least-once delivery safe: a message offered again (the core's answer was lost, or the
    /// core restarted holding it) is answered from here and never typed into the agent a second time, and the core's
    /// question about which messages the agent has (`input.received`) is answered from here too. The record is written
    /// after the hand-over, so a crash between the two offers the message once more: typed twice rather than never.
    /// Message ids are the core's own (unique), so the thread and the id name a message.
    delivered: Mutex<Record>,
    replay_tx: mpsc::Sender<()>,
    host_agents: Arc<HostAgents>,
    registry: Arc<Registry>,
    owner_serial: AtomicU64,
    client_count: AtomicU64,
    activity_generation: AtomicU64,
    managed: bool,
    shutdown: watch::Sender<bool>,
    closed_by_client: Mutex<HashMap<String, String>>,
    /// Voluntary agent exits waiting to be replayed to Core after a transport or Core restart.
    left_by_agent: Mutex<HashMap<String, Value>>,
    latest_closed: Mutex<Option<(String, String)>>,
    resume_blocked: AtomicBool,
    resume_block_reason: AtomicU8,
    conversation_guard: std::sync::Mutex<fs::File>,
    conversation_state_serial: Mutex<()>,
}

impl Daemon {
    fn new(
        profile: Profile,
        link: Arc<Link>,
        replay_tx: mpsc::Sender<()>,
        registry: Registry,
        managed: bool,
        shutdown: watch::Sender<bool>,
    ) -> Result<Arc<Self>> {
        let outbox = read_outbox(&profile.data.join("outbox.json"))?;
        let delivered = files::read_delivered(&profile.data.join(DELIVERED_FILE));
        let host_agents = HostAgents::new(profile.clone())?;
        let guard_path = profile.data.join("conversation-state.guard");
        let (guard_state, conversation_guard) = prepare_conversation_guard(&guard_path)?;
        let state_bytes = read_conversation_state(&profile.data.join("conversation-state.json"))?;
        let state = state_bytes
            .as_deref()
            .map(serde_json::from_slice::<Value>)
            .transpose()
            .context("invalid conversation state")?
            .unwrap_or_else(|| json!({}));
        let resume_blocked = state
            .get("resume_blocked")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || guard_state != GUARD_CLEAR;
        let resume_block_reason = if guard_state != GUARD_CLEAR
            || state.get("resume_block_reason").and_then(Value::as_str)
                == Some("state_write_failed")
        {
            RESUME_REASON_STORAGE
        } else if resume_blocked {
            RESUME_REASON_HISTORY
        } else {
            RESUME_REASON_NONE
        };
        let closed_by_client = state
            .get("closed_by_client")
            .and_then(Value::as_object)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|(client_ref, reason)| {
                        reason
                            .as_str()
                            .map(|reason| (client_ref.clone(), reason.to_owned()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let left_by_agent = state
            .get("left_by_agent")
            .and_then(Value::as_object)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|(client_ref, leave)| {
                        let thread = leave.get("thread").and_then(Value::as_str)?;
                        let id = leave.get("binding_id").and_then(Value::as_str)?;
                        (client_ref.len() <= 200
                            && !client_ref.is_empty()
                            && !thread.is_empty()
                            && thread.len() <= 200
                            && !id.is_empty()
                            && id.len() <= 200)
                            .then(|| (client_ref.clone(), leave.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let daemon = Arc::new(Self {
            profile,
            link,
            bindings: Mutex::new(HashMap::new()),
            outbox: Mutex::new(outbox),
            delivered: Mutex::new(delivered),
            replay_tx,
            host_agents,
            registry: Arc::new(registry),
            owner_serial: AtomicU64::new(1),
            client_count: AtomicU64::new(0),
            activity_generation: AtomicU64::new(0),
            managed,
            shutdown,
            closed_by_client: Mutex::new(closed_by_client),
            left_by_agent: Mutex::new(left_by_agent),
            latest_closed: Mutex::new(None),
            resume_blocked: AtomicBool::new(resume_blocked),
            resume_block_reason: AtomicU8::new(resume_block_reason),
            conversation_guard: std::sync::Mutex::new(conversation_guard),
            conversation_state_serial: Mutex::new(()),
        });
        let announcer = daemon.clone();
        let mut stop = daemon.shutdown.subscribe();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = interval.tick() => announcer.reannounce_working().await,
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() { break; }
                    }
                }
            }
        });
        let poller = daemon.clone();
        let mut stop = daemon.shutdown.subscribe();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(POLL);
            loop {
                tokio::select! {
                    _ = interval.tick() => poller.poll().await,
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() { break; }
                    }
                }
            }
        });
        Ok(daemon)
    }

    /// How each joined conversation is now, and which of the messages handed to it it has: what changed is told to
    /// the core.
    async fn poll(&self) {
        let bindings: Vec<_> = self.bindings.lock().await.values().cloned().collect();
        for binding in bindings {
            let state = self
                .state_of(&binding.conversation(), binding.endpoint.is_some())
                .await;
            let working = match state.status {
                Status::Working => Some(true),
                Status::Idle => Some(false),
                Status::Unknown => None,
            };
            let told = *binding.working.lock().await;
            if let Some(working) = working.filter(|working| told != Some(*working)) {
                self.report_working(&binding, working).await;
            }
            if let Some(model) = &state.model {
                self.report_engine(&binding, model.engine()).await;
            }
            *binding.state.lock().await = state;
            self.check_received(&binding).await;
        }
    }

    /// How `conversation` is, from its harness; one whose agent named its own HTTP endpoint takes messages there.
    async fn state_of(&self, conversation: &Conversation, endpoint: bool) -> State {
        let mut state = self
            .registry
            .get(&conversation.harness)
            .state(conversation)
            .await;
        if endpoint {
            state.inbound = Inbound::Open;
        }
        state
    }

    async fn register_core(&self, binding: &Arc<Binding>) -> Result<Value> {
        let mut current = binding.id.lock().await;
        let engine = binding.engine.lock().await.clone().unwrap_or(Value::Null);
        let (capabilities, inbound) = declaration(&*binding.state.lock().await);
        tracing::debug!(
            stage = "register",
            result = "attempt",
            inject = capabilities["deliver"] == "supported",
            "delivery"
        );
        let mut frame = json!({"client_ref":binding.client_ref,"harness":binding.harness,"thread":binding.thread,"title":binding.title,
            "inbound":inbound,"capabilities":capabilities,"engine":engine,"focus":false});
        if !current.starts_with("local-") {
            frame["binding_id"] = json!(*current);
        }
        let reply = match self
            .link
            .request("binding.register", frame, Duration::from_secs(10))
            .await
        {
            Ok(reply) => reply,
            Err(error) => {
                tracing::warn!(
                    stage = "register",
                    result = "link_failed",
                    code = error_code(&error),
                    "delivery"
                );
                return Err(error);
            }
        };
        // An answer may carry fields this connector does not know, `"error": null` among them: only an error is one.
        if let Some(error) = reply.get("error").filter(|error| !error.is_null()) {
            tracing::warn!(stage = "register", result = "core_refused", "delivery");
            if error.as_str() == Some("room.binding_closed") {
                bail!("CORE_BINDING_CLOSED");
            }
            bail!("Core binding refusal: {error}");
        }
        let id = reply
            .get("binding_id")
            .and_then(Value::as_str)
            .context("Core binding ID missing")?;
        if !self.bindings.lock().await.contains_key(&binding.client_ref) {
            let _ = self
                .link
                .notify("binding.unregister", json!({"binding_id":id}))
                .await;
            bail!("façade left while Core registered binding");
        }
        if *current != id {
            let mut outbox = self.outbox.lock().await;
            let old = outbox.clone();
            let mut changed = false;
            for item in outbox.iter_mut() {
                if item["attempted"] == true {
                    continue;
                }
                if item.get("binding_id").and_then(Value::as_str) == Some(current.as_str())
                    || item.get("client_ref").and_then(Value::as_str)
                        == Some(binding.client_ref.as_str())
                {
                    item["binding_id"] = json!(id);
                    changed = true;
                }
            }
            if changed {
                if let Err(error) = Files::write_json(&self.outbox_path(), &*outbox) {
                    *outbox = old;
                    return Err(error.into());
                }
            }
        }
        *current = id.to_owned();
        *binding.initial_reply_context.lock().await = reply
            .get("initial_reply_context")
            .cloned()
            .unwrap_or(Value::Null);
        tracing::info!(stage = "register", result = "registered", "delivery");
        Ok(reply)
    }

    async fn on_welcome(self: &Arc<Self>) {
        if let Err(error) = self.retire_held_audio().await {
            tracing::warn!(code = error_code(&error), "speech outbox retained");
            return;
        }
        // Core may have restarted after a deliberate agent exit. Replay leaves before any live bindings so an old
        // registration cannot make a closed conversation visible while Core rebuilds its inventory.
        let _state_transaction = self.conversation_state_serial.lock().await;
        let left = self.left_by_agent.lock().await.clone();
        for (client_ref, leave) in left {
            if let Err(error) = self.confirm_agent_leave(&client_ref, &leave).await {
                tracing::warn!(code = error_code(&error), "agent leave replay failed");
            }
        }
        drop(_state_transaction);
        let bindings: Vec<_> = self.bindings.lock().await.values().cloned().collect();
        for binding in bindings {
            if let Err(error) = self.register_core(&binding).await {
                if error.to_string() == "CORE_BINDING_CLOSED" {
                    if let Err(close_error) = self
                        .close_binding_from_client(&binding, "binding_closed")
                        .await
                    {
                        tracing::warn!(
                            code = error_code(&close_error),
                            "closing a refused binding failed"
                        );
                    }
                    continue;
                }
                tracing::warn!(code = error_code(&error), "binding replay failed");
                continue;
            }
            let working = *binding.working.lock().await;
            if let Some(working) = working {
                self.report_working(&binding, working).await;
            }
        }
        self.request_replay();
    }

    async fn close_binding_from_client(&self, binding: &Arc<Binding>, reason: &str) -> Result<()> {
        let _state_transaction = self.conversation_state_serial.lock().await;
        if !self
            .bindings
            .lock()
            .await
            .get(&binding.client_ref)
            .is_some_and(|current| Arc::ptr_eq(current, binding))
        {
            return Ok(());
        }
        if let Err(error) = self.arm_conversation_guard() {
            self.mark_state_write_failed();
            let _ = self.shutdown.send(true);
            return Err(error);
        }
        self.bindings.lock().await.remove(&binding.client_ref);
        {
            let mut closed = self.closed_by_client.lock().await;
            closed.insert(binding.client_ref.clone(), reason.to_owned());
            *self.latest_closed.lock().await =
                Some((binding.client_ref.clone(), reason.to_owned()));
        }
        if !self.persist_conversation_state().await {
            bail!("closed binding could not be saved");
        }
        // Closed from the room: the core lets go of its messages and stops asking about them, so its record goes too. An
        // agent's own leave keeps it: the core still holds that conversation's messages if it joins again.
        self.forget_delivered(&binding.thread).await;
        Ok(())
    }

    /// A new Core confirms a leave only after persisting it. An older Core returns no confirmation, so the
    /// record stays pending until a compatible Core answers. Call while holding conversation_state_serial.
    async fn confirm_agent_leave(&self, client_ref: &str, leave: &Value) -> Result<()> {
        let answer = self
            .link
            .request("binding.unregister", leave.clone(), Duration::from_secs(3))
            .await?;
        if answer.get("left") != Some(&Value::Bool(true)) {
            bail!("Core did not confirm durable agent leave");
        }
        let removed = {
            let mut pending = self.left_by_agent.lock().await;
            if pending.get(client_ref) != Some(leave) {
                return Ok(());
            }
            pending.remove(client_ref)
        };
        if !self.persist_conversation_state().await {
            if let Some(leave) = removed {
                self.left_by_agent
                    .lock()
                    .await
                    .insert(client_ref.to_owned(), leave);
            }
            bail!("confirmed agent leave could not be cleared from local state");
        }
        Ok(())
    }

    fn request_replay(&self) {
        // A full slot already requests a pass; the run loop owns its one replay task.
        let _ = self.replay_tx.try_send(());
    }

    fn resume_block_reason(&self) -> Option<&'static str> {
        match self.resume_block_reason.load(Ordering::Acquire) {
            RESUME_REASON_STORAGE => Some("state_write_failed"),
            RESUME_REASON_HISTORY => Some("conversation_history_limit"),
            _ => None,
        }
    }

    fn arm_conversation_guard(&self) -> Result<()> {
        let state = GUARD_BLOCK_RESUME;
        let mut guard = self
            .conversation_guard
            .lock()
            .map_err(|_| anyhow::anyhow!("conversation guard lock poisoned"))?;
        write_conversation_guard(&mut guard, state)
    }

    async fn check_registration_allowed(&self, client_ref: &str, resume: bool) -> Result<()> {
        if resume {
            if self.left_by_agent.lock().await.contains_key(client_ref) {
                bail!("CLOSED_BY_CLIENT:left_by_agent:{client_ref}");
            }
            let closed = self.closed_by_client.lock().await;
            if self.resume_blocked.load(Ordering::Acquire) {
                bail!("CLOSED_BY_CLIENT:conversation_history_limit:{client_ref}");
            }
            if let Some(reason) = closed.get(client_ref) {
                bail!("CLOSED_BY_CLIENT:{reason}:{client_ref}");
            }
        }
        Ok(())
    }

    fn mark_state_write_failed(&self) {
        self.resume_blocked.store(true, Ordering::Release);
        self.resume_block_reason
            .store(RESUME_REASON_STORAGE, Ordering::Release);
        let guard = GUARD_BLOCK_RESUME;
        let result = self
            .conversation_guard
            .lock()
            .map_err(|_| anyhow::anyhow!("conversation guard lock poisoned"))
            .and_then(|mut file| write_conversation_guard(&mut file, guard));
        if let Err(error) = result {
            tracing::error!(
                code = error_code(&error),
                "emergency conversation guard failed"
            );
            let _ = self.shutdown.send(true);
        }
    }

    async fn persist_conversation_state(&self) -> bool {
        let mut closed = self.closed_by_client.lock().await;
        let left = self.left_by_agent.lock().await.clone();
        let mut state = json!({"closed_by_client":closed.clone(),"left_by_agent":left,
            "resume_blocked":self.resume_blocked.load(Ordering::Acquire),
            "resume_block_reason":self.resume_block_reason()});
        let encoded = serde_json::to_vec(&state);
        match encoded {
            Ok(bytes) if bytes.len() > CONVERSATION_STATE_MAX_BYTES => {
                if !self.resume_blocked.swap(true, Ordering::AcqRel) {
                    self.resume_block_reason
                        .store(RESUME_REASON_HISTORY, Ordering::Release);
                }
                let latest_closed = self.latest_closed.lock().await.clone();
                closed.clear();
                if let Some((client_ref, reason)) = latest_closed {
                    closed.insert(client_ref, reason);
                }
                state = json!({
                    "closed_by_client":closed.clone(),"left_by_agent":left,"resume_blocked":true,
                    "resume_block_reason":self.resume_block_reason()});
            }
            Err(_) => {
                tracing::error!(kind = "encode", "conversation state not saved");
                self.mark_state_write_failed();
                return false;
            }
            _ => {}
        }
        if serde_json::to_vec(&state)
            .map_or(true, |bytes| bytes.len() > CONVERSATION_STATE_MAX_BYTES)
        {
            closed.clear();
            self.mark_state_write_failed();
            state = json!({"closed_by_client":{},"left_by_agent":left,
                "resume_blocked":true,"resume_block_reason":self.resume_block_reason()});
            if serde_json::to_vec(&state)
                .map_or(true, |bytes| bytes.len() > CONVERSATION_STATE_MAX_BYTES)
            {
                tracing::error!(code = "state_too_large", "conversation state not saved");
                return false;
            }
        }
        if let Err(error) =
            Files::write_json(&self.profile.data.join("conversation-state.json"), &state)
        {
            tracing::error!(kind = ?error.kind(), os_error = error.raw_os_error(), "conversation state not saved");
            self.mark_state_write_failed();
            false
        } else if let Err(error) = self
            .conversation_guard
            .lock()
            .map_err(|_| anyhow::anyhow!("conversation guard lock poisoned"))
            .and_then(|mut file| write_conversation_guard(&mut file, GUARD_CLEAR))
        {
            tracing::error!(code = error_code(&error), "clear conversation guard failed");
            self.mark_state_write_failed();
            false
        } else {
            true
        }
    }

    async fn send_working_event(&self, binding: &Binding, mut event: Value) {
        let current = binding.working.lock().await;
        if event.get("working").and_then(Value::as_bool) != *current {
            return;
        }
        let id = binding.id.lock().await.clone();
        if id.starts_with("local-") {
            return;
        }
        event["binding_id"] = json!(id);
        let _ = self.link.notify("input.working", event).await;
    }

    async fn report_working_event(&self, binding: &Binding, event: Value) {
        if let Some(working) = event.get("working").and_then(Value::as_bool) {
            *binding.working.lock().await = Some(working);
            self.send_working_event(binding, event).await;
        }
    }

    async fn report_working(&self, binding: &Binding, working: bool) {
        self.report_working_event(binding, json!({"working":working,"turn_id":Value::Null}))
            .await;
    }

    async fn report_engine(&self, binding: &Binding, engine: Value) {
        if !engine
            .get("model")
            .and_then(Value::as_str)
            .is_some_and(|model| !model.is_empty())
        {
            return;
        }
        let mut current = binding.engine.lock().await;
        let same = current.as_ref().is_some_and(|previous| {
            ["model", "effort", "thinking"]
                .iter()
                .all(|key| previous.get(key) == engine.get(key))
        });
        if same {
            return;
        }
        *current = Some(engine.clone());
        drop(current);
        let id = binding.id.lock().await.clone();
        if !id.starts_with("local-") {
            let _ = self
                .link
                .notify("input.engine", json!({"binding_id":id,"engine":engine}))
                .await;
        }
    }

    async fn reannounce_working(&self) {
        let bindings: Vec<_> = self.bindings.lock().await.values().cloned().collect();
        for binding in bindings {
            let working = *binding.working.lock().await;
            if let Some(working) = working {
                self.send_working_event(&binding, json!({"working":working,"turn_id":Value::Null}))
                    .await;
            }
        }
    }

    fn outbox_path(&self) -> PathBuf {
        self.profile.data.join("outbox.json")
    }

    async fn queue(&self, speech: Value) -> Result<()> {
        let mut outbox = self.outbox.lock().await;
        let before = outbox.clone();
        outbox.push(speech);
        while outbox
            .iter()
            .filter(|item| item.get("resolved").is_some())
            .count()
            > 2048
            || serde_json::to_vec(&*outbox)?.len() > 8 << 20
        {
            if let Some(index) = outbox
                .iter()
                .position(|item| item.get("resolved").is_some())
            {
                outbox.remove(index);
            } else {
                *outbox = before;
                bail!("outbox full");
            }
        }
        if let Err(error) = Files::write_json(&self.outbox_path(), &*outbox) {
            *outbox = before;
            return Err(error.into());
        }
        Ok(())
    }

    fn ack_allows_removal(speech: &Value, reply: &Value) -> bool {
        let ids_match = ["event_id", "utterance_id"].iter().all(|key| {
            speech.get(key).and_then(Value::as_str).is_some() && speech.get(key) == reply.get(key)
        });
        ids_match
            && (reply.get("text_saved") == Some(&json!(true))
                || (reply.get("status") == Some(&json!("rejected"))
                    && reply.get("terminal") == Some(&json!(true))))
    }

    async fn make_history_only(&self, event_id: &Value) -> Result<()> {
        let mut outbox = self.outbox.lock().await;
        let before = outbox.clone();
        if let Some(item) = outbox
            .iter_mut()
            .find(|item| item.get("event_id") == Some(event_id))
        {
            if item["attempted"] == true || item["history_only"] == true {
                return Ok(());
            }
            item["history_only"] = json!(true);
            if let Err(error) = Files::write_json(&self.outbox_path(), &*outbox) {
                *outbox = before;
                return Err(error.into());
            }
        }
        Ok(())
    }

    async fn clear_attempted(&self, event_id: &Value) -> Result<()> {
        let mut outbox = self.outbox.lock().await;
        let before = outbox.clone();
        if let Some(item) = outbox
            .iter_mut()
            .find(|item| item.get("event_id") == Some(event_id))
        {
            item.as_object_mut().unwrap().remove("attempted");
            if let Err(error) = Files::write_json(&self.outbox_path(), &*outbox) {
                *outbox = before;
                return Err(error.into());
            }
        }
        Ok(())
    }

    async fn rebind_known_unknown(&self, event_id: &Value, binding_id: &str) -> Result<Value> {
        let mut outbox = self.outbox.lock().await;
        let before = outbox.clone();
        let item = outbox
            .iter_mut()
            .find(|item| item.get("event_id") == Some(event_id))
            .context("speech disappeared during binding repair")?;
        item["binding_id"] = json!(binding_id);
        let published = item.clone();
        if let Err(error) = Files::write_json(&self.outbox_path(), &*outbox) {
            *outbox = before;
            return Err(error.into());
        }
        Ok(published)
    }

    async fn retire_held_audio(&self) -> Result<()> {
        let mut outbox = self.outbox.lock().await;
        if outbox
            .iter()
            .all(|item| item["history_only"] == true || item["attempted"] == true)
        {
            return Ok(());
        }
        let before = outbox.clone();
        for item in outbox.iter_mut() {
            if item["attempted"] != true {
                item["history_only"] = json!(true);
            }
        }
        if let Err(error) = Files::write_json(&self.outbox_path(), &*outbox) {
            *outbox = before;
            return Err(error.into());
        }
        Ok(())
    }

    async fn publish_one(&self, speech: &Value) -> Result<Option<Value>> {
        if !self.link.connected().await {
            self.make_history_only(&speech["event_id"]).await?;
            return Ok(None);
        }
        // Persist the attempt before transport can take the frame. A lost answer leaves an
        // uncertain admission; its immutable fingerprint must never be rewritten or replayed
        // with late-audio risk.
        let mut published = {
            let mut outbox = self.outbox.lock().await;
            let before = outbox.clone();
            let Some(item) = outbox
                .iter_mut()
                .find(|item| item.get("event_id") == speech.get("event_id"))
            else {
                return Ok(None);
            };
            if let Some(reply) = item.get("resolved") {
                return Ok(Some(reply.clone()));
            }
            if item["attempted"] == true {
                let original = item.clone();
                drop(outbox);
                return self.recover_one(&original).await;
            }
            item["attempted"] = json!(true);
            let published = item.clone();
            if let Err(error) = Files::write_json(&self.outbox_path(), &*outbox) {
                *outbox = before;
                return Err(error.into());
            }
            published
        };
        published.as_object_mut().unwrap().remove("attempted");
        let result = self
            .link
            .request("speech.publish", published.clone(), Duration::from_secs(15))
            .await;
        let mut reply = match result {
            Ok(v) => v,
            Err(error) => {
                tracing::warn!(code = error_code(&error), "speech retry retained");
                return Ok(None);
            }
        };
        if reply.get("status") == Some(&json!("unknown_binding")) {
            if let Some(binding) = self
                .binding_for_id(
                    published
                        .get("binding_id")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                )
                .await
            {
                if self.register_core(&binding).await.is_ok() {
                    published = self
                        .rebind_known_unknown(&speech["event_id"], &binding.id.lock().await)
                        .await?;
                    published.as_object_mut().unwrap().remove("attempted");
                    reply = match self
                        .link
                        .request("speech.publish", published.clone(), Duration::from_secs(15))
                        .await
                    {
                        Ok(value) => value,
                        Err(error) => {
                            tracing::warn!(code = error_code(&error), "speech retry retained");
                            return Ok(None);
                        }
                    };
                } else {
                    self.clear_attempted(&speech["event_id"]).await?;
                    return Ok(None);
                }
            } else {
                self.clear_attempted(&speech["event_id"]).await?;
                return Ok(None);
            }
        }
        if Self::ack_allows_removal(&published, &reply) {
            let event_id = speech.get("event_id");
            let mut outbox = self.outbox.lock().await;
            let before = outbox.clone();
            outbox.retain(|item| item.get("event_id") != event_id);
            if let Err(error) = Files::write_json(&self.outbox_path(), &*outbox) {
                *outbox = before;
                return Err(error.into());
            }
        }
        if Self::ack_allows_removal(&published, &reply) {
            Ok(Some(reply))
        } else {
            Ok(None)
        }
    }

    async fn recover_one(&self, speech: &Value) -> Result<Option<Value>> {
        let client_ref = speech
            .get("client_ref")
            .and_then(Value::as_str)
            .unwrap_or("");
        let Some(binding) = self.bindings.lock().await.get(client_ref).cloned() else {
            return Ok(None);
        };
        let mut original = speech.clone();
        original.as_object_mut().unwrap().remove("attempted");
        for retry in 0..2 {
            if !self.link.connected().await {
                return Ok(None);
            }
            let mut request = original.clone();
            request["recovery_binding_id"] = json!(*binding.id.lock().await);
            let reply = match self
                .link
                .request("speech.recover", request, Duration::from_secs(15))
                .await
            {
                Ok(reply) => reply,
                Err(error) => {
                    tracing::warn!(code = error_code(&error), "speech recovery retained");
                    return Ok(None);
                }
            };
            if reply.get("status") == Some(&json!("unknown_binding")) {
                if retry == 0 && self.register_core(&binding).await.is_ok() {
                    continue;
                }
                return Ok(None);
            }
            if !Self::ack_allows_removal(&original, &reply) {
                return Ok(None);
            }
            let mut outbox = self.outbox.lock().await;
            let before = outbox.clone();
            if let Some(item) = outbox
                .iter_mut()
                .find(|item| item.get("event_id") == speech.get("event_id"))
            {
                item["resolved"] = reply.clone();
            }
            if let Err(error) = Files::write_json(&self.outbox_path(), &*outbox) {
                *outbox = before;
                return Err(error.into());
            }
            return Ok(Some(reply));
        }
        Ok(None)
    }

    async fn flush_outbox(&self) {
        let items = self.outbox.lock().await.clone();
        let mut blocked = std::collections::HashSet::new();
        for speech in items {
            if speech.get("resolved").is_some() {
                continue;
            }
            let Some(client_ref) = speech.get("client_ref").and_then(Value::as_str) else {
                continue;
            };
            let Some(binding) = self.bindings.lock().await.get(client_ref).cloned() else {
                continue;
            };
            let _publication = binding.publication.lock().await;
            if blocked.contains(client_ref) {
                let _ = self.make_history_only(&speech["event_id"]).await;
            }
            if self.publish_one(&speech).await.ok().flatten().is_none() {
                blocked.insert(client_ref.to_owned());
            }
        }
    }

    async fn binding_for_id(&self, id: &str) -> Option<Arc<Binding>> {
        let candidates: Vec<_> = self.bindings.lock().await.values().cloned().collect();
        for b in candidates {
            if *b.id.lock().await == id {
                return Some(b);
            }
        }
        None
    }

    async fn incoming(self: Arc<Self>, mut item: Incoming) {
        let answer = if let Some(reply) = item.reply.as_mut() {
            tokio::select! {
                _ = reply.closed() => return,
                answer = self.handle_incoming(&item.method, &item.params) => answer,
            }
        } else {
            self.handle_incoming(&item.method, &item.params).await
        };
        if let Some(tx) = item.reply {
            let _ = tx.send(answer);
        }
    }

    async fn handle_incoming(self: &Arc<Self>, method: &str, params: &Value) -> Value {
        match method {
            "connector.welcome" => {
                self.on_welcome().await;
                json!({})
            }
            "input.deliver" => self.deliver(params).await,
            "input.received" => self.received(params).await,
            "binding.close" => {
                let id = params
                    .get("binding_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if let Some(binding) = self.binding_for_id(id).await {
                    let reason = params
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("closed_from_client");
                    if let Err(error) = self.close_binding_from_client(&binding, reason).await {
                        tracing::warn!(code = error_code(&error), "close binding failed");
                    }
                }
                json!({})
            }
            "agents.list" | "agents.connect" | "agents.disconnect" | "agents.dismiss"
            | "agents.check" => self.host_agents.handle(method, params.clone()).await,
            "connector.error" => json!({}),
            "node.status" => self.node_status().await,
            _ => json!({"error":{"key":"connector.unknown-method"}}),
        }
    }

    async fn node_status(&self) -> Value {
        crate::service::node_status(&self.profile).await
    }

    /// What the record says of `message_id`, handed to `thread`'s harness: `accepted` or `received`.
    async fn record_of(&self, thread: &str, message_id: &str) -> Option<String> {
        self.delivered
            .lock()
            .await
            .status(thread, message_id)
            .map(str::to_owned)
    }

    /// The messages handed to `thread`'s harness that the agent has not been seen to have, oldest first.
    async fn awaiting_receipt(&self, thread: &str) -> Vec<String> {
        self.delivered.lock().await.awaiting(thread)
    }

    /// Records `message_id` as `status` for `thread` and writes the record. A message received stays received.
    async fn remember(&self, thread: &str, message_id: &str, status: &str) {
        let mut delivered = self.delivered.lock().await;
        if delivered.set(thread, message_id, status) {
            self.write_delivered(&delivered);
        }
    }

    /// Drops the record of `thread`, whose conversation ended.
    async fn forget_delivered(&self, thread: &str) {
        let mut delivered = self.delivered.lock().await;
        if delivered.forget(thread) {
            self.write_delivered(&delivered);
        }
    }

    fn write_delivered(&self, delivered: &Record) {
        if let Err(error) = Files::write_json(
            &self.profile.data.join(DELIVERED_FILE),
            &delivered.to_json(),
        ) {
            tracing::warn!(stage = "daemon", result = "delivered_not_kept", kind = ?error.kind(), "delivery");
        }
    }

    /// Asks `binding`'s harness which of the messages handed to it the agent now has; each one newly seen is recorded and
    /// told to the core at once (`input.read`). The core's question (`input.received`) recovers one told and lost. A
    /// message the agent took while this connector was down is found too: each harness follows its conversation from
    /// the start of its transcript or rollout the first time it is asked about it.
    async fn check_received(&self, binding: &Binding) {
        let awaiting = self.awaiting_receipt(&binding.thread).await;
        if awaiting.is_empty() || binding.endpoint.is_some() {
            return;
        }
        let harness = self.registry.get(&binding.harness);
        for id in harness.received(&binding.conversation(), &awaiting).await {
            self.remember(&binding.thread, &id, "received").await;
            self.report_read(binding, &id).await;
        }
    }

    /// The core asks which of `message_ids`, handed to a conversation, its agent has. Answered from the record (after
    /// one look at the harness for those still awaited), never by handing anything to the agent: `received` names those
    /// it has, `unknown` those this connector has no record of (never handed over: the core offers them again).
    async fn received(&self, params: &Value) -> Value {
        let id = params
            .get("binding_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let Some(binding) = self.binding_for_id(id).await else {
            return json!({"error":{"key":"connector.unknown-binding"}});
        };
        self.check_received(&binding).await;
        let mut received = Vec::new();
        let mut unknown = Vec::new();
        for message_id in params
            .get("message_ids")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            match self.record_of(&binding.thread, message_id).await.as_deref() {
                Some("received") => received.push(message_id),
                Some(_) => {}
                None => unknown.push(message_id),
            }
        }
        json!({"received":received,"unknown":unknown})
    }

    #[tracing::instrument(
        skip_all,
        fields(binding, message_id, session, revision, epoch, channel, harness)
    )]
    async fn deliver(&self, frame: &Value) -> Value {
        let id = frame
            .get("binding_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let message_id = frame
            .get("message_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let session_id = frame
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let revision = frame.get("revision").and_then(Value::as_i64).unwrap_or(0);
        let text = frame.get("text").and_then(Value::as_str).unwrap_or("");
        // A note from Sidevoice carries no words of the user's.
        let note = frame.get("channel").and_then(Value::as_str) == Some("note");
        // The ids are the sender's choice: they are logged only as `ClientId` shows them.
        let span = tracing::Span::current();
        span.record("binding", tracing::field::display(ClientId(id)))
            .record("message_id", tracing::field::display(ClientId(message_id)))
            .record("session", tracing::field::display(ClientId(session_id)))
            .record("revision", revision)
            .record("epoch", frame["epoch"].as_u64().unwrap_or(0))
            .record("channel", if note { "note" } else { "voice" });
        tracing::info!(stage = "daemon", result = "received", "delivery");
        let Some(binding) = self.binding_for_id(id).await else {
            tracing::warn!(stage = "daemon", result = "unknown_binding", "delivery");
            return json!({"status":"unknown_binding"});
        };
        span.record("harness", binding.harness.as_str());
        let _serial = binding.serial.lock().await;
        // An id the record cannot keep is refused before anything is handed over: unremembered, it could be typed in
        // again on every offer.
        if message_id.is_empty()
            || message_id.len() > record::ID_MAX
            || session_id.is_empty()
            || (text.is_empty() && !note)
        {
            tracing::warn!(stage = "daemon", result = "invalid_input", "delivery");
            return json!({"status":"failed","detail":"invalid input"});
        }
        // Offered again what the harness already has (the core's answer was lost, or it restarted holding the message):
        // answered from the record, never handed over twice.
        if let Some(status) = self.record_of(&binding.thread, message_id).await {
            tracing::info!(stage = "daemon", result = "already_delivered", "delivery");
            return json!({"status":"accepted","received":status == "received",
                "detail":"already handed to the agent"});
        }
        binding
            .remember_input(
                message_id,
                session_id,
                revision,
                frame["epoch"].as_u64().unwrap_or(0),
            )
            .await;
        tracing::debug!(
            stage = "adapter",
            result = "attempt",
            endpoint = binding.endpoint.is_some(),
            "delivery"
        );
        let sent = match &binding.endpoint {
            Some(endpoint) => crate::adapters::post(endpoint, &binding.thread, frame).await,
            None => {
                self.registry
                    .get(&binding.harness)
                    .send(&binding.conversation(), frame)
                    .await
            }
        };
        let result = match sent {
            Sent::Accepted => {
                // The agent's own endpoint answers once it has the message; any other way in only took it.
                let received = binding.endpoint.is_some();
                self.remember(
                    &binding.thread,
                    message_id,
                    if received { "received" } else { "accepted" },
                )
                .await;
                json!({"status":"accepted","received":received})
            }
            Sent::Refused(limit) => {
                binding.forget_input(message_id, session_id, revision).await;
                let status = match limit {
                    Limit::NoRoute(_) => "unsupported",
                    _ => "failed",
                };
                json!({"status":status,"detail":limit.detail()})
            }
        };
        let status = match result.get("status").and_then(Value::as_str) {
            Some("accepted") => "accepted",
            Some("failed") => "failed",
            Some("unsupported") => "unsupported",
            _ => "unrecognized",
        };
        tracing::info!(stage = "adapter", result = status, "delivery");
        binding
            .record_delivery(json!({"message_id":message_id,
                "channel":frame.get("channel").and_then(Value::as_str).unwrap_or("voice"),"at":crate::logfile::timestamp(),"result":status}))
            .await;
        result
    }

    async fn command(self: &Arc<Self>, owner: u64, method: &str, params: Value) -> Result<Value> {
        match method {
            "register" => {
                let client_ref = params
                    .get("client_ref")
                    .and_then(Value::as_str)
                    .context("client_ref required")?
                    .to_owned();
                let resume = params.get("resume") == Some(&json!(true));
                let thread = params
                    .get("thread")
                    .and_then(Value::as_str)
                    .context("thread required")?
                    .to_owned();
                let title = params
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let harness = params
                    .get("harness")
                    .and_then(Value::as_str)
                    .context("harness required")?
                    .to_owned();
                // The agent's own HTTP endpoint for messages, when it names one: used whatever its harness.
                let endpoint = present(&params, "endpoint")
                    .and_then(Value::as_str)
                    .filter(|url| !url.is_empty())
                    .map(str::to_owned);
                let detachable = params.get("detachable") == Some(&json!(true));
                let _state_transaction = self.conversation_state_serial.lock().await;
                self.check_registration_allowed(&client_ref, resume).await?;
                let existing = { self.bindings.lock().await.get(&client_ref).cloned() };
                if let Some(existing) = existing {
                    let previous_owner = existing.owner.load(Ordering::Relaxed);
                    if previous_owner == 0 && existing.detachable {
                        existing.owner.store(owner, Ordering::Relaxed);
                    } else if previous_owner != owner {
                        bail!("binding belongs to another façade");
                    }
                    let registered = self.register_core(&existing).await?;
                    let binding_id = existing.id.lock().await.clone();
                    drop(_state_transaction);
                    let mut answer = json!({"binding_id":binding_id,"thread":thread,"connected":self.link.connected().await,
                        "epoch":registered.get("epoch"),"initial_reply_context":registered.get("initial_reply_context")});
                    joined_way_in(&mut answer, &existing.state.lock().await.inbound);
                    return Ok(answer);
                }
                drop(_state_transaction);
                let conversation = Conversation {
                    harness,
                    id: thread.clone(),
                    agent: Agent::from_json(present(&params, "agent")),
                };
                let state = self.state_of(&conversation, endpoint.is_some()).await;
                if let Inbound::Blocked(Limit::InboundBlockedByPermissions { reason, remedy }) =
                    &state.inbound
                {
                    bail!("This conversation cannot connect: {reason} {remedy}");
                }
                let binding = Arc::new(Binding {
                    client_ref: client_ref.clone(),
                    harness: conversation.harness,
                    thread: thread.clone(),
                    agent: conversation.agent,
                    title,
                    endpoint,
                    initial_reply_context: Mutex::new(Value::Null),
                    engine: Mutex::new(state.model.as_ref().map(|model| model.engine())),
                    state: Mutex::new(state),
                    detachable,
                    owner: AtomicU64::new(owner),
                    id: Mutex::new(format!("local-{}", uuid::Uuid::new_v4())),
                    serial: Mutex::new(()),
                    publication: Mutex::new(()),
                    turns: Mutex::new(HashMap::new()),
                    input_context: Mutex::new(HashMap::new()),
                    recent_inputs: Mutex::new(VecDeque::new()),
                    deliveries: Mutex::new(VecDeque::new()),
                    working: Mutex::new(None),
                });
                let _state_transaction = self.conversation_state_serial.lock().await;
                self.check_registration_allowed(&client_ref, resume).await?;
                if let Some(existing) = self.bindings.lock().await.get(&client_ref).cloned() {
                    let previous_owner = existing.owner.load(Ordering::Relaxed);
                    if previous_owner == 0 && existing.detachable {
                        existing.owner.store(owner, Ordering::Relaxed);
                    } else if previous_owner != owner {
                        bail!("binding belongs to another façade");
                    }
                    let registered = self.register_core(&existing).await?;
                    let binding_id = existing.id.lock().await.clone();
                    drop(_state_transaction);
                    let mut answer = json!({"binding_id":binding_id,"thread":thread,
                        "connected":self.link.connected().await,
                        "epoch":registered.get("epoch"),"initial_reply_context":registered.get("initial_reply_context")});
                    joined_way_in(&mut answer, &existing.state.lock().await.inbound);
                    return Ok(answer);
                }
                self.bindings
                    .lock()
                    .await
                    .insert(client_ref, binding.clone());
                let prior_latest = self.latest_closed.lock().await.clone();
                let prior_closed = {
                    let mut closed = self.closed_by_client.lock().await;
                    let prior = closed.remove(&binding.client_ref);
                    let mut latest_closed = self.latest_closed.lock().await;
                    if latest_closed
                        .as_ref()
                        .is_some_and(|(client_ref, _)| client_ref == &binding.client_ref)
                    {
                        *latest_closed = None;
                    }
                    prior
                };
                let prior_leave = self.left_by_agent.lock().await.remove(&binding.client_ref);
                if !self.persist_conversation_state().await {
                    self.bindings.lock().await.remove(&binding.client_ref);
                    if let Some(reason) = prior_closed {
                        self.closed_by_client
                            .lock()
                            .await
                            .insert(binding.client_ref.clone(), reason);
                    }
                    *self.latest_closed.lock().await = prior_latest;
                    if let Some(leave) = prior_leave {
                        self.left_by_agent
                            .lock()
                            .await
                            .insert(binding.client_ref.clone(), leave);
                    }
                    bail!("explicit join could not be saved");
                }
                drop(_state_transaction);
                let result = self.register_core(&binding).await;
                let id = binding.id.lock().await.clone();
                if result.is_ok() {
                    self.request_replay();
                }
                let mut answer = json!({"binding_id":id,"thread":thread,"connected":result.is_ok(),"pending":result.is_err(),
                    "epoch":result.as_ref().ok().and_then(|v| v.get("epoch")),
                    "initial_reply_context":result.as_ref().ok().and_then(|v| v.get("initial_reply_context"))});
                joined_way_in(&mut answer, &binding.state.lock().await.inbound);
                Ok(answer)
            }
            "publish" => {
                if let Some(client_ref) = params.get("client_ref").and_then(Value::as_str) {
                    if let Some(reason) =
                        self.closed_by_client.lock().await.get(client_ref).cloned()
                    {
                        bail!("CLOSED_BY_CLIENT:{reason}:{client_ref}");
                    }
                }
                if let Some(refs) = params.get("client_refs").and_then(Value::as_array) {
                    for item in refs {
                        if let Some(client_ref) = item.as_str() {
                            if let Some(reason) =
                                self.closed_by_client.lock().await.get(client_ref).cloned()
                            {
                                bail!("CLOSED_BY_CLIENT:{reason}:{client_ref}");
                            }
                        }
                    }
                }
                let session_id = params
                    .get("session_id")
                    .and_then(Value::as_str)
                    .context("session_id required")?;
                let revision = params
                    .get("revision")
                    .and_then(Value::as_i64)
                    .context("revision required")?;
                let epoch = params
                    .get("epoch")
                    .and_then(Value::as_u64)
                    .context("epoch required")?;
                let routed = present(&params, "client_refs").is_some()
                    || params.get("adopt_orphans") == Some(&json!(true));
                let (binding, adopted) = if routed {
                    self.find_for_turn(owner, &params, session_id, revision)
                        .await?
                } else {
                    (self.find_owned(owner, &params).await?, false)
                };
                let text = params
                    .get("text")
                    .and_then(Value::as_str)
                    .context("text required")?;
                if text.is_empty() || text.len() > 65536 {
                    bail!("text length invalid");
                }
                let event_id = params.get("event_id").and_then(Value::as_str);
                let utterance_id = params.get("utterance_id").and_then(Value::as_str);
                if event_id.is_some() != utterance_id.is_some()
                    || event_id.is_some_and(str::is_empty)
                    || utterance_id.is_some_and(str::is_empty)
                {
                    bail!("publish requires nonempty event_id and utterance_id together");
                }
                let _publication = binding.publication.lock().await;
                let id = binding.id.lock().await;
                let history_only = !self.link.connected().await;
                let speech = json!({"event_id":event_id.map(str::to_owned).unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    "binding_id":*id,"client_ref":binding.client_ref,"session_id":session_id,"revision":revision,"epoch":epoch,
                    "utterance_id":utterance_id.map(str::to_owned).unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    "text":text,"language":params.get("language"),"history_only":history_only});
                let prior = self
                    .outbox
                    .lock()
                    .await
                    .iter()
                    .find(|item| {
                        item["event_id"] == speech["event_id"]
                            || item["utterance_id"] == speech["utterance_id"]
                    })
                    .cloned();
                if let Some(prior) = prior {
                    for field in [
                        "event_id",
                        "utterance_id",
                        "client_ref",
                        "session_id",
                        "revision",
                        "epoch",
                        "text",
                        "language",
                    ] {
                        if prior[field] != speech[field] {
                            bail!("publish retry IDs conflict with a pending publication");
                        }
                    }
                    drop(id);
                    let answer = self.publish_one(&prior).await?;
                    let mut result = answer.unwrap_or_else(|| {
                        if prior["attempted"] == true {
                            json!({"status":"admission_unknown","event_id":prior["event_id"],
                                "utterance_id":prior["utterance_id"],"instruction":"Core admission is uncertain; wait for the next message before replying again."})
                        } else {
                            json!({"status":"queued","event_id":prior["event_id"],"utterance_id":prior["utterance_id"]})
                        }
                    });
                    if adopted {
                        result["adopted"] = json!({"client_ref":binding.client_ref,"binding_id":*binding.id.lock().await,
                            "harness":binding.harness,"title":binding.title,
                            "inbound":binding.state.lock().await.inbound.view()});
                    }
                    return Ok(result);
                }
                drop(id);
                // Settle older attempts before deciding whether this new answer can play.
                // An uncertain first admission is recovered, never published again.
                let earlier = self
                    .outbox
                    .lock()
                    .await
                    .iter()
                    .filter(|item| {
                        item["client_ref"] == speech["client_ref"] && item.get("resolved").is_none()
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                let mut unresolved = false;
                for item in earlier {
                    if self.publish_one(&item).await?.is_none() {
                        unresolved = true;
                        break;
                    }
                }
                self.queue(speech.clone()).await?;
                if unresolved {
                    self.make_history_only(&speech["event_id"]).await?;
                }
                let answer = self.publish_one(&speech).await?;
                let mut result = if let Some(answer) = answer {
                    answer
                } else {
                    let attempted = self
                        .outbox
                        .lock()
                        .await
                        .iter()
                        .find(|item| item["event_id"] == speech["event_id"])
                        .is_some_and(|item| item["attempted"] == true);
                    if attempted {
                        json!({"status":"admission_unknown","event_id":speech["event_id"],
                            "utterance_id":speech["utterance_id"],"instruction":"Core admission is uncertain; wait for the next message before replying again."})
                    } else {
                        json!({"status":"queued","event_id":speech["event_id"],"utterance_id":speech["utterance_id"]})
                    }
                };
                if adopted {
                    result["adopted"] = json!({"client_ref":binding.client_ref,"binding_id":*binding.id.lock().await,
                        "harness":binding.harness,"title":binding.title,
                        "inbound":binding.state.lock().await.inbound.view()});
                }
                Ok(result)
            }
            "get_messages" => {
                let binding = self.find_owned(owner, &params).await?;
                let _serial = binding.serial.lock().await;
                let operation = params
                    .get("operation")
                    .and_then(Value::as_str)
                    .unwrap_or("get");
                if !matches!(operation, "get" | "check") {
                    bail!("invalid pull operation");
                }
                let binding_id = binding.id.lock().await.clone();
                let mut request = json!({"binding_id":binding_id,"operation":operation});
                if let Some(after) = params.get("after") {
                    request["after"] = after.clone();
                }
                if let Some(ack_ids) = params.get("ack_ids") {
                    request["ack_ids"] = ack_ids.clone();
                }
                let mut result = self
                    .link
                    .request("input.pull", request, Duration::from_secs(10))
                    .await?;
                if let Err(error) = crate::adapters::prepare_pull_page(
                    &mut result,
                    params.get("after").and_then(Value::as_u64).unwrap_or(0),
                ) {
                    // A malformed or unrepresentable page cannot be delivered or acknowledged.
                    let _ = self
                        .link
                        .notify("binding.unregister", json!({"binding_id":binding_id}))
                        .await;
                    let _ = self.register_core(&binding).await;
                    return Err(error);
                }
                if let Some(ids) = params.get("ack_ids").and_then(Value::as_array) {
                    let mut context = binding.input_context.lock().await;
                    for id in ids.iter().filter_map(Value::as_str) {
                        context.remove(id);
                    }
                }
                for message in result["messages"].as_array().expect("validated pull page") {
                    let session = message["session_id"].as_str().unwrap();
                    let revision = message["revision"].as_i64().unwrap();
                    let epoch = message["epoch"].as_u64().unwrap();
                    let id = message["message_id"].as_str().unwrap();
                    binding.remember_input(id, session, revision, epoch).await;
                }
                Ok(result)
            }
            "unregister" => {
                let _state_transaction = self.conversation_state_serial.lock().await;
                let binding = self.find_owned(owner, &params).await?;
                let leave = json!({"binding_id":*binding.id.lock().await,"thread":binding.thread,"leave":true});
                self.arm_conversation_guard()?;
                self.left_by_agent
                    .lock()
                    .await
                    .insert(binding.client_ref.clone(), leave.clone());
                if !self.persist_conversation_state().await {
                    bail!("agent leave could not be saved");
                }
                self.bindings.lock().await.remove(&binding.client_ref);
                if self.link.connected().await {
                    if let Err(error) = self.confirm_agent_leave(&binding.client_ref, &leave).await
                    {
                        tracing::warn!(
                            code = error_code(&error),
                            "agent leave confirmation failed"
                        );
                    }
                }
                drop(_state_transaction);
                Ok(json!({"left":true,"connected":self.link.connected().await}))
            }
            "node.status" => Ok(self.node_status().await),
            "identity" => {
                let executable = std::env::current_exe()?.canonicalize()?;
                let mut identity = json!({"pid":std::process::id(),"executable":executable,"managed":self.managed,
                    "version":self.profile.connector_version()});
                if let Some(fields) = self.profile.runtime_identity().as_object() {
                    for (key, value) in fields {
                        identity[key] = value.clone();
                    }
                }
                Ok(identity)
            }
            "shutdown" => {
                if self.managed {
                    bail!("managed connector is stopped by its service manager");
                }
                let executable = std::env::current_exe()?.canonicalize()?;
                if params.get("expected_pid").and_then(Value::as_u64)
                    != Some(std::process::id() as u64)
                    || params.get("expected_executable").and_then(Value::as_str)
                        != Some(executable.to_string_lossy().as_ref())
                {
                    bail!("private connector identity changed");
                }
                Ok(json!({"pid":std::process::id(),"stopping":true}))
            }
            "adopt" => {
                let client_ref = params
                    .get("client_ref")
                    .and_then(Value::as_str)
                    .context("client_ref required")?;
                let binding = self
                    .bindings
                    .lock()
                    .await
                    .get(client_ref)
                    .cloned()
                    .context("unknown editor conversation")?;
                if !binding.detachable {
                    bail!("conversation cannot be adopted");
                }
                let previous = binding.owner.load(Ordering::Relaxed);
                if previous != 0 && previous != owner {
                    bail!("binding belongs to another façade");
                }
                binding.owner.store(owner, Ordering::Relaxed);
                Ok(
                    json!({"client_ref":binding.client_ref,"binding_id":*binding.id.lock().await,"thread":binding.thread,
                    "harness":binding.harness,"title":binding.title,
                    "inbound":binding.state.lock().await.inbound.view(),"detachable":binding.detachable,"agent":binding.agent.to_json(),
                    "engine":binding.engine.lock().await.clone(),
                    "initial_reply_context":*binding.initial_reply_context.lock().await}),
                )
            }
            "status" => {
                let bindings: Vec<_> = self.bindings.lock().await.values().cloned().collect();
                let mut listed = Vec::with_capacity(bindings.len());
                for binding in bindings {
                    let engine = binding.engine.lock().await.clone();
                    let working = *binding.working.lock().await;
                    let mut item = json!({"client_ref":binding.client_ref,"binding_id":*binding.id.lock().await,
                        "harness":binding.harness,"thread":binding.thread,"title":binding.title,
                        "inbound":binding.state.lock().await.inbound.view(),
                        "engine":engine,"working":working,
                        "initial_reply_context":*binding.initial_reply_context.lock().await});
                    if params.get("deliveries") == Some(&json!(true)) {
                        // Each recent delivery, and whether the agent has it since, from the record.
                        let awaiting = self.awaiting_receipt(&binding.thread).await;
                        item["deliveries"] = binding
                            .deliveries
                            .lock()
                            .await
                            .iter()
                            .map(|delivery| {
                                let id = delivery["message_id"].as_str().unwrap_or("");
                                let mut delivery = delivery.clone();
                                delivery["read"] = json!(
                                    delivery["result"] == "accepted"
                                        && !awaiting.iter().any(|waiting| waiting == id)
                                );
                                delivery
                            })
                            .collect();
                        item["awaiting_read"] = json!(awaiting.len());
                    }
                    listed.push(item);
                }
                let closed = self.closed_by_client.lock().await.clone();
                let closed_by_client = closed.keys().cloned().collect::<Vec<_>>();
                let resume_block_reason = self.resume_block_reason();
                Ok(
                    json!({"version":self.profile.connector_version(),"connected":self.link.connected().await,
                    "core":self.link.core().await,
                    "bindings":listed,
                    "closed_by_client":closed_by_client,"closed_reasons":closed,
                    "resume_blocked":self.resume_blocked.load(Ordering::Acquire),
                    "resume_block_reason":resume_block_reason}),
                )
            }
            "pair_device" => {
                // A connector started a moment ago (on demand, or with its core) links within seconds: the code
                // waits for that link, up to a bound, instead of failing while it comes.
                let deadline = Instant::now() + PAIR_DEVICE_LINK_WAIT;
                if !self.link.connected().await {
                    let layout = crate::service::layout::Layout::from_profile(&self.profile);
                    if let Some(refusal) = crate::service::launcher::core_refusal(&layout).await {
                        return Err(refusal.into());
                    }
                }
                while !self.link.connected().await {
                    if Instant::now() >= deadline {
                        bail!("this machine's core did not come up in time; ask again in a moment");
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                self.link
                    .request("device.pairing_code", json!({}), Duration::from_secs(10))
                    .await
            }
            _ => bail!("unknown IPC method"),
        }
    }

    async fn find_owned(&self, owner: u64, params: &Value) -> Result<Arc<Binding>> {
        let candidates: Vec<_> = self.bindings.lock().await.values().cloned().collect();
        for b in candidates {
            let id = b.id.lock().await.clone();
            if (params.get("client_ref").and_then(Value::as_str) == Some(b.client_ref.as_str())
                || params.get("binding_id").and_then(Value::as_str) == Some(id.as_str()))
                && b.owner.load(Ordering::Relaxed) == owner
            {
                return Ok(b);
            }
        }
        bail!("unknown binding")
    }

    async fn find_for_turn(
        &self,
        owner: u64,
        params: &Value,
        session_id: &str,
        revision: i64,
    ) -> Result<(Arc<Binding>, bool)> {
        let allowed = params
            .get("client_refs")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<std::collections::HashSet<_>>()
            })
            .unwrap_or_default();
        let adopt_orphans = params.get("adopt_orphans") == Some(&json!(true));
        let key = format!("{session_id}:{revision}");
        let candidates: Vec<_> = self.bindings.lock().await.values().cloned().collect();
        let mut matches = Vec::new();
        for binding in candidates {
            let previous = binding.owner.load(Ordering::Relaxed);
            let is_allowed = allowed.contains(&binding.client_ref)
                || (adopt_orphans && previous == 0 && binding.detachable);
            if !is_allowed
                || binding
                    .turns
                    .lock()
                    .await
                    .get(&key)
                    .is_none_or(|(_, epoch)| {
                        Some(*epoch) != params.get("epoch").and_then(Value::as_u64)
                    })
            {
                continue;
            }
            if previous == owner {
                matches.push((binding, false));
            } else if previous == 0 && adopt_orphans && binding.detachable {
                matches.push((binding, true));
            }
        }
        if matches.len() == 1 {
            let (binding, adopted) = matches.pop().unwrap();
            if adopted {
                binding.owner.store(owner, Ordering::Relaxed);
            }
            return Ok((binding, adopted));
        }
        if matches.is_empty() {
            bail!("no joined conversation received this voice message");
        }
        let titles = matches
            .iter()
            .map(|(binding, _)| format!("\"{}\"", binding.title))
            .collect::<Vec<_>>()
            .join(", ");
        bail!("AMBIGUOUS: several conversations received this voice message ({titles}); pass the session_id and revision from its header")
    }

    /// That the agent now has `message_id`: told to the core at once. Not retried: the core asks again (`input.received`)
    /// until it knows, and the record answers.
    async fn report_read(&self, binding: &Binding, message_id: &str) {
        let (session_id, revision) = binding
            .input_context
            .lock()
            .await
            .get(message_id)
            .cloned()
            .unzip();
        let _ = self
            .link
            .notify(
                "input.read",
                json!({"binding_id":*binding.id.lock().await,"message_id":message_id,
            "session_id":session_id,"revision":revision,"turn_id":Value::Null}),
            )
            .await;
    }

    async fn serve_client(self: Arc<Self>, stream: Stream) -> Result<()> {
        let owner = self.owner_serial.fetch_add(1, Ordering::Relaxed);
        let mut active = false;
        let mut shutdown = self.shutdown.subscribe();
        let mut connection = frame::connection(stream);
        let result = async {
            loop {
                let request = tokio::select! {
                    _ = shutdown.changed() => break,
                    request = frame::read(&mut connection) => request?,
                };
                let Some(request) = request else {
                    break;
                };
                let id = request.get("id").cloned().unwrap_or(Value::Null);
                let method = request.get("method").and_then(Value::as_str).unwrap_or("");
                let params = request.get("params").cloned().unwrap_or(json!({}));
                if !matches!(method, "status" | "node.status" | "identity") {
                    self.activity_generation.fetch_add(1, Ordering::Relaxed);
                    if !active {
                        self.client_count.fetch_add(1, Ordering::Relaxed);
                        active = true;
                    }
                }
                let answer = answer(id, self.command(owner, method, params).await);
                let shutdown_after_reply =
                    method == "shutdown" && answer.get("ok") == Some(&json!(true));
                frame::write(&mut connection, &answer).await?;
                if shutdown_after_reply {
                    self.shutdown.send_replace(true);
                    break;
                }
            }
            Ok(())
        }
        .await;
        let owned: Vec<_> = self
            .bindings
            .lock()
            .await
            .values()
            .filter(|b| b.owner.load(Ordering::Relaxed) == owner)
            .cloned()
            .collect();
        for b in owned {
            if b.detachable {
                b.owner.store(0, Ordering::Relaxed);
                continue;
            }
            self.bindings.lock().await.remove(&b.client_ref);
            let _ = self
                .link
                .notify(
                    "binding.unregister",
                    json!({"binding_id":*b.id.lock().await}),
                )
                .await;
        }
        if active {
            self.client_count.fetch_sub(1, Ordering::Relaxed);
        }
        result
    }

    async fn is_idle(&self) -> bool {
        self.client_count.load(Ordering::Relaxed) == 0 && self.bindings.lock().await.is_empty()
    }
}

/// What the core is told of a conversation's way in, as it reads it: `capabilities` (`deliver` unsupported when there is no
/// route in, `working` unsupported when the harness cannot tell) and `inbound` (refused only when the agent's own
/// settings hold messages back; any other limit passes, and a message is refused when it is offered).
fn declaration(state: &State) -> (Value, Value) {
    let supported = |yes: bool| if yes { "supported" } else { "unsupported" };
    let capabilities = json!({
        "deliver":supported(!matches!(state.inbound, Inbound::Blocked(Limit::NoRoute(_)))),
        "working":supported(state.status != Status::Unknown),
    });
    let inbound = match &state.inbound {
        Inbound::Blocked(Limit::InboundBlockedByPermissions { .. }) => state.inbound.view(),
        _ => json!({"ok":true}),
    };
    (capabilities, inbound)
}

/// Adds to a join answer how a message reaches the conversation, for its façade: `inbound` as it is now, and `delivery`,
/// by itself (`push`) or only through get_messages when there is no route in.
fn joined_way_in(answer: &mut Value, inbound: &Inbound) {
    answer["inbound"] = inbound.view();
    answer["delivery"] = json!(match inbound {
        Inbound::Blocked(Limit::NoRoute(_)) => "get_messages",
        _ => "push",
    });
}

/// The answer to one command on the socket. A failure says `error`, its English text, as every version reads it; a
/// keyed one (a [`crate::messages::Keyed`] or a [`crate::service::Failure`]) also says `keyed_error`,
/// `{key, params, message}`, for a client that acts or translates by key.
fn answer(id: Value, outcome: Result<Value>) -> Value {
    match outcome {
        Ok(result) => json!({"id":id,"ok":true,"result":result}),
        Err(error) => {
            let mut answer = json!({"id":id,"ok":false,"error":error.to_string()});
            let keyed = match error.downcast_ref::<crate::service::Failure>() {
                Some(failure) => Some(failure.value()),
                None => error
                    .downcast_ref::<crate::messages::Keyed>()
                    .map(crate::messages::Keyed::value),
            };
            if let Some(keyed) = keyed {
                answer["keyed_error"] = keyed["error"].clone();
            }
            answer
        }
    }
}

#[cfg(test)]
mod tests {
    use super::run::prune_binding_order;
    use super::*;
    use super::{answer, Daemon};
    use crate::adapters::Conversation;
    use anyhow::Result;
    use serde_json::json;
    use serde_json::Value;
    use std::collections::HashMap;
    use tokio::sync::oneshot;

    #[test]
    fn a_keyed_failure_keeps_its_text_and_adds_its_key() {
        let refusal = crate::service::launcher::core_needs_service();
        let message = refusal.message();
        let refused = answer(json!(7), Err(refusal.into()));
        assert_eq!(refused["ok"], false);
        assert_eq!(refused["error"], message, "the text every version reads");
        assert_eq!(
            refused["keyed_error"],
            json!({"key": "service.core-needs-service", "params": {}, "message": message})
        );
        let keyed = crate::messages::Keyed::new("pair-device.failed", json!({"detail": "x"}));
        let refused = answer(json!(8), Err(keyed.clone().into()));
        assert_eq!(refused["keyed_error"]["key"], "pair-device.failed");
        assert_eq!(refused["keyed_error"]["params"], json!({"detail": "x"}));
        assert_eq!(refused["error"], keyed.message());
        let plain = answer(json!(9), Err(anyhow::anyhow!("unknown IPC method")));
        assert_eq!(
            plain,
            json!({"id": 9, "ok": false, "error": "unknown IPC method"})
        );
        assert_eq!(
            answer(json!(1), Ok(json!({"a": 1}))),
            json!({"id": 1, "ok": true, "result": {"a": 1}})
        );
    }

    #[test]
    fn completed_binding_tails_do_not_accumulate() {
        let mut order = HashMap::new();
        for index in 0..100 {
            let (done, tail) = oneshot::channel();
            order.insert(index.to_string(), tail);
            done.send(()).unwrap();
        }
        let (_pending, tail) = oneshot::channel();
        order.insert("active".into(), tail);
        prune_binding_order(&mut order);
        assert_eq!(order.len(), 1);
        assert!(order.contains_key("active"));
    }

    #[test]
    fn outbox_requires_matching_admission_or_audited_terminal_result() {
        let speech = json!({"event_id":"e","utterance_id":"u"});
        assert!(Daemon::ack_allows_removal(
            &speech,
            &json!({"event_id":"e","utterance_id":"u","text_saved":true})
        ));
        assert!(Daemon::ack_allows_removal(
            &speech,
            &json!({"event_id":"e","utterance_id":"u","status":"rejected","terminal":true,"reason_code":"application_refusal"})
        ));
        for reply in [
            json!(null),
            json!({}),
            json!({"event_id":"e","utterance_id":"u","status":"unknown_binding"}),
            json!({"event_id":"e","utterance_id":"wrong","text_saved":true}),
            json!({"event_id":"e","utterance_id":"u","status":"rejected"}),
        ] {
            assert!(!Daemon::ack_allows_removal(&speech, &reply));
        }
    }

    /// A harness the daemon has never heard of, registered by its id: the daemon delivers to, follows and lists its
    /// conversation only through the trait; an id no harness claims is served by the fallback, with no way in.
    #[tokio::test]
    async fn a_harness_registered_by_id_is_driven_through_its_trait() {
        use crate::adapters::{CallContext, Harness, Model, Registry};
        use crate::profile::Profile;
        use futures_util::future::BoxFuture;
        use std::sync::{Arc, Mutex};

        struct Fake {
            calls: Arc<Mutex<Vec<String>>>,
        }

        impl Harness for Fake {
            fn id(&self) -> &'static str {
                "fake"
            }
            fn registration(&self) -> &dyn crate::adapters::registration::Registration {
                &crate::adapters::Unregistered
            }
            fn identify(&self, _: &CallContext) -> Result<Option<Conversation>> {
                Ok(None)
            }
            fn send<'a>(
                &'a self,
                conversation: &'a Conversation,
                frame: &'a Value,
            ) -> BoxFuture<'a, Sent> {
                Box::pin(async move {
                    self.calls
                        .lock()
                        .unwrap()
                        .push(format!("send {} {}", conversation.id, frame["text"]));
                    Sent::Accepted
                })
            }
            fn state<'a>(&'a self, _: &'a Conversation) -> BoxFuture<'a, State> {
                Box::pin(async {
                    State {
                        status: Status::Working,
                        model: Some(Model {
                            name: "fake-1".into(),
                            effort: Some("low".into()),
                            thinking: None,
                        }),
                        inbound: Inbound::Open,
                    }
                })
            }
            fn received<'a>(
                &'a self,
                _: &'a Conversation,
                _: &'a [String],
            ) -> BoxFuture<'a, Vec<String>> {
                Box::pin(async { Vec::new() })
            }
        }

        let scratch = crate::test_support::temp_dir("fake-harness");
        let profile = Profile::for_test(scratch.path(), &crate::adapters::Registry::homes());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let registry = Registry::new(vec![Arc::new(Fake {
            calls: calls.clone(),
        })]);
        let (replay_tx, _replay_rx) = tokio::sync::mpsc::channel(1);
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let daemon = Daemon::new(
            profile,
            crate::link::Link::new(),
            replay_tx,
            registry,
            false,
            shutdown,
        )
        .unwrap();

        let joined = daemon
            .command(
                1,
                "register",
                json!({"client_ref":"fake-1","thread":"t-1","title":"Fake","harness":"fake"}),
            )
            .await
            .unwrap();
        assert_eq!(joined["delivery"], "push");
        assert_eq!(joined["inbound"]["ok"], true);

        let delivered = daemon
            .deliver(
                &json!({"binding_id":joined["binding_id"],"message_id":"m-1","session_id":"s",
                "revision":1,"text":"hola"}),
            )
            .await;
        assert_eq!(delivered["status"], "accepted");

        let mut listed = Value::Null;
        for _ in 0..100 {
            listed = daemon.command(1, "status", json!({})).await.unwrap()["bindings"][0].clone();
            if listed["working"] == true {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(listed["harness"], "fake");
        assert_eq!(listed["working"], true);
        assert_eq!(listed["engine"]["model"], "fake-1");
        assert_eq!(listed["inbound"]["ok"], true);

        daemon
            .command(1, "unregister", json!({"client_ref":"fake-1"}))
            .await
            .unwrap();
        assert_eq!(*calls.lock().unwrap(), ["send t-1 \"hola\""]);

        let unclaimed = daemon
            .command(
                1,
                "register",
                json!({"client_ref":"other-1","thread":"o-1","harness":"other"}),
            )
            .await
            .unwrap();
        assert_eq!(unclaimed["delivery"], "get_messages");
        let refused = daemon
            .deliver(
                &json!({"binding_id":unclaimed["binding_id"],"message_id":"m-2","session_id":"s",
                "revision":1,"text":"hola"}),
            )
            .await;
        assert_eq!(refused["status"], "unsupported", "{refused}");
    }

    /// A harness whose answers are scripted: what each handing over of a message answers, in turn, and which
    /// messages the agent has.
    struct Scripted {
        handed: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        answers: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<&'static str>>>,
        read: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl crate::adapters::Harness for Scripted {
        fn id(&self) -> &'static str {
            "scripted"
        }
        fn registration(&self) -> &dyn crate::adapters::registration::Registration {
            &crate::adapters::Unregistered
        }
        fn identify(&self, _: &crate::adapters::CallContext) -> Result<Option<Conversation>> {
            Ok(None)
        }
        fn send<'a>(
            &'a self,
            _: &'a Conversation,
            frame: &'a Value,
        ) -> futures_util::future::BoxFuture<'a, Sent> {
            Box::pin(async move {
                let answer = self
                    .answers
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or("accepted");
                if answer == "link_down" {
                    return Sent::Refused(Limit::Failed("the harness is unreachable".into()));
                }
                self.handed
                    .lock()
                    .unwrap()
                    .push(frame["text"].as_str().unwrap_or("").to_owned());
                Sent::Accepted
            })
        }
        fn state<'a>(&'a self, _: &'a Conversation) -> futures_util::future::BoxFuture<'a, State> {
            Box::pin(async { State::unknown(Inbound::Open) })
        }
        fn received<'a>(
            &'a self,
            _: &'a Conversation,
            ids: &'a [String],
        ) -> futures_util::future::BoxFuture<'a, Vec<String>> {
            Box::pin(async move {
                let read = self.read.lock().unwrap();
                ids.iter().filter(|id| read.contains(id)).cloned().collect()
            })
        }
    }

    async fn scripted_daemon(
        scratch: &std::path::Path,
        answers: &[&'static str],
    ) -> (
        Arc<Daemon>,
        String,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let profile =
            crate::profile::Profile::for_test(scratch, &crate::adapters::Registry::homes());
        let handed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let read = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let registry = Registry::new(vec![Arc::new(Scripted {
            handed: handed.clone(),
            answers: std::sync::Arc::new(std::sync::Mutex::new(answers.iter().copied().collect())),
            read: read.clone(),
        })]);
        let (replay_tx, _rx) = tokio::sync::mpsc::channel(1);
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let daemon = Daemon::new(
            profile,
            crate::link::Link::new(),
            replay_tx,
            registry,
            false,
            shutdown,
        )
        .unwrap();
        let joined = daemon
            .command(
                1,
                "register",
                json!({"client_ref":"s-1","thread":"t-1","title":"S","harness":"scripted"}),
            )
            .await
            .unwrap();
        (
            daemon,
            joined["binding_id"].as_str().unwrap().to_owned(),
            handed,
            read,
        )
    }

    fn frame(binding: &str, id: &str, text: &str) -> Value {
        json!({"binding_id":binding,"message_id":id,"session_id":"s","revision":1,"text":text})
    }

    /// The garage: the harness is unreachable for a while with several messages queued, and the core, which offers
    /// the first one until it is taken and only then the next, gets each handed over once and in order; the
    /// connector restarting in the middle does not hand over what the harness already has.
    #[tokio::test]
    async fn the_garage_messages_reach_the_harness_once_and_in_order_across_a_restart() {
        let scratch = crate::test_support::temp_dir("garage");
        let (daemon, binding, handed, _) = scripted_daemon(
            scratch.path(),
            &["link_down", "link_down", "link_down", "accepted"],
        )
        .await;
        let mut offered = 0;
        // The core's side: the first message is offered again until it is taken; the later ones wait behind it.
        loop {
            offered += 1;
            let answer = daemon.deliver(&frame(&binding, "m-1", "one")).await;
            if answer["status"] == "accepted" {
                break;
            }
            assert_eq!(
                answer["status"], "failed",
                "an unreachable harness is a failure the core retries"
            );
            assert!(
                handed.lock().unwrap().is_empty(),
                "nothing later went ahead of it"
            );
        }
        assert_eq!(offered, 4);
        assert_eq!(
            daemon.deliver(&frame(&binding, "m-2", "two")).await["status"],
            "accepted"
        );
        // The core, restarted, holds all three again and offers them in order from the start.
        drop(daemon);
        let (daemon, binding, handed_after, _) = scripted_daemon(scratch.path(), &[]).await;
        for (id, text) in [("m-1", "one"), ("m-2", "two"), ("m-3", "three")] {
            assert_eq!(
                daemon.deliver(&frame(&binding, id, text)).await["status"],
                "accepted"
            );
        }
        assert_eq!(
            *handed.lock().unwrap(),
            ["one", "two"],
            "each was handed over once"
        );
        assert_eq!(
            *handed_after.lock().unwrap(),
            ["three"],
            "what the harness had is not typed in again after a restart"
        );
    }

    /// The core asks which messages the agent has: answered from the per-id record, which survives a restart, without
    /// handing anything to the agent; an id never handed over is named unknown. A redelivered id is never typed in again
    /// (sidevoice/sidevoice-connector#343).
    #[tokio::test]
    async fn which_messages_the_agent_has_is_answered_from_the_record_across_a_restart() {
        let scratch = crate::test_support::temp_dir("received-record");
        let (daemon, binding, handed, read) = scripted_daemon(scratch.path(), &[]).await;
        for (id, text) in [("m-1", "one"), ("m-2", "two")] {
            let answer = daemon.deliver(&frame(&binding, id, text)).await;
            assert_eq!(answer["status"], "accepted");
            assert_eq!(
                answer["received"], false,
                "taken, not yet seen in the conversation"
            );
        }
        read.lock().unwrap().push("m-1".into());
        let asked =
            json!({"binding_id":binding,"message_ids":["m-1","m-2","m-never"],"added_later":true});
        let answer = daemon.handle_incoming("input.received", &asked).await;
        assert_eq!(answer["received"], json!(["m-1"]));
        assert_eq!(answer["unknown"], json!(["m-never"]));
        // The agent's harness forgets (a restart of the connector): the record still knows.
        drop(daemon);
        let (daemon, binding, handed_after, _) = scripted_daemon(scratch.path(), &[]).await;
        let asked = json!({"binding_id":binding,"message_ids":["m-1","m-2"]});
        let answer = daemon.handle_incoming("input.received", &asked).await;
        assert_eq!(answer["received"], json!(["m-1"]));
        assert_eq!(answer["unknown"], json!([]));
        for (id, text) in [("m-1", "one"), ("m-2", "two")] {
            let again = daemon.deliver(&frame(&binding, id, text)).await;
            assert_eq!(again["status"], "accepted");
            assert_eq!(again["received"], id == "m-1");
        }
        assert_eq!(*handed.lock().unwrap(), ["one", "two"]);
        assert!(
            handed_after.lock().unwrap().is_empty(),
            "nothing typed in twice"
        );
        // An id the record could not keep is refused before anything reaches the agent.
        let long = "m".repeat(record::ID_MAX + 1);
        assert_eq!(
            daemon.deliver(&frame(&binding, &long, "x")).await["status"],
            "failed"
        );
        assert!(handed_after.lock().unwrap().is_empty());
    }

    /// A record that says anything but "handed over" is not believed: a message it names `failed` is handed over.
    #[tokio::test]
    async fn only_a_handed_over_answer_in_the_record_is_believed() {
        let scratch = crate::test_support::temp_dir("delivered-record");
        std::fs::create_dir_all(scratch.path().join("data")).ok();
        let (first, _, _, _) = scripted_daemon(scratch.path(), &[]).await;
        let path = first.profile.data.join(DELIVERED_FILE);
        drop(first);
        Files::write_json(
            &path,
            &json!({"t-1":{"used":1,"ids":[["m-1","failed"],["m-2","accepted"],["","accepted"]]}}),
        )
        .unwrap();
        let (daemon, binding, handed, _) = scripted_daemon(scratch.path(), &[]).await;
        assert_eq!(
            daemon.deliver(&frame(&binding, "m-1", "one")).await["status"],
            "accepted"
        );
        assert_eq!(
            daemon.deliver(&frame(&binding, "m-2", "two")).await["status"],
            "accepted"
        );
        assert_eq!(
            *handed.lock().unwrap(),
            ["one"],
            "m-1 was handed over; m-2 was already"
        );
    }
}
