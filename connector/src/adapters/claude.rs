mod binary;
mod harness;
mod registration;
#[cfg(any(test, feature = "testbench"))]
pub(crate) use registration::PROGRAMS;

pub use harness::Sessions;

/// Claude Code, as Sidevoice is installed in it (its [`Registration`](super::registration::Registration)).
pub struct Claude;

/// Claude Code's home: its data (projects, sessions) and, when the variable is set, its user configuration. Passed
/// on only when the person's environment sets it: Claude Code keeps its user configuration, MCP servers included,
/// in `~/.claude.json` without it and in `$CLAUDE_CONFIG_DIR/.claude.json` with it.
pub const HOME: crate::profile::AgentHome = crate::profile::AgentHome {
    var: "CLAUDE_CONFIG_DIR",
    default: crate::profile::HomeDefault::Home(".claude"),
    always_passed: false,
    config: true,
    check_parent: false,
};

use super::envelope;
use super::harness::{Conversation, Inbound, Limit, Sent};
use crate::platform::env::{Env, Environment};
use crate::platform::foreign::{Foreign, Foreigners};
use crate::platform::process::{ChildProcess, Process, Processes};
use crate::profile::Profile;
use anyhow::Result;
use serde_json::{json, Value};

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The file Claude Code keeps its user configuration in, user-scope MCP servers included, resolved as its CLI resolves
/// it: `$CLAUDE_CONFIG_DIR/.claude.json` when the person's environment sets the variable, else `~/.claude.json`.
pub(crate) fn user_config(profile: &Profile) -> PathBuf {
    if profile.agent_home_set(HOME.var) {
        profile.agent_home(HOME.var).join(".claude.json")
    } else {
        profile.home.join(".claude.json")
    }
}

/// The largest `.claude.json` read: it also holds Claude Code's per-project state.
const USER_CONFIG_LIMIT: u64 = 64 << 20;

/// Claude Code's user configuration cannot be parsed or cannot be read safely: it is left alone, whatever else is
/// there.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Unreadable;

/// Reads [`user_config`] with the guarded reader, before anything else is judged: a regular file of the person's,
/// writable by nobody else, and JSON. A link is followed wherever it leads (dotfiles managers link it), and the file
/// it leads to is judged; one that leads nowhere is unreadable. `None` when there is no such file.
pub(crate) fn read_user_config(profile: &Profile) -> Result<Option<serde_json::Value>, Unreadable> {
    let file = user_config(profile);
    match fs::symlink_metadata(&file) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(Unreadable),
        Ok(_) => {}
    }
    let text =
        super::registration::read_profile_file(&file, USER_CONFIG_LIMIT).map_err(|_| Unreadable)?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|_| Unreadable)
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// Where Claude Code keeps its state, as the agent's environment `env` names it: `$CLAUDE_CONFIG_DIR`, else
/// `~/.claude`.
pub fn agent_home(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    env(HOME.var)
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env(Environment::HOME)
                .filter(|home| !home.is_empty())
                .map(|home| PathBuf::from(home).join(".claude"))
        })
}

/// How many processes up from the MCP server Claude Code's own process is looked for: Claude Code may start the
/// server through a launcher (npx, the npm package's script).
const ANCESTORS: usize = 8;

/// Claude Code's own process, which the MCP server runs below: `CLAUDE_PID` when Claude Code names it, else the
/// nearest ancestor with a session record in `home`. Claude Code keeps one record per process
/// (`sessions/<pid>.json`), and rewrites the session id in it when the session changes (`/clear`, a resumed one).
pub fn agent_process(home: &Path, env: impl Fn(&str) -> Option<String>) -> Option<u32> {
    let has_record = |pid: u32| home.join("sessions").join(format!("{pid}.json")).is_file();
    if let Some(pid) = env("CLAUDE_PID").and_then(|pid| pid.parse().ok()) {
        return Some(pid);
    }
    let mut pid = std::process::id();
    for _ in 0..ANCESTORS {
        pid = Processes::parent(pid)?;
        if has_record(pid) {
            return Some(pid);
        }
    }
    None
}

/// A Claude Code session as the daemon finds it: from the agent's home and process, as its MCP server saw them at
/// join, and the session id it joined with.
#[derive(Clone, Debug)]
pub struct Session {
    home: Option<PathBuf>,
    pid: Option<u32>,
    id: String,
}

/// A session record: `sessions/<pid>.json`.
#[derive(Debug)]
pub struct Record {
    pid: Option<u32>,
    value: Value,
}

impl Record {
    fn text(&self, key: &str) -> Option<&str> {
        self.value
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
    }

    /// The session the process runs now.
    pub fn session_id(&self) -> Option<&str> {
        self.text("sessionId")
    }
}

impl Session {
    pub fn of(conversation: &Conversation) -> Self {
        Self {
            home: conversation.agent.home.clone(),
            pid: conversation.agent.pid,
            id: conversation.id.clone(),
        }
    }

    /// The session id it joined with.
    pub fn id(&self) -> &str {
        &self.id
    }

    fn sessions(&self) -> Option<PathBuf> {
        Some(self.home.as_ref()?.join("sessions"))
    }

    /// The session's record: the agent process's own while it runs (whatever session it runs now), else the records
    /// naming the session id it joined with, a live process's first, then the newest.
    pub fn record(&self) -> Option<Record> {
        let sessions = self.sessions()?;
        if let Some(pid) = self.pid.filter(|pid| alive(*pid)) {
            if let Some(value) = read_json(&sessions.join(format!("{pid}.json"))) {
                return Some(Record {
                    pid: Some(pid),
                    value,
                });
            }
        }
        let mut found = Vec::new();
        for entry in fs::read_dir(&sessions).ok()?.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let Some(value) = read_json(&path) else {
                continue;
            };
            if value.get("sessionId").and_then(Value::as_str) != Some(self.id.as_str()) {
                continue;
            }
            let pid = value
                .get("pid")
                .and_then(Value::as_u64)
                .and_then(|pid| u32::try_from(pid).ok());
            let modified = entry.metadata().and_then(|meta| meta.modified()).ok();
            found.push((pid.is_some_and(alive), modified, Record { pid, value }));
        }
        found.sort_by_key(|(alive, modified, _)| std::cmp::Reverse((*alive, *modified)));
        found.into_iter().next().map(|(_, _, record)| record)
    }

    /// The `peerToken` of the key file of process `pid` (`sessions/<pid>.<hash>.key`), the newest if there are several.
    fn peer_token(&self, pid: u32) -> Option<String> {
        let prefix = format!("{pid}.");
        let mut keys: Vec<_> = fs::read_dir(self.sessions()?)
            .ok()?
            .flatten()
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.starts_with(&prefix) && name.ends_with(".key")
            })
            .map(|entry| {
                let modified = entry.metadata().and_then(|meta| meta.modified()).ok();
                (modified, entry.path())
            })
            .collect();
        keys.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
        keys.into_iter().find_map(|(_, path)| {
            read_json(&path)?
                .get("peerToken")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
    }

    /// The inbox of the session's process: the socket (a named pipe on Windows) its record names, and the token it
    /// takes ([`inbox_token`]); why not when there is none.
    fn inbox(&self) -> Result<(String, String), Limit> {
        let unreachable = |why: String| Limit::Unreachable(why);
        let Some(sessions) = self.sessions() else {
            return Err(unreachable(
                "the agent's environment does not say where Claude Code keeps its sessions".into(),
            ));
        };
        let Some(record) = self.record() else {
            return Err(unreachable(format!(
                "Claude Code's record of this session is not in {}: is the session still running?",
                sessions.display()
            )));
        };
        let Some(socket) = record.text("messagingSocketPath").map(str::to_owned) else {
            return Err(unreachable(
                "Claude Code's record of this session names no inbox for messages".into(),
            ));
        };
        let token = inbox_token(record.pid.and_then(|pid| self.peer_token(pid)))?;
        Ok((socket, token))
    }

    /// The transcript of the session the process runs now.
    pub fn transcript(&self, session_id: &str) -> Option<PathBuf> {
        let projects = self.home.as_ref()?.join("projects");
        for entry in fs::read_dir(projects).ok()?.flatten() {
            let path = entry.path().join(format!("{session_id}.jsonl"));
            if path.exists() {
                return Some(path);
            }
        }
        None
    }

    /// Whether the record says busy.
    pub fn working(&self) -> Option<bool> {
        match self.record()?.text("status")? {
            "busy" => Some(true),
            "idle" | "ready" | "waiting" => Some(false),
            _ => None,
        }
    }

    /// The engine the process was launched with (`--model`, `--effort`, `--thinking`).
    pub fn engine(&self) -> Option<Value> {
        let args = launch_args(self.record()?.pid);
        if args.is_empty() {
            return None;
        }
        let model = flag(&args, "--model");
        let effort = flag(&args, "--effort");
        let thinking = flag(&args, "--thinking");
        if model.is_none() && effort.is_none() {
            return None;
        }
        Some(json!({"model":model,"effort":effort,"thinking":thinking}))
    }

    /// Whether messages can get into the session: its permissions, then whether its inbox can be found and is one
    /// this OS can open (native Windows documents a named pipe and nothing else).
    pub fn inbound(&self) -> Inbound {
        if let Inbound::Blocked(limit) = self.permissions() {
            return Inbound::Blocked(limit);
        }
        match self.inbox() {
            Err(limit) => Inbound::Blocked(limit),
            Ok((socket, _)) => match Foreigners::check(std::ffi::OsStr::new(&socket)) {
                Err(error) => Inbound::Blocked(Limit::NoRoute(format!(
                    "Claude Code's inbox cannot be reached from here: {error}"
                ))),
                Ok(_) => Inbound::Open,
            },
        }
    }

    /// Whether Claude Code lets messages from other processes into the session: a session that bypasses permission
    /// prompts holds them for the user unless its settings accept them, and `crossSessionInbound: "refuse"` refuses
    /// them. Read from the session's launch flags and the user's settings.
    fn permissions(&self) -> Inbound {
        let settings = self
            .home
            .as_ref()
            .map(|home| home.join("settings.json"))
            .unwrap_or_default();
        let args = launch_args(self.record().and_then(|record| record.pid));
        let user = read_json(&settings).unwrap_or_else(|| json!({}));
        let flagged = settings_from_flags(&args).unwrap_or_else(|| json!({}));
        let mode = flag(&args, "--permission-mode")
            .or_else(|| {
                user.pointer("/permissions/defaultMode")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "default".into());
        let inbound = flagged
            .get("crossSessionInbound")
            .or_else(|| user.get("crossSessionInbound"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let bypassing = mode == "bypassPermissions";
        if !bypassing || inbound.as_deref() == Some("accept") {
            return Inbound::Open;
        }
        let refuses = inbound.as_deref() == Some("refuse");
        let reason = if refuses {
            "This session refuses messages from other local processes (crossSessionInbound is \"refuse\")."
        } else {
            "This session bypasses permission prompts, so Claude Code holds messages from other local processes for the user to approve instead of delivering them, and it sends no receipt to say so. Voice will appear to be sent and nothing will arrive."
        };
        let remedy = if refuses {
            "Change crossSessionInbound from \"refuse\" to \"accept\", or start the session in a permission mode that prompts.".to_owned()
        } else {
            format!("Two ways out. Per session: start it with --settings '{{\"crossSessionInbound\":\"accept\"}}'. For every session on this machine: add \"crossSessionInbound\": \"accept\" to {} — that takes effect immediately, releases any messages already held, and also lets any other local process post into all your sessions, which is the safeguard it removes. Or run the conversation in a prompting mode such as --permission-mode auto.", settings.display())
        };
        Inbound::Blocked(Limit::InboundBlockedByPermissions {
            reason: reason.into(),
            remedy,
        })
    }
}

/// Whether `pid` runs now.
fn alive(pid: u32) -> bool {
    Processes::identity(pid).is_some()
}

/// The token for the inbox, from the key file's `peerToken`. Native Windows checks it: without one a message would be
/// dropped, so there is no way in.
#[cfg(windows)]
fn inbox_token(found: Option<String>) -> Result<String, Limit> {
    found.ok_or_else(|| {
        Limit::Unreachable(
            "Claude Code's key file for this session (sessions/<pid>.<hash>.key) has no token, and its inbox takes no message without one".into(),
        )
    })
}

/// The token for the inbox, from the key file's `peerToken`. A Unix socket does not check it: none is sent then.
#[cfg(unix)]
fn inbox_token(found: Option<String>) -> Result<String, Limit> {
    Ok(found.unwrap_or_default())
}

pub fn assistant_model(entry: &Value) -> Option<&str> {
    (entry.get("type") == Some(&json!("assistant")))
        .then(|| entry.pointer("/message/model").and_then(Value::as_str))
        .flatten()
        .filter(|model| !model.is_empty() && !is_placeholder_model(model))
}

/// Claude Code records its own synthetic turns (a rate-limit error, for instance) under a bracketed
/// placeholder such as `<synthetic>`. That names no engine, so the last real model stays.
fn is_placeholder_model(model: &str) -> bool {
    model.starts_with('<') && model.ends_with('>')
}

/// The engine an assistant transcript entry records: its model and the effort of that turn,
/// falling back to the launch settings for an effort the entry lacks and for thinking.
pub fn assistant_engine(entry: &Value, launch: &Value) -> Option<Value> {
    let model = assistant_model(entry)?;
    let effort = entry
        .get("effort")
        .filter(|effort| effort.as_str().is_some_and(|effort| !effort.is_empty()))
        .or_else(|| launch.get("effort"))
        .cloned()
        .unwrap_or(Value::Null);
    let thinking = launch.get("thinking").cloned().unwrap_or(Value::Null);
    Some(json!({"model":model,"effort":effort,"thinking":thinking}))
}

fn launch_args(pid: Option<u32>) -> String {
    let Some(pid) = pid else { return String::new() };
    Processes::identity(pid)
        .map(|found| found.command)
        .unwrap_or_default()
}

fn flag(args: &str, name: &str) -> Option<String> {
    for (index, _) in args.match_indices(name) {
        let end = index + name.len();
        if end != args.len()
            && !args.as_bytes()[end].is_ascii_whitespace()
            && args.as_bytes()[end] != b'='
        {
            continue;
        }
        let mut value = end;
        if args.as_bytes().get(value) == Some(&b'=') {
            value += 1;
        }
        while args
            .as_bytes()
            .get(value)
            .is_some_and(u8::is_ascii_whitespace)
        {
            value += 1;
        }
        if args.as_bytes().get(value) == Some(&b'\'') || args.as_bytes().get(value) == Some(&b'"') {
            let quote = args.as_bytes()[value] as char;
            value += 1;
            let finish = args[value..].find(quote)? + value;
            return Some(args[value..finish].to_owned());
        }
        let finish = args[value..]
            .find(char::is_whitespace)
            .unwrap_or(args.len() - value)
            + value;
        return Some(args[value..finish].to_owned());
    }
    None
}

fn settings_from_flags(args: &str) -> Option<Value> {
    let value = flag(args, "--settings")?;
    if value.trim_start().starts_with('{') {
        serde_json::from_str(&value).ok()
    } else {
        read_json(Path::new(&value))
    }
}

/// Writes `event` into the session's inbox: an auth line, then the message as a user turn. The inbox answers nothing
/// when it takes a message, and closes the connection when it does not.
pub async fn send(session: &Session, event: &Value) -> Sent {
    let (socket, token) = match session.inbox() {
        Ok(inbox) => inbox,
        Err(limit) => return Sent::Refused(limit),
    };
    match write(&socket, &token, event).await {
        Ok(sent) => sent,
        Err(error) => Sent::Refused(Limit::Failed(error.to_string())),
    }
}

async fn write(socket: &str, token: &str, event: &Value) -> Result<Sent> {
    let content = envelope(event)?;
    let mut socket = Foreigners::connect(socket)
        .await
        .map_err(|error| anyhow::anyhow!("Claude inbox refused the connection: {error}"))?;
    socket
        .write_all(json!({"type":"auth","token":token}).to_string().as_bytes())
        .await?;
    socket.write_all(b"\n").await?;
    socket
        .write_all(
            json!({"type":"user","message":{"role":"user","content":content}})
                .to_string()
                .as_bytes(),
        )
        .await?;
    socket.write_all(b"\n").await?;
    let mut peer = Vec::new();
    match tokio::time::timeout(Duration::from_millis(1500), socket.read_to_end(&mut peer)).await {
        Err(_) => Ok(Sent::Unconfirmed),
        Ok(Ok(_)) => Ok(Sent::Refused(Limit::Failed(
            "Claude Code inbox closed before confirming delivery".into(),
        ))),
        Ok(Err(error)) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude Code keeps its user configuration in `~/.claude.json` unless `CLAUDE_CONFIG_DIR` is set (in
    /// `$CLAUDE_CONFIG_DIR/.claude.json` then): its home reaches a child only when the person's environment sets it.
    #[test]
    fn claude_config_dir_is_passed_on_only_when_the_environment_sets_it() {
        use crate::profile::Profile;
        let scratch = crate::test_support::temp_dir("claude-home");
        let home = scratch.path().to_string_lossy().into_owned();
        let homes = crate::adapters::Registry::homes();
        let data = scratch.path().join("data").into_os_string();
        let unset = Profile::from_vars(
            |key| match key {
                key if key == Environment::HOME => Some(home.clone().into()),
                "SIDEVOICE_DATA_DIR" => Some(data.clone()),
                _ => None,
            },
            &homes,
        )
        .unwrap();
        assert_eq!(
            unset.agent_home(HOME.var),
            scratch.path().join(".claude"),
            "its data stays in ~/.claude"
        );
        assert!(!unset
            .variables()
            .iter()
            .any(|(key, _)| *key == "CLAUDE_CONFIG_DIR"));

        let named = scratch.path().join("elsewhere");
        let set = Profile::from_vars(
            |key| match key {
                key if key == Environment::HOME => Some(home.clone().into()),
                "SIDEVOICE_DATA_DIR" => Some(data.clone()),
                "CLAUDE_CONFIG_DIR" => Some(named.clone().into_os_string()),
                _ => None,
            },
            &homes,
        )
        .unwrap();
        assert!(set
            .variables()
            .iter()
            .any(|(key, path)| *key == "CLAUDE_CONFIG_DIR" && *path == named));
    }

    fn write_at(path: &Path, value: Value, age_secs: u64) {
        fs::write(path, value.to_string()).unwrap();
        let at = std::time::SystemTime::now() - Duration::from_secs(age_secs);
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(at)
            .unwrap();
    }

    /// The daemon finds a session through the agent's own home and process: the process's record whatever session it
    /// runs now (after `/clear`), else the records naming the joined session, a live process's before a newer stale
    /// one; the newest key file of that process gives the token; nothing found is a limit that says why.
    #[test]
    fn a_session_is_found_by_its_process_then_by_its_id_and_its_newest_key() {
        let scratch = crate::test_support::temp_dir("claude-sessions");
        let home = scratch.path().join("claude-home");
        let sessions = home.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let me = std::process::id();
        let parent = Processes::parent(me).unwrap();
        let dead = u32::MAX - 1;
        write_at(
            &sessions.join(format!("{me}.json")),
            json!({"pid":me,"sessionId":"after-clear","messagingSocketPath":"/run/me.sock"}),
            60,
        );
        write_at(
            &sessions.join(format!("{parent}.json")),
            json!({"pid":parent,"sessionId":"joined","messagingSocketPath":"/run/parent.sock"}),
            60,
        );
        write_at(
            &sessions.join(format!("{dead}.json")),
            json!({"pid":dead,"sessionId":"joined","messagingSocketPath":"/run/dead.sock"}),
            0,
        );
        write_at(
            &sessions.join(format!("{me}.old.key")),
            json!({"peerToken":"old"}),
            60,
        );
        write_at(
            &sessions.join(format!("{me}.new.key")),
            json!({"peerToken":"new"}),
            0,
        );
        // The live parent's key: native Windows takes no message without one.
        write_at(
            &sessions.join(format!("{parent}.p.key")),
            json!({"peerToken":"parent"}),
            0,
        );
        let session = |pid: Option<u32>, home: &Path| {
            Session::of(&Conversation {
                harness: "claude".into(),
                id: "joined".into(),
                agent: crate::adapters::Agent {
                    pid,
                    home: Some(home.to_owned()),
                },
            })
        };

        let by_process = session(Some(me), &home);
        assert_eq!(
            by_process.record().unwrap().session_id(),
            Some("after-clear")
        );
        let (socket, token) = by_process.inbox().unwrap();
        assert_eq!(socket, "/run/me.sock");
        assert_eq!(token, "new", "the newest key file of the process");

        let by_id = session(None, &home);
        assert_eq!(
            by_id.inbox().unwrap().0,
            "/run/parent.sock",
            "a live process before a newer stale record"
        );
        let gone = session(Some(dead), &home);
        assert_eq!(
            gone.inbox().unwrap().0,
            "/run/parent.sock",
            "a dead process falls back to the id"
        );

        let nowhere = session(Some(me), &scratch.path().join("elsewhere"));
        assert!(matches!(nowhere.inbox(), Err(Limit::Unreachable(_))));
        assert!(matches!(
            nowhere.inbound(),
            Inbound::Blocked(Limit::Unreachable(_))
        ));
    }

    /// A Unix socket takes no token; native Windows takes none without one, so a missing key file is no way in.
    #[test]
    fn the_inbox_token_is_needed_only_where_it_is_checked() {
        assert_eq!(inbox_token(Some("t".into())), Ok("t".into()));
        #[cfg(unix)]
        assert_eq!(inbox_token(None), Ok(String::new()));
        #[cfg(windows)]
        assert!(matches!(inbox_token(None), Err(Limit::Unreachable(_))));
    }

    #[test]
    fn assistant_engine_takes_the_turn_effort() {
        let entry =
            json!({"type":"assistant","effort":"high","message":{"model":"claude-opus-5-5"}});
        let launch = json!({"effort":"low","thinking":"enabled"});
        assert_eq!(
            assistant_engine(&entry, &launch),
            Some(json!({"model":"claude-opus-5-5","effort":"high","thinking":"enabled"}))
        );
    }

    #[test]
    fn assistant_engine_falls_back_to_the_launch_effort() {
        let entry = json!({"type":"assistant","message":{"model":"claude-opus-5-5"}});
        assert_eq!(
            assistant_engine(&entry, &json!({"effort":"low"})),
            Some(json!({"model":"claude-opus-5-5","effort":"low","thinking":null}))
        );
        assert_eq!(
            assistant_engine(&entry, &json!({})),
            Some(json!({"model":"claude-opus-5-5","effort":null,"thinking":null}))
        );
    }

    #[test]
    fn a_synthetic_placeholder_model_is_not_an_engine() {
        let synthetic = json!({"type":"assistant","message":{"model":"<synthetic>"}});
        assert_eq!(assistant_model(&synthetic), None);
        assert_eq!(
            assistant_engine(&synthetic, &json!({"effort":"medium"})),
            None
        );
        let real = json!({"type":"assistant","message":{"model":"claude-opus-5-5"}});
        assert_eq!(assistant_model(&real), Some("claude-opus-5-5"));
    }

    #[test]
    fn assistant_engine_ignores_other_entries() {
        let entry = json!({"type":"user","effort":"high","message":{"model":"claude-opus-5-5"}});
        assert_eq!(assistant_engine(&entry, &json!({})), None);
    }
}
