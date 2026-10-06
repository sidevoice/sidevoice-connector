//! What the tests that run the connector against the real core share: a private profile, the core, the connector
//! daemon, an MCP client, the core's local HTTP routes and a presentation (call) socket.
//!
//! The core is the sidevoice-core release pinned in `core.pin`, unpacked by `cargo xtask core` into
//! `target/sidevoice-core` (or the directory `SIDEVOICE_TEST_CORE_DIR` names). Without it these tests are skipped
//! locally and fail in CI (`CI` set), so a missing core never passes as green.

#![allow(dead_code)]

pub mod package;
pub mod service;

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

pub const CONNECTOR: &str = env!("CARGO_BIN_EXE_sidevoice-connector");

/// The unpacked core (`<dir>/bin/sidevoice-core-rust`, `<dir>/models`), or `None` when the test must be skipped.
pub fn core_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("SIDEVOICE_TEST_CORE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../target/sidevoice-core/sidevoice-core-rust")
        });
    if dir.join("bin/sidevoice-core-rust").is_file() {
        return Some(dir.canonicalize().expect("core directory"));
    }
    if std::env::var_os("CI").is_some() {
        panic!(
            "no core at {}: run `cargo xtask core` before `cargo test`",
            dir.display()
        );
    }
    eprintln!(
        "skipped: no core at {} (run `cargo xtask core`)",
        dir.display()
    );
    None
}

pub fn until<T>(what: &str, seconds: u64, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Some(value) = probe() {
            return value;
        }
        if Instant::now() > deadline {
            panic!("timed out: {what}");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

pub fn private_dir(path: &Path) {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

pub fn read_json(path: &Path) -> Option<Value> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

/// A disposable private profile, removed when dropped: every place the connector reads comes from the environment
/// [`Profile::env`] gives its processes, all inside it. Under `/tmp`, short and canonical: a
/// Unix socket path has a 104-byte limit on macOS, and the connector compares canonical paths.
pub struct Profile {
    pub root: PathBuf,
}

impl Profile {
    pub fn new(label: &str) -> Self {
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let base = Path::new("/tmp").canonicalize().unwrap().join(format!(
            "sv{label}-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&base);
        private_dir(&base);
        for child in [
            "home",
            "claude",
            "codex",
            "cursor/config",
            "cursor/data",
            "xdg/config",
            "xdg/data",
            "sidevoice/core",
        ] {
            let mut path = base.clone();
            for part in child.split('/') {
                path.push(part);
                private_dir(&path);
            }
        }
        Self { root: base }
    }

    pub fn data(&self) -> PathBuf {
        self.root.join("sidevoice")
    }

    pub fn core_data(&self) -> PathBuf {
        self.root.join("sidevoice/core")
    }

    pub fn core_socket(&self) -> PathBuf {
        self.core_data().join("local.sock")
    }

    pub fn connector_socket(&self) -> PathBuf {
        self.data().join("connector.sock")
    }

    /// The environment a process of this profile gets: nothing inherited but `PATH`, so neither the developer's
    /// home nor an agent session the tests run inside can leak in.
    pub fn env(&self) -> BTreeMap<String, String> {
        let path = |child: &str| self.root.join(child).to_string_lossy().into_owned();
        BTreeMap::from([
            ("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into()),
            ("HOME".into(), path("home")),
            ("CLAUDE_CONFIG_DIR".into(), path("claude")),
            ("CODEX_HOME".into(), path("codex")),
            ("CURSOR_CONFIG_DIR".into(), path("cursor/config")),
            ("CURSOR_DATA_DIR".into(), path("cursor/data")),
            ("XDG_CONFIG_HOME".into(), path("xdg/config")),
            ("XDG_DATA_HOME".into(), path("xdg/data")),
            ("SIDEVOICE_DATA_DIR".into(), path("sidevoice")),
        ])
    }

    pub fn command(&self, program: impl AsRef<Path>) -> Command {
        let mut command = Command::new(program.as_ref());
        command.env_clear().envs(self.env());
        command
    }

    pub fn connector(&self, args: &[&str]) -> Command {
        let mut command = self.command(CONNECTOR);
        command.args(args);
        command
    }

    /// The core's own environment beyond the profile: its models, no STUN, no browser keepalive (the test's
    /// presentation socket does not answer one).
    pub fn core_env(&self, core: &Path) -> BTreeMap<String, String> {
        let mut env = self.env();
        env.insert(
            "RUSTVANI_CACHE_DIR".into(),
            core.join("models").to_string_lossy().into_owned(),
        );
        env.insert("SIDEVOICE_STUN_URLS".into(), String::new());
        env.insert("VOICE_BROWSER_HEARTBEAT_SECONDS".into(), "0".into());
        env
    }

    /// Starts the core on this profile's data directory and socket and waits for its ready file.
    pub fn start_core(&self, core: &Path, launch_id: &str) -> Process {
        let ready = self.core_data().join("core.json");
        let log = fs::File::create(self.root.join(format!("core-{launch_id}.log"))).unwrap();
        let child = self
            .command(core.join("bin/sidevoice-core-rust"))
            .envs(self.core_env(core))
            .arg("--data-dir")
            .arg(self.core_data())
            .arg("--socket")
            .arg(self.core_socket())
            .args(["--port", "0", "--idle-exit", "0", "--launch-id", launch_id])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("start the core");
        let process = Process(child);
        until("the core's ready file", 60, || {
            read_json(&ready).filter(|ready| ready["launch_id"] == launch_id)
        });
        process
    }

    /// Starts the connector daemon (`connector`, not a service) and waits for its socket.
    pub fn start_connector(&self) -> Process {
        let log = fs::File::create(self.root.join("connector.log")).unwrap();
        let child = self
            .connector(&["connector"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("start the connector");
        let process = Process(child);
        until("the connector's socket", 30, || {
            self.connector_socket().exists().then_some(())
        });
        process
    }

    /// The core the connector says it is linked to (its `status` over the connector's socket), once that is the core
    /// started as `launch_id`.
    pub fn wait_linked(&self, launch_id: &str) -> Value {
        until(
            &format!("the connector linked to core {launch_id}"),
            60,
            || {
                connector_ipc(&self.connector_socket(), "status")
                    .map(|status| status["core"].clone())
                    .filter(|core| core["launch_id"] == launch_id)
            },
        )
    }

    pub fn log_tail(&self) -> String {
        let mut out = String::new();
        for entry in fs::read_dir(&self.root).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "log") {
                let text = fs::read_to_string(&path).unwrap_or_default();
                out.push_str(&format!(
                    "--- {}\n{}\n",
                    path.display(),
                    &text[text.len().saturating_sub(4000)..]
                ));
            }
        }
        out
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        if thread::panicking() {
            eprintln!("{}", self.log_tail());
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A child process stopped (SIGTERM, then SIGKILL) when dropped.
pub struct Process(pub Child);

impl Process {
    pub fn pid(&self) -> u32 {
        self.0.id()
    }

    pub fn stop(&mut self) {
        if self.0.try_wait().ok().flatten().is_some() {
            return;
        }
        unsafe {
            libc::kill(self.0.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.0.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One request to the connector over its socket (JSON lines `{id, method, params}`): its result, or `None` when the
/// connector is not there or refused.
pub fn connector_ipc(socket: &Path, method: &str) -> Option<Value> {
    let mut stream = UnixStream::connect(socket).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    writeln!(
        stream,
        "{}",
        json!({"id": 1, "method": method, "params": {}})
    )
    .ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    let answer: Value = serde_json::from_str(&line).ok()?;
    (answer["ok"] == true).then(|| answer["result"].clone())
}

/// One HTTP/1.0 request over the core's Unix socket; returns the status and the JSON body.
pub fn core_http(
    socket: &Path,
    method: &str,
    path: &str,
    body: Option<&Value>,
    token: Option<&str>,
) -> (u16, Value) {
    let mut stream = UnixStream::connect(socket).expect("core socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
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
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let response = String::from_utf8_lossy(&response).into_owned();
    let (head, payload) = response
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("malformed response: {response}"));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (
        status,
        serde_json::from_str(payload).unwrap_or(Value::String(payload.into())),
    )
}

pub fn core_json(
    socket: &Path,
    method: &str,
    path: &str,
    body: Option<&Value>,
    token: Option<&str>,
) -> Value {
    let (status, value) = core_http(socket, method, path, body, token);
    assert!(status < 400, "{method} {path}: HTTP {status} {value}");
    value
}

/// An MCP client over a child's stdio: one JSON-RPC message per line.
pub struct Mcp {
    process: Process,
    stdin: ChildStdin,
    lines: Receiver<String>,
    serial: u64,
}

impl Mcp {
    pub fn start(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start the MCP server");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            process: Process(child),
            stdin,
            lines,
            serial: 0,
        }
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        let frame = json!({"jsonrpc": "2.0", "method": method, "params": params});
        writeln!(self.stdin, "{frame}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// A request and its result; an error answer fails the test.
    pub fn request(&mut self, method: &str, params: Value) -> Value {
        self.serial += 1;
        let id = self.serial;
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{frame}").unwrap();
        self.stdin.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self
                .lines
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("MCP gave no answer to {method}"));
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message["id"] == id {
                assert!(
                    message.get("error").is_none(),
                    "MCP {method}: {}",
                    message["error"]
                );
                return message["result"].clone();
            }
        }
    }

    /// `initialize` and `notifications/initialized`; returns the initialize result.
    pub fn initialize(&mut self, client: &str) -> Value {
        let result = self.request(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": client, "version": "test"}}),
        );
        self.notify("notifications/initialized", json!({}));
        result
    }

    /// A tool call whose text content is JSON; a tool error fails the test.
    pub fn tool(&mut self, name: &str, arguments: Value) -> Value {
        let result = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        assert_ne!(result["isError"], true, "{name}: {result}");
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap_or("null"))
            .unwrap_or_else(|_| panic!("{name}: not JSON: {result}"))
    }

    pub fn stop(&mut self) {
        self.process.stop();
    }
}

/// An HTTP endpoint on loopback recording every JSON body POSTed to it: what an agent with an HTTP receiver
/// (`SIDEVOICE_DELIVERY_URL`) would get.
pub struct HttpReceiver {
    pub url: String,
    pub bodies: Receiver<Value>,
}

pub fn http_receiver() -> HttpReceiver {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/input", listener.local_addr().unwrap());
    let (tx, bodies) = channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0usize;
            let mut expect_continue = false;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(value) = lower.strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap_or(0);
                }
                if lower.starts_with("expect:") && lower.contains("100-continue") {
                    expect_continue = true;
                }
                if line == "\r\n" {
                    break;
                }
            }
            if expect_continue {
                let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
            }
            let mut body = vec![0; length];
            if reader.read_exact(&mut body).is_ok() {
                if let Ok(value) = serde_json::from_slice(&body) {
                    let _ = tx.send(value);
                }
            }
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    HttpReceiver { url, bodies }
}

/// A room on loopback that answers every request with `status` and `answer`, and hands over what it was asked:
/// `(method and path, JSON body)`.
pub struct FakeRoom {
    pub url: String,
    pub requests: Receiver<(String, Value)>,
}

pub fn fake_room(status: u16, answer: Value) -> FakeRoom {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (tx, requests) = channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            let _ = reader.read_line(&mut request_line);
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body);
            let target = request_line
                .split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join(" ");
            let _ = tx.send((target, serde_json::from_slice(&body).unwrap_or(Value::Null)));
            let text = answer.to_string();
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                )
                .as_bytes(),
            );
        }
    });
    FakeRoom { url, requests }
}

/// A call (presentation) socket open on the core, as the app holds one: it pairs a local device, joins, and
/// keeps reading until dropped. `session` is the voice session the core gave it.
pub struct Presentation {
    pub token: String,
    pub session: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Presentation {
    pub fn open(socket: &Path) -> Self {
        let token = core_json(
            socket,
            "POST",
            "/api/device/local/pair",
            Some(&json!({"name": "connector-test"})),
            None,
        )["token"]
            .as_str()
            .expect("device token")
            .to_string();
        let (session_tx, session_rx) = channel();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let socket = socket.to_path_buf();
        let protocol_token = token.clone();
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                use futures_util::{SinkExt, StreamExt};
                use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
                let mut request = "ws://localhost/api/presentation/ws"
                    .into_client_request()
                    .unwrap();
                request.headers_mut().insert(
                    "Sec-WebSocket-Protocol",
                    format!("sidevoice, sidevoice.token.{protocol_token}")
                        .parse()
                        .unwrap(),
                );
                let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
                let (mut ws, _) = tokio_tungstenite::client_async(request, stream)
                    .await
                    .expect("presentation socket");
                let ready = json!({"label": "rtvi-ai", "type": "client-ready", "id": "connector-test",
                                   "data": {"settings": {"turn_end_mode": "timer"}}});
                ws.send(Message::Text(ready.to_string().into())).await.unwrap();
                let mut stopped = stopped;
                loop {
                    tokio::select! {
                        _ = &mut stopped => break,
                        frame = ws.next() => {
                            let Some(Ok(frame)) = frame else { break };
                            if let Message::Text(text) = frame {
                                let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                                if value["type"] == "voice-session" {
                                    let _ = session_tx.send(value["data"]["session_id"].as_str().unwrap_or("").to_string());
                                }
                            }
                        }
                    }
                }
                let _ = ws.close(None).await;
            });
        });
        let session = session_rx
            .recv_timeout(Duration::from_secs(15))
            .expect("the core gave the call socket no voice session");
        Self {
            token,
            session,
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for Presentation {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
