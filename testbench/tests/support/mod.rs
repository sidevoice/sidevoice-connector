//! What the test bench's integration test needs of the connector's own test helpers: a private profile, a child
//! process stopped when dropped, an MCP client and an HTTP receiver. A copy of the subset of
//! connector/tests/support/mod.rs the test uses: a test binary cannot share another package's `tests/`.
// The test speaks the connector's wire raw, as a client of another version would.
#![allow(clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sidevoice_connector::platform::data_dir;
use sidevoice_connector::platform::fs::Files;
use sidevoice_connector::platform::fs::Fs as _;
use sidevoice_connector::platform::process::{Process as _, Processes};

/// The checked core archive and its record (`core.json`), or `None` when the test must be skipped. Without the core
/// (`cargo xtask core`) the test is skipped locally and fails in CI, like every test that needs the real core.
pub fn pinned_core() -> Option<(PathBuf, Value)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/sidevoice-core");
    let record: Option<Value> = fs::read(dir.join("core.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let found = record.and_then(|record| {
        let archive = dir.join(record["archive"].as_str()?);
        archive.is_file().then_some((archive, record))
    });
    if found.is_none() {
        assert!(
            std::env::var_os("CI").is_none(),
            "no core archive in {}: run `cargo xtask core` before `cargo test`",
            dir.display()
        );
        eprintln!(
            "skipped: no core archive in {} (run `cargo xtask core`)",
            dir.display()
        );
    }
    found
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
    Files::ensure_private_dir(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
}

/// Where a test profile lives. Unix: `/tmp`, short and canonical (a Unix socket path has a 104-byte limit on macOS,
/// and the connector compares canonical paths). Elsewhere the temporary directory (a pipe name has no such limit).
pub fn short_temp_dir() -> PathBuf {
    let base = if cfg!(unix) {
        PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    base.canonicalize().unwrap()
}

pub fn read_json(path: &Path) -> Option<Value> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

/// A disposable private profile, removed when dropped. Under `/tmp`, short and canonical: a Unix socket path has a
/// 104-byte limit on macOS, and the connector compares canonical paths.
pub struct Profile {
    pub root: PathBuf,
}

impl Profile {
    pub fn new(label: &str) -> Self {
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let base = short_temp_dir().join(format!(
            "sv{label}-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&base);
        private_dir(&base);
        let data_core = format!(
            "home/{}/core",
            data_dir::standard()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
        );
        for child in [
            "home",
            "claude",
            "codex",
            "cursor/config",
            "cursor/data",
            "xdg/config",
            "home/.local/share",
            data_core.as_str(),
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
        self.root
            .join("home")
            .join(data_dir::standard().unwrap().file_name().unwrap())
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
            (
                "SIDEVOICE_DATA_DIR".into(),
                self.data().to_string_lossy().into_owned(),
            ),
        ])
    }

    pub fn command(&self, program: impl AsRef<Path>) -> Command {
        let mut command = Command::new(program.as_ref());
        command.env_clear().envs(self.env());
        command
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A child process stopped (SIGTERM, then SIGKILL) when dropped.
pub struct Process(pub Child);

impl Process {
    pub fn stop(&mut self) {
        if self.0.try_wait().ok().flatten().is_some() {
            return;
        }
        stop(self.0.id(), Duration::from_secs(10));
        if self.0.try_wait().ok().flatten().is_some() {
            return;
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
    http_receiver_with_status(200)
}

pub fn http_receiver_with_status(status: u16) -> HttpReceiver {
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
            let response =
                format!("HTTP/1.1 {status} Test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(response.as_bytes());
        }
    });
    HttpReceiver { url, bodies }
}

/// Asks `pid` to leave (`SIGTERM` where the OS has it) and waits up to `grace` for it to go, then stops it at once.
pub fn stop(pid: u32, grace: Duration) {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(Processes::stop(pid, |_| true, grace));
}
