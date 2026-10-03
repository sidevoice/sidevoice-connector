use crate::proof::{verify_socket, Profile};
use anyhow::{bail, Context, Result};
use rmcp::{model::*, service::RequestContext, transport::stdio, ErrorData as McpError, RoleServer, ServerHandler, ServiceExt};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::{timeout, Duration};

const INSTRUCTIONS: &str = "Sidevoice connects this conversation to the user's voice room.\n- Call voice_connect only when the user asks to join the room or enable voice; never as a side effect.\n- Voice input is a user message: a JSON header ({\"channel\":\"voice\",\"session_id\",\"revision\",\"message_id\"}), the user's literal words, then a [Sidevoice] line that is not the user's. A repeated message_id is a redelivery: do not act on it again.\n- Reply by voice with voice_say, using that message's session_id and revision for every publication. For substantive work: first a short acknowledgement, then meaningful checkpoints, then the result.\n- Between steps, take in newly arrived user input before starting the next step.\n- 'published' means the room stored it, not that the user heard it. If publishing fails, continue in writing.\n- Read receipts and working state come from the Codex transcript; accepted delivery is not a read receipt.\n- This is an isolated Codex proof. Pairing a room is unavailable here.";

struct Ipc {
    profile: Profile,
    tx: Mutex<Option<mpsc::Sender<Value>>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    registrations: Mutex<HashMap<String, Value>>,
    serial: AtomicU64,
    connect_lock: Mutex<()>,
}

impl Ipc {
    fn new(profile: Profile) -> Arc<Self> {
        let client = Arc::new(Self { profile, tx: Mutex::new(None), pending: Mutex::new(HashMap::new()), registrations: Mutex::new(HashMap::new()), serial: AtomicU64::new(1), connect_lock: Mutex::new(()) });
        let supervisor = client.clone();
        tokio::spawn(async move { supervisor.supervise().await; });
        client
    }

    async fn ensure(self: &Arc<Self>) -> Result<()> {
        let _guard = self.connect_lock.lock().await;
        if self.tx.lock().await.is_some() { return Ok(()); }
        verify_socket(&self.profile.socket)?;
        let stream = timeout(Duration::from_secs(2), UnixStream::connect(&self.profile.socket)).await??;
        let (read, mut write) = stream.into_split();
        let (tx, mut rx) = mpsc::channel::<Value>(64);
        *self.tx.lock().await = Some(tx);
        let this = self.clone();
        tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if write.write_all(frame.to_string().as_bytes()).await.is_err() || write.write_all(b"\n").await.is_err() { break; }
            }
        });
        tokio::spawn(async move {
            let mut lines = BufReader::new(read).lines();
            loop {
                let line = match lines.next_line().await { Ok(Some(v)) if v.len() <= 1 << 20 => v, _ => break };
                let Ok(reply) = serde_json::from_str::<Value>(&line) else { break; };
                let Some(id) = reply.get("id").and_then(Value::as_u64) else { continue; };
                if let Some(waiting) = this.pending.lock().await.remove(&id) {
                    let result = if reply.get("ok") == Some(&json!(true)) { Ok(reply.get("result").cloned().unwrap_or(Value::Null)) }
                        else { Err(reply.get("error").and_then(Value::as_str).unwrap_or("IPC error").to_owned()) };
                    let _ = waiting.send(result);
                }
            }
            *this.tx.lock().await = None;
            for (_, waiter) in this.pending.lock().await.drain() { let _ = waiter.send(Err("connector went away".into())); }
        });
        Ok(())
    }

    async fn supervise(self: Arc<Self>) {
        let mut delay = 100;
        loop {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            if self.registrations.lock().await.is_empty() || self.tx.lock().await.is_some() { delay = 100; continue; }
            if self.ensure().await.is_ok() {
                let saved: Vec<_> = self.registrations.lock().await.values().cloned().collect();
                let mut all = true;
                for params in saved { if self.call("register", params).await.is_err() { all = false; break; } }
                if all { delay = 100; continue; }
            }
            delay = (delay * 2).min(5000);
        }
    }

    async fn call(self: &Arc<Self>, method: &str, params: Value) -> Result<Value> {
        self.ensure().await?;
        let id = self.serial.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let sender = self.tx.lock().await.clone().context("connector went away")?;
        sender.send(json!({"id":id,"method":method,"params":params})).await?;
        let answer = timeout(Duration::from_secs(20), rx).await;
        self.pending.lock().await.remove(&id);
        let value = answer.context("connector request timed out")??.map_err(anyhow::Error::msg)?;
        if method == "register" { if let Some(client_ref) = params.get("client_ref").and_then(Value::as_str) { self.registrations.lock().await.insert(client_ref.to_owned(), params.clone()); } }
        if method == "unregister" { if let Some(client_ref) = params.get("client_ref").and_then(Value::as_str) { self.registrations.lock().await.remove(client_ref); } }
        Ok(value)
    }
}

#[derive(Clone)]
struct Facade { ipc: Arc<Ipc>, joined: Arc<Mutex<HashMap<String, String>>> }

impl Facade {
    fn new(profile: Profile) -> Self { Self { ipc: Ipc::new(profile), joined: Arc::new(Mutex::new(HashMap::new())) } }

    async fn invoke(&self, name: &str, args: Value, meta: Value) -> Result<Value> {
        match name {
            "voice_connect" => {
                if args.get("room").and_then(Value::as_str).is_some() { bail!("Room selection is unavailable in the isolated Codex proof"); }
                let thread = thread_from_meta(&meta).or_else(|| std::env::var("CODEX_THREAD_ID").ok()).context("Codex did not provide a thread ID")?;
                let title = args.get("title").and_then(Value::as_str).unwrap_or("Voice conversation");
                let params = json!({"client_ref":thread,"harness":"codex","thread":thread,"title":title,"delivery":{"kind":"codex-queue","thread":thread}});
                let result = self.ipc.call("register", params).await?;
                let binding_id = result.get("binding_id").and_then(Value::as_str).context("connector omitted binding ID")?;
                self.joined.lock().await.insert(thread.clone(), binding_id.to_owned());
                Ok(json!({"status":if result.get("pending") == Some(&json!(true)) {"joining"} else {"joined"},
                    "harness":"codex","conversation":thread,"binding_id":binding_id,"delivery":"push","room_reachable":result.get("connected"),
                    "capabilities":{"deliver":"supported","inspectInbound":"unsupported","working":"supported","endOfTurn":"supported","sessionIdentity":"supported"},
                    "local_only":"This isolated proof uses the local Core and has no room pairing."}))
            }
            "voice_status" => {
                let chosen = self.pick(args.get("conversation").and_then(Value::as_str)).await?;
                let status = self.ipc.call("status", json!({})).await?;
                Ok(json!({"joined":chosen.is_some(),"conversation":chosen,"connector":status}))
            }
            "voice_say" => {
                let chosen = self.pick(args.get("conversation").and_then(Value::as_str)).await?.context("not connected")?;
                let text = args.get("text").and_then(Value::as_str).context("text required")?;
                let (session_id, revision) = if let (Some(s), Some(r)) = (args.get("session_id").and_then(Value::as_str), args.get("revision").and_then(Value::as_i64)) { (s.to_owned(), r) }
                    else { (format!("typed:{chosen}"), 0) };
                let binding_id = self.joined.lock().await.get(&chosen).cloned().context("not connected")?;
                let result = self.ipc.call("publish", json!({"binding_id":binding_id,"client_ref":chosen,"session_id":session_id,"revision":revision,"text":text,
                    "utterance_id":args.get("utterance_id"),"language":args.get("language")})).await?;
                if result.get("text_saved") == Some(&json!(true)) { Ok(json!({"status":"published","text_saved":true,"audio":result.get("status"),"reason":result.get("reason")})) }
                else { Ok(result) }
            }
            "voice_disconnect" => {
                let chosen = self.pick(args.get("conversation").and_then(Value::as_str)).await?.context("not connected")?;
                let id = self.joined.lock().await.remove(&chosen).context("not connected")?;
                let result = self.ipc.call("unregister", json!({"binding_id":id,"client_ref":chosen})).await?;
                Ok(json!({"status":"left","conversation":chosen,"room_reachable":result.get("connected")}))
            }
            "voice_pair_device" => self.ipc.call("pair_device", json!({})).await,
            "voice_pair" => bail!("Room pairing is unavailable in the isolated Codex proof"),
            _ => bail!("unknown tool"),
        }
    }

    async fn pick(&self, named: Option<&str>) -> Result<Option<String>> {
        let joined = self.joined.lock().await;
        if let Some(name) = named { if joined.contains_key(name) { return Ok(Some(name.to_owned())); } bail!("unknown conversation"); }
        if joined.len() == 1 { return Ok(joined.keys().next().cloned()); }
        if joined.is_empty() { return Ok(None); }
        bail!("several conversations joined; pass conversation")
    }
}

fn thread_from_meta(meta: &Value) -> Option<String> {
    let turn = meta.get("x-codex-turn-metadata").and_then(|v| if v.is_string() { serde_json::from_str::<Value>(v.as_str().unwrap()).ok() } else { Some(v.clone()) });
    let found = [meta.get("openai/threadId"), meta.get("openai/thread_id"), meta.get("codexThreadId"), meta.get("codex_thread_id"), turn.as_ref().and_then(|v| v.get("thread_id"))]
        .into_iter().flatten().find_map(|v| v.as_str().filter(|s| !s.is_empty()).map(str::to_owned));
    found
}

fn tools() -> Vec<Tool> {
    let spec = [
        ("voice_connect", "Connect this conversation to the voice room, only on an explicit user request.", json!({"title":{"type":"string"},"room":{"type":"string"}}), vec![]),
        ("voice_pair", "Pair this machine with a room using a code the user provided. Unavailable in this isolated proof.", json!({"room":{"type":"string"},"code":{"type":"string"}}), vec!["room","code"]),
        ("voice_say", "Publish a concise spoken reply using the voice message session_id and revision.", json!({"text":{"type":"string"},"session_id":{"type":"string"},"revision":{"type":"integer","minimum":0},"conversation":{"type":"string"},"utterance_id":{"type":"string"},"language":{"type":"string","enum":["es","en","fr","it","pt","hi"]}}), vec!["text"]),
        ("voice_disconnect", "Leave the voice room; continue the conversation in writing.", json!({"conversation":{"type":"string"}}), vec![]),
        ("voice_pair_device", "Show a one-time device pairing code only when the user asks.", json!({}), vec![]),
        ("voice_status", "Report whether this conversation is joined and reachable.", json!({"conversation":{"type":"string"}}), vec![]),
    ];
    spec.into_iter().map(|(name, description, properties, required)| {
        let schema = json!({"type":"object","properties":properties,"required":required,"additionalProperties":false});
        Tool::new(name, description, schema.as_object().unwrap().clone())
    }).collect()
}

impl ServerHandler for Facade {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_prompts().build())
            .with_server_info(Implementation::new("sidevoice", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(&self, _request: Option<PaginatedRequestParams>, _context: RequestContext<RoleServer>) -> std::result::Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> { tools().into_iter().find(|tool| tool.name == name) }

    async fn call_tool(&self, request: CallToolRequestParams, _context: RequestContext<RoleServer>) -> std::result::Result<CallToolResponse, McpError> {
        let args = Value::Object(request.arguments.unwrap_or_else(Map::new));
        let meta = request.meta.and_then(|m| serde_json::to_value(m).ok()).unwrap_or(json!({}));
        let result = self.invoke(&request.name, args, meta).await;
        let value = match result {
            Ok(value) => CallToolResult::success(vec![ContentBlock::text(value.to_string())]),
            Err(error) => CallToolResult::error(vec![ContentBlock::text(error.to_string())]),
        };
        Ok(value.into())
    }

    async fn list_prompts(&self, _request: Option<PaginatedRequestParams>, _context: RequestContext<RoleServer>) -> std::result::Result<ListPromptsResult, McpError> {
        let prompt: Prompt = serde_json::from_value(json!({"name":"voice-room","description":"Join the user's Sidevoice voice room with this conversation.","arguments":[{"name":"title","required":false}]}))
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(ListPromptsResult::with_all_items(vec![prompt]))
    }

    async fn get_prompt(&self, request: GetPromptRequestParams, _context: RequestContext<RoleServer>) -> std::result::Result<GetPromptResponse, McpError> {
        if request.name != "voice-room" { return Err(McpError::invalid_params("unknown prompt", None)); }
        let title = request.arguments.as_ref().and_then(|v| v.get("title")).and_then(Value::as_str).unwrap_or("a short label");
        let message = format!("Join the voice room for this conversation. Call voice_status first. If it is not joined, call voice_connect with title {title:?}. Tell the user whether this isolated Core is reachable. Reply to later voice messages with voice_say using their session_id and revision.");
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(Role::User, message)]).into())
    }
}

pub async fn run(profile: Profile) -> Result<()> {
    let service = Facade::new(profile).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
