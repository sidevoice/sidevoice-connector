use crate::adapters::{self, Identity, Registry};
use crate::profile::Profile;
use crate::secure_fs::verify_socket;
use anyhow::{bail, Context, Result};
use rmcp::{
    model::*, service::RequestContext, ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::{timeout, timeout_at, Duration, Instant};

const INSTRUCTIONS: &str = r#"Sidevoice connects this conversation to the person's voice room.
- Call voice_connect only when the person asks to join or enable voice. Select input_mode pull when this harness will call get_messages; otherwise the core injects input. One binding uses one mode.
- Each voice or note message carries a stable message_id, conversation epoch, session_id and revision. A repeated ID is redelivery, not new input. A note contains Sidevoice facts and no words from the person. The full notice and unheard evidence, including corrections and related message IDs, matter in both modes.
- In pull mode, call get_messages at useful hooks. Acknowledge IDs with ack_ids only after processing them. Pull may learn later than injection. After is a journal position, not an acknowledgement.
- Reply with voice_say using the message's session_id, revision and epoch. For speech before any message, use voice_connect's initial_reply_context. Typing does not advance the epoch. Several replies in one epoch play in publication order.
- If voice_say says the person is speaking, the call was refused and nothing was queued: wait for their message. If that turn ends without words, a Sidevoice note may ask you to resend what still matters. A published reply is not proof it was heard.
- When a notice says a reply was interrupted, not played, skipped, later heard, or that the person returned, use its full evidence and instruction. Do not automatically resend an unheard backlog; say what still matters in the current context.
- For substantive work, first a short acknowledgement, then meaningful checkpoints, then the result. No filler or tool narration. Take in new input at tool results before starting the next step.
- Pairing a machine or device is the person's act. Ask for their room address and one-time code only when needed; never fetch a code yourself. If inbound.ok is false, explain its reason and remedy without changing settings unasked."#;

/// Overall budget for an IPC round trip, before the daemon's reply (or lack of one) gives up on
/// its own. `pair_device` gets longer: the daemon can spend up to 15 s waiting for the core link
/// plus 10 s generating the code, so the MCP-side limit must clear that with room to spare.
const DEFAULT_IPC_LIMIT: Duration = Duration::from_secs(20);
const PAIR_DEVICE_IPC_LIMIT: Duration = Duration::from_secs(30);

fn ipc_limit(method: &str) -> Duration {
    if method == "pair_device" {
        PAIR_DEVICE_IPC_LIMIT
    } else {
        DEFAULT_IPC_LIMIT
    }
}

struct Ipc {
    profile: Profile,
    joined: Arc<Mutex<HashMap<String, Joined>>>,
    tx: Mutex<Option<mpsc::Sender<Value>>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    registrations: Mutex<HashMap<String, Value>>,
    serial: AtomicU64,
    connect_lock: Mutex<()>,
    replay_lock: Mutex<()>,
    generation: AtomicU64,
    replayed_generation: AtomicU64,
}

impl Ipc {
    fn new(profile: Profile, joined: Arc<Mutex<HashMap<String, Joined>>>) -> Arc<Self> {
        let client = Arc::new(Self {
            profile,
            joined,
            tx: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            registrations: Mutex::new(HashMap::new()),
            serial: AtomicU64::new(1),
            connect_lock: Mutex::new(()),
            replay_lock: Mutex::new(()),
            generation: AtomicU64::new(0),
            replayed_generation: AtomicU64::new(0),
        });
        let supervisor = client.clone();
        tokio::spawn(async move {
            supervisor.supervise().await;
        });
        client
    }

    async fn ensure(self: &Arc<Self>) -> Result<()> {
        let _guard = self.connect_lock.lock().await;
        if self.tx.lock().await.is_some() {
            return Ok(());
        }
        verify_socket(&self.profile.socket)?;
        let stream = timeout(
            Duration::from_secs(2),
            UnixStream::connect(&self.profile.socket),
        )
        .await??;
        let (read, mut write) = stream.into_split();
        let (tx, mut rx) = mpsc::channel::<Value>(64);
        *self.tx.lock().await = Some(tx);
        self.generation.fetch_add(1, Ordering::Relaxed);
        let this = self.clone();
        tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if write.write_all(frame.to_string().as_bytes()).await.is_err()
                    || write.write_all(b"\n").await.is_err()
                {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            let mut reader = BufReader::new(read);
            loop {
                let line = match crate::bounded_line(&mut reader, 1 << 20).await {
                    Ok(Some(v)) => v,
                    _ => break,
                };
                let Ok(reply) = serde_json::from_str::<Value>(&line) else {
                    break;
                };
                let Some(id) = reply.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                if let Some(waiting) = this.pending.lock().await.remove(&id) {
                    let result = if reply.get("ok") == Some(&json!(true)) {
                        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
                    } else {
                        Err(crate::ipc_failure(&reply, "IPC error"))
                    };
                    let _ = waiting.send(result);
                }
            }
            *this.tx.lock().await = None;
            for (_, waiter) in this.pending.lock().await.drain() {
                let _ = waiter.send(Err("connector went away".into()));
            }
        });
        Ok(())
    }

    async fn supervise(self: Arc<Self>) {
        let mut delay = 100;
        loop {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            if self.registrations.lock().await.is_empty() || self.tx.lock().await.is_some() {
                delay = 100;
                continue;
            }
            if self.ensure_registered().await.is_ok() {
                delay = 100;
                continue;
            }
            delay = (delay * 2).min(5000);
        }
    }

    async fn call(self: &Arc<Self>, method: &str, params: Value) -> Result<Value> {
        self.ensure_registered().await?;
        self.call_ready(method, params).await
    }

    async fn remember_registration(&self, params: Value) {
        if let Some(client_ref) = params.get("client_ref").and_then(Value::as_str) {
            self.registrations
                .lock()
                .await
                .insert(client_ref.to_owned(), params);
        }
    }

    async fn forget_registration(&self, client_ref: &str) {
        self.registrations.lock().await.remove(client_ref);
    }

    async fn ensure_registered(self: &Arc<Self>) -> Result<()> {
        self.ensure().await?;
        let _guard = self.replay_lock.lock().await;
        let generation = self.generation.load(Ordering::Relaxed);
        if self.replayed_generation.load(Ordering::Relaxed) == generation {
            return Ok(());
        }
        let saved: Vec<_> = self.registrations.lock().await.values().cloned().collect();
        for params in saved {
            let client_ref = params
                .get("client_ref")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let mut replay = params;
            replay["resume"] = json!(true);
            let result = match self.call_ready("register", replay).await {
                Ok(result) => result,
                Err(error) if error.to_string().starts_with("CLOSED_BY_ROOM:") => {
                    if let Some(client_ref) = client_ref {
                        self.registrations.lock().await.remove(&client_ref);
                        self.joined.lock().await.remove(&client_ref);
                    }
                    continue;
                }
                Err(error) => return Err(error),
            };
            if let (Some(client_ref), Some(binding_id)) =
                (client_ref, result.get("binding_id").and_then(Value::as_str))
            {
                if let Some(joined) = self.joined.lock().await.get_mut(&client_ref) {
                    joined.binding_id = binding_id.to_owned();
                    if let Some(context) = result.get("initial_reply_context") {
                        joined.initial_reply_context = context.clone();
                    }
                }
            }
        }
        self.replayed_generation
            .store(generation, Ordering::Relaxed);
        Ok(())
    }

    async fn call_ready(self: &Arc<Self>, method: &str, params: Value) -> Result<Value> {
        let deadline = Instant::now() + ipc_limit(method);
        let id = self.serial.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let sender = self
            .tx
            .lock()
            .await
            .clone()
            .context("connector went away")?;
        let sent = timeout_at(
            deadline,
            sender.send(json!({"id":id,"method":method,"params":params})),
        )
        .await;
        if !matches!(sent, Ok(Ok(()))) {
            self.pending.lock().await.remove(&id);
            bail!("connector IPC send timed out or disconnected");
        }
        let answer = timeout_at(deadline, rx).await;
        self.pending.lock().await.remove(&id);
        let value = answer
            .context("connector request timed out")??
            .map_err(anyhow::Error::msg)?;
        if method == "register" {
            if let Some(client_ref) = params.get("client_ref").and_then(Value::as_str) {
                let mut registration = params.clone();
                if let Some(object) = registration.as_object_mut() {
                    object.remove("resume");
                }
                self.registrations
                    .lock()
                    .await
                    .insert(client_ref.to_owned(), registration);
            }
        }
        if method == "unregister" {
            if let Some(client_ref) = params.get("client_ref").and_then(Value::as_str) {
                self.registrations.lock().await.remove(client_ref);
            }
        }
        Ok(value)
    }
}

#[derive(Clone)]
struct Facade {
    profile: Profile,
    ipc: Arc<Ipc>,
    joined: Arc<Mutex<HashMap<String, Joined>>>,
    registry: Arc<Registry>,
    /// Whether this client serves several of its chats at once (from `initialize`): a call that names no
    /// conversation may then be any of them.
    several_chats: Arc<AtomicBool>,
}

#[derive(Clone)]
struct Joined {
    binding_id: String,
    input_mode: String,
    initial_reply_context: Value,
    title: String,
    identity: Identity,
    capabilities: Value,
    experimental: Vec<String>,
}

/// Asked to join a named room: what to ask the user when this machine is not paired with it, or nothing when it is.
/// The code exists only on the room's screen, so the agent asks the user and never fetches one itself.
fn pairing_needed(room: &str, paired: Option<String>) -> Option<String> {
    let Some(target) = crate::pairing::normalize_origin(room) else {
        return Some(format!(
            "\"{room}\" is not a room address; expected something like https://voice.example"
        ));
    };
    match paired {
        None => Some(format!(
            "This machine is not paired with the room at {target}. Ask the user for the room's address (confirm {target}) and the one-time pairing code the room shows under \"Emparejar máquina\", then call voice_pair with both. Do not fetch a code yourself."
        )),
        Some(paired) if paired != target => Some(format!(
            "This machine is paired with {paired}, not {target}. One room per machine: to switch, ask the user for the pairing code that {target} shows under \"Emparejar máquina\" and call voice_pair (it replaces the current pairing); to stay, call voice_connect without a room."
        )),
        Some(_) => None,
    }
}

fn closed_note(reason: &str) -> &'static str {
    if reason == "conversation_history_limit" {
        "The connector reached its private conversation history limit and cannot safely restore this older conversation automatically. Its prior room state may be unavailable. Call voice_connect again if the user wants to rejoin."
    } else if reason == "state_write_failed" {
        "The connector could not save conversation state and cannot safely restore this older conversation automatically. Call voice_connect again if the user wants to rejoin; confirm the connector can write to its private data folder."
    } else if reason == "connector_revoked" {
        "This machine's pairing was revoked from the room, so this conversation has no voice. Tell the user; to have voice again they must pair this machine with the one-time code the room shows under Emparejar máquina (voice_pair), and then you can call voice_connect. Continue in writing meanwhile."
    } else {
        "The user closed this conversation's voice channel from the room. Continue in writing and do not publish speech; call voice_connect again only if the user asks for voice."
    }
}

impl Facade {
    fn several_chats(&self) -> bool {
        self.several_chats.load(Ordering::Relaxed)
    }

    fn new(profile: Profile) -> Self {
        let joined = Arc::new(Mutex::new(HashMap::new()));
        Self {
            ipc: Ipc::new(profile.clone(), joined.clone()),
            profile,
            joined,
            registry: Arc::new(Registry::standard()),
            several_chats: Arc::new(AtomicBool::new(false)),
        }
    }

    async fn refresh_binding_ids(&self, status: &Value) {
        let Some(bindings) = status.pointer("/bindings").and_then(Value::as_array) else {
            return;
        };
        let mut joined = self.joined.lock().await;
        for item in bindings {
            let (Some(client_ref), Some(binding_id)) = (
                item.get("client_ref").and_then(Value::as_str),
                item.get("binding_id").and_then(Value::as_str),
            ) else {
                continue;
            };
            if let Some(binding) = joined.get_mut(client_ref) {
                binding.binding_id = binding_id.to_owned();
                if let Some(context) = item.get("initial_reply_context") {
                    binding.initial_reply_context = context.clone();
                }
            }
        }
    }

    async fn has_active_binding(&self, status: &Value, named: Option<&str>) -> bool {
        let Some(bindings) = status.pointer("/bindings").and_then(Value::as_array) else {
            return false;
        };
        let joined = self.joined.lock().await;
        if let Some(name) = named {
            joined.contains_key(name)
                && bindings
                    .iter()
                    .any(|binding| binding.get("client_ref").and_then(Value::as_str) == Some(name))
        } else {
            joined.keys().any(|name| {
                bindings.iter().any(|binding| {
                    binding.get("client_ref").and_then(Value::as_str) == Some(name.as_str())
                })
            })
        }
    }

    async fn invoke(
        &self,
        name: &str,
        args: Value,
        meta: Value,
        client: Value,
        client_caps: Value,
    ) -> Result<Value> {
        match name {
            "voice_connect" => {
                let input_mode = args
                    .get("input_mode")
                    .and_then(Value::as_str)
                    .unwrap_or("push");
                if !matches!(input_mode, "push" | "pull") {
                    bail!("input_mode must be push or pull");
                }
                if let Some(room) = args.get("room").and_then(Value::as_str) {
                    if let Some(refusal) =
                        pairing_needed(room, crate::pairing::previous_room(&self.profile))
                    {
                        bail!("{refusal}");
                    }
                }
                let mut identity = adapters::identify(&meta, Some(&client), &client_caps)?;
                let explicit_title = args.get("title").and_then(Value::as_str).unwrap_or("");
                let title = if !explicit_title.is_empty() {
                    explicit_title.to_owned()
                } else if let Ok(configured) = std::env::var("SIDEVOICE_TITLE") {
                    if configured.is_empty() {
                        String::new()
                    } else {
                        configured
                    }
                } else {
                    String::new()
                };
                let title = if title.is_empty() {
                    std::env::current_dir()
                        .ok()
                        .and_then(|path| {
                            path.file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                        })
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| {
                            "a short label of what this conversation is about".into()
                        })
                } else {
                    title
                };
                let title = title.chars().take(200).collect::<String>();
                let given_title = explicit_title.trim().chars().take(200).collect::<String>();
                let harness = self.registry.get(&identity.harness).clone();
                harness.refine_identity(&mut identity, &given_title).await;
                let inbound = adapters::inspect_inbound(&identity)?;
                if inbound.as_ref().and_then(|value| value.get("ok")) == Some(&json!(false)) {
                    let info = inbound.as_ref().unwrap();
                    bail!(
                        "This conversation cannot connect: {} {}",
                        info["reason"]
                            .as_str()
                            .unwrap_or("incoming messages are refused"),
                        info["remedy"]
                            .as_str()
                            .unwrap_or("change this session's settings")
                    );
                }
                identity.inbound = inbound.clone();
                let capabilities = adapters::conversation_capabilities(&identity);
                let experimental = adapters::experimental(&identity, &capabilities);
                if !identity.editor {
                    let old: Vec<_> = self
                        .joined
                        .lock()
                        .await
                        .iter()
                        .filter(|(thread, joined)| {
                            thread.as_str() != identity.thread && !joined.identity.editor
                        })
                        .map(|(thread, joined)| (thread.clone(), joined.binding_id.clone()))
                        .collect();
                    for (thread, binding_id) in old {
                        let _ = self
                            .ipc
                            .call(
                                "unregister",
                                json!({"binding_id":binding_id,"client_ref":thread}),
                            )
                            .await;
                        self.joined.lock().await.remove(&thread);
                    }
                }
                let mut params = json!({
                    "client_ref":identity.thread,"harness":identity.harness,"thread":identity.thread,"title":title,
                    "delivery":identity.delivery,"route":identity.route,"inbound":identity.inbound,
                    "capabilities":capabilities,"experimental":experimental,"detachable":identity.detachable,
                    "input_mode":input_mode
                });
                if let Some(engine) = adapters::engine(&identity) {
                    params["engine"] = engine;
                }
                let result = self.ipc.call("register", params).await?;
                let binding_id = result
                    .get("binding_id")
                    .and_then(Value::as_str)
                    .context("connector omitted binding ID")?;
                self.joined.lock().await.insert(
                    identity.thread.clone(),
                    Joined {
                        binding_id: binding_id.to_owned(),
                        input_mode: input_mode.to_owned(),
                        initial_reply_context: result
                            .get("initial_reply_context")
                            .cloned()
                            .unwrap_or(Value::Null),
                        title: title.clone(),
                        identity: identity.clone(),
                        capabilities: capabilities.clone(),
                        experimental: experimental.clone(),
                    },
                );
                let pushed = capabilities["deliver"] == "supported";
                let connector = self
                    .ipc
                    .call("status", json!({}))
                    .await
                    .unwrap_or_else(|_| json!({}));
                let mut response = json!({
                    "status":if result.get("pending") == Some(&json!(true)) {"joining"} else {"joined"},
                    "harness":identity.harness,"conversation":identity.thread,"binding_id":binding_id,
                    "delivery":if input_mode == "pull" {"pull"} else if pushed {"push"} else {"none"},"room_reachable":result.get("connected"),
                    "epoch":result.get("epoch"),"initial_reply_context":result.get("initial_reply_context"),
                    "capabilities":capabilities,"inbound":identity.inbound,
                    "local_only":"This machine is not paired with any room, so this conversation is reachable only from devices paired with this machine itself (the Sidevoice app on this computer, at this machine's own address). Tell the user in one line. Pairing the app is voice_pair_device, only if they ask; reaching it from elsewhere needs a room's address and code (voice_pair).",
                    "version":self.profile.connector_version(),"connector_version":connector.get("version")
                });
                if !pushed && input_mode != "pull" {
                    response["voice_in"] = json!({"supported":false,"speak_with":result.get("initial_reply_context"),
                        "reason":format!("What the user says in the room cannot reach this conversation: {} The user types here as usual.", identity.deliver_note.as_deref().unwrap_or("this harness offers no way to put a message into it.")),
                        "how":"Use initial_reply_context to speak first. Tell the user once that the room hears this conversation but cannot talk to it."});
                }
                if let Some(note) = &identity.watch_note {
                    response["watch_note"] = json!(note);
                }
                if !experimental.is_empty() {
                    response["experimental"] = json!(experimental);
                    response["experimental_notes"] = json!(experimental
                        .iter()
                        .filter_map(|name| harness.experimental_note(name, &identity))
                        .collect::<Vec<_>>());
                }
                if let Some(client_harness) = self.registry.client(&client) {
                    for (key, value) in client_harness.joined_fields(&identity, &result) {
                        response[key] = value;
                    }
                }
                Ok(response)
            }
            "voice_status" => {
                let named = args
                    .get("conversation")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty());
                if let Some(name) = named {
                    self.adopt_by_id(name, &client).await;
                }
                let status = self.ipc.call("status", json!({})).await?;
                self.refresh_binding_ids(&status).await;
                if status.get("refused").and_then(Value::as_str).is_some() {
                    return Ok(json!({"joined":false,"conversation":named,
                        "room_reachable":status.get("connected"),"connector":status,
                        "closed_by_room":true,"note":closed_note("connector_revoked")}));
                }
                let closed_now = status
                    .get("closed_by_room")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let closed_titles = {
                    let joined = self.joined.lock().await;
                    closed_now
                        .iter()
                        .filter_map(|item| {
                            item.as_str().and_then(|name| {
                                joined
                                    .get(name)
                                    .map(|binding| (name.to_owned(), binding.title.clone()))
                            })
                        })
                        .collect::<HashMap<_, _>>()
                };
                let closed_reasons = status
                    .get("closed_reasons")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                for item in &closed_now {
                    if let Some(name) = item.as_str() {
                        self.ipc.forget_registration(name).await;
                        self.joined.lock().await.remove(name);
                    }
                }
                if let Some(name) =
                    named.filter(|name| closed_now.iter().any(|item| item.as_str() == Some(*name)))
                {
                    let reason = closed_reasons
                        .get(name)
                        .and_then(Value::as_str)
                        .unwrap_or("closed_from_room");
                    return Ok(
                        json!({"joined":false,"conversation":name,"room_reachable":status.get("connected"),
                        "connector":status,"closed_by_room":true,"note":closed_note(reason)}),
                    );
                }
                if status.get("resume_blocked") == Some(&json!(true))
                    && !self.has_active_binding(&status, named).await
                {
                    return Ok(json!({"joined":false,"conversation":named,
                        "room_reachable":status.get("connected"),"connector":status,
                        "note":closed_note(status.pointer("/resume_block_reason").and_then(Value::as_str).unwrap_or("conversation_history_limit"))}));
                }
                if named.is_none() && self.several_chats() && !self.joined.lock().await.is_empty() {
                    let conversations = self.joined.lock().await.values().map(|binding| json!({"title":binding.title,"harness":binding.identity.harness})).collect::<Vec<_>>();
                    return Ok(
                        json!({"joined":Value::Null,"conversation":Value::Null,"room_reachable":status.get("connected"),"connector":status,
                        "conversations":conversations,"closed_by_room":closed_now.iter().filter_map(|item| item.as_str()).map(|name| json!({
                            "title":closed_titles.get(name),"note":closed_note(closed_reasons.get(name).and_then(Value::as_str).unwrap_or("closed_from_room"))
                        })).collect::<Vec<_>>(),
                        "note":"Chats of this window are joined, and this call does not say which chat is asking. If voice_connect returned a conversation id in this chat, pass it as conversation to ask about this one; if it never did, this chat is not joined."}),
                    );
                }
                let chosen = self.pick(named).await?;
                if let Some(name) = chosen {
                    let binding = self
                        .joined
                        .lock()
                        .await
                        .get(&name)
                        .cloned()
                        .context("conversation disappeared")?;
                    let daemon_binding = status
                        .pointer("/bindings")
                        .and_then(Value::as_array)
                        .and_then(|items| {
                            items.iter().find(|item| {
                                item.get("client_ref").and_then(Value::as_str)
                                    == Some(name.as_str())
                            })
                        });
                    let mut response = json!({"joined":true,"conversation":name,"binding_id":binding.binding_id,"harness":binding.identity.harness,
                        "room_reachable":status.get("connected"),"connector":status,"capabilities":binding.capabilities,
                        "experimental":binding.experimental,"inbound":binding.identity.inbound});
                    if let Some(client_harness) = self.registry.client(&client) {
                        for (key, value) in
                            client_harness.status_fields(&binding.identity, daemon_binding)
                        {
                            response[key] = value;
                        }
                    }
                    Ok(response)
                } else {
                    Ok(
                        json!({"joined":false,"conversation":Value::Null,"room_reachable":status.get("connected"),"connector":status,
                            "closed_by_room":closed_now.iter().filter_map(|item| item.as_str()).map(|name| json!({
                                "title":closed_titles.get(name),"note":closed_note(closed_reasons.get(name).and_then(Value::as_str).unwrap_or("closed_from_room"))
                            })).collect::<Vec<_>>()}),
                    )
                }
            }
            "get_messages" => {
                let named = args
                    .get("conversation")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty());
                if let Some(name) = named {
                    self.adopt_by_id(name, &client).await;
                }
                let chosen = self.pick(named).await?.context("not connected")?;
                let joined = self
                    .joined
                    .lock()
                    .await
                    .get(&chosen)
                    .cloned()
                    .context("not connected")?;
                if joined.input_mode != "pull" {
                    bail!("This conversation uses injection; reconnect with input_mode pull to use get_messages.");
                }
                let mut request = json!({"binding_id":joined.binding_id,"client_ref":chosen,
                    "operation":args.get("operation").and_then(Value::as_str).unwrap_or("get")});
                if let Some(after) = args.get("after") {
                    request["after"] = after.clone();
                }
                if let Some(ids) = args.get("ack_ids") {
                    request["ack_ids"] = ids.clone();
                }
                self.ipc.call("get_messages", request).await
            }
            "voice_say" => {
                let named = args
                    .get("conversation")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty());
                if let Some(name) = named {
                    self.adopt_by_id(name, &client).await;
                }
                let status = self.ipc.call("status", json!({})).await?;
                self.refresh_binding_ids(&status).await;
                if status.get("refused").and_then(Value::as_str).is_some() {
                    bail!("{}", closed_note("connector_revoked"));
                }
                let closed_now = status
                    .get("closed_by_room")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let closed_reasons = status
                    .get("closed_reasons")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let closed_name = named
                    .filter(|name| closed_now.iter().any(|item| item.as_str() == Some(*name)))
                    .map(str::to_owned)
                    .or_else(|| {
                        (named.is_none()
                            && status
                                .pointer("/bindings")
                                .and_then(Value::as_array)
                                .is_none_or(Vec::is_empty))
                        .then(|| closed_now.iter().find_map(Value::as_str).map(str::to_owned))
                        .flatten()
                    });
                if let Some(name) = closed_name {
                    self.ipc.forget_registration(&name).await;
                    self.joined.lock().await.remove(&name);
                    let reason = closed_reasons
                        .get(&name)
                        .and_then(Value::as_str)
                        .unwrap_or("closed_from_room");
                    bail!("{}", closed_note(reason));
                }
                if status.get("resume_blocked") == Some(&json!(true))
                    && !self.has_active_binding(&status, named).await
                {
                    bail!(
                        "{}",
                        closed_note(
                            status
                                .pointer("/resume_block_reason")
                                .and_then(Value::as_str)
                                .unwrap_or("conversation_history_limit")
                        )
                    );
                }
                let text = args
                    .get("text")
                    .and_then(Value::as_str)
                    .context("text required")?;
                let event_id = args.get("event_id").and_then(Value::as_str);
                let utterance_id = args.get("utterance_id").and_then(Value::as_str);
                if event_id.is_some() != utterance_id.is_some()
                    || event_id.is_some_and(str::is_empty)
                    || utterance_id.is_some_and(str::is_empty)
                {
                    bail!("voice_say retry requires nonempty event_id and utterance_id together");
                }
                let supplied_session = args.get("session_id").and_then(Value::as_str);
                let named_binding = if let Some(name) = named {
                    self.joined.lock().await.get(name).cloned()
                } else {
                    None
                };
                let initial = named_binding
                    .as_ref()
                    .map(|joined| &joined.initial_reply_context);
                let (session_id, revision, epoch) = if let (
                    Some(session),
                    Some(revision),
                    Some(epoch),
                ) = (
                    supplied_session,
                    args.get("revision").and_then(Value::as_i64),
                    args.get("epoch").and_then(Value::as_u64),
                ) {
                    (session.to_owned(), revision, epoch)
                } else if supplied_session.is_none()
                    && args.get("revision").is_none()
                    && args.get("epoch").is_none()
                {
                    let context = initial.context("voice_say needs the message's session_id, revision and epoch, or a conversation with initial_reply_context")?;
                    (
                        context["session_id"]
                            .as_str()
                            .context("initial session_id missing")?
                            .to_owned(),
                        context["revision"]
                            .as_i64()
                            .context("initial revision missing")?,
                        context["epoch"].as_u64().context("initial epoch missing")?,
                    )
                } else {
                    bail!("voice_say needs the message's session_id, revision and epoch together");
                };
                let turn_route = self.several_chats() && named.is_none();
                let mut publish = json!({"session_id":session_id,"revision":revision,"epoch":epoch,"text":text,
                    "event_id":args.get("event_id"),"utterance_id":args.get("utterance_id"),"language":args.get("language")});
                if turn_route {
                    publish["client_refs"] =
                        json!(self.joined.lock().await.keys().cloned().collect::<Vec<_>>());
                    publish["adopt_orphans"] = json!(true);
                } else {
                    let chosen = self.pick(named).await?.context("not connected")?;
                    let joined = self
                        .joined
                        .lock()
                        .await
                        .get(&chosen)
                        .cloned()
                        .context("not connected")?;
                    publish["binding_id"] = json!(joined.binding_id);
                    publish["client_ref"] = json!(chosen);
                }
                let result = match self.ipc.call("publish", publish).await {
                    Ok(result) => result,
                    Err(error) => {
                        let detail = error.to_string();
                        if let Some(rest) = detail.strip_prefix("CLOSED_BY_ROOM:") {
                            let mut parts = rest.splitn(2, ':');
                            let reason = parts.next().unwrap_or("closed_from_room");
                            if let Some(name) = parts.next() {
                                self.ipc.forget_registration(name).await;
                                self.joined.lock().await.remove(name);
                            }
                            bail!("{}", closed_note(reason));
                        }
                        return Err(error);
                    }
                };
                let adopted = result
                    .pointer("/adopted/client_ref")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if let Some(adopted) = &adopted {
                    self.adopt_by_id(adopted, &client).await;
                }
                if result.get("text_saved") == Some(&json!(true)) {
                    Ok(
                        json!({"status":"published","text_saved":true,"audio":result.get("status"),"reason":result.get("reason")}),
                    )
                } else {
                    if adopted.is_some() {
                        let mut response = result;
                        if let Some(fields) = response.as_object_mut() {
                            fields.remove("adopted");
                        }
                        Ok(response)
                    } else {
                        Ok(result)
                    }
                }
            }
            "voice_disconnect" => {
                let named = args
                    .get("conversation")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty());
                if let Some(name) = named {
                    self.adopt_by_id(name, &client).await;
                }
                if self.several_chats() && named.is_none() {
                    bail!("Several chats of this window may be joined; pass the conversation id voice_connect returned in this chat.");
                }
                let chosen = self.pick(named).await?.context("not connected")?;
                let id = self
                    .joined
                    .lock()
                    .await
                    .get(&chosen)
                    .map(|binding| binding.binding_id.clone())
                    .context("not connected")?;
                let result = self
                    .ipc
                    .call("unregister", json!({"binding_id":id,"client_ref":chosen}))
                    .await?;
                self.joined.lock().await.remove(&chosen);
                Ok(
                    json!({"status":"left","conversation":chosen,"room_reachable":result.get("connected")}),
                )
            }
            "voice_pair_device" => {
                let answer =
                    crate::pairing::device_answer(self.ipc.call("pair_device", json!({})).await?)?;
                Ok(Value::String(crate::pairing::device_text(&answer)?))
            }
            "voice_pair" => {
                let room = args
                    .get("room")
                    .and_then(Value::as_str)
                    .filter(|room| !room.is_empty())
                    .context(
                        "voice_pair needs the room's address and the code the user read from it.",
                    )?;
                let code = args
                    .get("code")
                    .and_then(Value::as_str)
                    .filter(|code| !code.is_empty())
                    .context(
                        "voice_pair needs the room's address and the code the user read from it.",
                    )?;
                let previous = crate::pairing::previous_room(&self.profile);
                let redeemed =
                    crate::pairing::pair(&self.profile, room, &code.trim().to_uppercase()).await?;
                let origin = redeemed.origin.as_str();
                let connector_id = redeemed.connector_id.as_str();
                let active = !self.joined.lock().await.is_empty();
                let mut paired = json!({"status":"paired","room":origin,"connector_id":connector_id,
                    "next":if active {"Conversations already joined stay joined; the room reaches them within seconds."}
                        else {"Call voice_connect to join."}});
                if let Some(previous) = previous.filter(|previous| previous != origin) {
                    paired["replaced"] = json!(previous);
                    paired["note"] = json!(format!("Every conversation on this machine is reached through {origin} now, not {previous}."));
                }
                Ok(paired)
            }
            _ => bail!("unknown tool"),
        }
    }

    async fn pick(&self, named: Option<&str>) -> Result<Option<String>> {
        let joined = self.joined.lock().await;
        if let Some(name) = named {
            if joined.contains_key(name) {
                return Ok(Some(name.to_owned()));
            }
            bail!("unknown conversation");
        }
        if joined.len() == 1 {
            return Ok(joined.keys().next().cloned());
        }
        if joined.is_empty() {
            return Ok(None);
        }
        let titles = joined
            .values()
            .map(|binding| format!("\"{}\"", binding.title))
            .collect::<Vec<_>>()
            .join(", ");
        bail!("Several chats of this window are joined ({titles}); pass conversation: the id voice_connect returned in this chat.")
    }

    /// A conversation another façade of this client left, taken over by its id: only one of a harness that draws
    /// views and adopts it.
    async fn adopt_by_id(&self, name: &str, client: &Value) {
        let Some(harness) = self.registry.client(client).cloned() else {
            return;
        };
        if !self.several_chats() || !harness.adopts(Some(name)) {
            return;
        }
        if self.joined.lock().await.contains_key(name) {
            return;
        }
        let Ok(result) = self.ipc.call("adopt", json!({"client_ref":name})).await else {
            return;
        };
        let Some(binding_id) = result.get("binding_id").and_then(Value::as_str) else {
            return;
        };
        let Some((identity, title)) = harness.adopted_identity(name, &result) else {
            return;
        };
        let capabilities = identity.capabilities.clone();
        let experimental = identity.experimental.clone();
        let registration = json!({"client_ref":name,"harness":identity.harness,"thread":identity.thread,
            "title":title,"delivery":identity.delivery,"route":identity.route,"inbound":identity.inbound,
            "capabilities":capabilities,"experimental":experimental,"detachable":true,
            "engine":result.get("engine").cloned().unwrap_or(Value::Null),
            "input_mode":result.get("input_mode").cloned().unwrap_or(json!("push"))});
        self.ipc.remember_registration(registration).await;
        self.joined.lock().await.insert(
            name.to_owned(),
            Joined {
                binding_id: binding_id.to_owned(),
                input_mode: result["input_mode"].as_str().unwrap_or("push").to_owned(),
                initial_reply_context: result
                    .get("initial_reply_context")
                    .cloned()
                    .unwrap_or(Value::Null),
                title,
                identity,
                capabilities,
                experimental,
            },
        );
    }
}

fn tools() -> Vec<Tool> {
    let spec = [
        ("voice_connect", "Connect this conversation to the voice room. Only on an explicit request to join or enable voice. Choose input_mode pull to read the ordinary queue with get_messages; otherwise input is injected. Pull learns messages only when called. The reply includes the core's initial_reply_context for speaking first.", json!({"title":{"type":"string","description":"Short label for this conversation in the room"},"room":{"type":"string","description":"The room's address (https://…) when the user names one; omitted, the room this machine is paired with"},"input_mode":{"type":"string","description":"push (injection, default) or pull (get_messages)"}}), vec![]),
        ("voice_pair", "Pair this machine with a room using the one-time code the user read from the room's interface (\"Emparejar máquina\"). Only with a code the user gave you; one room per machine, a new pairing replaces the previous one.", json!({"room":{"type":"string","description":"The room's address (https://…)"},"code":{"type":"string","description":"The one-time pairing code shown by the room"}}), vec!["room","code"]),
        ("voice_say", "Publish a spoken reply with the session_id, revision and epoch from the message header. To speak first, pass conversation and use the initial_reply_context from voice_connect. For an idempotent retry, reuse both event_id and utterance_id unchanged. A current open user turn is refused immediately: wait for its message. A saved reply is not proof it was heard.", json!({"text":{"type":"string"},"session_id":{"type":"string"},"revision":{"type":"integer","minimum":0},"epoch":{"type":"integer","minimum":0},"conversation":{"type":"string","description":"The conversation id voice_connect returned in this chat: to speak first"},"event_id":{"type":"string","description":"Supply with utterance_id and reuse both unchanged on retry"},"utterance_id":{"type":"string","description":"Supply with event_id and reuse both unchanged on retry"},"language":{"type":"string","description":"The language of the reply, as a code: es, en, fr, it, pt or hi are the ones known now; the room decides what it speaks"}}), vec!["text"]),
        ("get_messages", "Read this conversation's ordered ordinary input queue in pull mode. Messages include stable IDs, full notice evidence, an envelope, and a journal position. Pass ack_ids only after consuming them. Without an acknowledgement, a claimed message is returned again; after is a journal position, not an acknowledgement. Pull may learn of speech later than injection.", json!({"conversation":{"type":"string","description":"Conversation id when several chats are joined"},"operation":{"type":"string","description":"get (default) or check"},"after":{"type":"integer","minimum":0},"ack_ids":{"type":"array","items":{"type":"string"}}}), vec![]),
        ("voice_disconnect", "Leave the voice room. The conversation and its work continue in writing.", json!({"conversation":{"type":"string","description":"Only when several chats of this window are joined: the conversation id voice_connect returned in this chat"}}), vec![]),
        ("voice_pair_device", "Show a one-time code to pair a device (the desktop app or a browser) with this machine: the code, a QR of it and how long it is valid. Only when the user asks to pair a device, never on your own initiative; show the user the result as it is.", json!({}), vec![]),
        ("voice_status", "Whether the room can currently reach this conversation.", json!({"conversation":{"type":"string","description":"Only when several chats of this window are joined: the conversation id voice_connect returned in this chat"}}), vec![]),
    ];
    // No `additionalProperties: false`: an argument this server does not know (an agent's own, or one a newer server
    // added) is ignored, never refused, by the server or by a client that checks calls against the schema.
    spec.into_iter()
        .map(|(name, description, properties, required)| {
            let schema = json!({"type":"object","properties":properties,"required":required});
            Tool::new(name, description, schema.as_object().unwrap().clone())
        })
        .collect()
}

fn prompt_text(args: Option<&serde_json::Map<String, Value>>) -> String {
    let title = args
        .and_then(|values| values.get("title"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let title = if title.is_empty() {
        "a short label of what this conversation is about".to_owned()
    } else {
        serde_json::to_string(title)
            .unwrap_or_else(|_| "a short label of what this conversation is about".into())
    };
    format!("Join the voice room for this conversation and keep it reachable.\n\n1. Call voice_status. If it reports joined and room_reachable (or local_only), say so in one line and stop.\n2. Call voice_connect with the title {title}. If the user named a room, pass its address as room.\n   If it returns local_only, the machine has no room: relay that note in one line and continue.\n   If it fails saying this machine is not paired with the room (or is paired with a different one), ask the user for the room's address and the one-time code the room shows them under \"Emparejar máquina\"; call voice_pair with both, then voice_connect again. Never try to get a code from the room yourself.\n3. Tell the user in one line whether the room can reach this conversation. If inbound.ok is false, relay inbound.reason and offer inbound.remedy in your own words, including what safeguard it removes; change nothing yourself.\n\nNothing else is registered: the room learns that a message was read and whether this conversation is working from what the harness itself records about it. How to behave once joined is in this server's instructions.")
}

impl ServerHandler for Facade {
    fn get_info(&self) -> ServerConfig {
        let mut capabilities = ServerCapabilities::default();
        capabilities.tools = Some(Default::default());
        capabilities.prompts = Some(Default::default());
        ServerConfig::new(capabilities)
            .with_server_info(Implementation::new(
                "sidevoice",
                self.profile.connector_version(),
            ))
            .with_instructions(INSTRUCTIONS)
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<InitializeResult, McpError> {
        let client = serde_json::to_value(&request.client_info).unwrap_or_else(|_| json!({}));
        let client_caps = serde_json::to_value(&request.capabilities).unwrap_or_else(|_| json!({}));
        let several = self
            .registry
            .client(&client)
            .is_some_and(|harness| harness.hosts_several_chats(&client_caps));
        self.several_chats.store(several, Ordering::Relaxed);
        context.peer.set_peer_info(request.clone());
        self.negotiate_initialize(&request)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tools().into_iter().find(|tool| tool.name == name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, McpError> {
        let args = Value::Object(request.arguments.unwrap_or_default());
        let meta = call_meta(&context.meta, request.meta);
        let client = context
            .client_info()
            .and_then(|value| serde_json::to_value(value).ok())
            .unwrap_or_else(|| json!({}));
        let client_caps = context
            .client_capabilities()
            .and_then(|value| serde_json::to_value(value).ok())
            .unwrap_or_else(|| json!({}));
        let result = self
            .invoke(&request.name, args, meta, client, client_caps)
            .await;
        let value = match result {
            Ok(Value::String(text)) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Ok(value) => CallToolResult::success(vec![ContentBlock::text(value.to_string())]),
            Err(error) => CallToolResult::error(vec![ContentBlock::text(error.to_string())]),
        };
        Ok(value.into())
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListPromptsResult, McpError> {
        let prompt: Prompt = serde_json::from_value(json!({"name":"voice-room","description":"Join the user's Sidevoice voice room with this conversation.","arguments":[{"name":"title","description":"Title for this conversation in the room","required":false}]}))
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(ListPromptsResult::with_all_items(vec![prompt]))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<GetPromptResponse, McpError> {
        if request.name != "voice-room" {
            return Err(McpError::invalid_params("unknown prompt", None));
        }
        let message = prompt_text(request.arguments.as_ref());
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(Role::User, message)]).into())
    }
}

/// Set by the agents' health check (`agents.rs`): the server is started only for the handshake and the tool list,
/// neither of which reaches the daemon, so it must not start one (or fail because a person stopped it).
pub const HEALTH_CHECK: &str = "SIDEVOICE_HEALTH_CHECK";

pub async fn run(profile: Profile) -> Result<()> {
    if std::env::var_os(HEALTH_CHECK).is_none() {
        crate::service::ensure_connector(&profile).await?;
    }
    // rmcp reads the client's identity into closed types (an icon's `theme` is `light` or `dark` only), and a value
    // it does not know fails the whole `initialize`, or drops a call's client info. Each message passes through
    // `tolerant_message` first, which keeps of the client info only what Sidevoice reads.
    let (to_rmcp, from_agent) = tokio::io::duplex(1 << 20);
    tokio::spawn(forward_tolerantly(tokio::io::stdin(), from_agent));
    let service = Facade::new(profile)
        .serve((to_rmcp, tokio::io::stdout()))
        .await?;
    service.waiting().await?;
    Ok(())
}

/// The agent's messages, one JSON-RPC message per line, each through `tolerant_message`, until the agent closes them.
async fn forward_tolerantly(
    agent: impl tokio::io::AsyncRead + Unpin,
    mut rmcp: impl tokio::io::AsyncWrite + Unpin,
) {
    use tokio::io::AsyncBufReadExt;
    let mut reader = BufReader::new(agent);
    let mut line = Vec::new();
    loop {
        line.clear();
        if !matches!(reader.read_until(b'\n', &mut line).await, Ok(1..)) {
            break;
        }
        if rmcp.write_all(&tolerant_message(&line)).await.is_err() {
            break;
        }
    }
}

/// One line from the agent as rmcp is to read it: a client info (`initialize`'s `clientInfo`, a call's
/// `_meta["io.modelcontextprotocol/clientInfo"]`) is kept to the text fields Sidevoice reads, its name and version
/// among them, so that nothing a newer client adds to it (an icon theme rmcp does not know) refuses the message.
/// Anything that is not a JSON object passes as it came.
fn tolerant_message(line: &[u8]) -> Vec<u8> {
    let Ok(mut message) = serde_json::from_slice::<Value>(line) else {
        return line.to_vec();
    };
    let Some(params) = message.get_mut("params") else {
        return line.to_vec();
    };
    let mut changed = false;
    for pointer in ["/clientInfo", "/_meta/io.modelcontextprotocol~1clientInfo"] {
        if let Some(info) = params.pointer_mut(pointer) {
            let kept = client_info(info);
            if *info != kept {
                *info = kept;
                changed = true;
            }
        }
    }
    if !changed {
        return line.to_vec();
    }
    let mut out = message.to_string().into_bytes();
    out.push(b'\n');
    out
}

/// A client's identity, as the protocol defines it (`Implementation`), kept to its text fields; one without a name
/// or a version is left for rmcp to judge.
fn client_info(raw: &Value) -> Value {
    if !["name", "version"]
        .iter()
        .all(|key| raw.get(key).is_some_and(Value::is_string))
    {
        return raw.clone();
    }
    let kept = ["name", "title", "version", "description", "websiteUrl"]
        .iter()
        .filter_map(|key| {
            raw.get(key)
                .filter(|value| value.is_string())
                .map(|value| (key.to_string(), value.clone()))
        })
        .collect();
    Value::Object(kept)
}

/// A tool call's `_meta`, as the agent sent it. rmcp moves the request's `_meta` into its context before the handler
/// runs (`context.meta`), so that is where an agent's per-call identity is (a harness may name the conversation in it
/// on every call). What the params still carry is merged over it.
fn call_meta(context: &impl serde::Serialize, params: Option<impl serde::Serialize>) -> Value {
    let mut meta = serde_json::to_value(context)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    if let (Some(meta), Some(Value::Object(params))) = (
        meta.as_object_mut(),
        params.and_then(|params| serde_json::to_value(params).ok()),
    ) {
        meta.extend(params);
    }
    meta
}

#[cfg(test)]
mod tests {
    use super::{closed_note, ipc_limit, pairing_needed, tools};
    use rmcp::model::InitializeRequestParams;
    use serde_json::{json, Value};
    use std::time::Duration;

    /// Whether a client that checks a call against `schema` (the keywords these schemas use: `required`, each
    /// property's `type` and `enum`, `additionalProperties: false`) lets `arguments` through.
    fn schema_lets_through(schema: &serde_json::Map<String, Value>, arguments: &Value) -> bool {
        let arguments = arguments.as_object().unwrap();
        let properties = schema["properties"].as_object().unwrap();
        let required = schema["required"].as_array().unwrap();
        required
            .iter()
            .all(|name| arguments.contains_key(name.as_str().unwrap()))
            && arguments
                .iter()
                .all(|(name, value)| match properties.get(name) {
                    None => schema.get("additionalProperties") != Some(&json!(false)),
                    Some(property) => {
                        let typed = match property["type"].as_str() {
                            Some("string") => value.is_string(),
                            Some("integer") => value.is_i64(),
                            _ => true,
                        };
                        typed
                            && property["enum"]
                                .as_array()
                                .is_none_or(|values| values.contains(value))
                    }
                })
    }

    /// No tool refuses an argument it does not know, or a value a closed list would not name, in the server or in a
    /// client that checks calls against the schema: a reply in a language the room learns later goes through.
    #[test]
    fn no_tool_schema_refuses_an_argument_or_a_value_it_does_not_know() {
        for tool in tools() {
            assert_eq!(
                tool.input_schema.get("additionalProperties"),
                None,
                "{}",
                tool.name
            );
            for (name, property) in tool.input_schema["properties"].as_object().unwrap() {
                assert_eq!(property.get("enum"), None, "{}.{name}", tool.name);
            }
        }
        let say = tools()
            .into_iter()
            .find(|tool| tool.name == "voice_say")
            .unwrap();
        for arguments in [
            json!({"text":"hola","session_id":"s","revision":1,"language":"es"}),
            json!({"text":"hello","session_id":"s","revision":1,"language":"tlh","future":{"nested":1}}),
        ] {
            assert!(
                schema_lets_through(&say.input_schema, &arguments),
                "{arguments}"
            );
        }
        assert!(!schema_lets_through(
            &say.input_schema,
            &json!({"session_id":"s"})
        ));
    }

    /// rmcp reads a client info into closed types: an icon theme it does not know refuses the whole `initialize`
    /// and drops a call's client info. Each line is kept to the client info's text fields before rmcp reads it;
    /// anything else passes as it came.
    #[test]
    fn a_client_info_rmcp_would_refuse_is_kept_to_what_sidevoice_reads() {
        let client = json!({"name":"an-agent","version":"0.99.0","title":"An agent",
            "icons":[{"src":"https://example.invalid/i.png","theme":"high-contrast"}],"future":{"nested":true}});
        let initialize =
            json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":client});
        assert!(serde_json::from_value::<InitializeRequestParams>(initialize.clone()).is_err());
        let line =
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":initialize}).to_string();
        let read: Value =
            serde_json::from_slice(&super::tolerant_message(line.as_bytes())).unwrap();
        assert_eq!(
            read["params"]["clientInfo"],
            json!({"name":"an-agent","version":"0.99.0","title":"An agent"})
        );
        let params: InitializeRequestParams =
            serde_json::from_value(read["params"].clone()).unwrap();
        assert_eq!(
            (
                params.client_info.name.as_str(),
                params.client_info.version.as_str()
            ),
            ("an-agent", "0.99.0")
        );

        let call =
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"voice_status",
            "_meta":{"io.modelcontextprotocol/clientInfo":client,"x-other":1}}})
            .to_string();
        let read: Value =
            serde_json::from_slice(&super::tolerant_message(call.as_bytes())).unwrap();
        assert_eq!(
            read["params"]["_meta"],
            json!({"io.modelcontextprotocol/clientInfo":{"name":"an-agent","version":"0.99.0","title":"An agent"},
                "x-other":1})
        );

        for untouched in [
            b"not json\n".to_vec(),
            b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n".to_vec(),
            format!("{}\n", json!({"jsonrpc":"2.0","id":3,"method":"initialize","params":{"clientInfo":{"name":"x"}}}))
                .into_bytes(),
        ] {
            assert_eq!(super::tolerant_message(&untouched), untouched);
        }
    }
    /// A reason the room closes a conversation for that this connector does not know reads as the room's close.
    #[test]
    fn an_unknown_close_reason_reads_as_closed_from_the_room() {
        assert_eq!(
            closed_note("a_reason_from_the_future"),
            closed_note("closed_from_room")
        );
    }

    #[test]
    fn pair_device_gets_30s_everything_else_keeps_20s() {
        assert_eq!(ipc_limit("pair_device"), Duration::from_secs(30));
        for method in ["register", "status", "publish", "unregister", "adopt"] {
            assert_eq!(ipc_limit(method), Duration::from_secs(20), "{method}");
        }
    }

    #[test]
    fn a_named_room_joins_only_when_this_machine_is_paired_with_it() {
        assert_eq!(
            pairing_needed(
                "https://Room.example/x",
                Some("https://room.example".into())
            ),
            None
        );
        let unpaired = pairing_needed("https://room.example", None).unwrap();
        assert!(
            unpaired.contains("not paired with the room at https://room.example"),
            "{unpaired}"
        );
        let other =
            pairing_needed("https://other.example", Some("https://room.example".into())).unwrap();
        assert!(
            other.starts_with(
                "This machine is paired with https://room.example, not https://other.example"
            ),
            "{other}"
        );
        assert!(pairing_needed("room", None)
            .unwrap()
            .contains("is not a room address"));
    }
}
