use crate::secure_fs::{atomic_json, private_file};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Mutex, Notify, Semaphore};

const POLL_TIMEOUT: Duration = Duration::from_secs(25);
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(20);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

pub fn resource_html() -> String {
    include_str!("cursor-app.html")
        .replace("__SIDEVOICE_EN__", include_str!("../messages/card/en.json"))
        .replace("__SIDEVOICE_ES__", include_str!("../messages/card/es.json"))
}

#[derive(Debug)]
pub enum AppNotice {
    Dispatched {
        thread: String,
        message_id: String,
    },
    Answered {
        thread: String,
        message_id: String,
        ok: bool,
    },
}

#[derive(Default)]
struct MailboxState {
    queue: VecDeque<Value>,
    waiting: HashMap<String, oneshot::Sender<std::result::Result<(), String>>>,
    seen: Option<Instant>,
    tool_call: Option<String>,
}

struct Mailbox {
    key: String,
    state: Mutex<MailboxState>,
    changed: Notify,
}

pub struct CursorApps {
    boxes: Mutex<HashMap<String, Arc<Mailbox>>>,
    port: Mutex<Option<u16>>,
    port_file: PathBuf,
    saved_port: Option<u16>,
    start_lock: Mutex<()>,
    notices: mpsc::UnboundedSender<AppNotice>,
}

impl CursorApps {
    pub fn new(notices: mpsc::UnboundedSender<AppNotice>, port_file: PathBuf) -> Result<Arc<Self>> {
        let saved_port = match fs::symlink_metadata(&port_file) {
            Ok(_) => {
                private_file(&port_file)?;
                if fs::metadata(&port_file)?.len() > 1024 {
                    anyhow::bail!("saved Cursor card port file is too large");
                }
                let value: Value = serde_json::from_slice(&fs::read(&port_file)?)
                    .context("invalid saved Cursor card port")?;
                let port = value
                    .get("port")
                    .and_then(Value::as_u64)
                    .and_then(|port| u16::try_from(port).ok())
                    .filter(|port| *port != 0)
                    .context("invalid saved Cursor card port")?;
                Some(port)
            }
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        Ok(Arc::new(Self {
            boxes: Mutex::new(HashMap::new()),
            port: Mutex::new(None),
            port_file,
            saved_port,
            start_lock: Mutex::new(()),
            notices,
        }))
    }

    pub async fn port(&self) -> Option<u16> {
        *self.port.lock().await
    }

    pub async fn open(self: &Arc<Self>, thread: &str, key: &str) -> Result<u16> {
        if !thread.starts_with("cursor-editor-")
            || key.len() != 64
            || !key.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            anyhow::bail!("Cursor editor conversation needs a valid id and view key");
        }
        let current_port = *self.port.lock().await;
        let port = if let Some(port) = current_port {
            port
        } else {
            let _guard = self.start_lock.lock().await;
            let current_port = *self.port.lock().await;
            if let Some(port) = current_port {
                port
            } else {
                let requested_port = self.saved_port.unwrap_or(0);
                let listener = TcpListener::bind(("127.0.0.1", requested_port))
                    .await
                    .with_context(|| {
                        if requested_port == 0 {
                            "could not open Cursor card listener on loopback".to_owned()
                        } else {
                            format!(
                                "could not restore Cursor card listener on 127.0.0.1:{requested_port}"
                            )
                        }
                    })?;
                let port = listener.local_addr()?.port();
                if self.saved_port.is_none() {
                    atomic_json(&self.port_file, &json!({"port":port}))
                        .context("could not persist Cursor card listener port")?;
                }
                *self.port.lock().await = Some(port);
                let server = self.clone();
                tokio::spawn(async move {
                    let clients = Arc::new(Semaphore::new(32));
                    loop {
                        match listener.accept().await {
                            Ok((stream, _)) => {
                                let Ok(permit) = clients.clone().try_acquire_owned() else {
                                    continue;
                                };
                                let server = server.clone();
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    let _ = server.handle(stream).await;
                                });
                            }
                            Err(error) => {
                                crate::logfile::log(&format!("Cursor card listener: {error}"));
                                break;
                            }
                        }
                    }
                });
                port
            }
        };
        self.boxes.lock().await.insert(
            thread.to_owned(),
            Arc::new(Mailbox {
                key: key.to_owned(),
                state: Mutex::new(MailboxState::default()),
                changed: Notify::new(),
            }),
        );
        Ok(port)
    }

    pub async fn close(&self, thread: &str) {
        if let Some(mailbox) = self.boxes.lock().await.remove(thread) {
            for (_, pending) in mailbox.state.lock().await.waiting.drain() {
                let _ = pending.send(Err("Cursor card closed".into()));
            }
            mailbox.changed.notify_waiters();
        }
    }

    pub async fn deliver(&self, thread: &str, message_id: &str, text: String) -> Result<Value> {
        let mailbox = self.boxes.lock().await.get(thread).cloned().context(
            "That Cursor conversation is not open in this connector; ask it to join the room again",
        )?;
        let (tx, rx) = oneshot::channel();
        let message = json!({"message_id":message_id,"text":text});
        {
            let mut state = mailbox.state.lock().await;
            if state.waiting.contains_key(message_id) {
                anyhow::bail!("Cursor card already holds this input");
            }
            state.waiting.insert(message_id.to_owned(), tx);
            state.queue.push_back(message);
        }
        mailbox.changed.notify_one();
        match tokio::time::timeout(DELIVERY_TIMEOUT, rx).await {
            Ok(Ok(Ok(()))) => Ok(
                json!({"status":"unknown","detail":"dispatched by the chat's Sidevoice view (MCP App ui/message); Cursor answers when the turn ends"}),
            ),
            Ok(Ok(Err(error))) => anyhow::bail!("{error}"),
            Ok(Err(_)) => anyhow::bail!("Cursor card disconnected before dispatch"),
            Err(_) => {
                let mut state = mailbox.state.lock().await;
                state.waiting.remove(message_id);
                state.queue.retain(|queued| {
                    queued.get("message_id").and_then(Value::as_str) != Some(message_id)
                });
                anyhow::bail!("No Sidevoice card is open in that Cursor chat to take the message (open the chat, or ask it to join the room again)")
            }
        }
    }

    pub async fn status(&self, thread: &str) -> Value {
        let mailbox = self.boxes.lock().await.get(thread).cloned();
        let Some(mailbox) = mailbox else {
            return Value::Null;
        };
        let state = mailbox.state.lock().await;
        let age = state.seen.map(|seen| seen.elapsed().as_millis() as u64);
        json!({"card_connected":age.is_some_and(|ms| ms < 30_000),"card_last_seen_ms_ago":age,"bridge_chat_known":false,
            "tool_call":state.tool_call})
    }

    async fn handle(self: Arc<Self>, mut stream: TcpStream) -> Result<()> {
        let request = tokio::time::timeout(REQUEST_TIMEOUT, read_request(&mut stream))
            .await
            .context("Cursor card request timed out")??;
        let origin = request
            .headers
            .get("origin")
            .map(String::as_str)
            .unwrap_or("");
        if !origin.starts_with("vscode-webview://")
            || !origin.is_ascii()
            || origin.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
        {
            return write_response(&mut stream, 403, origin, "", "").await;
        }
        let Some((path, query)) = request.target.split_once('?') else {
            return write_response(&mut stream, 404, origin, "", "").await;
        };
        let query = query_values(query);
        let thread = query.get("thread").map(String::as_str).unwrap_or("");
        let Some(mailbox) = self.boxes.lock().await.get(thread).cloned() else {
            return write_response(&mut stream, 404, origin, "", "").await;
        };
        let expected = hmac_hex(&mailbox.key, &format!("poll:{thread}"));
        if !constant_eq(
            query.get("auth").map(String::as_str).unwrap_or(""),
            &expected,
        ) {
            return write_response(&mut stream, 403, origin, "", "").await;
        }
        {
            let mut state = mailbox.state.lock().await;
            state.seen = Some(Instant::now());
            if let Some(tool_call) = query.get("tool_call") {
                state.tool_call = Some(tool_call.chars().take(200).collect());
            }
        }
        if request.method == "GET" && path == "/cursor-app/next" {
            let message = next_message(&mailbox).await;
            if let Some(message) = message {
                let message_id = message
                    .get("message_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let text = message.get("text").and_then(Value::as_str).unwrap_or("");
                let signature = hmac_hex(&mailbox.key, &format!("msg:{message_id}\n{text}"));
                let mut signed = message;
                signed["sig"] = json!(signature);
                write_response(
                    &mut stream,
                    200,
                    origin,
                    "application/json",
                    &signed.to_string(),
                )
                .await
            } else {
                write_response(&mut stream, 204, origin, "", "").await
            }
        } else if request.method == "POST" && path == "/cursor-app/done" {
            let body: Value = serde_json::from_slice(&request.body).unwrap_or_else(|_| json!({}));
            let message_id = body.get("message_id").and_then(Value::as_str).unwrap_or("");
            match body.get("stage").and_then(Value::as_str) {
                Some("dispatched") => {
                    let pending = mailbox.state.lock().await.waiting.remove(message_id);
                    if let Some(pending) = pending {
                        let _ = pending.send(Ok(()));
                    }
                    let _ = self.notices.send(AppNotice::Dispatched {
                        thread: thread.to_owned(),
                        message_id: message_id.to_owned(),
                    });
                }
                Some("answered") => {
                    let _ = self.notices.send(AppNotice::Answered {
                        thread: thread.to_owned(),
                        message_id: message_id.to_owned(),
                        ok: body.get("ok") == Some(&json!(true)),
                    });
                }
                _ => {}
            }
            write_response(&mut stream, 204, origin, "", "").await
        } else {
            write_response(&mut stream, 404, origin, "", "").await
        }
    }
}

struct HttpRequest {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

async fn read_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        if bytes.len() > 1 << 20 {
            anyhow::bail!("HTTP request headers too large");
        }
        let mut block = [0u8; 4096];
        let count = stream.read(&mut block).await?;
        if count == 0 {
            anyhow::bail!("HTTP request ended before headers");
        }
        bytes.extend_from_slice(&block[..count]);
    };
    let header_text = std::str::from_utf8(&bytes[..header_end])?;
    let mut lines = header_text.split("\r\n");
    let mut first = lines.next().unwrap_or("").split_whitespace();
    let method = first.next().unwrap_or("").to_owned();
    let target = first.next().unwrap_or("").to_owned();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > 65_536 {
        anyhow::bail!("HTTP request body too large");
    }
    while bytes.len() < header_end + content_length {
        let mut block = [0u8; 4096];
        let count = stream.read(&mut block).await?;
        if count == 0 {
            anyhow::bail!("HTTP request ended before body");
        }
        bytes.extend_from_slice(&block[..count]);
    }
    Ok(HttpRequest {
        method,
        target,
        headers,
        body: bytes[header_end..header_end + content_length].to_vec(),
    })
}

async fn next_message(mailbox: &Mailbox) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + POLL_TIMEOUT;
    loop {
        let notified = mailbox.changed.notified();
        if let Some(message) = mailbox.state.lock().await.queue.pop_front() {
            return Some(message);
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            return None;
        }
    }
}

fn query_values(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter_map(|part| {
            let (name, value) = part.split_once('=')?;
            Some((decode_query(name), decode_query(value)))
        })
        .collect()
}

fn decode_query(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut result = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[index + 1..index + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    result.push(byte);
                    index += 3;
                    continue;
                }
            }
        }
        result.push(if bytes[index] == b'+' {
            b' '
        } else {
            bytes[index]
        });
        index += 1;
    }
    String::from_utf8_lossy(&result).into_owned()
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    origin: &str,
    mime: &str,
    body: &str,
) -> Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    let mut headers = format!(
        "HTTP/1.1 {status} {reason}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if !origin.is_empty() {
        headers.push_str(&format!(
            "Access-Control-Allow-Origin: {origin}\r\nVary: origin\r\n"
        ));
    }
    if !mime.is_empty() {
        headers.push_str(&format!("Content-Type: {mime}\r\n"));
    }
    headers.push_str("\r\n");
    tokio::time::timeout(REQUEST_TIMEOUT, async {
        stream.write_all(headers.as_bytes()).await?;
        stream.write_all(body.as_bytes()).await
    })
    .await
    .context("Cursor card response timed out")??;
    Ok(())
}

fn hmac_hex(key_hex: &str, message: &str) -> String {
    let key = hex::decode(key_hex).unwrap_or_default();
    let mut block = [0u8; 64];
    let normalized = if key.len() > 64 {
        Sha256::digest(&key).to_vec()
    } else {
        key
    };
    block[..normalized.len()].copy_from_slice(&normalized);
    let mut inner = Vec::with_capacity(64 + message.len());
    let mut outer = Vec::with_capacity(96);
    for byte in block {
        inner.push(byte ^ 0x36);
        outer.push(byte ^ 0x5c);
    }
    inner.extend_from_slice(message.as_bytes());
    outer.extend_from_slice(&Sha256::digest(inner));
    hex::encode(Sha256::digest(outer))
}

fn constant_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}
