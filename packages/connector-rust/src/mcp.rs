use crate::adapters::{self, Identity};
use crate::proof::{verify_socket, Profile};
use anyhow::{bail, Context, Result};
use rmcp::{
    model::*, service::RequestContext, transport::stdio, ErrorData as McpError, RoleServer,
    ServerHandler, ServiceExt,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::{timeout, timeout_at, Duration, Instant};

const INSTRUCTIONS: &str = r#"Sidevoice connects this conversation to the user's voice room.
- Call voice_connect only when the user asks to join the room or enable voice; never as a side effect.
- Voice input is a user message: a JSON header ({"channel":"voice","session_id","revision","message_id"}), the user's literal words, then a [Sidevoice] line that is not the user's. The header is opaque reply metadata. A repeated message_id is a redelivery: do not act on it again.
- Reply by voice with voice_say, using that message's session_id and revision for every publication. For substantive work: first a short acknowledgement (what you understood, what you will do next), then meaningful checkpoints, then the result. No filler, no narrating tool calls. Publish questions too, and wait.
- Between steps, at each tool result, take in newly arrived user input before starting the next step: an addition, a refinement or a replacement, by its meaning. Drop obsolete work not yet started; keep what remains useful; say what you now understand. No artificial pauses.
- "published" means the room stored it, not that the user heard it. If publishing fails, continue in writing.
- If the user closes this conversation's voice from the room, voice_say fails saying so: continue in writing, do not retry, and call voice_connect again only if asked.
- Pairing is the user's act. If voice_connect says this machine is not paired with the room, ask the user for the room's address and the one-time code the room shows under "Emparejar máquina", then call voice_pair and voice_connect again. Never try to obtain a code from the room yourself.
- If voice_connect returns inbound.ok false, voice will look sent and never arrive: tell the user inbound.reason, offer inbound.remedy in your own words including the safeguard the machine-wide option removes, and change no settings unasked.
- Read receipts and working state need nothing from you: the room observes what the harness records.
- Pairing a device is the user's act: voice_pair_device only when asked; show just its result."#;

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
                        Err(reply
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("IPC error")
                            .to_owned())
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
                }
            }
        }
        self.replayed_generation
            .store(generation, Ordering::Relaxed);
        Ok(())
    }

    async fn call_ready(self: &Arc<Self>, method: &str, params: Value) -> Result<Value> {
        let deadline = Instant::now() + Duration::from_secs(20);
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
    ipc: Arc<Ipc>,
    joined: Arc<Mutex<HashMap<String, Joined>>>,
    cursor_views: Arc<AtomicBool>,
    card_reads: Arc<AtomicU64>,
}

#[derive(Clone)]
struct Joined {
    binding_id: String,
    title: String,
    identity: Identity,
    capabilities: Value,
    experimental: Vec<String>,
}

fn closed_note(reason: &str) -> &'static str {
    if reason == "connector_revoked" {
        "This machine's pairing was revoked from the room, so this conversation has no voice. Tell the user; to have voice again they must pair this machine with the one-time code the room shows under Emparejar máquina (voice_pair), and then you can call voice_connect. Continue in writing meanwhile."
    } else {
        "The user closed this conversation's voice channel from the room. Continue in writing and do not publish speech; call voice_connect again only if the user asks for voice."
    }
}

impl Facade {
    fn new(profile: Profile) -> Self {
        let joined = Arc::new(Mutex::new(HashMap::new()));
        Self {
            ipc: Ipc::new(profile, joined.clone()),
            joined,
            cursor_views: Arc::new(AtomicBool::new(false)),
            card_reads: Arc::new(AtomicU64::new(0)),
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
            }
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
                if args.get("room").and_then(Value::as_str).is_some() {
                    bail!("This isolated Rust proof is not paired with the requested room. Ask the user for the room's one-time code; pairing is not available in this proof.");
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
                let card_title = explicit_title.trim().chars().take(200).collect::<String>();
                if identity.harness == "cursor" {
                    adapters::cursor::refine_identity(&mut identity, &card_title).await;
                }
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
                    "capabilities":capabilities,"experimental":experimental,"detachable":identity.detachable
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
                    "delivery":if pushed {"push"} else {"none"},"room_reachable":result.get("connected"),
                    "capabilities":capabilities,"inbound":identity.inbound,
                    "local_only":"This machine is not paired with any room, so this conversation is reachable only from devices paired with this machine itself (the Sidevoice app on this computer, at this machine's own address). Tell the user in one line. Pairing the app is voice_pair_device, only if they ask; reaching it from elsewhere needs a room's address and code (voice_pair).",
                    "version":env!("CARGO_PKG_VERSION"),"connector_version":connector.get("version")
                });
                if !pushed {
                    response["voice_in"] = json!({"supported":false,"speak_with":{"session_id":format!("typed:{}", identity.thread),"revision":0},
                        "reason":format!("What the user says in the room cannot reach this conversation: {} The user types here as usual.", identity.deliver_note.as_deref().unwrap_or("this harness offers no way to put a message into it.")),
                        "how":format!("Reply by voice to what the user types, as the server's instructions say for voice messages, calling voice_say with session_id \"typed:{}\" and revision 0. Tell the user once that the room hears this conversation but cannot talk to it.", identity.thread)});
                }
                if let Some(note) = &identity.watch_note {
                    response["watch_note"] = json!(note);
                }
                if !experimental.is_empty() {
                    response["experimental"] = json!(experimental);
                    response["experimental_notes"] = json!(experimental.iter().filter_map(|name| {
                        match (name.as_str(), identity.delivery["kind"].as_str()) {
                            ("deliver", Some("cursor-tmux")) => Some("Voice from the room reaches this conversation by an experimental route: it is pasted into this chat's terminal through tmux and sent with Enter. If the user is typing at the same moment, the two texts mix. Tell the user once."),
                            ("deliver", Some("cursor-app")) => Some("Voice from the room reaches this chat by an experimental route: the small Sidevoice card under this call submits it as if the user had typed it; it must stay open in this chat. If the user is typing at the same moment, the two may mix. Tell the user once."),
                            ("sessionIdentity", _) => Some("This conversation is identified by the Sidevoice card drawn in it, not by an id Cursor gives: each chat of this window that joins gets a card and a conversation of its own. Asking this same chat to join again gives it a second one; leave the old one with voice_disconnect and its conversation id."),
                            _ => None,
                        }
                    }).collect::<Vec<_>>());
                }
                if identity.editor {
                    response["view"] = json!(format!("Voice reaches this chat through the small Sidevoice card drawn under this call: it must stay open in this chat. Other chats of this window can join too, each with its own card. Remember this chat's conversation id ({}): voice_status and voice_disconnect need it when several chats are joined.", identity.thread));
                    response["speak_first"] = json!(format!("To speak before the user has said anything by voice, call voice_say with conversation \"{}\" and no session_id. To answer a voice message, use its session_id and revision as usual.", identity.thread));
                }
                if adapters::cursor::is_cursor_client(&client) {
                    response["card"] = json!({"requested":identity.editor && result.pointer("/prepared/port").is_some(),
                        "bridge":identity.route.as_deref() == Some("cursor-editor-bridge"),
                        "note":if identity.route.as_deref() == Some("cursor-editor-bridge") {"Cursor's Desktop Bridge is on: once Cursor has recorded this call (up to ~30 s), voice reaches this chat by its own id, on screen or not. Until then, and whenever the bridge cannot, it comes through the card under this call."}
                            else if identity.editor && result.pointer("/prepared/port").is_some() {"A Sidevoice card should appear under this call within seconds. If none does, Cursor did not draw it: tell the user, and that ~/.sidevoice/mcp.log says whether Cursor read the card (a \"resources/read\" line) — without the card, what they say in the room does not reach this chat."}
                            else if identity.editor {"The connector could not open the card's loopback bridge, so no card will work: tell the user to look at ~/.sidevoice/connector.log."}
                            else if !self.cursor_views.load(Ordering::Relaxed) {"This Cursor did not declare MCP Apps support in initialize, so it draws no card: what the user says in the room cannot reach this chat. ~/.sidevoice/mcp.log shows what it declared."}
                            else {"Cursor CLI: no card; voice reaches the chat only under cursor-agent persist."}});
                }
                if identity.delivery["kind"] == "cursor-app"
                    && result.pointer("/prepared/port").is_some()
                {
                    response["view_link"] = json!({"conversation":identity.thread,"port":result["prepared"]["port"],"key":identity.view_key});
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
                if named.is_none()
                    && self.cursor_views.load(Ordering::Relaxed)
                    && !self.joined.lock().await.is_empty()
                {
                    let conversations = self.joined.lock().await.values().map(|binding| json!({"title":binding.title,"harness":binding.identity.harness})).collect::<Vec<_>>();
                    return Ok(
                        json!({"joined":Value::Null,"conversation":Value::Null,"room_reachable":status.get("connected"),"connector":status,
                        "conversations":conversations,"closed_by_room":closed_now.iter().filter_map(|item| item.as_str()).map(|name| json!({
                            "title":closed_titles.get(name),"note":closed_note(closed_reasons.get(name).and_then(Value::as_str).unwrap_or("closed_from_room"))
                        })).collect::<Vec<_>>(),
                        "note":"Chats of this Cursor window are joined, and this call does not say which chat is asking. If voice_connect returned a conversation id in this chat, pass it as conversation to ask about this one; if it never did, this chat is not joined."}),
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
                    let delivery_state = daemon_binding
                        .and_then(|item| item.get("delivery_state"))
                        .cloned();
                    let mut response = json!({"joined":true,"conversation":name,"binding_id":binding.binding_id,"harness":binding.identity.harness,
                        "room_reachable":status.get("connected"),"connector":status,"capabilities":binding.capabilities,
                        "experimental":binding.experimental,"inbound":binding.identity.inbound});
                    if binding.identity.editor {
                        let state = delivery_state.unwrap_or_else(|| json!({}));
                        let reads = self.card_reads.load(Ordering::Relaxed);
                        response["card"] = json!({"html_read_by_cursor":reads,"card_connected":state.get("card_connected"),
                            "card_last_seen_ms_ago":state.get("card_last_seen_ms_ago"),"bridge_chat_known":state.get("bridge_chat_known"),
                            "note":if state.get("card_connected") == Some(&json!(true)) {"The card in this chat is connected."}
                                else if reads > 0 {"Cursor read the card but it has not connected to this machine's connector: it may not be on screen, or it failed to load."}
                                else {"Cursor never read the card, so it drew none: tell the user; ~/.sidevoice/mcp.log has what Cursor declared."}});
                        if let Some(port) = daemon_binding
                            .and_then(|item| item.get("cursor_app_port"))
                            .and_then(Value::as_u64)
                        {
                            response["view_link"] = json!({"conversation":name,"port":port,"key":binding.identity.view_key});
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
                let text = args
                    .get("text")
                    .and_then(Value::as_str)
                    .context("text required")?;
                let supplied_session = args.get("session_id").and_then(Value::as_str);
                let editor_views = self.cursor_views.load(Ordering::Relaxed);
                let is_typed =
                    supplied_session.is_some_and(|session| session.starts_with("typed:"));
                if editor_views && is_typed && named.is_none() {
                    bail!("To speak before the first voice message in a Cursor chat, pass the conversation id voice_connect returned in that chat.");
                }
                let direct_typed = named.is_some() && (supplied_session.is_none() || is_typed);
                let turn_route = editor_views && !direct_typed;
                let (session_id, revision) = if direct_typed {
                    (format!("typed:{}", named.unwrap()), 0)
                } else if let (Some(s), Some(r)) = (
                    args.get("session_id").and_then(Value::as_str),
                    args.get("revision").and_then(Value::as_i64),
                ) {
                    (s.to_owned(), r)
                } else {
                    bail!("voice_say needs the session_id and revision of the voice message you answer, or the conversation id from voice_connect to speak first")
                };
                let mut publish = json!({"session_id":session_id,"revision":revision,"text":text,
                    "utterance_id":args.get("utterance_id"),"language":args.get("language")});
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
                    if named.is_some() && !joined.identity.editor {
                        bail!("conversation is only for a chat of the Cursor editor: answer with the session_id and revision of the voice message.");
                    }
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
                if self.cursor_views.load(Ordering::Relaxed) && named.is_none() {
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
            "voice_pair_device" => self.ipc.call("pair_device", json!({})).await,
            "voice_pair" => {
                bail!("Room pairing is not available in this isolated Rust connector slice")
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

    async fn adopt_by_id(&self, name: &str, client: &Value) {
        if !self.cursor_views.load(Ordering::Relaxed)
            || !adapters::cursor::is_cursor_client(client)
            || !name.starts_with("cursor-editor-")
        {
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
        let delivery = result
            .get("delivery")
            .cloned()
            .unwrap_or_else(|| json!({"kind":"cursor-app","thread":name}));
        let identity = Identity {
            harness: result
                .get("harness")
                .and_then(Value::as_str)
                .unwrap_or("cursor")
                .to_owned(),
            thread: name.to_owned(),
            delivery,
            capability_overrides: json!({}),
            route: result
                .get("route")
                .and_then(Value::as_str)
                .map(str::to_owned),
            capabilities: result
                .get("capabilities")
                .cloned()
                .unwrap_or_else(|| adapters::advertised_capabilities("cursor")),
            experimental: result
                .get("experimental")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            inbound: result
                .get("inbound")
                .filter(|value| !value.is_null())
                .cloned(),
            editor: true,
            detachable: true,
            view_key: result
                .pointer("/delivery/key")
                .and_then(Value::as_str)
                .map(str::to_owned),
            deliver_note: None,
            watch_note: None,
        };
        let title = result
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Cursor conversation")
            .to_owned();
        let capabilities = result
            .get("capabilities")
            .cloned()
            .unwrap_or_else(|| adapters::advertised_capabilities("cursor"));
        let experimental = identity.experimental.clone();
        let registration = json!({"client_ref":name,"harness":identity.harness,"thread":identity.thread,
            "title":title,"delivery":identity.delivery,"route":identity.route,"inbound":identity.inbound,
            "capabilities":capabilities,"experimental":experimental,"detachable":true,
            "engine":result.get("engine").cloned().unwrap_or(Value::Null)});
        self.ipc.remember_registration(registration).await;
        self.joined.lock().await.insert(
            name.to_owned(),
            Joined {
                binding_id: binding_id.to_owned(),
                title,
                identity,
                capabilities,
                experimental,
            },
        );
    }
}

fn tools(cursor_views: bool) -> Vec<Tool> {
    let spec = [
        ("voice_connect", "Connect this conversation to the voice room. Only on an explicit request to join or enable voice. Fails, saying what to ask the user, when a room is named that this machine is not paired with; with no room paired it joins this machine only (local_only).", json!({"title":{"type":"string","description":"Short label for this conversation in the room"},"room":{"type":"string","description":"The room's address (https://…) when the user names one; omitted, the room this machine is paired with"}}), vec![]),
        ("voice_pair", "Pair this machine with a room using the one-time code the user read from the room's interface (\"Emparejar máquina\"). Only with a code the user gave you; one room per machine, a new pairing replaces the previous one.", json!({"room":{"type":"string","description":"The room's address (https://…)"},"code":{"type":"string","description":"The one-time pairing code shown by the room"}}), vec!["room","code"]),
        ("voice_say", "Publish a concise spoken version of your reply to the room, with the session_id and revision from the voice message header. To speak before any voice message arrived, pass instead the conversation id voice_connect returned in this chat.", json!({"text":{"type":"string"},"session_id":{"type":"string"},"revision":{"type":"integer","minimum":0},"conversation":{"type":"string","description":"The conversation id voice_connect returned in this chat: to speak with no voice message to answer"},"utterance_id":{"type":"string"},"language":{"type":"string","enum":["es","en","fr","it","pt","hi"]}}), vec!["text"]),
        ("voice_disconnect", "Leave the voice room. The conversation and its work continue in writing.", json!({"conversation":{"type":"string","description":"Only when several chats of this window are joined: the conversation id voice_connect returned in this chat"}}), vec![]),
        ("voice_pair_device", "Show a one-time code to pair a device (the desktop app or a browser) with this machine: the code, a QR of it and how long it is valid. Only when the user asks to pair a device, never on your own initiative; show the user the result as it is.", json!({}), vec![]),
        ("voice_status", "Whether the room can currently reach this conversation.", json!({"conversation":{"type":"string","description":"Only when several chats of this window are joined: the conversation id voice_connect returned in this chat"}}), vec![]),
    ];
    spec.into_iter().map(|(name, description, properties, required)| {
        let schema = json!({"type":"object","properties":properties,"required":required,"additionalProperties":false});
        let mut tool = Tool::new(name, description, schema.as_object().unwrap().clone());
        if cursor_views && name == "voice_connect" {
            tool.meta = Some(serde_json::from_value(json!({"ui":{"resourceUri":"ui://sidevoice/voice-link"},"ui/resourceUri":"ui://sidevoice/voice-link"})).unwrap());
        }
        tool
    }).collect()
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

const CURSOR_RESOURCE_URI: &str = "ui://sidevoice/voice-link";

fn cursor_resource() -> Value {
    json!({"uri":CURSOR_RESOURCE_URI,"name":"Sidevoice voice link","mimeType":"text/html;profile=mcp-app"})
}

impl ServerHandler for Facade {
    fn get_info(&self) -> ServerConfig {
        let mut capabilities = ServerCapabilities::default();
        capabilities.tools = Some(Default::default());
        capabilities.prompts = Some(Default::default());
        if self.cursor_views.load(Ordering::Relaxed) {
            capabilities.resources = Some(Default::default());
        }
        ServerConfig::new(capabilities)
            .with_server_info(Implementation::new("sidevoice", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<InitializeResult, McpError> {
        let client = serde_json::to_value(&request.client_info).unwrap_or_else(|_| json!({}));
        let client_caps = serde_json::to_value(&request.capabilities).unwrap_or_else(|_| json!({}));
        let cursor_views = adapters::cursor::is_cursor_client(&client)
            && adapters::cursor::draws_views(&client_caps);
        self.cursor_views.store(cursor_views, Ordering::Relaxed);
        context.peer.set_peer_info(request.clone());
        self.negotiate_initialize(&request)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(tools(
            self.cursor_views.load(Ordering::Relaxed),
        )))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tools(self.cursor_views.load(Ordering::Relaxed))
            .into_iter()
            .find(|tool| tool.name == name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, McpError> {
        let args = Value::Object(request.arguments.unwrap_or_default());
        let meta = request
            .meta
            .and_then(|m| serde_json::to_value(m).ok())
            .unwrap_or(json!({}));
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

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourcesResult, McpError> {
        let resources = if self.cursor_views.load(Ordering::Relaxed) {
            vec![serde_json::from_value(cursor_resource()).unwrap()]
        } else {
            Vec::new()
        };
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ReadResourceResponse, McpError> {
        if !self.cursor_views.load(Ordering::Relaxed) || request.uri != CURSOR_RESOURCE_URI {
            return Err(McpError::invalid_params("unknown resource", None));
        }
        self.card_reads.fetch_add(1, Ordering::Relaxed);
        let html = crate::cursor_app::resource_html();
        let meta: MetaObject = serde_json::from_value(
            json!({"ui":{"csp":{"connectDomains":["http://127.0.0.1:*"]},"prefersBorder":true}}),
        )
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        Ok(
            ReadResourceResult::new(vec![ResourceContents::text(html, CURSOR_RESOURCE_URI)
                .with_mime_type("text/html;profile=mcp-app")
                .with_meta(meta)])
            .into(),
        )
    }
}

pub async fn run(profile: Profile) -> Result<()> {
    crate::service::ensure_connector(&profile).await?;
    let service = Facade::new(profile).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
