use crate::core_ready::Ready;
use crate::platform::ipc::{Channel, Channels};
use crate::profile::Profile;
use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};
use tokio::task::JoinSet;
use tokio::time::{timeout, timeout_at, Duration, Instant};
use tokio_tungstenite::{
    client_async_with_config,
    tungstenite::{protocol::WebSocketConfig, Message},
};

pub struct Incoming {
    pub session: u64,
    pub method: String,
    pub params: Value,
    pub reply: Option<oneshot::Sender<Value>>,
}

pub struct Link {
    tx: RwLock<Option<mpsc::Sender<Message>>>,
    pending: Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>,
    serial: AtomicU64,
    generation: AtomicU64,
    /// The core this link is connected to (`launch_id`, `pid`, `protocol`), for status and diagnostics.
    core: RwLock<Option<Value>>,
}

impl Link {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            tx: RwLock::new(None),
            pending: Mutex::new(HashMap::new()),
            serial: AtomicU64::new(2),
            generation: AtomicU64::new(1),
            core: RwLock::new(None),
        })
    }

    pub async fn connected(&self) -> bool {
        self.tx.read().await.is_some()
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let tx = self
            .tx
            .read()
            .await
            .clone()
            .context("Core link unavailable")?;
        tx.send(Message::Text(
            json!({"jsonrpc":"2.0","method":method,"params":params})
                .to_string()
                .into(),
        ))
        .await?;
        Ok(())
    }

    pub async fn request(&self, method: &str, params: Value, limit: Duration) -> Result<Value> {
        let deadline = Instant::now() + limit;
        let tx = self
            .tx
            .read()
            .await
            .clone()
            .context("Core link unavailable")?;
        let id = format!("c:{}", self.serial.fetch_add(1, Ordering::Relaxed));
        let (answer_tx, answer_rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            if pending.len() >= 64 {
                bail!("Core link pending limit");
            }
            pending.insert(id.clone(), answer_tx);
        }
        let frame = Message::Text(
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
                .to_string()
                .into(),
        );
        let sent = timeout_at(deadline, tx.send(frame)).await;
        if !matches!(sent, Ok(Ok(()))) {
            self.pending.lock().await.remove(&id);
            bail!("Core link send timed out or disconnected");
        }
        let answer = timeout_at(deadline, answer_rx).await;
        self.pending.lock().await.remove(&id);
        match answer {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(error))) => bail!("Core refused {method}: {error}"),
            _ => bail!("Core {method} timed out or disconnected"),
        }
    }

    /// A frame without a method: the answer to one of this connector's requests, its fields this connector does not
    /// know ignored. An answer whose ID is `null` (JSON-RPC's answer to a request the core could not read) names no
    /// request and is only logged; any other ID this connector could never have sent ends the link.
    async fn settle(&self, frame: &Value) -> Result<()> {
        let Some(id) = frame.get("id").filter(|id| !id.is_null()) else {
            crate::logfile::log(&format!(
                "Core answer without an ID ignored: {}",
                frame.get("error").unwrap_or(&Value::Null)
            ));
            return Ok(());
        };
        let Some(id) = id.as_str().filter(|id| id.starts_with("c:")) else {
            bail!("Core response ID invalid");
        };
        if let Some(waiting) = self.pending.lock().await.remove(id) {
            let result = match frame.get("result") {
                Some(result) => Ok(result.clone()),
                None => Err(frame
                    .get("error")
                    .map(Value::to_string)
                    .unwrap_or_else(|| "missing result".into())),
            };
            let _ = waiting.send(result);
        } // A response to a timed-out request is harmless.
        Ok(())
    }

    /// A link whose frames go to the returned receiver and whose answers come through `settle`: the core's side of
    /// the link, for tests that play it.
    #[cfg(test)]
    pub async fn attached(&self) -> mpsc::Receiver<Message> {
        let (tx, rx) = mpsc::channel(128);
        *self.tx.write().await = Some(tx);
        rx
    }

    /// An answer from the core's side of an `attached` link, read as the link reads it.
    #[cfg(test)]
    pub async fn answer(&self, frame: Value) -> Result<()> {
        self.settle(&frame).await
    }

    /// The core this link is connected to, when it is.
    pub async fn core(&self) -> Option<Value> {
        self.core.read().await.clone()
    }

    async fn reset(&self) {
        *self.tx.write().await = None;
        *self.core.write().await = None;
        for (_, waiting) in self.pending.lock().await.drain() {
            let _ = waiting.send(Err("disconnected".into()));
        }
    }

    pub async fn run(
        self: Arc<Self>,
        profile: Profile,
        incoming: mpsc::Sender<Incoming>,
        first: oneshot::Sender<Result<Ready, String>>,
        supervisor: Arc<crate::service::launcher::CoreSupervisor>,
    ) {
        let mut first = Some(first);
        let mut backoff = 100u64;
        loop {
            let session = self.generation.fetch_add(1, Ordering::Relaxed);
            let outcome = self.session(&profile, &incoming, &mut first, session).await;
            self.reset().await;
            let _ = incoming
                .send(Incoming {
                    session,
                    method: "connector.lost".into(),
                    params: Value::Null,
                    reply: None,
                })
                .await;
            match outcome {
                Ok(()) => backoff = 100,
                Err(error) => {
                    let message = error.to_string();
                    crate::logfile::log(&format!("Core link: {message}"));
                    if let Some(first) = first.take() {
                        let _ = first.send(Err(message.clone()));
                    }
                    // A core this connector starts (no core job) is started again when none answers.
                    supervisor.link_failed().await;
                    if message.contains("authentication refused")
                        || message.contains("Core v3 upgrade required")
                    {
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(backoff)).await;
            backoff = (backoff * 2).min(5000);
        }
    }

    async fn session(
        &self,
        profile: &Profile,
        incoming: &mpsc::Sender<Incoming>,
        first: &mut Option<oneshot::Sender<Result<Ready, String>>>,
        session: u64,
    ) -> Result<()> {
        let ready = profile.ready().await?;
        let stream = timeout(Duration::from_secs(2), Channels::connect(&ready.endpoint)).await??;
        let mut config = WebSocketConfig::default();
        config.max_message_size = Some(1 << 20);
        config.max_frame_size = Some(1 << 20);
        let (mut ws, _) = timeout(
            Duration::from_secs(3),
            client_async_with_config("ws://localhost/api/connectors/v3", stream, Some(config)),
        )
        .await??;
        let mut params = crate::identity::machine(profile);
        params["protocol"] = json!(3);
        params["connector_id"] = json!(ready.connector_id);
        params["token"] = json!(ready.token);
        let hello = json!({"jsonrpc":"2.0","id":"c:1","method":"connector.hello","params":params});
        ws.send(Message::Text(hello.to_string().into())).await?;
        let reply = timeout(Duration::from_secs(5), ws.next())
            .await?
            .context("hello closed")??;
        let value: Value = serde_json::from_str(reply.to_text()?)?;
        if value.get("id") != Some(&json!("c:1"))
            || value.get("result").and_then(|v| v.get("protocol")) != Some(&json!(3))
        {
            bail!("Core v3 authentication refused");
        }
        if let Some(first) = first.take() {
            let _ = first.send(Ok(ready.clone()));
        }
        let (mut write, mut read) = ws.split();
        let (tx, mut rx) = mpsc::channel::<Message>(128);
        *self.tx.write().await = Some(tx.clone());
        *self.core.write().await =
            Some(json!({"launch_id":ready.launch_id,"pid":ready.pid,"protocol":3}));
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            while let Some(message) = rx.recv().await {
                if write.send(message).await.is_err() {
                    break;
                }
            }
            let _ = write.close().await;
            true
        });
        let mut ping = tokio::time::interval(Duration::from_secs(15));
        ping.tick().await;
        let mut unanswered: Option<Instant> = None;
        loop {
            tokio::select! {
                Some(result) = tasks.join_next() => {
                    if result? { bail!("Core writer ended"); }
                }
                _ = ping.tick() => {
                    if unanswered.is_some_and(|at| at.elapsed() > Duration::from_secs(30)) { bail!("Core Pong timeout"); }
                    tx.send(Message::Ping(b"sidevoice".to_vec().into())).await?;
                    if unanswered.is_none() { unanswered = Some(Instant::now()); }
                }
                frame = read.next() => {
                    let frame = frame.context("Core disconnected")??;
                    match frame {
                        Message::Pong(payload) => { if payload.as_ref() == b"sidevoice" { unanswered = None; } }
                        Message::Ping(payload) => { tx.send(Message::Pong(payload)).await?; }
                        Message::Text(text) => {
                            if text.len() > 1 << 20 { bail!("Core frame too large"); }
                            let value: Value = serde_json::from_str(&text)?;
                            if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") { bail!("invalid Core JSON-RPC"); }
                            if let Some(method) = value.get("method").and_then(Value::as_str) {
                                if method == "input.deliver" {
                                    crate::logfile::log("delivery stage=core_link result=received");
                                }
                                if method == "connector.welcome" {
                                    let _ = incoming.send(Incoming { session, method: method.into(), params: value.get("params").cloned().unwrap_or(Value::Null), reply: None }).await;
                                    continue;
                                }
                                let id = value.get("id").cloned();
                                if tasks.len() >= 65 { bail!("Core handler limit"); }
                                let reply_tx = tx.clone();
                                let method = method.to_owned();
                                let delivery_frame = method == "input.deliver";
                                let params = value.get("params").cloned().unwrap_or(Value::Null);
                                let (answer_tx, answer_rx) = oneshot::channel();
                                let inbound = Incoming {
                                    session,
                                    method,
                                    params,
                                    reply: id.as_ref().map(|_| answer_tx),
                                };
                                if !matches!(timeout(Duration::from_secs(2), incoming.send(inbound)).await, Ok(Ok(()))) {
                                    if delivery_frame {
                                        crate::logfile::log("delivery stage=core_link result=queue_unavailable");
                                    }
                                    bail!("Core inbound queue unavailable");
                                }
                                if let Some(id) = id {
                                    tasks.spawn(async move {
                                        let result = timeout(Duration::from_secs(65), answer_rx)
                                            .await.ok().and_then(|v| v.ok())
                                            .unwrap_or(json!({"status":"failed"}));
                                        let frame = json!({"jsonrpc":"2.0","id":id,"result":result});
                                        let _ = reply_tx.send(Message::Text(frame.to_string().into())).await;
                                        false
                                    });
                                }
                            } else {
                                self.settle(&value).await?;
                            }
                        }
                        Message::Close(frame) => {
                            if let Some(frame) = frame {
                                bail!(
                                    "Core WebSocket closed with code {}: {}",
                                    frame.code,
                                    frame.reason
                                );
                            }
                            bail!("Core WebSocket closed without a close frame");
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Link;
    use serde_json::{json, Value};
    use tokio::time::Duration;
    use tokio_tungstenite::tungstenite::Message;

    /// The core answers with fields this connector does not know, at the top of the frame and inside its result: the
    /// request gets its result whole. JSON-RPC's answer to a request the core could not read (`"id": null`) names no
    /// request: it is ignored and the link stays. An ID this connector never sent still ends the link.
    #[tokio::test]
    async fn an_answer_with_unknown_fields_is_taken_and_one_without_an_id_is_ignored() {
        let link = Link::new();
        let mut core = link.attached().await;
        let asking = {
            let link = link.clone();
            tokio::spawn(async move {
                link.request("speech.publish", json!({}), Duration::from_secs(5))
                    .await
            })
        };
        let Some(Message::Text(sent)) = core.recv().await else {
            panic!("nothing sent")
        };
        let sent: Value = serde_json::from_str(sent.as_str()).unwrap();
        link.answer(
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}),
        )
        .await
        .unwrap();
        link.answer(json!({"jsonrpc":"2.0","id":sent["id"],"trace":{"span":"x"},
            "result":{"text_saved":true,"future":{"nested":[1]}}}))
            .await
            .unwrap();
        assert_eq!(
            asking.await.unwrap().unwrap(),
            json!({"text_saved":true,"future":{"nested":[1]}})
        );
        assert!(link
            .answer(json!({"jsonrpc":"2.0","id":7,"result":{}}))
            .await
            .is_err());
    }
}
