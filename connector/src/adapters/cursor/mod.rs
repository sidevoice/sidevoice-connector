// adapters block: this is the harness's own socket, not ours; it moves behind platform::ipc with that block.
#![allow(clippy::disallowed_types)]

pub mod bridge_setup;
mod engine;
mod harness;
mod registration;
mod state_db;

pub use engine::engine;
pub use harness::Cursor;

/// Cursor's configuration home (`mcp.json`), checked through its parent, and its data below it.
pub const HOMES: [crate::profile::AgentHome; 2] = [
    crate::profile::AgentHome {
        var: "CURSOR_CONFIG_DIR",
        default: crate::profile::HomeDefault::Home(".cursor"),
        always_passed: true,
        config: true,
        check_parent: true,
    },
    crate::profile::AgentHome {
        var: "CURSOR_DATA_DIR",
        default: crate::profile::HomeDefault::Under("CURSOR_CONFIG_DIR", "data"),
        always_passed: true,
        config: false,
        check_parent: false,
    },
];

/// Cursor's configuration home, as the profile resolves it.
pub fn config_home(profile: &crate::profile::Profile) -> &std::path::Path {
    profile.agent_home(HOMES[0].var)
}

use super::{envelope, Identity};
use crate::platform::process::{Process, Processes};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
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

const EDITOR_NOTE: &str = "Sidevoice cannot tell which Cursor chat this is: this Cursor did not declare MCP Apps support (the io.modelcontextprotocol/ui extension) when it started this server, which is how the editor is told apart, nor is this a Cursor CLI chat (cursor-agent), whose open chat Sidevoice can see. voice_diagnose shows where Sidevoice's logs are.";
const NOT_PERSISTED: &str = "This chat is not running under cursor-agent persist, so nothing can put a message into it. To let Sidevoice talk to a Cursor CLI chat (experimental), start it with  cursor-agent persist  (it needs tmux).";

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

/// Whether the client is Cursor's editor: it declares MCP Apps support (HTML views) when it starts a server, and its
/// CLI does not. Nothing is drawn; it only tells an editor chat from a CLI one.
pub fn is_editor(capabilities: &Value) -> bool {
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
    if is_editor(capabilities) {
        let thread = format!("cursor-editor-{}", uuid::Uuid::new_v4());
        // Unique to this join: the join call's answer carries it, so the Desktop Bridge finds this chat by it in
        // Cursor's own record of the call.
        let marker = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let route = "cursor-editor-bridge";
        let delivery = json!({"kind":"cursor-bridge","thread":thread,"marker":marker,"route":route,"joined_at":unix_ms()});
        let mut identity = Identity::new("cursor", thread.clone(), delivery);
        identity.route = Some(route.into());
        identity.editor = true;
        identity.detachable = true;
        identity.chat_marker = Some(marker);
        identity.experimental.extend([
            "sessionIdentity".into(),
            "working".into(),
            "endOfTurn".into(),
        ]);
        identity.watch_note = Some("Sidevoice learns that this chat took a voice message, and when it is working, from Cursor itself: from this chat's transcript, once the Desktop Bridge knows which chat it is.".into());
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
        let parent = Processes::parent(pid)?;
        if parent <= 1 {
            return None;
        }
        let mut chats = Vec::<Chat>::new();
        for path in Processes::open_files(parent).unwrap_or_default() {
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
    transcript_path_in(chat_id, &transcript_roots())
}

fn transcript_path_in(chat_id: &str, roots: &[PathBuf]) -> Option<PathBuf> {
    for root in roots {
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

#[cfg(test)]
mod pinned_transcript_tests {
    use super::*;

    #[test]
    fn joined_chat_transcript_does_not_switch_to_newer_unrelated_chat() {
        let root = std::env::temp_dir().join(format!("sv-cursor-pin-{}", uuid::Uuid::new_v4()));
        let joined = root.join("projects/project/agent-transcripts/joined/joined.jsonl");
        let other = root.join("projects/project/agent-transcripts/other/other.jsonl");
        fs::create_dir_all(joined.parent().unwrap()).unwrap();
        fs::create_dir_all(other.parent().unwrap()).unwrap();
        fs::write(&joined, b"{\"role\":\"user\"}\n").unwrap();
        assert_eq!(
            transcript_path_in("joined", std::slice::from_ref(&root)),
            Some(joined.clone())
        );
        fs::write(&other, b"{\"role\":\"user\"}\n").unwrap();
        assert_eq!(
            transcript_path_in("joined", std::slice::from_ref(&root)),
            Some(joined)
        );
        fs::remove_dir_all(root).unwrap();
    }
}

/// Find the Cursor chat whose transcript records this join or one of its pending inputs.
/// Editor conversation ids are ours; Cursor's own chat id is only available in the transcript
/// or in the Desktop Bridge state database.
pub fn editor_transcript_chat(
    delivery: &Value,
    expected_ids: &[String],
    excluded: &HashSet<String>,
) -> Option<(String, bool)> {
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

    if expected_matches.len() == 1 {
        return expected_matches.pop().map(|chat| (chat, true));
    }
    if title_matches.len() == 1 {
        return title_matches.pop().map(|chat| (chat, false));
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
    if identity.delivery.get("kind").and_then(Value::as_str) != Some("cursor-bridge") {
        return;
    }
    if !title.is_empty() {
        identity.delivery["title"] = json!(title);
    }
    let instances = bridge_instances();
    if let Some(candidate) = calling_thread(&instances).await {
        identity.delivery["candidate"] = json!(candidate);
    }
}

#[derive(Clone)]
struct BridgeInstance {
    socket: PathBuf,
    token: String,
    user_data: PathBuf,
    created_at: u64,
}

/// Diagnostics contain only fixed stage/result codes and a short one-way digest of a random ID.
/// Never pass a bridge token, message text, response body, database path or socket path here.
fn bridge_diagnostic(
    stage: &'static str,
    result: &'static str,
    id: Option<&str>,
    instance: Option<usize>,
    http_status: Option<u16>,
) -> String {
    let id = id
        .map(|value| hex::encode(Sha256::digest(value.as_bytes()))[..12].to_owned())
        .unwrap_or_else(|| "none".into());
    format!(
        "cursor_bridge stage={stage} result={result} id_hash={id} instance={} http_status={}",
        instance.map_or_else(|| "none".into(), |index| index.to_string()),
        http_status.map_or_else(|| "none".into(), |status| status.to_string())
    )
}

fn log_bridge(
    stage: &'static str,
    result: &'static str,
    id: Option<&str>,
    instance: Option<usize>,
    http_status: Option<u16>,
) {
    crate::logfile::log(&bridge_diagnostic(stage, result, id, instance, http_status));
}

fn short_hash(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))[..12].to_owned()
}

fn instance_hash(instance: &BridgeInstance) -> String {
    short_hash(&format!(
        "{}:{}",
        instance.socket.display(),
        instance.created_at
    ))
}

/// A mapping or accepted send can be correlated across descriptor reordering without exposing IDs or paths.
fn correlation_diagnostic(
    stage: &'static str,
    result: &'static str,
    marker: Option<&str>,
    chat: &str,
    instance: Option<&BridgeInstance>,
) -> String {
    format!(
        "cursor_bridge stage={stage} result={result} marker_hash={} chat_hash={} instance_id={}",
        marker.map(short_hash).unwrap_or_else(|| "none".into()),
        short_hash(chat),
        instance.map(instance_hash).unwrap_or_else(|| "none".into())
    )
}

fn log_correlation(
    stage: &'static str,
    result: &'static str,
    marker: Option<&str>,
    chat: &str,
    instance: Option<&BridgeInstance>,
) {
    let line = correlation_diagnostic(stage, result, marker, chat, instance);
    #[cfg(test)]
    correlation_lines()
        .lock()
        .expect("correlation test sink")
        .push(line.clone());
    crate::logfile::log(&line);
}

#[cfg(test)]
fn correlation_lines() -> &'static std::sync::Mutex<Vec<String>> {
    static LINES: std::sync::OnceLock<std::sync::Mutex<Vec<String>>> = std::sync::OnceLock::new();
    LINES.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

fn bridge_instances() -> Vec<BridgeInstance> {
    let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let dir = env::var("CURSOR_DESKTOP_BRIDGE_DIR")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cursor/desktop-bridge"));
    bridge_instances_in(&dir)
}

fn bridge_instances_in(dir: &Path) -> Vec<BridgeInstance> {
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

/// Whether a Desktop Bridge of Cursor's is running for this user.
pub fn bridge_running() -> bool {
    !bridge_instances().is_empty()
}

fn process_alive(pid: u64) -> bool {
    u32::try_from(pid).is_ok_and(|pid| Processes::identity(pid).is_some())
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

fn confirmed_send_outcome(response: &BridgeResponse) -> Option<&str> {
    let outcome = response
        .body
        .get("outcome")
        .and_then(Value::as_str)
        .or_else(|| response.body.get("status").and_then(Value::as_str));
    (response.status == 200)
        .then_some(outcome?)
        .filter(|outcome| matches!(*outcome, "submitted" | "queued"))
}

/// What a `sendMessage` reply means for delivery.
///
/// The send always carries `force:false`: Cursor decides what to do with a message for a chat whose agent is
/// running. `queued` is the bridge saying it took the message but will not run it now (it waits for the running
/// turn); `submitted` is a turn started. The connector cannot steer a running Cursor turn: no documented request
/// field delivers input mid-turn (`force` is undocumented and its effect unverified, and it may interrupt the turn),
/// so a busy chat holds the message and Cursor runs it when its queue reaches it.
#[derive(Debug, PartialEq)]
enum SendReply {
    /// Taken: `submitted` (idle chat, turn started) or `queued` (busy chat, held behind the running turn).
    Accepted(&'static str),
    UnknownThread,
    /// The bridge says it did not send it, even with HTTP 200: nothing reached the chat.
    Error(String),
    /// Refused outright (4xx): nothing reached the chat.
    Refused(String),
    /// Anything else: the message may or may not have been taken.
    Unconfirmed(String),
}

fn classify_send_reply(response: &BridgeResponse, sent: &str) -> SendReply {
    let outcome = response
        .body
        .get("outcome")
        .and_then(Value::as_str)
        .or_else(|| response.body.get("status").and_then(Value::as_str))
        .unwrap_or("");
    let plain_reason = || {
        ["reason", "message", "error"]
            .iter()
            .find_map(|key| response.body.get(*key).and_then(Value::as_str))
            .unwrap_or(outcome)
            .to_owned()
    };
    match confirmed_send_outcome(response) {
        Some("submitted") => return SendReply::Accepted("submitted"),
        Some(_) => return SendReply::Accepted("queued"),
        None => {}
    }
    if matches!(outcome, "not-found" | "unknown-thread") {
        SendReply::UnknownThread
    } else if outcome == "error" {
        SendReply::Error(bridge_error_reason(&response.body, sent))
    } else if (400..500).contains(&response.status) {
        SendReply::Refused(plain_reason())
    } else {
        SendReply::Unconfirmed(plain_reason())
    }
}

/// The bridge's own error text, short and on one line; withheld if it echoes the message that was sent.
fn bridge_error_reason(body: &Value, sent: &str) -> String {
    let reason = ["message", "error", "reason"]
        .iter()
        .find_map(|key| body.get(*key).and_then(Value::as_str))
        .unwrap_or("")
        .trim();
    if reason.is_empty() {
        return "no reason given".into();
    }
    if reason.len() > 200 || sent.contains(reason) || reason.contains(sent.trim()) {
        return "[withheld]".into();
    }
    reason.replace(['\n', '\r'], " ")
}

/// Keep the shape of an unexpected bridge reply, never its arbitrary strings (which may echo input).
fn diagnostic_reply_body(body: &Value) -> String {
    fn redact(value: &Value, field: &str) -> Value {
        match value {
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .map(|(key, value)| {
                        let safe_key = match key.as_str() {
                            "outcome" | "status" | "success" | "ok" | "error" | "reason"
                            | "message" | "threadId" | "queued" | "submitted" | "result"
                            | "code" | "detail" | "data" | "text" | "content" => key.as_str(),
                            _ => "<other>",
                        };
                        (safe_key.to_owned(), redact(value, safe_key))
                    })
                    .collect(),
            ),
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .take(8)
                    .map(|item| redact(item, field))
                    .collect(),
            ),
            Value::String(text)
                if matches!(field, "outcome" | "status")
                    && matches!(
                        text.as_str(),
                        "submitted"
                            | "queued"
                            | "not-found"
                            | "unknown-thread"
                            | "success"
                            | "ok"
                            | "error"
                            | "failed"
                            | "accepted"
                            | "done"
                    ) =>
            {
                Value::String(text.clone())
            }
            Value::String(_) => Value::String("[redacted]".into()),
            _ => value.clone(),
        }
    }
    let rendered = redact(body, "").to_string();
    if rendered.len() <= 512 {
        rendered
    } else {
        let mut end = 512;
        while !rendered.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &rendered[..end])
    }
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
            if let Some(headers_end) = response.windows(4).position(|window| window == b"\r\n\r\n")
            {
                let headers = String::from_utf8_lossy(&response[..headers_end]);
                if let Some(length) = headers.lines().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                }) {
                    if response.len() >= headers_end + 4 + length {
                        break;
                    }
                }
            }
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
    composer_holding_in(delivery, &bridge_instances(), false).await
}

pub async fn composer_holding_for_delivery(delivery: &Value) -> Option<String> {
    composer_holding_in(delivery, &bridge_instances(), true).await
}

async fn composer_holding_in(
    delivery: &Value,
    instances: &[BridgeInstance],
    diagnostics: bool,
) -> Option<String> {
    let marker = delivery.get("marker").and_then(Value::as_str)?;
    if marker.len() != 64 || !marker.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        if diagnostics {
            log_bridge("lookup", "invalid_marker", None, None, None);
        }
        return None;
    }
    let candidate = delivery.get("candidate").and_then(Value::as_str);
    if diagnostics && instances.is_empty() {
        log_bridge("lookup", "no_bridge_instance", Some(marker), None, None);
    }
    if let Some(candidate) = candidate {
        if safe_chat_id(candidate) {
            if let Some(chat) =
                query_bridge_chat(instances, marker, Some(candidate), diagnostics).await
            {
                return Some(chat);
            }
        }
    }
    query_bridge_chat(instances, marker, None, diagnostics).await
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
    diagnostics: bool,
) -> Option<String> {
    for (index, instance) in instances.iter().enumerate() {
        if instance.user_data.as_os_str().is_empty() {
            if diagnostics {
                log_bridge("lookup", "no_user_data", Some(marker), Some(index), None);
            }
            continue;
        }
        let database = instance.user_data.join("User/globalStorage/state.vscdb");
        if !database.is_file() {
            if diagnostics {
                log_bridge(
                    "lookup",
                    "database_missing",
                    Some(marker),
                    Some(index),
                    None,
                );
            }
            continue;
        }
        let (from, to) = match candidate.filter(|value| safe_chat_id(value)) {
            Some(chat) => (format!("bubbleId:{chat}:"), format!("bubbleId:{chat};")),
            None => ("bubbleId:".into(), "bubbleId;".into()),
        };
        let sql = format!("SELECT DISTINCT substr(key, 10, instr(substr(key, 10), ':') - 1) AS chat FROM cursorDiskKV WHERE key >= '{from}' AND key < '{to}' AND instr(CAST(value AS TEXT), '{marker}') > 0 LIMIT 2;");
        let rows = match state_db::query(&database, &sql).await {
            Ok(rows) => rows,
            Err(error) => {
                if diagnostics {
                    let result = match error {
                        state_db::DbError::Open => "sqlite_start_failed",
                        state_db::DbError::TimedOut => "sqlite_timeout",
                        state_db::DbError::Query => "sqlite_failed",
                        state_db::DbError::TooLarge | state_db::DbError::InvalidValue => {
                            "sqlite_output_invalid"
                        }
                    };
                    log_bridge("lookup", result, Some(marker), Some(index), None);
                }
                continue;
            }
        };
        if rows.len() == 1 {
            if let Some(chat) = rows[0]
                .get("chat")
                .and_then(Value::as_str)
                .filter(|chat| safe_chat_id(chat))
            {
                log_correlation(
                    "mapping",
                    if candidate.is_some() {
                        "sqlite_candidate"
                    } else {
                        "sqlite_scan"
                    },
                    Some(marker),
                    chat,
                    Some(instance),
                );
                if diagnostics {
                    log_bridge("lookup", "chat_found", Some(chat), Some(index), None);
                }
                return Some(chat.to_owned());
            }
        }
        if diagnostics {
            log_bridge(
                "lookup",
                if rows.is_empty() {
                    "marker_absent"
                } else {
                    "chat_ambiguous"
                },
                Some(marker),
                Some(index),
                None,
            );
        }
    }
    None
}

pub async fn deliver_editor(delivery: &Value, event: &Value) -> Option<Value> {
    let instances = bridge_instances();
    deliver_editor_to_instances(delivery, event, &instances).await
}

async fn deliver_editor_to_instances(
    delivery: &Value,
    event: &Value,
    instances: &[BridgeInstance],
) -> Option<Value> {
    let composer = if let Some(composer) = delivery.get("composer").and_then(Value::as_str) {
        composer.to_owned()
    } else {
        composer_holding_in(delivery, instances, true).await?
    };
    let text = match envelope(event) {
        Ok(text) => text,
        Err(error) => return Some(json!({"status":"failed","detail":error.to_string()})),
    };
    if instances.is_empty() {
        log_bridge("send", "no_bridge_instance", Some(&composer), None, None);
    }
    for (index, instance) in instances.iter().enumerate() {
        match bridge_request(
            instance,
            &json!({"type":"sendMessage","threadId":composer,"text":text,"force":false}),
        )
        .await
        {
            Err(BridgeError::Before) => {
                log_bridge("send", "connect_failed", Some(&composer), Some(index), None);
                continue;
            }
            Err(BridgeError::After(error)) => {
                log_bridge(
                    "send",
                    "reply_uncertain",
                    Some(&composer),
                    Some(index),
                    None,
                );
                return Some(
                    json!({"status":"unknown","detail":format!("Cursor Desktop Bridge may have taken the message: {error}")}),
                );
            }
            Ok(response) => match classify_send_reply(&response, &text) {
                SendReply::Accepted(outcome) => {
                    log_correlation(
                        "accepted",
                        outcome,
                        delivery.get("marker").and_then(Value::as_str),
                        &composer,
                        Some(instance),
                    );
                    log_bridge(
                        "send",
                        outcome,
                        Some(&composer),
                        Some(index),
                        Some(response.status),
                    );
                    return Some(
                        json!({"status":"accepted","detail":format!("Cursor Desktop Bridge: {outcome}")}),
                    );
                }
                SendReply::UnknownThread => {
                    log_bridge(
                        "send",
                        "unknown_thread",
                        Some(&composer),
                        Some(index),
                        Some(response.status),
                    );
                    continue;
                }
                SendReply::Error(reason) => {
                    crate::logfile::log(&format!(
                        "{} reason={reason}",
                        bridge_diagnostic(
                            "send",
                            "bridge_error",
                            Some(&composer),
                            Some(index),
                            Some(response.status)
                        )
                    ));
                    return Some(
                        json!({"status":"failed","detail":format!("Cursor Desktop Bridge could not send the message: {reason}")}),
                    );
                }
                SendReply::Refused(reason) => {
                    log_bridge(
                        "send",
                        "refused",
                        Some(&composer),
                        Some(index),
                        Some(response.status),
                    );
                    return Some(
                        json!({"status":"failed","detail":format!("Cursor Desktop Bridge refused the message ({}): {reason}", response.status)}),
                    );
                }
                SendReply::Unconfirmed(reason) => {
                    crate::logfile::log(&format!(
                        "{} reply_body={}",
                        bridge_diagnostic(
                            "send",
                            "unconfirmed_reply",
                            Some(&composer),
                            Some(index),
                            Some(response.status)
                        ),
                        diagnostic_reply_body(&response.body)
                    ));
                    return Some(
                        json!({"status":"unknown","detail":format!("Cursor Desktop Bridge did not confirm delivery: {reason}")}),
                    );
                }
            },
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

#[cfg(test)]
mod bridge_tests {
    use super::*;
    use tokio::net::UnixListener;

    #[test]
    fn send_reply_requires_explicit_confirmation() {
        for (status, body, expected) in [
            (200, json!({"outcome":"submitted"}), Some("submitted")),
            (200, json!({"status":"queued","extra":true}), Some("queued")),
            (
                200,
                json!({"outcome":null,"status":"submitted"}),
                Some("submitted"),
            ),
            (200, json!({"success":true}), None),
            (200, json!({"status":"success"}), None),
            (200, json!({"outcome":"unknown-thread"}), None),
            (200, Value::Null, None),
            (202, json!({"outcome":"queued"}), None),
        ] {
            let response = BridgeResponse { status, body };
            assert_eq!(confirmed_send_outcome(&response), expected);
        }
    }

    #[test]
    fn send_replies_are_classified_for_idle_and_busy_chats() {
        let reply = |status, body| classify_send_reply(&BridgeResponse { status, body }, "hola");
        // Idle chat: a turn starts. Busy chat: held behind the running turn. Both are accepted.
        assert_eq!(
            reply(200, json!({"outcome":"submitted"})),
            SendReply::Accepted("submitted")
        );
        assert_eq!(
            reply(200, json!({"status":"queued"})),
            SendReply::Accepted("queued")
        );
        // HTTP 200 alone, or an unknown outcome, proves nothing and is never reported as a failure.
        assert!(matches!(reply(200, json!({})), SendReply::Unconfirmed(_)));
        assert!(matches!(
            reply(200, json!({"outcome":"busy"})),
            SendReply::Unconfirmed(r) if r == "busy"
        ));
        assert!(matches!(reply(500, Value::Null), SendReply::Unconfirmed(_)));
        // A busy-chat rejection is a failure (the core can fall back), not a retry loop.
        assert_eq!(
            reply(200, json!({"status":"error","message":"agent is running"})),
            SendReply::Error("agent is running".into())
        );
        assert_eq!(
            reply(409, json!({"reason":"agent is running"})),
            SendReply::Refused("agent is running".into())
        );
        assert_eq!(
            reply(200, json!({"outcome":"unknown-thread"})),
            SendReply::UnknownThread
        );
        assert_eq!(
            reply(404, json!({"status":"not-found"})),
            SendReply::UnknownThread
        );
    }

    #[test]
    fn bridge_error_reason_is_logged_unless_it_echoes_the_message() {
        let sent = "please summarise the diff";
        let reason = |body: Value| bridge_error_reason(&body, sent);
        assert_eq!(
            reason(json!({"status":"error","message":"No composer for thread"})),
            "No composer for thread"
        );
        assert_eq!(
            reason(json!({"status":"error","message":sent})),
            "[withheld]"
        );
        assert_eq!(
            reason(json!({"status":"error","message":format!("failed: {sent}")})),
            "[withheld]"
        );
        assert_eq!(reason(json!({"status":"error"})), "no reason given");
        assert_eq!(
            reason(json!({"status":"error","message":"x".repeat(300)})),
            "[withheld]"
        );
    }

    #[test]
    fn unconfirmed_reply_diagnostic_keeps_shape_without_private_text() {
        let secret = "private voice message";
        let body = json!({"success":true,"status":"success","message":secret,
            "data":{"text":secret,"error":"not in focused chat","count":3},
            "unknown_field":secret});
        let rendered = diagnostic_reply_body(&body);
        assert!(rendered.contains("\"success\":true"));
        assert!(rendered.contains("\"status\":\"success\""));
        assert!(!rendered.contains("\"count\":3"));
        assert!(!rendered.contains(secret));
        assert!(!rendered.contains("not in focused chat"));
        assert!(rendered.len() <= 515);
        assert!(diagnostic_reply_body(&json!({"message":"x".repeat(1000)})).len() <= 515);
    }

    #[tokio::test]
    async fn bridge_response_finishes_at_content_length_without_waiting_for_socket_close() {
        // Short label: its socket must fit macOS's 104-byte SUN_LEN (the temp dir alone is ~59 bytes there).
        let scratch = crate::test_support::temp_dir("bridge-length");
        let socket = scratch.path().join("bridge.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let instance = BridgeInstance {
            socket,
            token: "fixture-secret".into(),
            user_data: scratch.path().to_path_buf(),
            created_at: 1,
        };
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 8192];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let body = r#"{"outcome":"submitted"}"#;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
        });
        let response = tokio::time::timeout(
            Duration::from_millis(100),
            bridge_request(&instance, &json!({"type":"sendMessage"})),
        )
        .await
        .expect("a complete response must beat the ten-second delivery deadline")
        .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body["outcome"], "submitted");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn queued_bridge_submission_has_no_editor_consumption_receipt() {
        // Short label, for the same SUN_LEN reason.
        let scratch = crate::test_support::temp_dir("bridge-delivery");
        let descriptors = scratch.path().join("desktop-bridge");
        fs::create_dir(&descriptors).unwrap();
        let user_data = scratch.path().join("Cursor");
        let storage = user_data.join("User/globalStorage");
        fs::create_dir_all(&storage).unwrap();
        let database = storage.join("state.vscdb");
        let marker = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let sql = format!(
            "CREATE TABLE cursorDiskKV(key TEXT, value TEXT); INSERT INTO cursorDiskKV VALUES ('bubbleId:chat-1:turn', 'join {marker}');"
        );
        state_db::fixture(&database, &sql);

        let socket = scratch.path().join("bridge.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        fs::write(
            descriptors.join("one.json"),
            serde_json::to_vec(&json!({
                "protocolVersion":1,"pid":std::process::id(),"socketPath":socket,
                "token":"fixture-secret","userDataDir":user_data,"createdAt":1
            }))
            .unwrap(),
        )
        .unwrap();
        let instances = bridge_instances_in(&descriptors);
        assert_eq!(instances.len(), 1);
        // The background watcher uses this quiet lookup. Its successful mapping must still
        // carry enough provenance to correlate a later cached send with the accepting bridge.
        let chat = composer_holding_in(&json!({"marker":marker}), &instances, false).await;
        assert_eq!(chat.as_deref(), Some("chat-1"));

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let count = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            assert!(request.contains("\"type\":\"sendMessage\""));
            assert!(request.contains("\"threadId\":\"chat-1\""));
            let response = r#"{"outcome":"queued"}"#;
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            // This fake bridge never writes to an editor transcript. The HTTP reply says only
            // that it queued the request; no later input.read can be inferred from it.
        });
        let event = json!({"channel":"voice","session_id":"s","revision":1,"epoch":0,"message_id":"m","text":"private words"});
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            deliver_editor_to_instances(
                &json!({"composer":chat,"marker":marker}),
                &event,
                &instances,
            ),
        )
        .await
        .expect("bridge acceptance must arrive before the ten-second delivery deadline")
        .unwrap();
        server.await.unwrap();
        assert_eq!(result["status"], "accepted");
        assert_eq!(result["detail"], "Cursor Desktop Bridge: queued");
        assert!(!scratch.path().join("chat.jsonl").exists());
        let lines = correlation_lines().lock().unwrap();
        let matching = lines
            .iter()
            .filter(|line| line.contains(&format!("marker_hash={}", short_hash(&marker))))
            .collect::<Vec<_>>();
        let mapping = matching
            .iter()
            .find(|line| line.contains("stage=mapping result=sqlite_scan"))
            .unwrap();
        let accepted = matching
            .iter()
            .find(|line| line.contains("stage=accepted result=queued"))
            .unwrap();
        for field in ["marker_hash", "chat_hash", "instance_id"] {
            let value = mapping
                .split_whitespace()
                .find(|word| word.starts_with(&format!("{field}=")))
                .unwrap();
            assert!(
                accepted.contains(value),
                "{field} cannot correlate: {accepted}"
            );
        }
        for secret in [&marker[..], "chat-1", "fixture-secret", "private words"] {
            assert!(!mapping.contains(secret) && !accepted.contains(secret));
        }
    }

    #[tokio::test]
    async fn successful_sqlite_query_with_no_rows_is_marker_absent() {
        let scratch = crate::test_support::temp_dir("cursor-empty-lookup");
        let database = scratch.path().join("state.vscdb");
        state_db::fixture(
            &database,
            "CREATE TABLE cursorDiskKV(key TEXT, value TEXT);",
        );
        let rows = state_db::query(
            &database,
            "SELECT key FROM cursorDiskKV WHERE key = 'missing';",
        )
        .await
        .unwrap();
        assert!(rows.is_empty());
        assert_eq!(
            state_db::query(&database, "not-sql").await,
            Err(state_db::DbError::Query)
        );
    }

    #[test]
    fn diagnostic_never_contains_ids_or_message_material() {
        let line = bridge_diagnostic(
            "send",
            "queued",
            Some("private-chat-id"),
            Some(1),
            Some(200),
        );
        assert!(line.contains("cursor_bridge stage=send result=queued"));
        assert!(line.contains("instance=1 http_status=200"));
        assert!(!line.contains("private-chat-id"));
        assert!(!line.contains("private words"));
        assert!(!line.contains("fixture-secret"));
    }
}
