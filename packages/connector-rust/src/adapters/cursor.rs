use super::{envelope, Identity};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const EDITOR_NOTE: &str = "This Cursor did not declare MCP Apps support (the io.modelcontextprotocol/ui extension) when it started this server, so it would draw no Sidevoice card, and a chat of the editor has no other way to receive the room; nor is this a Cursor CLI chat (cursor-agent), whose open chat Sidevoice can see. ~/.sidevoice/mcp.log shows what it declared.";
const NOT_PERSISTED: &str = "This chat is not running under cursor-agent persist, so nothing can put a message into it. To let the room talk to a Cursor CLI chat (experimental), start it with  cursor-agent persist  (it needs tmux).";

pub fn capabilities() -> Value {
    json!({"deliver":"supported","inspectInbound":"unsupported","working":"supported","endOfTurn":"supported","sessionIdentity":"supported"})
}

pub fn is_cursor_client(client: &Value) -> bool {
    client
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|name| {
            let name = name.to_ascii_lowercase();
            name.strip_prefix("cursor").is_some_and(|rest| {
                rest.is_empty()
                    || !rest.as_bytes()[0].is_ascii_alphanumeric() && rest.as_bytes()[0] != b'_'
            })
        })
}

pub fn draws_views(capabilities: &Value) -> bool {
    let Some(ui) = capabilities.pointer("/extensions/io.modelcontextprotocol~1ui") else {
        return false;
    };
    ui.get("mimeTypes")
        .and_then(Value::as_array)
        .is_none_or(|types| {
            types.iter().any(|value| {
                value
                    .as_str()
                    .is_some_and(|name| name.to_ascii_lowercase().starts_with("text/html"))
            })
        })
}

pub fn identity(_client: &Value, capabilities: &Value) -> Result<Identity> {
    if draws_views(capabilities) {
        let thread = format!("cursor-editor-{}", uuid::Uuid::new_v4());
        let key = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let bridge = bridge_instances();
        let route = if bridge.is_empty() {
            "cursor-editor-view"
        } else {
            "cursor-editor-bridge"
        };
        let delivery = json!({"kind":"cursor-app","thread":thread,"key":key,"route":route,"joined_at":unix_ms()});
        let mut identity = Identity::new("cursor", thread.clone(), delivery);
        identity.route = Some(route.into());
        identity.editor = true;
        identity.detachable = true;
        identity.view_key = Some(key);
        identity.experimental.extend([
            "sessionIdentity".into(),
            "working".into(),
            "endOfTurn".into(),
        ]);
        identity.watch_note = Some("The room learns that this chat took a voice message, and when it is working, from Cursor itself: from its answer to the card, and from this chat's transcript once the first voice message it takes shows which one is its own.".into());
        return Ok(identity);
    }
    let Some(found) = find_chat() else {
        bail!("{EDITOR_NOTE}");
    };
    let persisted = persist_session(&found.chat)?;
    if let Some((name, pane, in_mode, attached)) = persisted {
        let mut identity = Identity::new(
            "cursor",
            found.chat.clone(),
            json!({"kind":"cursor-tmux","chat":found.chat,"session":name,"pane":pane,"in_mode":in_mode,"attached":attached}),
        );
        identity.route = Some("cursor-cli-persist".into());
        Ok(identity)
    } else {
        let mut identity = Identity::new(
            "cursor",
            found.chat.clone(),
            json!({"kind":"none","chat":found.chat}),
        );
        identity.route = Some("cursor-cli".into());
        identity.capability_overrides = json!({"deliver":"unsupported"});
        identity.deliver_note = Some(NOT_PERSISTED.into());
        Ok(identity)
    }
}

struct Chat {
    chat: String,
    updated: u64,
}

fn open_files(pid: u32) -> Vec<PathBuf> {
    let linux = PathBuf::from(format!("/proc/{pid}/fd"));
    if let Ok(entries) = fs::read_dir(linux) {
        return entries
            .flatten()
            .filter_map(|entry| fs::read_link(entry.path()).ok())
            .collect();
    }
    let output = std::process::Command::new("lsof")
        .args(["-n", "-P", "-Fn", "-p", &pid.to_string()])
        .output()
        .ok();
    output
        .filter(|out| out.status.success())
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|line| line.strip_prefix('n').map(PathBuf::from))
                .collect()
        })
        .unwrap_or_default()
}

struct StorePath {
    chat: String,
    path: PathBuf,
}

fn chat_from_store(path: &Path) -> Option<StorePath> {
    if path.file_name()?.to_str()? != "store.db" {
        return None;
    }
    let chat_dir = path.parent()?;
    let workspace = chat_dir.parent()?;
    if workspace.file_name()?.to_str()?.len() != 32
        || workspace
            .file_name()?
            .to_str()?
            .chars()
            .any(|c| !c.is_ascii_hexdigit())
    {
        return None;
    }
    Some(StorePath {
        chat: chat_dir.file_name()?.to_str()?.to_owned(),
        path: path.to_path_buf(),
    })
}

fn find_chat() -> Option<Chat> {
    let mut pid = std::process::id();
    for _ in 0..4 {
        let parent = std::process::Command::new("ps")
            .args(["-o", "ppid=", "-p", &pid.to_string()])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .and_then(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .trim()
                    .parse::<u32>()
                    .ok()
            })?;
        if parent <= 1 {
            return None;
        }
        let mut chats = Vec::<Chat>::new();
        for path in open_files(parent) {
            let Some(store) = chat_from_store(&path) else {
                continue;
            };
            let metadata_path = store.path.parent()?.join("meta.json");
            let metadata = fs::read(&metadata_path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .unwrap_or_else(|| json!({}));
            if metadata.get("isSubagent") == Some(&json!(true)) {
                continue;
            }
            let modified = fs::metadata(&store.path)
                .ok()
                .map(|meta| meta.mtime() as u64 * 1000 + meta.mtime_nsec() as u64 / 1_000_000)
                .unwrap_or(0);
            let updated = metadata
                .get("updatedAtMs")
                .and_then(Value::as_u64)
                .unwrap_or(modified);
            if let Some(existing) = chats.iter_mut().find(|chat| chat.chat == store.chat) {
                existing.updated = existing.updated.max(updated);
            } else {
                chats.push(Chat {
                    chat: store.chat,
                    updated,
                });
            }
        }
        if let Some(chat) = chats.into_iter().max_by_key(|chat| chat.updated) {
            return Some(chat);
        }
        pid = parent;
    }
    None
}

pub fn transcript_path(chat_id: &str) -> Option<PathBuf> {
    for root in transcript_roots() {
        let Ok(projects) = fs::read_dir(root.join("projects")) else {
            continue;
        };
        for project in projects.flatten() {
            let path = project
                .path()
                .join("agent-transcripts")
                .join(chat_id)
                .join(format!("{chat_id}.jsonl"));
            if path.exists() {
                return Some(path);
            }
        }
    }
    None
}

/// Find the Cursor chat whose transcript records this join or one of its pending inputs.
/// Editor conversation ids are ours; Cursor's own chat id is only available in the transcript
/// or in the Desktop Bridge state database.
pub fn editor_transcript_chat(
    delivery: &Value,
    expected_ids: &[String],
    excluded: &HashSet<String>,
) -> Option<String> {
    let title = delivery
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let joined_at = delivery
        .get("joined_at")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .saturating_sub(5_000);
    let expected = expected_ids.iter().cloned().collect::<HashSet<_>>();
    let candidate = delivery
        .get("candidate")
        .and_then(Value::as_str)
        .filter(|chat| safe_chat_id(chat));
    let mut expected_matches = Vec::new();
    let mut title_matches = Vec::new();

    for root in transcript_roots() {
        let Ok(projects) = fs::read_dir(root.join("projects")) else {
            continue;
        };
        for project in projects.flatten() {
            let Ok(chats) = fs::read_dir(project.path().join("agent-transcripts")) else {
                continue;
            };
            for chat_entry in chats.flatten() {
                let Some(chat) = chat_entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if !safe_chat_id(&chat) {
                    continue;
                }
                let path = chat_entry.path().join(format!("{chat}.jsonl"));
                let Ok(metadata) = fs::metadata(&path) else {
                    continue;
                };
                let modified =
                    metadata.mtime() as u64 * 1000 + metadata.mtime_nsec() as u64 / 1_000_000;
                if modified < unix_ms().saturating_sub(10 * 60_000) {
                    continue;
                }
                let Some(text) = transcript_tail(&path) else {
                    continue;
                };
                let mut holds_expected = false;
                let mut records_join = false;
                for line in text.lines() {
                    let Ok(entry) = serde_json::from_str::<Value>(line) else {
                        continue;
                    };
                    if entry.get("role") == Some(&json!("user"))
                        && !expected.is_empty()
                        && cursor_user_text(&entry)
                            .and_then(|text| cursor_message_id(&text))
                            .is_some_and(|id| expected.contains(&id))
                    {
                        holds_expected = true;
                    }
                    if modified >= joined_at
                        && !title.is_empty()
                        && !excluded.contains(&chat)
                        && cursor_join_matches(&entry, title)
                    {
                        records_join = true;
                    }
                    if holds_expected && records_join {
                        break;
                    }
                }
                if holds_expected {
                    expected_matches.push(chat.clone());
                }
                if records_join {
                    title_matches.push(chat);
                }
            }
        }
    }

    if let Some(candidate) = candidate {
        if transcript_path(candidate).is_some_and(|path| {
            fs::metadata(&path).is_ok_and(|metadata| {
                metadata.mtime() as u64 * 1000 + metadata.mtime_nsec() as u64 / 1_000_000
                    >= joined_at
            }) && transcript_tail(&path).is_some_and(|text| {
                text.lines().any(|line| {
                    serde_json::from_str::<Value>(line)
                        .ok()
                        .is_some_and(|entry| cursor_join_matches(&entry, ""))
                })
            })
        }) {
            return Some(candidate.to_owned());
        }
    }
    if expected_matches.len() == 1 {
        return expected_matches.pop();
    }
    if title_matches.len() == 1 {
        return title_matches.pop();
    }
    None
}

fn transcript_roots() -> Vec<PathBuf> {
    let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let mut roots = env::var("CURSOR_DATA_DIR")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .into_iter()
        .collect::<Vec<_>>();
    let default = home.join(".cursor");
    if !roots.contains(&default) {
        roots.push(default);
    }
    roots
}

fn transcript_tail(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(1 << 20);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::with_capacity((length - start) as usize);
    file.take(1 << 20).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn cursor_user_text(entry: &Value) -> Option<String> {
    let content = entry.pointer("/message/content")?;
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

fn cursor_message_id(text: &str) -> Option<String> {
    let start = text.find("{\"channel\":")?;
    let tail = &text[start..];
    let end = tail.find('}')?;
    serde_json::from_str::<Value>(&tail[..=end])
        .ok()?
        .get("message_id")?
        .as_str()
        .map(str::to_owned)
}

fn cursor_join_matches(entry: &Value, title: &str) -> bool {
    if entry.get("role") != Some(&json!("assistant")) {
        return false;
    }
    entry
        .pointer("/message/content")
        .and_then(Value::as_array)
        .is_some_and(|parts| {
            parts.iter().any(|part| {
                part.get("type").and_then(Value::as_str) == Some("tool_use")
                    && part
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| name.ends_with("voice_connect"))
                    && (title.is_empty()
                        || part
                            .pointer("/input/title")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .trim()
                            == title)
            })
        })
}

fn tmux_prefix() -> Vec<String> {
    let name = env::var("CURSOR_AGENT_TMUX_SERVER_NAME")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 64
                && value.as_bytes()[0].is_ascii_alphanumeric()
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        })
        .unwrap_or_else(|| "cursor-agent".into());
    vec![
        "-u".into(),
        "-L".into(),
        name,
        "-f".into(),
        "/dev/null".into(),
    ]
}

fn tmux_process(binary: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    let mut command = std::process::Command::new(binary);
    command
        .env_clear()
        .env("HOME", env::var("HOME").unwrap_or_default())
        .env("TMUX_TMPDIR", "/tmp")
        .env(
            "PATH",
            env::var("PATH")
                .ok()
                .filter(|path| !path.is_empty())
                .unwrap_or_else(|| "/usr/bin:/bin".into()),
        );
    command
}

fn tmux_async_process(binary: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(binary);
    command
        .env_clear()
        .env("HOME", env::var("HOME").unwrap_or_default())
        .env("TMUX_TMPDIR", "/tmp")
        .env(
            "PATH",
            env::var("PATH")
                .ok()
                .filter(|path| !path.is_empty())
                .unwrap_or_else(|| "/usr/bin:/bin".into()),
        );
    command
}

fn tmux_binary() -> String {
    for variable in ["SIDEVOICE_TMUX_BIN", "CURSOR_AGENT_TMUX_PATH"] {
        if let Some(value) = env::var(variable)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
        {
            return value;
        }
    }
    if let Some(root) = env::var("AGENT_TMUX_ROOT_PATH")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        return PathBuf::from(root)
            .join("bin/tmux")
            .to_string_lossy()
            .into_owned();
    }
    "tmux".into()
}

fn tmux_command(args: &[&str]) -> Result<std::process::Output> {
    let binary = tmux_binary();
    let mut command = tmux_process(binary);
    command.args(tmux_prefix()).args(args);
    let output = command.output()?;
    if !output.status.success() {
        bail!("tmux did not find a cursor-agent persist session");
    }
    Ok(output)
}

fn persist_session(chat: &str) -> Result<Option<(String, String, bool, bool)>> {
    let Ok(output) = tmux_command(&["list-sessions", "-F", "#{session_name}\t#{@cursor_managed}\t#{@cursor_chat_id}\t#{session_attached}\t#{pane_id}\t#{pane_in_mode}"]) else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() < 6
            || fields[1] != "1"
            || !fields[2].eq_ignore_ascii_case(chat)
            || !fields[4].strip_prefix('%').is_some_and(|pane| {
                !pane.is_empty() && pane.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            continue;
        }
        let in_mode = fields[5] == "1";
        return Ok(Some((
            fields[0].into(),
            fields[4].into(),
            in_mode,
            fields[3] == "1",
        )));
    }
    Ok(None)
}

pub async fn refine_identity(identity: &mut Identity, title: &str) {
    if identity.delivery.get("kind").and_then(Value::as_str) != Some("cursor-app") {
        return;
    }
    if !title.is_empty() {
        identity.delivery["title"] = json!(title);
    }
    let instances = bridge_instances();
    if !instances.is_empty() {
        identity.route = Some("cursor-editor-bridge".into());
        identity.delivery["route"] = json!("cursor-editor-bridge");
        if let Some(candidate) = calling_thread(&instances).await {
            identity.delivery["candidate"] = json!(candidate);
        }
    } else {
        identity.route = Some("cursor-editor-view".into());
        identity.delivery["route"] = json!("cursor-editor-view");
    }
}

#[derive(Clone)]
struct BridgeInstance {
    socket: PathBuf,
    token: String,
    user_data: PathBuf,
    created_at: u64,
}

fn bridge_instances() -> Vec<BridgeInstance> {
    let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let dir = env::var("CURSOR_DESKTOP_BRIDGE_DIR")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cursor/desktop-bridge"));
    let mut found = fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            if entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "json")
            {
                return None;
            }
            let value: Value = serde_json::from_slice(&fs::read(entry.path()).ok()?).ok()?;
            if value.get("protocolVersion") != Some(&json!(1)) {
                return None;
            }
            let pid = value.get("pid").and_then(Value::as_u64)?;
            let socket = PathBuf::from(value.get("socketPath").and_then(Value::as_str)?);
            let token = value.get("token").and_then(Value::as_str)?.to_owned();
            let user_data = PathBuf::from(
                value
                    .get("userDataDir")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            );
            if token.is_empty() || !socket.exists() || !process_alive(pid) {
                return None;
            }
            Some(BridgeInstance {
                socket,
                token,
                user_data,
                created_at: value.get("createdAt").and_then(Value::as_u64).unwrap_or(0),
            })
        })
        .collect::<Vec<_>>();
    found.sort_by_key(|instance| std::cmp::Reverse(instance.created_at));
    found
}

fn process_alive(pid: u64) -> bool {
    unsafe {
        libc::kill(pid as i32, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug)]
enum BridgeError {
    Before,
    After(String),
}

struct BridgeResponse {
    status: u16,
    body: Value,
}

async fn bridge_request(
    instance: &BridgeInstance,
    body: &Value,
) -> std::result::Result<BridgeResponse, BridgeError> {
    let mut stream = match tokio::time::timeout(
        Duration::from_secs(3),
        UnixStream::connect(&instance.socket),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(_)) => return Err(BridgeError::Before),
        Err(_) => return Err(BridgeError::Before),
    };
    let payload = body.to_string();
    let request = format!("POST / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", instance.token, payload.len(), payload);
    let transaction = async {
        stream.write_all(request.as_bytes()).await?;
        let mut response = Vec::new();
        let mut block = [0u8; 8192];
        loop {
            let count = stream.read(&mut block).await?;
            if count == 0 {
                break;
            }
            if response.len() + count > 1 << 20 {
                anyhow::bail!("Cursor Desktop Bridge response too large");
            }
            response.extend_from_slice(&block[..count]);
        }
        Ok::<Vec<u8>, anyhow::Error>(response)
    };
    let bytes = match tokio::time::timeout(Duration::from_secs(10), transaction).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => return Err(BridgeError::After(error.to_string())),
        Err(_) => {
            return Err(BridgeError::After(
                "Cursor Desktop Bridge did not answer".into(),
            ))
        }
    };
    let split = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("invalid Desktop Bridge response")
        .map_err(|error| BridgeError::After(error.to_string()))?;
    let headers = String::from_utf8_lossy(&bytes[..split]);
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut response_body = bytes[split + 4..].to_vec();
    if headers.lines().any(|line| {
        line.to_ascii_lowercase().starts_with("transfer-encoding:")
            && line.to_ascii_lowercase().contains("chunked")
    }) {
        response_body =
            decode_chunks(&response_body).map_err(|error| BridgeError::After(error.to_string()))?;
    }
    let body = serde_json::from_slice(&response_body).unwrap_or(Value::Null);
    Ok(BridgeResponse { status, body })
}

fn decode_chunks(mut input: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    loop {
        let line_end = input
            .windows(2)
            .position(|window| window == b"\r\n")
            .context("invalid chunked response")?;
        let size_text = std::str::from_utf8(&input[..line_end])?
            .split(';')
            .next()
            .unwrap_or("");
        let size = usize::from_str_radix(size_text.trim(), 16)?;
        input = &input[line_end + 2..];
        if size == 0 {
            return Ok(output);
        }
        if input.len() < size + 2 || &input[size..size + 2] != b"\r\n" {
            anyhow::bail!("truncated chunked response");
        }
        output.extend_from_slice(&input[..size]);
        input = &input[size + 2..];
    }
}

async fn calling_thread(instances: &[BridgeInstance]) -> Option<String> {
    let mut running = Vec::new();
    for instance in instances {
        let Ok(reply) = bridge_request(instance, &json!({"type":"listThreads"})).await else {
            continue;
        };
        if reply.status != 200 {
            continue;
        }
        if let Some(threads) = reply.body.get("threads").and_then(Value::as_array) {
            running.extend(
                threads
                    .iter()
                    .filter(|thread| {
                        thread.get("status") == Some(&json!("running"))
                            && thread.get("source") == Some(&json!("local"))
                    })
                    .filter_map(|thread| {
                        thread.get("id").and_then(Value::as_str).map(str::to_owned)
                    }),
            );
        }
    }
    (running.len() == 1).then(|| running.remove(0))
}

pub async fn composer_holding(delivery: &Value) -> Option<String> {
    let marker = delivery.get("key").and_then(Value::as_str)?;
    if marker.len() != 64 || !marker.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let candidate = delivery.get("candidate").and_then(Value::as_str);
    let instances = bridge_instances();
    if let Some(candidate) = candidate {
        if safe_chat_id(candidate) {
            return query_bridge_chat(&instances, marker, Some(candidate)).await;
        }
    }
    query_bridge_chat(&instances, marker, None).await
}

fn safe_chat_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

async fn query_bridge_chat(
    instances: &[BridgeInstance],
    marker: &str,
    candidate: Option<&str>,
) -> Option<String> {
    for instance in instances {
        if instance.user_data.as_os_str().is_empty() {
            continue;
        }
        let database = instance.user_data.join("User/globalStorage/state.vscdb");
        if !database.is_file() {
            continue;
        }
        let (from, to) = match candidate.filter(|value| safe_chat_id(value)) {
            Some(chat) => (format!("bubbleId:{chat}:"), format!("bubbleId:{chat};")),
            None => ("bubbleId:".into(), "bubbleId;".into()),
        };
        let sql = format!("SELECT DISTINCT substr(key, 10, instr(substr(key, 10), ':') - 1) AS chat FROM cursorDiskKV WHERE key >= '{from}' AND key < '{to}' AND instr(CAST(value AS TEXT), '{marker}') > 0 LIMIT 2;");
        let mut command = tokio::process::Command::new("sqlite3");
        command
            .args(["-readonly", "-json"])
            .arg(&database)
            .arg(sql)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let result = command.output();
        let Ok(Ok(output)) = tokio::time::timeout(Duration::from_secs(3), result).await else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let Ok(rows) = serde_json::from_slice::<Vec<Value>>(&output.stdout) else {
            continue;
        };
        if rows.len() == 1 {
            if let Some(chat) = rows[0]
                .get("chat")
                .and_then(Value::as_str)
                .filter(|chat| safe_chat_id(chat))
            {
                return Some(chat.to_owned());
            }
        }
    }
    None
}

pub async fn deliver_editor(delivery: &Value, event: &Value) -> Option<Value> {
    let composer = if let Some(composer) = delivery.get("composer").and_then(Value::as_str) {
        composer.to_owned()
    } else {
        composer_holding(delivery).await?
    };
    let text = match envelope(event) {
        Ok(text) => text,
        Err(error) => return Some(json!({"status":"failed","detail":error.to_string()})),
    };
    let instances = bridge_instances();
    for instance in &instances {
        match bridge_request(
            instance,
            &json!({"type":"sendMessage","threadId":composer,"text":text,"force":false}),
        )
        .await
        {
            Err(BridgeError::Before) => continue,
            Err(BridgeError::After(error)) => {
                return Some(
                    json!({"status":"unknown","detail":format!("Cursor Desktop Bridge may have taken the message: {error}")}),
                )
            }
            Ok(response) => {
                let outcome = response
                    .body
                    .get("outcome")
                    .or_else(|| response.body.get("status"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if response.status == 200 && matches!(outcome, "submitted" | "queued") {
                    return Some(
                        json!({"status":"accepted","detail":format!("Cursor Desktop Bridge: {outcome}")}),
                    );
                }
                if matches!(outcome, "not-found" | "unknown-thread") {
                    continue;
                }
                return Some(
                    json!({"status":"unknown","detail":format!("Cursor Desktop Bridge did not confirm delivery: {}", response.body.get("reason").or_else(|| response.body.get("message")).or_else(|| response.body.get("error")).and_then(Value::as_str).unwrap_or(outcome))}),
                );
            }
        }
    }
    None
}

pub async fn deliver(delivery: &Value, event: &Value) -> Result<Value> {
    let chat = delivery
        .get("chat")
        .and_then(Value::as_str)
        .context("Cursor chat missing")?;
    let Some((session, pane, in_mode, attached)) = persist_session(chat)? else {
        return Ok(
            json!({"status":"unsupported","error":format!("Chat {chat} is no longer running in a cursor-agent persist session")}),
        );
    };
    if in_mode {
        let _ = tmux_command(&["send-keys", "-t", &pane, "-X", "cancel"]);
    }
    let text = envelope(event)?;
    let buffer = format!("sidevoice-{}", uuid::Uuid::new_v4());
    let binary = tmux_binary();
    let mut load = tmux_async_process(&binary)
        .args(tmux_prefix())
        .args(["load-buffer", "-b", &buffer, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    load.stdin
        .take()
        .context("tmux load-buffer stdin unavailable")?
        .write_all(text.as_bytes())
        .await?;
    if !load.wait().await?.success() {
        bail!("tmux could not load the voice message");
    }
    let mut paste = tmux_async_process(&binary);
    let pasted = paste
        .args(tmux_prefix())
        .args(["paste-buffer", "-p", "-r", "-d", "-b", &buffer, "-t", &pane])
        .output()
        .await?;
    if !pasted.status.success() {
        bail!("tmux could not paste the voice message");
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
    let mut send = tmux_async_process(binary);
    let entered = send
        .args(tmux_prefix())
        .args(["send-keys", "-t", &pane, "Enter"])
        .output()
        .await?;
    if !entered.status.success() {
        bail!("tmux could not send the voice message");
    }
    Ok(
        json!({"status":"unknown","detail":format!("typed into persistent session {session}{}", if attached {" (someone is attached)"} else {""})}),
    )
}
