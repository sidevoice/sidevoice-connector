use crate::link::{Incoming, Link};
use crate::proof::{atomic_json, private_dir, verify_socket, Profile};
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::Duration;

struct Binding {
    client_ref: String,
    thread: String,
    title: String,
    owner: u64,
    id: Mutex<String>,
    serial: Mutex<()>,
    pending: Mutex<HashMap<String, (String, i64)>>,
    read: Mutex<HashSet<String>>,
    stopped: AtomicBool,
}

pub struct Daemon {
    profile: Profile,
    link: Arc<Link>,
    bindings: Mutex<HashMap<String, Arc<Binding>>>,
    outbox: Mutex<Vec<Value>>,
    owner_serial: AtomicU64,
    rendezvous: Mutex<Option<Value>>,
}

impl Daemon {
    fn new(profile: Profile, link: Arc<Link>) -> Result<Arc<Self>> {
        let path = profile.data.join("outbox.json");
        let outbox = if path.exists() {
            let bytes = fs::read(&path)?;
            if bytes.len() > 8 << 20 { bail!("outbox too large"); }
            serde_json::from_slice::<Vec<Value>>(&bytes).context("invalid existing outbox")?
        } else { Vec::new() };
        Ok(Arc::new(Self { profile, link, bindings: Mutex::new(HashMap::new()), outbox: Mutex::new(outbox), owner_serial: AtomicU64::new(1), rendezvous: Mutex::new(None) }))
    }

    async fn register_core(&self, binding: &Arc<Binding>) -> Result<Value> {
        let current = binding.id.lock().await.clone();
        let mut frame = json!({"client_ref":binding.client_ref,"harness":"codex","thread":binding.thread,"title":binding.title,
            "inbound":{"ok":true}, "capabilities":{"deliver":"supported","inspectInbound":"unsupported","working":"supported","endOfTurn":"supported","sessionIdentity":"supported"},
            "experimental":[],"engine":null,"focus":false});
        if !current.starts_with("local-") { frame["binding_id"] = json!(current); }
        let reply = self.link.request("binding.register", frame, Duration::from_secs(10)).await?;
        if let Some(error) = reply.get("error") { bail!("Core binding refusal: {error}"); }
        let id = reply.get("binding_id").and_then(Value::as_str).context("Core binding ID missing")?;
        *binding.id.lock().await = id.to_owned();
        Ok(reply)
    }

    async fn on_welcome(self: &Arc<Self>) {
        let bindings: Vec<_> = self.bindings.lock().await.values().cloned().collect();
        for binding in bindings { if let Err(error) = self.register_core(&binding).await { eprintln!("[sidevoice rust proof] binding replay: {error}"); } }
        self.flush_outbox().await;
    }

    fn outbox_path(&self) -> PathBuf { self.profile.data.join("outbox.json") }

    async fn queue(&self, speech: Value) -> Result<()> {
        let mut outbox = self.outbox.lock().await;
        outbox.push(speech);
        if let Err(error) = atomic_json(&self.outbox_path(), &*outbox) { outbox.pop(); return Err(error); }
        Ok(())
    }

    fn durable(speech: &Value, reply: &Value) -> bool {
        let ids_match = ["event_id", "utterance_id"].iter().all(|key| speech.get(key).and_then(Value::as_str).is_some() && speech.get(key) == reply.get(key));
        ids_match && (reply.get("text_saved") == Some(&json!(true)) ||
            (reply.get("status") == Some(&json!("rejected")) && reply.get("terminal") == Some(&json!(true)) && reply.get("reason_code") == Some(&json!("application_refusal"))))
    }

    async fn publish_one(&self, speech: &Value) -> Result<Option<Value>> {
        if !self.link.connected().await { return Ok(None); }
        let result = self.link.request("speech.publish", speech.clone(), Duration::from_secs(15)).await;
        let reply = match result { Ok(v) => v, Err(error) => { eprintln!("[sidevoice rust proof] speech retry retained: {error}"); return Ok(None); } };
        if reply.get("status") == Some(&json!("unknown_binding")) {
            if let Some(binding) = self.binding_for_id(speech.get("binding_id").and_then(Value::as_str).unwrap_or("")).await {
                if self.register_core(&binding).await.is_ok() {
                    let new_id = binding.id.lock().await.clone();
                    let mut outbox = self.outbox.lock().await;
                    let before = outbox.clone();
                    for item in outbox.iter_mut().filter(|item| item.get("event_id") == speech.get("event_id")) { item["binding_id"] = json!(new_id); }
                    if let Err(error) = atomic_json(&self.outbox_path(), &*outbox) { *outbox = before; return Err(error); }
                }
            }
            return Ok(None);
        }
        if Self::durable(speech, &reply) {
            let event_id = speech.get("event_id");
            let mut outbox = self.outbox.lock().await;
            let before = outbox.clone();
            outbox.retain(|item| item.get("event_id") != event_id);
            if let Err(error) = atomic_json(&self.outbox_path(), &*outbox) { *outbox = before; return Err(error); }
        }
        if Self::durable(speech, &reply) { Ok(Some(reply)) } else { Ok(None) }
    }

    async fn flush_outbox(&self) {
        let items = self.outbox.lock().await.clone();
        for speech in items { let _ = self.publish_one(&speech).await; }
    }

    async fn binding_for_id(&self, id: &str) -> Option<Arc<Binding>> {
        let candidates: Vec<_> = self.bindings.lock().await.values().cloned().collect();
        for b in candidates { if *b.id.lock().await == id { return Some(b); } }
        None
    }

    async fn incoming(self: Arc<Self>, item: Incoming) {
        let answer = match item.method.as_str() {
            "connector.welcome" => { self.on_welcome().await; json!({}) }
            "input.deliver" => self.deliver(&item.params).await,
            "binding.close" => {
                let id = item.params.get("binding_id").and_then(Value::as_str).unwrap_or("");
                if let Some(binding) = self.binding_for_id(id).await {
                    binding.stopped.store(true, Ordering::Relaxed);
                    self.bindings.lock().await.remove(&binding.client_ref);
                }
                json!({})
            }
            "node.rendezvous" => { *self.rendezvous.lock().await = Some(item.params); json!({}) }
            "agents.list" | "agents.connect" | "agents.disconnect" | "agents.dismiss" => json!({"error":{"key":"agents.proof-only","message":"Host agent management is unavailable in the isolated Rust proof."}}),
            "pair.request" => json!({"error":{"key":"pair.proof-only","message":"Pairing is unavailable in the isolated Rust proof."}}),
            "connector.error" => json!({}),
            _ => json!({"error":{"key":"connector.unknown-method"}}),
        };
        if let Some(tx) = item.reply { let _ = tx.send(answer); }
    }

    async fn deliver(&self, frame: &Value) -> Value {
        let id = frame.get("binding_id").and_then(Value::as_str).unwrap_or("");
        let Some(binding) = self.binding_for_id(id).await else { return json!({"status":"unknown_binding"}); };
        let _serial = binding.serial.lock().await;
        let message_id = frame.get("message_id").and_then(Value::as_str).unwrap_or("");
        let session_id = frame.get("session_id").and_then(Value::as_str).unwrap_or("");
        let revision = frame.get("revision").and_then(Value::as_i64).unwrap_or(0);
        let text = frame.get("text").and_then(Value::as_str).unwrap_or("");
        if message_id.is_empty() || session_id.is_empty() || text.is_empty() { return json!({"status":"failed","detail":"invalid input"}); }
        binding.pending.lock().await.insert(message_id.to_owned(), (session_id.to_owned(), revision));
        let channel = frame.get("channel").and_then(Value::as_str).unwrap_or("voice");
        let header = json!({"channel":channel,"session_id":session_id,"revision":revision,"message_id":message_id});
        let note = if channel == "voice" { format!("\n\n[Sidevoice] Voice from the room: acknowledge with voice_say (session_id \"{session_id}\", revision {revision}) before any other tool, then work and reply by voice, as the sidevoice server's instructions say.") } else { String::new() };
        let envelope = format!("{header}\n\n{text}{note}");
        let binary = std::env::var("SIDEVOICE_CODEX_BIN").unwrap_or_else(|_| "codex".into());
        let output = tokio::time::timeout(Duration::from_secs(30), tokio::process::Command::new(binary)
            .env("CODEX_HOME", &self.profile.codex).args(["queue", "--thread", &binding.thread, "--message", &envelope]).output()).await;
        match output {
            Ok(Ok(result)) if result.status.success() => json!({"status":"accepted","detail":"codex queue confirmed the thread"}),
            _ => { binding.pending.lock().await.remove(message_id); json!({"status":"failed","detail":"codex queue failed"}) }
        }
    }

    async fn command(self: &Arc<Self>, owner: u64, method: &str, params: Value) -> Result<Value> {
        match method {
            "register" => {
                let client_ref = params.get("client_ref").and_then(Value::as_str).context("client_ref required")?.to_owned();
                let thread = params.get("thread").and_then(Value::as_str).context("thread required")?.to_owned();
                let title = params.get("title").and_then(Value::as_str).unwrap_or("").to_owned();
                if params.get("harness") != Some(&json!("codex")) || params.pointer("/delivery/kind") != Some(&json!("codex-queue")) { bail!("only Codex queued delivery is available in this proof"); }
                if params.pointer("/delivery/thread").and_then(Value::as_str) != Some(thread.as_str()) { bail!("delivery thread mismatch"); }
                if let Some(existing) = self.bindings.lock().await.get(&client_ref).cloned() {
                    if existing.owner != owner { bail!("binding belongs to another façade"); }
                    return Ok(json!({"binding_id":*existing.id.lock().await,"thread":thread,"connected":self.link.connected().await}));
                }
                let binding = Arc::new(Binding { client_ref: client_ref.clone(), thread: thread.clone(), title, owner,
                    id: Mutex::new(format!("local-{}", uuid::Uuid::new_v4())), serial: Mutex::new(()), pending: Mutex::new(HashMap::new()), read: Mutex::new(HashSet::new()), stopped: AtomicBool::new(false) });
                self.bindings.lock().await.insert(client_ref, binding.clone());
                self.clone().watch_rollout(binding.clone());
                let result = self.register_core(&binding).await;
                let id = binding.id.lock().await.clone();
                Ok(json!({"binding_id":id,"thread":thread,"connected":result.is_ok(),"pending":result.is_err()}))
            }
            "publish" => {
                let binding = self.find_owned(owner, &params).await?;
                let session_id = params.get("session_id").and_then(Value::as_str).context("session_id required")?;
                let revision = params.get("revision").and_then(Value::as_i64).context("revision required")?;
                let text = params.get("text").and_then(Value::as_str).context("text required")?;
                if text.is_empty() || text.len() > 65536 { bail!("text length invalid"); }
                let speech = json!({"event_id":params.get("event_id").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    "binding_id":*binding.id.lock().await,"session_id":session_id,"revision":revision,
                    "utterance_id":params.get("utterance_id").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    "text":text,"language":params.get("language")});
                self.queue(speech.clone()).await?;
                let answer = self.publish_one(&speech).await?;
                Ok(answer.unwrap_or_else(|| json!({"status":"queued","utterance_id":speech["utterance_id"]})))
            }
            "unregister" => {
                let binding = self.find_owned(owner, &params).await?;
                binding.stopped.store(true, Ordering::Relaxed);
                self.bindings.lock().await.remove(&binding.client_ref);
                let _ = self.link.notify("binding.unregister", json!({"binding_id":*binding.id.lock().await})).await;
                Ok(json!({"left":true,"connected":self.link.connected().await}))
            }
            "status" | "node.status" => Ok(json!({"version":env!("CARGO_PKG_VERSION"),"connected":self.link.connected().await,"room_reachable":self.rendezvous.lock().await.clone()})),
            "pair_device" => self.link.request("device.pairing_code", json!({}), Duration::from_secs(10)).await,
            "adopt" => Ok(json!({"adopted":false})),
            _ => bail!("unknown IPC method"),
        }
    }

    async fn find_owned(&self, owner: u64, params: &Value) -> Result<Arc<Binding>> {
        let candidates: Vec<_> = self.bindings.lock().await.values().cloned().collect();
        for b in candidates {
            let id = b.id.lock().await.clone();
            if (params.get("client_ref").and_then(Value::as_str) == Some(b.client_ref.as_str()) || params.get("binding_id").and_then(Value::as_str) == Some(id.as_str())) && b.owner == owner { return Ok(b); }
        }
        bail!("unknown binding")
    }

    fn watch_rollout(self: Arc<Self>, binding: Arc<Binding>) {
        tokio::spawn(async move {
            let mut path: Option<PathBuf> = None;
            let mut offset = 0usize;
            let mut turn: Option<String> = None;
            loop {
                if binding.stopped.load(Ordering::Relaxed) { break; }
                if path.is_none() { path = rollout_path(&self.profile.codex, &binding.thread); if let Some(file) = &path { offset = fs::metadata(file).map(|m| m.len() as usize).unwrap_or(0); } }
                if let Some(file) = &path {
                    if let Ok(bytes) = fs::read(file) {
                        if bytes.len() < offset { offset = bytes.len(); }
                        if bytes.len() > offset && bytes.len() - offset <= 1 << 20 {
                            let new = &bytes[offset..];
                            if let Some(last) = new.iter().rposition(|b| *b == b'\n') {
                                for line in new[..=last].split(|b| *b == b'\n') {
                                    if let Ok(item) = serde_json::from_slice::<Value>(line) { self.observe_rollout(&binding, &item, &mut turn).await; }
                                }
                                offset += last + 1;
                            }
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        });
    }

    async fn observe_rollout(&self, binding: &Binding, item: &Value, turn: &mut Option<String>) {
        let id = binding.id.lock().await.clone();
        if item.get("type") == Some(&json!("turn_context")) {
            if let Some(model) = item.pointer("/payload/model").and_then(Value::as_str) { let _ = self.link.notify("input.engine", json!({"binding_id":id,"engine":{"model":model}})).await; }
        }
        if item.get("type") == Some(&json!("event_msg")) {
            let phase = item.pointer("/payload/type").and_then(Value::as_str).unwrap_or("");
            let turn_id = item.pointer("/payload/turn_id").and_then(Value::as_str);
            if phase == "task_started" { *turn = turn_id.map(str::to_owned); let _ = self.link.notify("input.working", json!({"binding_id":id,"working":true,"turn_id":turn_id})).await; }
            if phase == "task_complete" || phase == "turn_aborted" { *turn = None; let _ = self.link.notify("input.working", json!({"binding_id":id,"working":false,"turn_id":turn_id})).await; }
        }
        if item.get("type") == Some(&json!("response_item")) && item.pointer("/payload/type") == Some(&json!("message")) && item.pointer("/payload/role") == Some(&json!("user")) {
            let text = item.pointer("/payload/content").and_then(Value::as_array).map(|parts| parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n")).unwrap_or_default();
            let Some(start) = text.find("{\"channel\":") else { return; };
            let Some(end) = text[start..].find('}') else { return; };
            let Ok(header) = serde_json::from_str::<Value>(&text[start..=start + end]) else { return; };
            let Some(message_id) = header.get("message_id").and_then(Value::as_str) else { return; };
            if binding.pending.lock().await.remove(message_id).is_none() { return; }
            if !binding.read.lock().await.insert(message_id.to_owned()) { return; }
            let _ = self.link.notify("input.read", json!({"binding_id":id,"message_id":message_id,"session_id":header["session_id"],"revision":header["revision"],"turn_id":turn})).await;
        }
    }

    async fn serve_client(self: Arc<Self>, stream: UnixStream) -> Result<()> {
        let owner = self.owner_serial.fetch_add(1, Ordering::Relaxed);
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        loop {
            let Some(line) = lines.next_line().await? else { break; };
            if line.len() > 1 << 20 { bail!("IPC line too large"); }
            let request: Value = serde_json::from_str(&line)?;
            let id = request.get("id").cloned().unwrap_or(Value::Null);
            let method = request.get("method").and_then(Value::as_str).unwrap_or("");
            let params = request.get("params").cloned().unwrap_or(json!({}));
            let answer = match self.command(owner, method, params).await {
                Ok(result) => json!({"id":id,"ok":true,"result":result}),
                Err(error) => json!({"id":id,"ok":false,"error":error.to_string()}),
            };
            write.write_all(answer.to_string().as_bytes()).await?;
            write.write_all(b"\n").await?;
        }
        let owned: Vec<_> = self.bindings.lock().await.values().filter(|b| b.owner == owner).cloned().collect();
        for b in owned {
            b.stopped.store(true, Ordering::Relaxed);
            self.bindings.lock().await.remove(&b.client_ref);
            let _ = self.link.notify("binding.unregister", json!({"binding_id":*b.id.lock().await})).await;
        }
        Ok(())
    }
}

fn rollout_path(home: &PathBuf, thread: &str) -> Option<PathBuf> {
    let root = home.join("sessions");
    let mut years: Vec<_> = fs::read_dir(root).ok()?.flatten().map(|e| e.path()).collect(); years.sort(); years.reverse();
    for year in years {
        let mut months: Vec<_> = fs::read_dir(year).ok()?.flatten().map(|e| e.path()).collect(); months.sort(); months.reverse();
        for month in months {
            let mut days: Vec<_> = fs::read_dir(month).ok()?.flatten().map(|e| e.path()).collect(); days.sort(); days.reverse();
            for day in days {
                if let Ok(files) = fs::read_dir(day) { for entry in files.flatten() {
                    let path = entry.path(); if path.file_name().is_some_and(|n| n.to_string_lossy().ends_with(&format!("-{thread}.jsonl"))) { return Some(path); }
                } }
            }
        }
    }
    None
}

pub async fn run(profile: Profile) -> Result<()> {
    private_dir(&profile.data)?;
    let lock_path = profile.data.join("connector.lock");
    let lock = OpenOptions::new().read(true).write(true).create(true).mode(0o600).open(lock_path)?;
    lock.try_lock_exclusive().context("another connector holds the proof lock")?;
    if profile.socket.exists() { verify_socket(&profile.socket)?; fs::remove_file(&profile.socket)?; }
    let link = Link::new();
    let daemon = Daemon::new(profile.clone(), link.clone())?;
    let (incoming_tx, mut incoming_rx) = mpsc::channel(128);
    let (ready_tx, ready_rx) = oneshot::channel();
    tokio::spawn(link.run(profile.clone(), incoming_tx, ready_tx));
    ready_rx.await.context("Core hello did not complete")?.map_err(anyhow::Error::msg)?;
    let listener = UnixListener::bind(&profile.socket)?;
    fs::set_permissions(&profile.socket, fs::Permissions::from_mode(0o600))?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let daemon = daemon.clone();
                tokio::spawn(async move { if let Err(error) = daemon.serve_client(stream).await { eprintln!("[sidevoice rust proof] IPC: {error}"); } });
            }
            item = incoming_rx.recv() => {
                let Some(item) = item else { bail!("Core link task exited"); };
                let daemon = daemon.clone();
                tokio::spawn(async move { daemon.incoming(item).await; });
            }
            _ = tokio::signal::ctrl_c() => { break; }
        }
    }
    fs::remove_file(&profile.socket)?;
    drop(lock);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Daemon;
    use serde_json::json;

    #[test]
    fn outbox_requires_matching_durable_or_audited_terminal_result() {
        let speech = json!({"event_id":"e","utterance_id":"u"});
        assert!(Daemon::durable(&speech, &json!({"event_id":"e","utterance_id":"u","text_saved":true})));
        assert!(Daemon::durable(&speech, &json!({"event_id":"e","utterance_id":"u","status":"rejected","terminal":true,"reason_code":"application_refusal"})));
        for reply in [json!(null), json!({}), json!({"event_id":"e","utterance_id":"u","status":"unknown_binding"}), json!({"event_id":"e","utterance_id":"wrong","text_saved":true}), json!({"event_id":"e","utterance_id":"u","status":"rejected","terminal":true})] {
            assert!(!Daemon::durable(&speech, &reply));
        }
    }
}
