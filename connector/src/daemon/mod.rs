use crate::adapters::{Conversation, Registry, Room};
use crate::agents::HostAgents;
use crate::link::{Incoming, Link};
use crate::profile::Profile;
use crate::secure_fs::atomic_json;
use anyhow::{bail, Context, Result};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::time::Duration;
use tokio::time::Instant;

mod files;
mod run;

use files::{
    prepare_conversation_guard, read_conversation_state, read_outbox, write_conversation_guard,
    CONVERSATION_STATE_MAX_BYTES, GUARD_BLOCK_RESUME, GUARD_CLEAR, GUARD_REVOKED,
};
pub use run::run;

/// How long `pair_device` waits for the link to the core (the code itself has 10 s more).
const PAIR_DEVICE_LINK_WAIT: Duration = Duration::from_secs(15);
const RESUME_REASON_NONE: u8 = 0;
const RESUME_REASON_HISTORY: u8 = 1;
const RESUME_REASON_STORAGE: u8 = 2;

pub struct Daemon {
    profile: Profile,
    link: Arc<Link>,
    bindings: Mutex<HashMap<String, Arc<Conversation>>>,
    outbox: Mutex<Vec<Value>>,
    replay_tx: mpsc::Sender<()>,
    host_agents: Arc<HostAgents>,
    registry: Arc<Registry>,
    owner_serial: AtomicU64,
    client_count: AtomicU64,
    activity_generation: AtomicU64,
    managed: bool,
    shutdown: watch::Sender<bool>,
    rendezvous: Mutex<Option<Value>>,
    refusal: Mutex<Option<String>>,
    closed_by_room: Mutex<HashMap<String, String>>,
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
        let closed_by_room = state
            .get("closed_by_room")
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
        let mut refusal = state
            .get("refused")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if guard_state == GUARD_REVOKED {
            refusal = Some("connector_revoked".to_owned());
        }
        if resume_blocked && refusal.as_ref().is_some_and(|reason| reason.len() > 4096) {
            refusal = Some("connector_revoked".to_owned());
        }
        let daemon = Arc::new(Self {
            profile,
            link,
            bindings: Mutex::new(HashMap::new()),
            outbox: Mutex::new(outbox),
            replay_tx,
            host_agents,
            registry: Arc::new(registry),
            owner_serial: AtomicU64::new(1),
            client_count: AtomicU64::new(0),
            activity_generation: AtomicU64::new(0),
            managed,
            shutdown,
            rendezvous: Mutex::new(None),
            refusal: Mutex::new(refusal.clone()),
            closed_by_room: Mutex::new(closed_by_room),
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
        let room: Arc<dyn Room> = daemon.clone();
        for harness in daemon.registry.all() {
            harness.attach(&daemon.profile, Arc::downgrade(&room))?;
        }
        Ok(daemon)
    }

    async fn register_core(&self, binding: &Arc<Conversation>) -> Result<Value> {
        let mut current = binding.id.lock().await;
        let engine = binding.engine.lock().await.clone().unwrap_or(Value::Null);
        let mut frame = json!({"client_ref":binding.client_ref,"harness":binding.harness,"thread":binding.thread,"title":binding.title,"delivery":binding.delivery,
            "inbound":binding.inbound,"capabilities":binding.capabilities,
            "experimental":binding.experimental,"route":binding.route,"engine":engine,"focus":false});
        if !current.starts_with("local-") {
            frame["binding_id"] = json!(*current);
        }
        let reply = self
            .link
            .request("binding.register", frame, Duration::from_secs(10))
            .await?;
        if let Some(error) = reply.get("error") {
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
                if item.get("binding_id").and_then(Value::as_str) == Some(current.as_str())
                    || item.get("client_ref").and_then(Value::as_str)
                        == Some(binding.client_ref.as_str())
                {
                    item["binding_id"] = json!(id);
                    changed = true;
                }
            }
            if changed {
                if let Err(error) = atomic_json(&self.outbox_path(), &*outbox) {
                    *outbox = old;
                    return Err(error);
                }
            }
        }
        *current = id.to_owned();
        Ok(reply)
    }

    async fn on_welcome(self: &Arc<Self>) {
        let bindings: Vec<_> = self.bindings.lock().await.values().cloned().collect();
        for binding in bindings {
            if let Err(error) = self.register_core(&binding).await {
                crate::logfile::log(&format!("binding replay: {error}"));
                continue;
            }
            let working = *binding.working.lock().await;
            if let Some(working) = working {
                self.report_working(&binding, working).await;
            }
        }
        self.request_replay();
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

    fn arm_conversation_guard(&self, revoked: bool) -> Result<()> {
        let state = if revoked {
            GUARD_REVOKED
        } else {
            GUARD_BLOCK_RESUME
        };
        let mut guard = self
            .conversation_guard
            .lock()
            .map_err(|_| anyhow::anyhow!("conversation guard lock poisoned"))?;
        write_conversation_guard(&mut guard, state)
    }

    async fn check_registration_allowed(&self, client_ref: &str, resume: bool) -> Result<()> {
        if let Some(reason) = self.refusal.lock().await.clone() {
            if resume {
                bail!("CLOSED_BY_ROOM:connector_revoked:{client_ref}");
            }
            bail!("the room revoked this connector pairing: {reason}");
        }
        if resume {
            let closed = self.closed_by_room.lock().await;
            if self.resume_blocked.load(Ordering::Acquire) {
                bail!("CLOSED_BY_ROOM:conversation_history_limit:{client_ref}");
            }
            if let Some(reason) = closed.get(client_ref) {
                bail!("CLOSED_BY_ROOM:{reason}:{client_ref}");
            }
        }
        Ok(())
    }

    fn mark_state_write_failed(&self, revoked: bool) {
        self.resume_blocked.store(true, Ordering::Release);
        self.resume_block_reason
            .store(RESUME_REASON_STORAGE, Ordering::Release);
        let guard = if revoked {
            GUARD_REVOKED
        } else {
            GUARD_BLOCK_RESUME
        };
        let result = self
            .conversation_guard
            .lock()
            .map_err(|_| anyhow::anyhow!("conversation guard lock poisoned"))
            .and_then(|mut file| write_conversation_guard(&mut file, guard));
        if let Err(error) = result {
            crate::logfile::log(&format!("emergency conversation guard: {error}"));
            let _ = self.shutdown.send(true);
        }
    }

    async fn persist_conversation_state(&self) {
        let mut refused = self.refusal.lock().await.clone();
        let mut closed = self.closed_by_room.lock().await;
        let mut state = json!({"refused":refused.clone(),"closed_by_room":closed.clone(),
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
                if refused.as_ref().is_some_and(|reason| reason.len() > 4096) {
                    refused = Some("connector_revoked".to_owned());
                    *self.refusal.lock().await = refused.clone();
                }
                state = json!({"refused":refused.clone(),
                    "closed_by_room":closed.clone(),"resume_blocked":true,
                    "resume_block_reason":self.resume_block_reason()});
            }
            Err(error) => {
                crate::logfile::log(&format!("conversation state: {error}"));
                self.mark_state_write_failed(refused.is_some());
                return;
            }
            _ => {}
        }
        if serde_json::to_vec(&state)
            .map_or(true, |bytes| bytes.len() > CONVERSATION_STATE_MAX_BYTES)
        {
            closed.clear();
            self.mark_state_write_failed(refused.is_some());
            state = json!({"refused":refused.clone(),"closed_by_room":{},
                "resume_blocked":true,"resume_block_reason":self.resume_block_reason()});
            if serde_json::to_vec(&state)
                .map_or(true, |bytes| bytes.len() > CONVERSATION_STATE_MAX_BYTES)
            {
                crate::logfile::log("compact conversation state exceeds its size limit");
                return;
            }
        }
        if let Err(error) = atomic_json(&self.profile.data.join("conversation-state.json"), &state)
        {
            crate::logfile::log(&format!("conversation state: {error}"));
            self.mark_state_write_failed(refused.is_some());
        } else if let Err(error) = self
            .conversation_guard
            .lock()
            .map_err(|_| anyhow::anyhow!("conversation guard lock poisoned"))
            .and_then(|mut file| write_conversation_guard(&mut file, GUARD_CLEAR))
        {
            crate::logfile::log(&format!("clear conversation guard: {error}"));
            self.mark_state_write_failed(refused.is_some());
        }
    }

    async fn send_working_event(&self, binding: &Conversation, mut event: Value) {
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

    async fn report_working_event(&self, binding: &Conversation, event: Value) {
        if let Some(working) = event.get("working").and_then(Value::as_bool) {
            *binding.working.lock().await = Some(working);
            self.send_working_event(binding, event).await;
        }
    }

    async fn report_working(&self, binding: &Conversation, working: bool) {
        self.report_working_event(binding, json!({"working":working,"turn_id":Value::Null}))
            .await;
    }

    async fn report_engine(&self, binding: &Conversation, engine: Value) {
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
        outbox.push(speech);
        if serde_json::to_vec(&*outbox)?.len() > 8 << 20 {
            outbox.pop();
            bail!("outbox full");
        }
        if let Err(error) = atomic_json(&self.outbox_path(), &*outbox) {
            outbox.pop();
            return Err(error);
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
                    && reply.get("terminal") == Some(&json!(true))
                    && reply.get("reason_code") == Some(&json!("application_refusal"))))
    }

    async fn publish_one(&self, speech: &Value) -> Result<Option<Value>> {
        if !self.link.connected().await {
            return Ok(None);
        }
        let mut published = self
            .outbox
            .lock()
            .await
            .iter()
            .find(|item| item.get("event_id") == speech.get("event_id"))
            .cloned()
            .unwrap_or_else(|| speech.clone());
        let result = self
            .link
            .request("speech.publish", published.clone(), Duration::from_secs(15))
            .await;
        let mut reply = match result {
            Ok(v) => v,
            Err(error) => {
                crate::logfile::log(&format!("speech retry retained: {error}"));
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
                        .outbox
                        .lock()
                        .await
                        .iter()
                        .find(|item| item.get("event_id") == speech.get("event_id"))
                        .cloned()
                        .context("speech disappeared during binding repair")?;
                    reply = match self
                        .link
                        .request("speech.publish", published.clone(), Duration::from_secs(15))
                        .await
                    {
                        Ok(value) => value,
                        Err(error) => {
                            crate::logfile::log(&format!("speech retry retained: {error}"));
                            return Ok(None);
                        }
                    };
                } else {
                    return Ok(None);
                }
            } else {
                return Ok(None);
            }
        }
        if Self::ack_allows_removal(&published, &reply) {
            let event_id = speech.get("event_id");
            let mut outbox = self.outbox.lock().await;
            let before = outbox.clone();
            outbox.retain(|item| item.get("event_id") != event_id);
            if let Err(error) = atomic_json(&self.outbox_path(), &*outbox) {
                *outbox = before;
                return Err(error);
            }
        }
        if Self::ack_allows_removal(&published, &reply) {
            Ok(Some(reply))
        } else {
            Ok(None)
        }
    }

    async fn flush_outbox(&self) {
        let items = self.outbox.lock().await.clone();
        for speech in items {
            let _ = self.publish_one(&speech).await;
        }
    }

    async fn binding_for_id(&self, id: &str) -> Option<Arc<Conversation>> {
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
            "binding.close" => {
                let _state_transaction = self.conversation_state_serial.lock().await;
                let id = params
                    .get("binding_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if let Some(binding) = self.binding_for_id(id).await {
                    if let Err(error) = self.arm_conversation_guard(false) {
                        crate::logfile::log(&format!("arm conversation guard: {error}"));
                        self.mark_state_write_failed(false);
                        let _ = self.shutdown.send(true);
                    }
                    binding.stopped.store(true, Ordering::Relaxed);
                    self.bindings.lock().await.remove(&binding.client_ref);
                    let reason = params
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("closed_from_room")
                        .to_owned();
                    {
                        let mut closed = self.closed_by_room.lock().await;
                        closed.insert(binding.client_ref.clone(), reason.clone());
                        *self.latest_closed.lock().await =
                            Some((binding.client_ref.clone(), reason));
                    }
                    self.persist_conversation_state().await;
                    self.registry.get(&binding.harness).close(&binding).await;
                    drop(_state_transaction);
                }
                json!({})
            }
            "node.rendezvous" => {
                let _state_transaction = self.conversation_state_serial.lock().await;
                let previous_refusal = self.refusal.lock().await.clone();
                *self.rendezvous.lock().await = Some(params.clone());
                let refusal = params
                    .get("refused")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned);
                if let Some(reason) = refusal {
                    if let Err(error) = self.arm_conversation_guard(true) {
                        crate::logfile::log(&format!("arm revocation guard: {error}"));
                        self.mark_state_write_failed(true);
                        let _ = self.shutdown.send(true);
                    }
                    *self.refusal.lock().await = Some(reason);
                    let bindings: Vec<_> = self.bindings.lock().await.values().cloned().collect();
                    for binding in &bindings {
                        binding.stopped.store(true, Ordering::Relaxed);
                        self.bindings.lock().await.remove(&binding.client_ref);
                        {
                            let mut closed = self.closed_by_room.lock().await;
                            closed.insert(binding.client_ref.clone(), "connector_revoked".into());
                            *self.latest_closed.lock().await =
                                Some((binding.client_ref.clone(), "connector_revoked".into()));
                        }
                    }
                    self.persist_conversation_state().await;
                    for binding in bindings {
                        self.registry.get(&binding.harness).close(&binding).await;
                    }
                    drop(_state_transaction);
                } else if previous_refusal.is_some() {
                    *self.refusal.lock().await = None;
                    self.persist_conversation_state().await;
                }
                json!({})
            }
            "agents.list" | "agents.connect" | "agents.disconnect" | "agents.dismiss" => {
                self.host_agents.handle(method, params.clone()).await
            }
            "pair.request" => {
                let room = params.get("room").and_then(Value::as_str).unwrap_or("");
                let code = params.get("code").and_then(Value::as_str).unwrap_or("");
                match crate::pairing::pair(&self.profile, room, code).await {
                    Ok(paired) => {
                        json!({"ok":true,"origin":paired.origin,"connector_id":paired.connector_id})
                    }
                    Err(error) => {
                        let keyed = crate::messages::keyed(&error);
                        json!({"ok":false,"key":keyed.key,"detail":keyed.message()})
                    }
                }
            }
            "connector.error" => json!({}),
            "node.status" => self.node_status().await,
            _ => json!({"error":{"key":"connector.unknown-method"}}),
        }
    }

    async fn node_status(&self) -> Value {
        crate::service::node_status(&self.profile).await
    }

    async fn deliver(&self, frame: &Value) -> Value {
        let id = frame
            .get("binding_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let Some(binding) = self.binding_for_id(id).await else {
            return json!({"status":"unknown_binding"});
        };
        let _serial = binding.serial.lock().await;
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
        // A note from the room carries no words of the user's.
        let note = frame.get("channel").and_then(Value::as_str) == Some("note");
        if message_id.is_empty() || session_id.is_empty() || (text.is_empty() && !note) {
            return json!({"status":"failed","detail":"invalid input"});
        }
        {
            let mut pending = binding.pending.lock().await;
            if !pending.iter().any(|id| id == message_id) {
                pending.push_back(message_id.to_owned());
            }
            if pending.len() > 64 {
                pending.pop_front();
            }
        }
        {
            let mut turns = binding.turns.lock().await;
            turns.insert(format!("{session_id}:{revision}"), message_id.to_owned());
            if turns.len() > 512 {
                if let Some(oldest) = turns.keys().next().cloned() {
                    turns.remove(&oldest);
                }
            }
        }
        binding
            .input_context
            .lock()
            .await
            .insert(message_id.to_owned(), (session_id.to_owned(), revision));
        match self
            .registry
            .get(&binding.harness)
            .deliver(&binding, frame, &self.profile)
            .await
        {
            Ok(result) => result,
            Err(error) => {
                binding.pending.lock().await.retain(|id| id != message_id);
                binding
                    .turns
                    .lock()
                    .await
                    .remove(&format!("{session_id}:{revision}"));
                binding.input_context.lock().await.remove(message_id);
                json!({"status":"failed","detail":error.to_string()})
            }
        }
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
                let delivery = params
                    .get("delivery")
                    .cloned()
                    .unwrap_or_else(|| json!({"kind":"none"}));
                let harness_adapter = self.registry.get(&harness).clone();
                if !harness_adapter.accepts(&delivery, &thread) {
                    bail!("harness identity does not match its delivery route");
                }
                let capabilities = params
                    .get("capabilities")
                    .cloned()
                    .unwrap_or_else(|| harness_adapter.capabilities());
                let experimental = params
                    .get("experimental")
                    .cloned()
                    .unwrap_or_else(|| json!([]));
                let inbound = params.get("inbound").cloned().unwrap_or(Value::Null);
                let route = params
                    .get("route")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let detachable = params.get("detachable") == Some(&json!(true));
                let _state_transaction = self.conversation_state_serial.lock().await;
                self.check_registration_allowed(&client_ref, resume).await?;
                let existing = { self.bindings.lock().await.get(&client_ref).cloned() };
                if let Some(existing) = existing {
                    if existing.owner.load(Ordering::Relaxed) != owner {
                        bail!("binding belongs to another façade");
                    }
                    let binding_id = existing.id.lock().await.clone();
                    drop(_state_transaction);
                    return Ok(
                        json!({"binding_id":binding_id,"thread":thread,"connected":self.link.connected().await,"prepared":null}),
                    );
                }
                drop(_state_transaction);
                let binding = Arc::new(Conversation {
                    client_ref: client_ref.clone(),
                    harness,
                    thread: thread.clone(),
                    title,
                    delivery: delivery.clone(),
                    engine: Mutex::new(
                        params
                            .get("engine")
                            .filter(|value| !value.is_null())
                            .cloned(),
                    ),
                    route,
                    capabilities,
                    experimental,
                    inbound,
                    detachable,
                    owner: AtomicU64::new(owner),
                    id: Mutex::new(format!("local-{}", uuid::Uuid::new_v4())),
                    serial: Mutex::new(()),
                    pending: Mutex::new(VecDeque::new()),
                    turns: Mutex::new(HashMap::new()),
                    input_context: Mutex::new(HashMap::new()),
                    read: Mutex::new(VecDeque::new()),
                    working: Mutex::new(None),
                    stopped: AtomicBool::new(false),
                    state: harness_adapter.conversation_state(),
                });
                let _state_transaction = self.conversation_state_serial.lock().await;
                self.check_registration_allowed(&client_ref, resume).await?;
                if let Some(existing) = self.bindings.lock().await.get(&client_ref).cloned() {
                    if existing.owner.load(Ordering::Relaxed) != owner {
                        bail!("binding belongs to another façade");
                    }
                    let binding_id = existing.id.lock().await.clone();
                    drop(_state_transaction);
                    return Ok(json!({"binding_id":binding_id,"thread":thread,
                        "connected":self.link.connected().await,"prepared":null}));
                }
                let prepared = harness_adapter.prepare(&binding).await?;
                self.bindings
                    .lock()
                    .await
                    .insert(client_ref, binding.clone());
                {
                    let mut closed = self.closed_by_room.lock().await;
                    closed.remove(&binding.client_ref);
                    let mut latest_closed = self.latest_closed.lock().await;
                    if latest_closed
                        .as_ref()
                        .is_some_and(|(client_ref, _)| client_ref == &binding.client_ref)
                    {
                        *latest_closed = None;
                    }
                }
                self.persist_conversation_state().await;
                drop(_state_transaction);
                self.clone().watch_binding(binding.clone());
                let result = self.register_core(&binding).await;
                let id = binding.id.lock().await.clone();
                if result.is_ok() {
                    self.request_replay();
                }
                Ok(
                    json!({"binding_id":id,"thread":thread,"connected":result.is_ok(),"pending":result.is_err(),"prepared":prepared}),
                )
            }
            "publish" => {
                if let Some(reason) = self.refusal.lock().await.clone() {
                    bail!("CLOSED_BY_ROOM:connector_revoked:{reason}");
                }
                if let Some(client_ref) = params.get("client_ref").and_then(Value::as_str) {
                    if let Some(reason) = self.closed_by_room.lock().await.get(client_ref).cloned()
                    {
                        bail!("CLOSED_BY_ROOM:{reason}:{client_ref}");
                    }
                }
                if let Some(refs) = params.get("client_refs").and_then(Value::as_array) {
                    for item in refs {
                        if let Some(client_ref) = item.as_str() {
                            if let Some(reason) =
                                self.closed_by_room.lock().await.get(client_ref).cloned()
                            {
                                bail!("CLOSED_BY_ROOM:{reason}:{client_ref}");
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
                let routed = params.get("client_refs").is_some()
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
                let id = binding.id.lock().await;
                let speech = json!({"event_id":params.get("event_id").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    "binding_id":*id,"client_ref":binding.client_ref,"session_id":session_id,"revision":revision,
                    "utterance_id":params.get("utterance_id").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    "text":text,"language":params.get("language")});
                self.queue(speech.clone()).await?;
                drop(id);
                let answer = self.publish_one(&speech).await?;
                let mut result = answer.unwrap_or_else(
                    || json!({"status":"queued","utterance_id":speech["utterance_id"]}),
                );
                if adopted {
                    result["adopted"] = json!({"client_ref":binding.client_ref,"binding_id":*binding.id.lock().await,
                        "harness":binding.harness,"title":binding.title,"delivery":binding.delivery,"route":binding.route,
                        "capabilities":binding.capabilities,"experimental":binding.experimental,"inbound":binding.inbound});
                }
                Ok(result)
            }
            "unregister" => {
                let _state_transaction = self.conversation_state_serial.lock().await;
                let binding = self.find_owned(owner, &params).await?;
                binding.stopped.store(true, Ordering::Relaxed);
                self.bindings.lock().await.remove(&binding.client_ref);
                self.registry.get(&binding.harness).close(&binding).await;
                drop(_state_transaction);
                let _ = self
                    .link
                    .notify(
                        "binding.unregister",
                        json!({"binding_id":*binding.id.lock().await}),
                    )
                    .await;
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
                if !binding.detachable
                    || !self.registry.get(&binding.harness).adopts(Some(client_ref))
                {
                    bail!("conversation cannot be adopted");
                }
                let previous = binding.owner.load(Ordering::Relaxed);
                if previous != 0 && previous != owner {
                    bail!("binding belongs to another façade");
                }
                binding.owner.store(owner, Ordering::Relaxed);
                Ok(
                    json!({"client_ref":binding.client_ref,"binding_id":*binding.id.lock().await,"thread":binding.thread,
                    "harness":binding.harness,"title":binding.title,"delivery":binding.delivery,"route":binding.route,
                    "capabilities":binding.capabilities,"experimental":binding.experimental,"inbound":binding.inbound,"detachable":binding.detachable,
                    "engine":binding.engine.lock().await.clone()}),
                )
            }
            "status" => {
                let bindings: Vec<_> = self.bindings.lock().await.values().cloned().collect();
                let mut listed = Vec::with_capacity(bindings.len());
                for binding in bindings {
                    let engine = binding.engine.lock().await.clone();
                    let working = *binding.working.lock().await;
                    let mut item = json!({"client_ref":binding.client_ref,"binding_id":*binding.id.lock().await,
                        "harness":binding.harness,"thread":binding.thread,"title":binding.title,"delivery":binding.delivery,
                        "capabilities":binding.capabilities,"experimental":binding.experimental,"inbound":binding.inbound,
                        "engine":engine,"working":working});
                    if let Some(fields) = self.registry.get(&binding.harness).status(&binding).await
                    {
                        for (key, value) in fields {
                            item[key] = value;
                        }
                    }
                    listed.push(item);
                }
                let closed = self.closed_by_room.lock().await.clone();
                let closed_by_room = closed.keys().cloned().collect::<Vec<_>>();
                let room_reachable = self.rendezvous.lock().await.clone();
                let refused = self.refusal.lock().await.clone();
                let resume_block_reason = self.resume_block_reason();
                Ok(
                    json!({"version":self.profile.connector_version(),"connected":self.link.connected().await,
                    "core":self.link.core().await,
                    "room_reachable":room_reachable,"bindings":listed,
                    "closed_by_room":closed_by_room,"closed_reasons":closed,"refused":refused,
                    "resume_blocked":self.resume_blocked.load(Ordering::Acquire),
                    "resume_block_reason":resume_block_reason}),
                )
            }
            "pair_device" => {
                // A connector started a moment ago (on demand, or with its core) links within seconds: the code
                // waits for that link, up to a bound, instead of failing while it comes.
                let deadline = Instant::now() + PAIR_DEVICE_LINK_WAIT;
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

    async fn find_owned(&self, owner: u64, params: &Value) -> Result<Arc<Conversation>> {
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
    ) -> Result<(Arc<Conversation>, bool)> {
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
                || (adopt_orphans
                    && previous == 0
                    && binding.detachable
                    && self.registry.get(&binding.harness).adopts(None));
            if !is_allowed || !binding.turns.lock().await.contains_key(&key) {
                continue;
            }
            if previous == owner {
                matches.push((binding, false));
            } else if previous == 0
                && adopt_orphans
                && binding.detachable
                && self.registry.get(&binding.harness).adopts(None)
            {
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

    fn watch_binding(self: Arc<Self>, binding: Arc<Conversation>) {
        let harness = self.registry.get(&binding.harness).clone();
        harness.watch(binding, self);
    }

    /// That `message_id`, delivered to the conversation, was read: told to the room once, if it is still pending.
    async fn report_read(
        &self,
        binding: &Conversation,
        message_id: &str,
        session_id: Value,
        revision: Value,
        turn_id: Value,
    ) {
        let mut pending = binding.pending.lock().await;
        if !pending.iter().any(|id| id == message_id) {
            return;
        }
        pending.retain(|id| id != message_id);
        drop(pending);
        let mut read = binding.read.lock().await;
        if read.iter().any(|id| id == message_id) {
            return;
        }
        read.push_back(message_id.to_owned());
        if read.len() > 512 {
            read.pop_front();
        }
        drop(read);
        let _ = self
            .link
            .notify(
                "input.read",
                json!({"binding_id":*binding.id.lock().await,"message_id":message_id,
            "session_id":session_id,"revision":revision,"turn_id":turn_id}),
            )
            .await;
    }

    async fn serve_client(self: Arc<Self>, stream: UnixStream) -> Result<()> {
        let owner = self.owner_serial.fetch_add(1, Ordering::Relaxed);
        let mut active = false;
        let mut shutdown = self.shutdown.subscribe();
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let result = async {
            loop {
                let line = tokio::select! {
                    _ = shutdown.changed() => break,
                    line = crate::bounded_line(&mut reader, 1 << 20) => line?,
                };
                let Some(line) = line else {
                    break;
                };
                let request: Value = serde_json::from_str(&line)?;
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
                let answer = match self.command(owner, method, params).await {
                    Ok(result) => json!({"id":id,"ok":true,"result":result}),
                    Err(error) => json!({"id":id,"ok":false,"error":error.to_string()}),
                };
                let shutdown_after_reply =
                    method == "shutdown" && answer.get("ok") == Some(&json!(true));
                write.write_all(answer.to_string().as_bytes()).await?;
                write.write_all(b"\n").await?;
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
            b.stopped.store(true, Ordering::Relaxed);
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

impl Room for Daemon {
    fn profile(&self) -> &Profile {
        &self.profile
    }

    fn engine<'a>(&'a self, conversation: &'a Conversation, engine: Value) -> BoxFuture<'a, ()> {
        Box::pin(self.report_engine(conversation, engine))
    }

    fn working<'a>(&'a self, conversation: &'a Conversation, event: Value) -> BoxFuture<'a, ()> {
        Box::pin(self.report_working_event(conversation, event))
    }

    fn read<'a>(
        &'a self,
        conversation: &'a Conversation,
        message_id: &'a str,
        session_id: Value,
        revision: Value,
        turn_id: Value,
    ) -> BoxFuture<'a, ()> {
        Box::pin(self.report_read(conversation, message_id, session_id, revision, turn_id))
    }

    fn conversations(&self) -> BoxFuture<'_, Vec<Arc<Conversation>>> {
        Box::pin(async { self.bindings.lock().await.values().cloned().collect() })
    }
}

#[cfg(test)]
mod tests {
    use super::run::prune_binding_order;
    use super::Daemon;
    use serde_json::json;
    use std::collections::HashMap;
    use tokio::sync::oneshot;

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
            json!({"event_id":"e","utterance_id":"u","status":"rejected","terminal":true}),
        ] {
            assert!(!Daemon::ack_allows_removal(&speech, &reply));
        }
    }

    /// A harness the daemon has never heard of, registered by its id: the daemon checks its route, prepares,
    /// delivers to, follows, lists and closes its conversation only through the trait.
    #[tokio::test]
    async fn a_harness_registered_by_id_is_driven_through_its_trait() {
        use crate::adapters::{Conversation, Harness, Registry, Room};
        use crate::profile::Profile;
        use anyhow::Result;
        use futures_util::future::BoxFuture;
        use serde_json::{Map, Value};
        use std::sync::{Arc, Mutex};

        struct Fake {
            calls: Arc<Mutex<Vec<String>>>,
        }

        impl Harness for Fake {
            fn id(&self) -> &'static str {
                "fake"
            }

            fn capabilities(&self) -> Value {
                json!({"deliver":"supported","working":"unsupported"})
            }

            fn accepts(&self, delivery: &Value, thread: &str) -> bool {
                delivery["kind"] == "fake-route" && delivery["thread"] == thread
            }

            fn prepare<'a>(&'a self, _: &'a Conversation) -> BoxFuture<'a, Result<Option<Value>>> {
                Box::pin(async { Ok(Some(json!({"ready":true}))) })
            }

            fn deliver<'a>(
                &'a self,
                conversation: &'a Conversation,
                frame: &'a Value,
                _: &'a Profile,
            ) -> BoxFuture<'a, Result<Value>> {
                Box::pin(async move {
                    self.calls
                        .lock()
                        .unwrap()
                        .push(format!("deliver {} {}", conversation.thread, frame["text"]));
                    Ok(json!({"status":"accepted"}))
                })
            }

            fn close<'a>(&'a self, conversation: &'a Conversation) -> BoxFuture<'a, ()> {
                Box::pin(async move {
                    self.calls
                        .lock()
                        .unwrap()
                        .push(format!("close {}", conversation.thread));
                })
            }

            fn status<'a>(
                &'a self,
                _: &'a Conversation,
            ) -> BoxFuture<'a, Option<Map<String, Value>>> {
                Box::pin(async { json!({"fake_state":"open"}).as_object().cloned() })
            }

            fn watch(&self, conversation: Arc<Conversation>, room: Arc<dyn Room>) {
                tokio::spawn(async move {
                    room.engine(
                        &conversation,
                        json!({"model":"fake-1","effort":"low","thinking":null}),
                    )
                    .await;
                });
            }
        }

        let scratch = crate::secure_fs::tests::Scratch::new("fake-harness");
        let profile = Profile::for_test(&scratch.0, &crate::adapters::Registry::standard().homes());
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

        let refused = daemon
            .command(
                1,
                "register",
                json!({"client_ref":"other","thread":"t-2","harness":"fake",
                "delivery":{"kind":"other-route","thread":"t-2"}}),
            )
            .await;
        assert!(
            refused.is_err(),
            "a route the harness does not take is refused"
        );

        let joined = daemon
            .command(
                1,
                "register",
                json!({"client_ref":"fake-1","thread":"t-1","title":"Fake","harness":"fake",
                "delivery":{"kind":"fake-route","thread":"t-1"}}),
            )
            .await
            .unwrap();
        assert_eq!(joined["prepared"], json!({"ready":true}));

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
            if !listed["engine"].is_null() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(listed["harness"], "fake");
        assert_eq!(listed["fake_state"], "open");
        assert_eq!(listed["engine"]["model"], "fake-1");
        assert_eq!(listed["capabilities"]["working"], "unsupported");

        daemon
            .command(1, "unregister", json!({"client_ref":"fake-1"}))
            .await
            .unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            ["deliver t-1 \"hola\"", "close t-1"]
        );
    }
}
