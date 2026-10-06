//! The bench as the app: a local device paired with the profile's core and a call held open on it, so that typed
//! text is voice input (`/api/presentation/select` + `/api/presentation/text`, as the app sends what was said) and
//! the call's events (receipts, working state, replies) are seen. When the core is not there, the installation's
//! connector is started, which starts its core, as an agent's MCP server would.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

use crate::setup::Profile;

/// The call events kept for the page.
const EVENTS: usize = 300;

pub struct Call {
    profile: Profile,
    command: Vec<String>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    token: Option<String>,
    session: Option<String>,
    serial: u64,
    events: VecDeque<Value>,
}

impl Call {
    pub fn new(profile: Profile, command: Vec<String>) -> Self {
        Self {
            profile,
            command,
            state: Mutex::default(),
        }
    }

    /// Keeps a call open on the core for as long as the bench runs, starting the core again when it is gone.
    pub async fn keep_open(self: std::sync::Arc<Self>) {
        loop {
            if !self.core_answers().await {
                self.note(json!({"type": "bench", "data": "the core is not running: starting the connector, which starts it"}));
                if let Err(error) = self.start_connector().await {
                    self.note(json!({"type": "bench", "data": format!("{error:#}")}));
                }
            }
            if let Err(error) = self.open().await {
                self.note(json!({"type": "bench", "data": format!("call: {error:#}")}));
            }
            self.state.lock().unwrap().session = None;
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn core_answers(&self) -> bool {
        http(
            &self.profile.core_socket(),
            "GET",
            "/api/local/health",
            None,
            None,
        )
        .await
        .is_ok_and(|(status, _)| status == 200)
    }

    /// The installation's connector, detached, as an MCP server starts it on demand; it starts the core.
    async fn start_connector(&self) -> Result<()> {
        let mut command: tokio::process::Command = self.profile.command(&self.command[0]).into();
        let mut child = command
            .args(&self.command[1..])
            .arg("connector")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()?;
        tokio::spawn(async move { child.wait().await });
        for _ in 0..600 {
            if self.core_answers().await {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!(
            "the core did not start within 60 s (see {})",
            self.profile.data().join("connector.log").display()
        )
    }

    /// Pairs a local device and holds its call socket until it closes.
    async fn open(&self) -> Result<()> {
        let socket = self.profile.core_socket();
        let paired = expect(
            http(
                &socket,
                "POST",
                "/api/device/local/pair",
                Some(&json!({"name": "sidevoice-bench"})),
                None,
            )
            .await?,
        )?;
        let token = paired["token"]
            .as_str()
            .ok_or_else(|| anyhow!("the core gave no device token: {paired}"))?
            .to_owned();
        self.state.lock().unwrap().token = Some(token.clone());
        let mut request = "ws://localhost/api/presentation/ws".into_client_request()?;
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            format!("sidevoice, sidevoice.token.{token}").parse()?,
        );
        let stream = UnixStream::connect(&socket).await?;
        let (mut ws, _) = tokio_tungstenite::client_async(request, stream).await?;
        let ready = json!({"label": "rtvi-ai", "type": "client-ready", "id": "sidevoice-bench",
                           "data": {"settings": {"turn_end_mode": "timer"}}});
        ws.send(Message::Text(ready.to_string().into())).await?;
        // Any frame tells the core the device is still there (its keepalive drops a silent one).
        let mut keepalive = tokio::time::interval(Duration::from_secs(10));
        loop {
            tokio::select! {
                _ = keepalive.tick() => ws.send(Message::Ping(Vec::new().into())).await?,
                frame = ws.next() => match frame {
                    Some(Ok(Message::Text(text))) => {
                        let mut event: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                        if event["type"] == "voice-session" {
                            self.state.lock().unwrap().session = event["data"]["session_id"].as_str().map(str::to_owned);
                        }
                        // Audio is not for the page.
                        if let Some(data) = event.get_mut("data").and_then(Value::as_object_mut) {
                            data.remove("audio");
                        }
                        self.note(event);
                    }
                    Some(Ok(Message::Close(_))) | None => return Ok(()),
                    Some(Err(error)) => return Err(error.into()),
                    Some(Ok(_)) => {}
                },
            }
        }
    }

    fn note(&self, event: Value) {
        let mut state = self.state.lock().unwrap();
        // The core repeats a conversation's state while it lasts: one line for it is enough.
        if state
            .events
            .back()
            .is_some_and(|last| last["event"] == event)
        {
            return;
        }
        state.serial += 1;
        let serial = state.serial;
        state
            .events
            .push_back(json!({"seq": serial, "event": event}));
        while state.events.len() > EVENTS {
            state.events.pop_front();
        }
    }

    fn device(&self) -> Result<(String, String)> {
        let state = self.state.lock().unwrap();
        match (&state.token, &state.session) {
            (Some(token), Some(session)) => Ok((token.clone(), session.clone())),
            _ => bail!("no call open on the core yet"),
        }
    }

    /// The conversations, the connectors and the call's events after `since`.
    pub async fn state(&self, since: u64) -> Value {
        let events: Vec<Value> = {
            let state = self.state.lock().unwrap();
            state
                .events
                .iter()
                .filter(|event| event["seq"].as_u64() > Some(since))
                .cloned()
                .collect()
        };
        let Ok((token, session)) = self.device() else {
            return json!({"call": false, "events": events});
        };
        let listing = http(
            &self.profile.core_socket(),
            "GET",
            "/api/connectors",
            None,
            Some(&token),
        )
        .await
        .and_then(expect)
        .unwrap_or_else(|error| json!({"error": format!("{error:#}")}));
        json!({"call": true, "session": session, "connectors": listing["connectors"],
               "bindings": listing["bindings"], "events": events})
    }

    /// The core's history of one conversation: what was said into it, with its receipt, and the replies.
    pub async fn history(&self, thread: &str) -> Result<Value> {
        let (token, _) = self.device()?;
        let path = format!("/api/presentation/history?thread_id={}", encode(thread));
        expect(
            http(
                &self.profile.core_socket(),
                "GET",
                &path,
                None,
                Some(&token),
            )
            .await?,
        )
    }

    /// Says `text` into the conversation `thread`, as the app sends what the person said in the call.
    pub async fn say(&self, thread: &str, text: &str) -> Result<Value> {
        let (token, session) = self.device()?;
        let socket = self.profile.core_socket();
        let selected = expect(
            http(
                &socket,
                "POST",
                "/api/presentation/select",
                Some(&json!({"session_id": session, "thread_id": thread})),
                Some(&token),
            )
            .await?,
        )?;
        let message_id = uuid::Uuid::new_v4().to_string();
        let sent = expect(
            http(&socket, "POST", "/api/presentation/text",
                 Some(&json!({"text": text, "session_id": session, "thread_id": thread,
                              "binding_id": selected["binding"]["binding_id"], "message_id": message_id})),
                 Some(&token)).await?,
        )?;
        Ok(json!({"message_id": message_id, "sent": sent}))
    }
}

fn expect((status, body): (u16, Value)) -> Result<Value> {
    if status >= 400 {
        bail!("HTTP {status}: {body}");
    }
    Ok(body)
}

/// One HTTP/1.0 request over the core's Unix socket: its status and JSON body.
async fn http(
    socket: &Path,
    method: &str,
    path: &str,
    body: Option<&Value>,
    token: Option<&str>,
) -> Result<(u16, Value)> {
    let exchange = async {
        let mut stream = UnixStream::connect(socket).await?;
        let body = body.map(Value::to_string).unwrap_or_default();
        let mut request = format!("{method} {path} HTTP/1.0\r\nHost: localhost\r\n");
        if let Some(token) = token {
            request.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        if !body.is_empty() {
            request.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            ));
        }
        request.push_str("\r\n");
        request.push_str(&body);
        stream.write_all(request.as_bytes()).await?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;
        anyhow::Ok(response)
    };
    let response = tokio::time::timeout(Duration::from_secs(15), exchange).await??;
    let response = String::from_utf8_lossy(&response);
    let (head, payload) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed answer from the core"))?;
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = serde_json::from_str(payload).unwrap_or_else(|_| Value::String(payload.into()));
    Ok((status, body))
}

/// A query value, percent-encoded.
pub fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}
