use crate::agents::HostAgents;
use crate::cursor_app::{AppNotice, CursorApps};
use crate::link::{Incoming, Link};
use crate::proof::{atomic_json, private_dir, private_file, verify_socket, Profile};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, watch, Mutex, Semaphore};
use tokio::task::JoinSet;
use tokio::time::Duration;
use tokio::time::Instant;

struct Binding {
    client_ref: String,
    harness: String,
    thread: String,
    title: String,
    delivery: Value,
    bridge_chat: Mutex<Option<String>>,
    route: Option<String>,
    capabilities: Value,
    experimental: Value,
    inbound: Value,
    detachable: bool,
    owner: AtomicU64,
    id: Mutex<String>,
    serial: Mutex<()>,
    pending: Mutex<VecDeque<String>>,
    turns: Mutex<HashMap<String, String>>,
    input_context: Mutex<HashMap<String, (String, i64)>>,
    read: Mutex<VecDeque<String>>,
    stopped: AtomicBool,
}

pub struct Daemon {
    profile: Profile,
    link: Arc<Link>,
    bindings: Mutex<HashMap<String, Arc<Binding>>>,
    outbox: Mutex<Vec<Value>>,
    replay_tx: mpsc::Sender<()>,
    host_agents: Arc<HostAgents>,
    cursor_apps: Arc<CursorApps>,
    owner_serial: AtomicU64,
    client_count: AtomicU64,
    activity_generation: AtomicU64,
    managed: bool,
    shutdown: watch::Sender<bool>,
    rendezvous: Mutex<Option<Value>>,
}

impl Daemon {
    fn new(
        profile: Profile,
        link: Arc<Link>,
        replay_tx: mpsc::Sender<()>,
        cursor_apps: Arc<CursorApps>,
        managed: bool,
        shutdown: watch::Sender<bool>,
    ) -> Result<Arc<Self>> {
        let path = profile.data.join("outbox.json");
        let outbox = if path.exists() {
            private_file(&path)?;
            let bytes = fs::read(&path)?;
            if bytes.len() > 8 << 20 {
                bail!("outbox too large");
            }
            serde_json::from_slice::<Vec<Value>>(&bytes).context("invalid existing outbox")?
        } else {
            Vec::new()
        };
        let host_agents = HostAgents::new(profile.clone())?;
        Ok(Arc::new(Self {
            profile,
            link,
            bindings: Mutex::new(HashMap::new()),
            outbox: Mutex::new(outbox),
            replay_tx,
            host_agents,
            cursor_apps,
            owner_serial: AtomicU64::new(1),
            client_count: AtomicU64::new(0),
            activity_generation: AtomicU64::new(0),
            managed,
            shutdown,
            rendezvous: Mutex::new(None),
        }))
    }

    async fn register_core(&self, binding: &Arc<Binding>) -> Result<Value> {
        let mut current = binding.id.lock().await;
        let mut frame = json!({"client_ref":binding.client_ref,"harness":binding.harness,"thread":binding.thread,"title":binding.title,"delivery":binding.delivery,
            "inbound":binding.inbound,"capabilities":binding.capabilities,
            "experimental":binding.experimental,"route":binding.route,"engine":null,"focus":false});
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
                eprintln!("[sidevoice rust proof] binding replay: {error}");
            }
        }
        self.request_replay();
    }

    fn request_replay(&self) {
        // A full slot already requests a pass; the run loop owns its one replay task.
        let _ = self.replay_tx.try_send(());
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
                eprintln!("[sidevoice rust proof] speech retry retained: {error}");
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
                            eprintln!("[sidevoice rust proof] speech retry retained: {error}");
                            return Ok(None);
                        }
                    };
                } else {
                    return Ok(None);
                }
            } else {
                if published.get("client_ref").is_none() {
                    eprintln!("[sidevoice rust proof] copied speech {} has an unknown binding; retaining for explicit migration", published.get("event_id").and_then(Value::as_str).unwrap_or("?"));
                }
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
            "binding.close" => {
                let id = params
                    .get("binding_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if let Some(binding) = self.binding_for_id(id).await {
                    binding.stopped.store(true, Ordering::Relaxed);
                    self.bindings.lock().await.remove(&binding.client_ref);
                    self.cursor_apps.close(&binding.thread).await;
                }
                json!({})
            }
            "node.rendezvous" => {
                *self.rendezvous.lock().await = Some(params.clone());
                json!({})
            }
            "agents.list" | "agents.connect" | "agents.disconnect" | "agents.dismiss" => {
                self.host_agents.handle(method, params.clone()).await
            }
            "pair.request" => {
                json!({"error":{"key":"pair.proof-only","message":"Pairing is unavailable in the isolated Rust proof."}})
            }
            "connector.error" => json!({}),
            "node.status" => crate::service::status(&self.profile, true).await,
            _ => json!({"error":{"key":"connector.unknown-method"}}),
        }
    }

    async fn handle_app_notice(&self, notice: AppNotice) {
        let (thread, message_id, answered) = match notice {
            AppNotice::Dispatched { thread, message_id } => (thread, message_id, None),
            AppNotice::Answered {
                thread,
                message_id,
                ok,
            } => (thread, message_id, Some(ok)),
        };
        let binding = self.bindings.lock().await.get(&thread).cloned();
        let Some(binding) = binding else {
            return;
        };
        if answered.is_none() || answered == Some(false) {
            return;
        }
        let context = binding.input_context.lock().await.get(&message_id).cloned();
        let Some((session_id, revision)) = context else {
            return;
        };
        let mut pending = binding.pending.lock().await;
        if !pending.iter().any(|id| id == &message_id) {
            return;
        }
        pending.retain(|id| id != &message_id);
        drop(pending);
        let mut read = binding.read.lock().await;
        if read.iter().any(|id| id == &message_id) {
            return;
        }
        read.push_back(message_id.clone());
        if read.len() > 512 {
            read.pop_front();
        }
        drop(read);
        let binding_id = binding.id.lock().await.clone();
        let _ = self
            .link
            .notify(
                "input.read",
                json!({"binding_id":binding_id,"message_id":message_id,
            "session_id":session_id,"revision":revision,"turn_id":Value::Null}),
            )
            .await;
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
        if message_id.is_empty() || session_id.is_empty() || text.is_empty() {
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
        if binding.delivery.get("kind").and_then(Value::as_str) == Some("cursor-app") {
            let mut delivery = binding.delivery.clone();
            let composer = binding.bridge_chat.lock().await.clone().or_else(|| {
                delivery
                    .get("composer")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
            let composer = match composer {
                Some(composer) => Some(composer),
                None => crate::adapters::cursor::composer_holding(&delivery).await,
            };
            if let Some(composer) = composer {
                delivery["composer"] = json!(composer);
                *binding.bridge_chat.lock().await = Some(composer);
                if let Some(result) =
                    crate::adapters::cursor::deliver_editor(&delivery, frame).await
                {
                    return result;
                }
            }
            return match self
                .cursor_apps
                .deliver(
                    &binding.thread,
                    message_id,
                    crate::adapters::envelope(frame).unwrap_or_else(|_| text.to_owned()),
                )
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
            };
        }
        match crate::adapters::deliver(
            &binding.delivery,
            &binding.thread,
            &self.profile.codex,
            frame,
        )
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
                let kind = delivery.get("kind").and_then(Value::as_str).unwrap_or("");
                let route_ok = match kind {
                    "codex-queue" => {
                        harness == "codex"
                            && delivery.get("thread").and_then(Value::as_str)
                                == Some(thread.as_str())
                    }
                    "claude-uds" => {
                        harness == "claude"
                            && delivery.get("socket").and_then(Value::as_str).is_some()
                    }
                    "cursor-tmux" => {
                        harness == "cursor"
                            && delivery.get("chat").and_then(Value::as_str) == Some(thread.as_str())
                    }
                    "cursor-app" => {
                        harness == "cursor"
                            && delivery.get("thread").and_then(Value::as_str)
                                == Some(thread.as_str())
                    }
                    "http" => {
                        delivery.get("thread").and_then(Value::as_str) == Some(thread.as_str())
                    }
                    "none" => harness == "cursor",
                    _ => false,
                };
                if !route_ok {
                    bail!("harness identity does not match its delivery route");
                }
                let capabilities = params
                    .get("capabilities")
                    .cloned()
                    .unwrap_or_else(|| crate::adapters::advertised_capabilities(&harness));
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
                let existing = { self.bindings.lock().await.get(&client_ref).cloned() };
                if let Some(existing) = existing {
                    if existing.owner.load(Ordering::Relaxed) != owner {
                        bail!("binding belongs to another façade");
                    }
                    return Ok(
                        json!({"binding_id":*existing.id.lock().await,"thread":thread,"connected":self.link.connected().await,"prepared":null}),
                    );
                }
                let binding = Arc::new(Binding {
                    client_ref: client_ref.clone(),
                    harness,
                    thread: thread.clone(),
                    title,
                    delivery: delivery.clone(),
                    bridge_chat: Mutex::new(None),
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
                    stopped: AtomicBool::new(false),
                });
                let prepared = if kind == "cursor-app" {
                    let key = delivery
                        .get("key")
                        .and_then(Value::as_str)
                        .context("Cursor card key missing")?;
                    Some(json!({"port":self.cursor_apps.open(&thread, key).await?}))
                } else {
                    None
                };
                self.bindings
                    .lock()
                    .await
                    .insert(client_ref, binding.clone());
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
                let binding = self.find_owned(owner, &params).await?;
                binding.stopped.store(true, Ordering::Relaxed);
                self.bindings.lock().await.remove(&binding.client_ref);
                self.cursor_apps.close(&binding.thread).await;
                let _ = self
                    .link
                    .notify(
                        "binding.unregister",
                        json!({"binding_id":*binding.id.lock().await}),
                    )
                    .await;
                Ok(json!({"left":true,"connected":self.link.connected().await}))
            }
            "node.status" => Ok(crate::service::status(&self.profile, true).await),
            "identity" => {
                let executable = std::env::current_exe()?.canonicalize()?;
                Ok(json!({"pid":std::process::id(),"executable":executable,"managed":self.managed}))
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
                    || binding.harness != "cursor"
                    || !client_ref.starts_with("cursor-editor-")
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
                    "capabilities":binding.capabilities,"experimental":binding.experimental,"inbound":binding.inbound,"detachable":binding.detachable}),
                )
            }
            "status" => {
                let bindings: Vec<_> = self.bindings.lock().await.values().cloned().collect();
                let mut listed = Vec::with_capacity(bindings.len());
                for binding in bindings {
                    let mut item = json!({"client_ref":binding.client_ref,"binding_id":*binding.id.lock().await,
                        "harness":binding.harness,"thread":binding.thread,"title":binding.title,"delivery":binding.delivery,
                        "capabilities":binding.capabilities,"experimental":binding.experimental,"inbound":binding.inbound});
                    if binding.delivery.get("kind").and_then(Value::as_str) == Some("cursor-app") {
                        let mut state = self.cursor_apps.status(&binding.thread).await;
                        state["bridge_chat_known"] =
                            json!(binding.bridge_chat.lock().await.is_some());
                        item["delivery_state"] = state;
                    }
                    listed.push(item);
                }
                Ok(
                    json!({"version":env!("CARGO_PKG_VERSION"),"connected":self.link.connected().await,
                    "room_reachable":self.rendezvous.lock().await.clone(),"bindings":listed}),
                )
            }
            "pair_device" => {
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
                || (adopt_orphans
                    && previous == 0
                    && binding.detachable
                    && binding.harness == "cursor");
            if !is_allowed || !binding.turns.lock().await.contains_key(&key) {
                continue;
            }
            if previous == owner {
                matches.push((binding, false));
            } else if previous == 0
                && adopt_orphans
                && binding.detachable
                && binding.harness == "cursor"
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

    fn watch_binding(self: Arc<Self>, binding: Arc<Binding>) {
        match (
            binding.harness.as_str(),
            binding.delivery.get("kind").and_then(Value::as_str),
        ) {
            ("codex", Some("codex-queue")) => self.watch_rollout(binding),
            ("codex", Some("http")) if binding.capabilities["working"] == "supported" => {
                self.watch_rollout(binding)
            }
            (_, Some("claude-uds")) => {
                self.clone().watch_transcript(binding.clone(), true);
                self.watch_claude_working(binding);
            }
            (_, Some("cursor-tmux" | "none")) if binding.harness == "cursor" => {
                self.watch_transcript(binding, false)
            }
            (_, Some("cursor-app")) => self.watch_editor_transcript(binding),
            _ => {}
        }
    }

    fn watch_claude_working(self: Arc<Self>, binding: Arc<Binding>) {
        tokio::spawn(async move {
            let mut last = None;
            while !binding.stopped.load(Ordering::Relaxed) {
                let current = crate::adapters::claude::working_state(&binding.thread);
                if current.is_some() && current != last {
                    let _ = self.link.notify("input.working", json!({"binding_id":*binding.id.lock().await,"working":current,"turn_id":Value::Null})).await;
                    last = current;
                }
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        });
    }

    fn watch_transcript(self: Arc<Self>, binding: Arc<Binding>, claude: bool) {
        self.watch_transcript_for(binding.clone(), claude, binding.thread.clone());
    }

    fn watch_editor_transcript(self: Arc<Self>, binding: Arc<Binding>) {
        tokio::spawn(async move {
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
            loop {
                if binding.stopped.load(Ordering::Relaxed) {
                    return;
                }
                let known_chat = { binding.bridge_chat.lock().await.clone() };
                if let Some(chat) = known_chat {
                    self.clone().watch_transcript_for(binding, false, chat);
                    return;
                }

                let expected = binding
                    .input_context
                    .lock()
                    .await
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>();
                let others = self
                    .bindings
                    .lock()
                    .await
                    .values()
                    .filter(|other| other.client_ref != binding.client_ref)
                    .cloned()
                    .collect::<Vec<_>>();
                let mut excluded = std::collections::HashSet::new();
                for other in others {
                    if let Some(chat) = other.bridge_chat.lock().await.clone() {
                        excluded.insert(chat);
                    }
                }
                if let Some(chat) = crate::adapters::cursor::editor_transcript_chat(
                    &binding.delivery,
                    &expected,
                    &excluded,
                ) {
                    *binding.bridge_chat.lock().await = Some(chat.clone());
                    self.clone().watch_transcript_for(binding, false, chat);
                    return;
                }

                if lookup_index < lookup_at.len() && started.elapsed() >= lookup_at[lookup_index] {
                    lookup_index += 1;
                    if let Some(chat) =
                        crate::adapters::cursor::composer_holding(&binding.delivery).await
                    {
                        *binding.bridge_chat.lock().await = Some(chat.clone());
                        self.clone().watch_transcript_for(binding, false, chat);
                        return;
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

    fn watch_transcript_for(self: Arc<Self>, binding: Arc<Binding>, claude: bool, chat: String) {
        tokio::spawn(async move {
            while !binding.stopped.load(Ordering::Relaxed) {
                let path = if claude {
                    crate::adapters::claude::transcript_path(&chat)
                } else {
                    crate::adapters::cursor::transcript_path(&chat)
                };
                if let Some(path) = path {
                    self.watch_transcript_file(binding, path, claude).await;
                    return;
                }
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        });
    }

    async fn watch_transcript_file(&self, binding: Arc<Binding>, path: PathBuf, claude: bool) {
        let mut file_id: Option<(u64, u64)> = None;
        let mut offset = 0u64;
        let mut partial = Vec::new();
        let mut tail = Vec::new();
        let mut working = None;
        let mut catching_up = true;
        let mut oversized = false;
        loop {
            if binding.stopped.load(Ordering::Relaxed) {
                break;
            }
            let Ok(mut input) = fs::File::open(&path) else {
                tokio::time::sleep(Duration::from_millis(400)).await;
                continue;
            };
            let Ok(meta) = input.metadata() else {
                continue;
            };
            let current = (meta.dev(), meta.ino());
            if file_id != Some(current) || meta.len() < offset {
                file_id = Some(current);
                offset = 0;
                partial.clear();
                tail.clear();
                working = None;
                catching_up = true;
                oversized = false;
            }
            if !claude && !tail.is_empty() && offset >= tail.len() as u64 {
                let mut current_tail = vec![0; tail.len()];
                if input
                    .seek(SeekFrom::Start(offset - tail.len() as u64))
                    .is_ok()
                    && input.read_exact(&mut current_tail).is_ok()
                    && current_tail != tail
                {
                    offset = 0;
                    partial.clear();
                    tail.clear();
                    working = None;
                    catching_up = true;
                }
            }
            if input.seek(SeekFrom::Start(offset)).is_ok() {
                let mut bytes = [0u8; 65536];
                let limit = bytes.len().min(meta.len().saturating_sub(offset) as usize);
                if let Ok(count) = input.read(&mut bytes[..limit]) {
                    offset += count as u64;
                    if !claude && count > 0 {
                        tail.extend_from_slice(&bytes[..count]);
                        if tail.len() > 64 {
                            tail.drain(..tail.len() - 64);
                        }
                    }
                    for segment in bytes[..count].split_inclusive(|byte| *byte == b'\n') {
                        if !oversized {
                            if partial.len() + segment.len() > 1 << 20 {
                                partial.clear();
                                oversized = true;
                            } else {
                                partial.extend_from_slice(segment);
                            }
                        }
                        if segment.last() == Some(&b'\n') {
                            if !oversized {
                                if let Ok(item) = serde_json::from_slice::<Value>(&partial) {
                                    if !claude {
                                        if item.get("type") == Some(&json!("turn_ended")) {
                                            working = Some(false);
                                            if !catching_up {
                                                self.report_working(&binding, false).await;
                                            }
                                        } else if item.get("role") == Some(&json!("user")) {
                                            working = Some(true);
                                            if !catching_up {
                                                self.report_working(&binding, true).await;
                                            }
                                        }
                                    }
                                    if let Some(text) = transcript_user_text(&item, claude) {
                                        if let Some(header) = voice_header(&text) {
                                            self.report_read(&binding, &header).await;
                                        }
                                    }
                                }
                            }
                            partial.clear();
                            oversized = false;
                        }
                    }
                }
            }
            if catching_up && offset >= meta.len() {
                catching_up = false;
                if !claude {
                    if let Some(state) = working {
                        self.report_working(&binding, state).await;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }

    async fn report_working(&self, binding: &Binding, working: bool) {
        let _ = self.link.notify("input.working", json!({"binding_id":*binding.id.lock().await,"working":working,"turn_id":Value::Null})).await;
    }

    async fn report_read(&self, binding: &Binding, header: &Value) {
        let Some(message_id) = header.get("message_id").and_then(Value::as_str) else {
            return;
        };
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
        let session_id = header
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let revision = header.get("revision").and_then(Value::as_i64).unwrap_or(0);
        let _ = self
            .link
            .notify(
                "input.read",
                json!({"binding_id":*binding.id.lock().await,"message_id":message_id,
            "session_id":session_id,"revision":revision,"turn_id":Value::Null}),
            )
            .await;
    }

    fn watch_rollout(self: Arc<Self>, binding: Arc<Binding>) {
        tokio::spawn(async move {
            let mut path: Option<PathBuf> = None;
            let mut offset = 0u64;
            let mut file_id: Option<(u64, u64)> = None;
            let mut partial = Vec::new();
            let mut oversized = false;
            let mut turn: Option<String> = None;
            let mut working = None;
            let mut catching_up = true;
            loop {
                if binding.stopped.load(Ordering::Relaxed) {
                    break;
                }
                if path.is_none() {
                    path = rollout_path(&self.profile.codex, &binding.thread);
                    if let Some(file) = &path {
                        if let Ok(meta) = fs::metadata(file) {
                            offset = 0;
                            file_id = Some((meta.dev(), meta.ino()));
                        }
                    }
                }
                if let Some(file) = &path {
                    if let Ok(mut input) = fs::File::open(file) {
                        let Ok(meta) = input.metadata() else {
                            continue;
                        };
                        let current = (meta.dev(), meta.ino());
                        if file_id != Some(current) || meta.len() < offset {
                            offset = 0;
                            partial.clear();
                            oversized = false;
                            turn = None;
                            working = None;
                            catching_up = true;
                        }
                        file_id = Some(current);
                        if input.seek(SeekFrom::Start(offset)).is_ok() {
                            let mut bytes = [0u8; 65536];
                            let limit = bytes.len().min(meta.len().saturating_sub(offset) as usize);
                            if let Ok(n) = input.read(&mut bytes[..limit]) {
                                offset += n as u64;
                                for segment in bytes[..n].split_inclusive(|byte| *byte == b'\n') {
                                    if !oversized {
                                        if partial.len() + segment.len() > 1 << 20 {
                                            partial.clear();
                                            oversized = true;
                                        } else {
                                            partial.extend_from_slice(segment);
                                        }
                                    }
                                    if segment.last() == Some(&b'\n') {
                                        if !oversized {
                                            if let Ok(item) =
                                                serde_json::from_slice::<Value>(&partial)
                                            {
                                                self.observe_rollout(
                                                    &binding,
                                                    &item,
                                                    &mut turn,
                                                    &mut working,
                                                    catching_up,
                                                )
                                                .await;
                                            }
                                        }
                                        partial.clear();
                                        oversized = false;
                                    }
                                }
                            }
                            if catching_up && offset >= meta.len() {
                                catching_up = false;
                                if let Some(state) = working {
                                    let _ = self.link.notify("input.working", json!({"binding_id":*binding.id.lock().await,
                                        "working":state,"turn_id":if state { turn.clone() } else { None }})).await;
                                }
                            }
                        }
                    } else {
                        path = None;
                        file_id = None;
                        partial.clear();
                        oversized = false;
                    }
                }
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        });
    }

    async fn observe_rollout(
        &self,
        binding: &Binding,
        item: &Value,
        turn: &mut Option<String>,
        working: &mut Option<bool>,
        catching_up: bool,
    ) {
        let id = binding.id.lock().await.clone();
        if item.get("type") == Some(&json!("turn_context")) {
            if let Some(model) = item.pointer("/payload/model").and_then(Value::as_str) {
                let _ = self
                    .link
                    .notify(
                        "input.engine",
                        json!({"binding_id":id,"engine":{"model":model}}),
                    )
                    .await;
            }
        }
        if item.get("type") == Some(&json!("event_msg")) {
            let phase = item
                .pointer("/payload/type")
                .and_then(Value::as_str)
                .unwrap_or("");
            let turn_id = item.pointer("/payload/turn_id").and_then(Value::as_str);
            if phase == "task_started" {
                if let Some(turn_id) = turn_id {
                    *turn = Some(turn_id.to_owned());
                    *working = Some(true);
                    if !catching_up {
                        let _ = self
                            .link
                            .notify(
                                "input.working",
                                json!({"binding_id":id,"working":true,"turn_id":turn_id}),
                            )
                            .await;
                    }
                }
            }
            if phase == "task_complete" || phase == "turn_aborted" {
                if let Some(turn_id) = turn_id {
                    if turn.as_deref() == Some(turn_id) {
                        *turn = None;
                    }
                    *working = Some(false);
                    if !catching_up {
                        let _ = self
                            .link
                            .notify(
                                "input.working",
                                json!({"binding_id":id,"working":false,"turn_id":turn_id}),
                            )
                            .await;
                    }
                }
            }
        }
        if item.get("type") == Some(&json!("response_item"))
            && item.pointer("/payload/type") == Some(&json!("message"))
            && item.pointer("/payload/role") == Some(&json!("user"))
        {
            let text = item
                .pointer("/payload/content")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter(|part| {
                            part.get("type").and_then(Value::as_str) == Some("input_text")
                        })
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            let Some(start) = text.find("{\"channel\":") else {
                return;
            };
            let Some(end) = text[start..].find('}') else {
                return;
            };
            let Ok(header) = serde_json::from_str::<Value>(&text[start..=start + end]) else {
                return;
            };
            let Some(message_id) = header.get("message_id").and_then(Value::as_str) else {
                return;
            };
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
            let _ = self.link.notify("input.read", json!({"binding_id":id,"message_id":message_id,"session_id":header["session_id"],"revision":header["revision"],"turn_id":turn})).await;
        }
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

fn transcript_user_text(item: &Value, claude: bool) -> Option<String> {
    if claude
        && item.get("type") == Some(&json!("attachment"))
        && item.pointer("/attachment/type") == Some(&json!("queued_command"))
    {
        return item
            .pointer("/attachment/prompt")
            .and_then(Value::as_str)
            .map(str::to_owned);
    }
    let is_user = if claude {
        item.get("type") == Some(&json!("user"))
            && item.pointer("/message/role") == Some(&json!("user"))
    } else {
        item.get("role") == Some(&json!("user"))
    };
    if !is_user {
        return None;
    }
    let content = item
        .pointer("/message/content")
        .or_else(|| item.get("content"))?;
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    let parts = content.as_array()?;
    let texts = parts
        .iter()
        .filter_map(|part| {
            (part.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| part.get("text").and_then(Value::as_str))
                .flatten()
        })
        .collect::<Vec<_>>();
    (!texts.is_empty()).then(|| texts.join("\n"))
}

fn voice_header(text: &str) -> Option<Value> {
    let start = text.find("{\"channel\":")?;
    let tail = &text[start..];
    let end = tail.find('}')?;
    let header: Value = serde_json::from_str(&tail[..=end]).ok()?;
    if !matches!(
        header.get("channel").and_then(Value::as_str),
        Some("voice" | "room-control")
    ) || header
        .get("session_id")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
        || header
            .get("message_id")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || header
            .get("revision")
            .and_then(Value::as_i64)
            .is_none_or(|revision| revision < 0)
    {
        return None;
    }
    Some(header)
}

fn rollout_path(home: &Path, thread: &str) -> Option<PathBuf> {
    let root = home.join("sessions");
    let mut years: Vec<_> = fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    years.sort();
    years.reverse();
    for year in years {
        let mut months: Vec<_> = fs::read_dir(year)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .collect();
        months.sort();
        months.reverse();
        for month in months {
            let mut days: Vec<_> = fs::read_dir(month)
                .ok()?
                .flatten()
                .map(|e| e.path())
                .collect();
            days.sort();
            days.reverse();
            for day in days {
                if let Ok(files) = fs::read_dir(day) {
                    for entry in files.flatten() {
                        let path = entry.path();
                        if path.file_name().is_some_and(|n| {
                            n.to_string_lossy().ends_with(&format!("-{thread}.jsonl"))
                        }) {
                            return Some(path);
                        }
                    }
                }
            }
        }
    }
    None
}

fn prune_binding_order(order: &mut HashMap<String, oneshot::Receiver<()>>) {
    order.retain(|_, tail| matches!(tail.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
}

pub async fn run(profile: Profile, managed: bool) -> Result<()> {
    profile.validate_private()?;
    if managed {
        #[cfg(not(target_os = "macos"))]
        bail!("the private managed connector is supported only by macOS launchd");
        #[cfg(target_os = "macos")]
        profile.validate_service_environment("launchd")?;
    }
    if profile.service_stopped()? {
        eprintln!(
            "{}",
            crate::agents::message("service.node-stopped", &serde_json::Value::Null)
        );
        return Ok(());
    }
    private_dir(&profile.data)?;
    let lock = profile.try_connector_lock()?;
    if profile.socket.exists() {
        verify_socket(&profile.socket)?;
        fs::remove_file(&profile.socket)?;
    }
    let link = Link::new();
    let (replay_tx, mut replay_rx) = mpsc::channel(1);
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let (app_tx, mut app_rx) = mpsc::unbounded_channel();
    let cursor_apps = CursorApps::new(app_tx);
    let daemon = Daemon::new(
        profile.clone(),
        link.clone(),
        replay_tx,
        cursor_apps,
        managed,
        shutdown_tx,
    )?;
    let (incoming_tx, mut incoming_rx) = mpsc::channel(128);
    let clients = Arc::new(Semaphore::new(32));
    let mut core_tasks = JoinSet::new();
    let mut replay_tasks = JoinSet::new();
    let mut client_tasks = JoinSet::new();
    let mut replay_again = false;
    let mut core_session = 0u64;
    let mut binding_order = HashMap::<String, oneshot::Receiver<()>>::new();
    // Accept MCP before Core is ready. The link task retries the private Core socket until the
    // separately manager-owned Core job becomes healthy.
    let listener = UnixListener::bind(&profile.socket)?;
    fs::set_permissions(&profile.socket, fs::Permissions::from_mode(0o600))?;
    let (ready_tx, ready_rx) = oneshot::channel();
    let link_task = tokio::spawn(link.run(profile.clone(), incoming_tx, ready_tx));
    drop(ready_rx);
    let scanner = if managed {
        let host_agents = daemon.host_agents.clone();
        let scan_profile = profile.clone();
        Some(tokio::spawn(async move {
            loop {
                if !scan_profile.service_stopped().unwrap_or(true) {
                    let _ = host_agents
                        .handle("agents.list", serde_json::json!({"rescan":true}))
                        .await;
                }
                tokio::time::sleep(Duration::from_secs(6 * 60 * 60)).await;
            }
        }))
    } else {
        None
    };
    let mut idle_since = Instant::now();
    let mut last_activity = daemon.activity_generation.load(Ordering::Relaxed);
    let mut idle_check = tokio::time::interval_at(
        Instant::now() + Duration::from_millis(200),
        Duration::from_millis(200),
    );
    idle_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut link_exited = false;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            Some(result) = replay_tasks.join_next(), if !replay_tasks.is_empty() => {
                if let Err(error) = result { eprintln!("[sidevoice rust proof] outbox replay: {error}"); }
                if replay_again {
                    replay_again = false;
                    let daemon = daemon.clone();
                    replay_tasks.spawn(async move { daemon.flush_outbox().await; });
                }
            }
            Some(result) = client_tasks.join_next(), if !client_tasks.is_empty() => {
                if let Err(error) = result { eprintln!("[sidevoice rust proof] IPC task: {error}"); }
            }
            Some(()) = replay_rx.recv() => {
                if replay_tasks.is_empty() {
                    let daemon = daemon.clone();
                    replay_tasks.spawn(async move { daemon.flush_outbox().await; });
                } else {
                    replay_again = true;
                }
            }
            Some(notice) = app_rx.recv() => {
                daemon.handle_app_notice(notice).await;
            }
            Some(result) = core_tasks.join_next(), if !core_tasks.is_empty() => {
                if let Err(error) = result { eprintln!("[sidevoice rust proof] Core handler: {error}"); }
                prune_binding_order(&mut binding_order);
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                if let Ok(permit) = clients.clone().try_acquire_owned() {
                    let daemon = daemon.clone();
                    client_tasks.spawn(async move { let _permit = permit; daemon.serve_client(stream).await });
                }
            }
            item = incoming_rx.recv() => {
                let Some(item) = item else { link_exited=true; break; };
                if item.method == "connector.lost" {
                    if item.session == core_session {
                        core_tasks.abort_all();
                        while core_tasks.join_next().await.is_some() {}
                        replay_tasks.abort_all();
                        while replay_tasks.join_next().await.is_some() {}
                        replay_again = false;
                        binding_order.clear();
                        core_session = 0;
                    }
                    continue;
                }
                if item.session != core_session {
                    core_tasks.abort_all();
                    while core_tasks.join_next().await.is_some() {}
                    replay_tasks.abort_all();
                    while replay_tasks.join_next().await.is_some() {}
                    replay_again = false;
                    binding_order.clear();
                    core_session = item.session;
                }
                if core_tasks.len() >= 64 {
                    if let Some(tx) = item.reply {
                        let _ = tx.send(json!({"status":"failed","detail":"connector handler capacity reached"}));
                    } else {
                        eprintln!("[sidevoice rust proof] Core notification capacity reached");
                    }
                    continue;
                }
                if item.method == "binding.close" {
                    if let Some(id) = item.params.get("binding_id").and_then(Value::as_str) {
                        binding_order.remove(id);
                    }
                }
                let daemon = daemon.clone();
                let (previous, completed) = if item.method == "input.deliver" {
                    let id = item.params.get("binding_id")
                        .and_then(Value::as_str).unwrap_or("").to_owned();
                    let (tx, rx) = oneshot::channel();
                    (binding_order.insert(id, rx), Some(tx))
                } else {
                    (None, None)
                };
                core_tasks.spawn(async move {
                    if let Some(previous) = previous {
                        let _ = previous.await;
                    }
                    daemon.incoming(item).await;
                    if let Some(completed) = completed {
                        let _ = completed.send(());
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => { break; }
            _ = terminate.recv() => { break; }
            _ = shutdown_rx.changed() => { break; }
            _ = idle_check.tick(), if !managed => {
                let activity = daemon.activity_generation.load(Ordering::Relaxed);
                if activity != last_activity {
                    idle_since = Instant::now();
                    last_activity = activity;
                }
                if daemon.is_idle().await {
                    if idle_since.elapsed() >= Duration::from_secs(15) { break; }
                } else {
                    idle_since = Instant::now();
                }
            }
        }
    }
    daemon.shutdown.send_replace(true);
    if !client_tasks.is_empty() {
        let drained = tokio::time::timeout(Duration::from_secs(2), async {
            while client_tasks.join_next().await.is_some() {}
        })
        .await;
        if drained.is_err() {
            client_tasks.abort_all();
            while client_tasks.join_next().await.is_some() {}
        }
    }
    if let Some(scanner) = scanner {
        scanner.abort();
        let _ = scanner.await;
    }
    link_task.abort();
    let _ = link_task.await;
    replay_tasks.abort_all();
    while replay_tasks.join_next().await.is_some() {}
    core_tasks.abort_all();
    while core_tasks.join_next().await.is_some() {}
    daemon.host_agents.shutdown().await;
    fs::remove_file(&profile.socket)?;
    drop(lock);
    if link_exited {
        bail!("Core authentication was refused or its link task exited");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{prune_binding_order, Daemon};
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
}
