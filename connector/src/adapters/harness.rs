//! The harness interface: what Sidevoice does with an agent's conversation. One implementation per agent (Claude
//! Code, Codex, Cursor, …), found by its id; how each one does it (sockets, CLIs, transcripts, its own cache) stays
//! inside it.

use super::registration::Registration;
use crate::profile::{AgentHome, Profile};
use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

/// One conversation of an agent: the harness that serves it, the conversation's id there, and where the agent runs.
/// Plain data, made by [`Harness::identify`] in the agent's MCP server and handed back to every other call in the
/// connector.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Conversation {
    pub harness: String,
    pub id: String,
    pub agent: Agent,
}

/// Where the agent of a conversation runs, as its MCP server found it at join: what lets the harness find the agent's
/// own records from the daemon, which is shared by every agent of the machine and started by none of them. Empty for
/// a harness that needs neither.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Agent {
    /// The agent's own process.
    pub pid: Option<u32>,
    /// The directory the agent keeps its state in, as the agent's environment names it.
    pub home: Option<PathBuf>,
}

impl Agent {
    /// As it crosses from the MCP server to the daemon: `{pid, home}`, each optional.
    pub fn to_json(&self) -> Value {
        json!({"pid":self.pid,"home":self.home})
    }

    /// From [`Agent::to_json`]; what is missing, `null` or of another type is absent.
    pub fn from_json(value: Option<&Value>) -> Self {
        let field = |key: &str| value.and_then(|value| value.get(key));
        Self {
            pid: field("pid")
                .and_then(Value::as_u64)
                .and_then(|pid| u32::try_from(pid).ok()),
            home: field("home")
                .and_then(Value::as_str)
                .filter(|home| !home.is_empty())
                .map(PathBuf::from),
        }
    }
}

/// What an agent's tool call brings with it: everything [`Harness::identify`] reads.
pub struct CallContext<'a> {
    /// The environment the agent started its MCP server with.
    pub env: &'a (dyn Fn(&str) -> Option<String> + Sync),
    /// The tool call's `_meta`.
    pub meta: &'a Value,
    /// The MCP client's `clientInfo`, from `initialize`.
    pub client: &'a Value,
    /// The MCP client's capabilities, from `initialize`.
    pub client_capabilities: &'a Value,
}

impl CallContext<'_> {
    /// A variable of the agent's environment, when it is set and not empty.
    pub fn var(&self, name: &str) -> Option<String> {
        (self.env)(name).filter(|value| !value.is_empty())
    }
}

/// How handing a message to a conversation went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sent {
    /// The agent took it and said so.
    Accepted,
    /// It was written where the agent reads, which answers nothing: [`Harness::received`] tells when the agent has
    /// it.
    Unconfirmed,
    /// It was not taken, and why.
    Refused(Limit),
}

/// How a conversation is right now. Polled; whatever the harness can tell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub status: Status,
    pub model: Option<Model>,
    pub inbound: Inbound,
}

impl State {
    /// A conversation the harness can tell nothing about, with messages getting in or not.
    pub fn unknown(inbound: Inbound) -> Self {
        Self {
            status: Status::Unknown,
            model: None,
            inbound,
        }
    }
}

/// Whether the agent is busy with the conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Idle,
    Working,
    /// The harness cannot tell.
    Unknown,
}

/// The model a conversation runs on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    pub name: String,
    pub effort: Option<String>,
    pub thinking: Option<String>,
}

impl Model {
    /// From an engine record, `{model, effort, thinking}`; none without a model.
    pub fn from_engine(engine: &Value) -> Option<Self> {
        let text = |key: &str| {
            engine
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        Some(Self {
            name: text("model")?,
            effort: text("effort"),
            thinking: text("thinking"),
        })
    }

    /// As the core reads an engine: `{model, effort, thinking}`.
    pub fn engine(&self) -> Value {
        json!({"model":self.name,"effort":self.effort,"thinking":self.thinking})
    }
}

/// Whether messages can be put into the conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inbound {
    Open,
    Blocked(Limit),
}

impl Inbound {
    /// As the agent and diagnostics read it: `{ok}`, and when blocked the limit's key, why, and the remedy.
    pub fn view(&self) -> Value {
        match self {
            Self::Open => json!({"ok":true}),
            Self::Blocked(limit) => {
                let mut view = json!({"ok":false,"limit":limit.key(),"reason":limit.detail()});
                if let Limit::InboundBlockedByPermissions { reason, remedy } = limit {
                    view["reason"] = json!(reason);
                    view["remedy"] = json!(remedy);
                }
                view
            }
        }
    }
}

/// Why a message cannot get into a conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Limit {
    /// There is no way in from here: the agent reads what is said with get_messages. Says why, for the agent.
    NoRoute(String),
    /// The agent's own settings hold or refuse messages from other processes.
    InboundBlockedByPermissions { reason: String, remedy: String },
    /// The agent's bridge for messages is not running.
    BridgeNotConnected,
    /// The agent has not yet recorded which of its chats this conversation is.
    ChatNotFound,
    /// The way in cannot be found or opened now (its record is gone, its token is missing); it may come back.
    Unreachable(String),
    /// The way in failed this time; trying again may work.
    Failed(String),
}

impl Limit {
    /// A stable key for it.
    pub fn key(&self) -> &'static str {
        match self {
            Self::NoRoute(_) => "no_route",
            Self::InboundBlockedByPermissions { .. } => "inbound_blocked_by_permissions",
            Self::BridgeNotConnected => "bridge_not_connected",
            Self::ChatNotFound => "chat_not_found",
            Self::Unreachable(_) => "unreachable",
            Self::Failed(_) => "failed",
        }
    }

    /// Why, in English, for the agent and the log.
    pub fn detail(&self) -> String {
        match self {
            Self::NoRoute(why) | Self::Unreachable(why) | Self::Failed(why) => why.clone(),
            Self::InboundBlockedByPermissions { reason, remedy } => format!("{reason} {remedy}"),
            Self::BridgeNotConnected => "the agent's Desktop Bridge is not running".into(),
            Self::ChatNotFound => "waiting for the agent to record this chat's join call".into(),
        }
    }
}

/// One agent: how Sidevoice is registered with it, and how its conversations are found, reached and followed.
pub trait Harness: Send + Sync {
    /// The id conversations and the agent list name it by (`claude`, `codex`, …).
    fn id(&self) -> &'static str;

    /// Registering Sidevoice with the agent, and its state on this machine.
    fn registration(&self) -> &dyn Registration;

    /// The conversation a tool call comes from, at join: `None` when the call is not this agent's; an error when it
    /// is, and it cannot tell which conversation.
    fn identify(&self, call: &CallContext) -> Result<Option<Conversation>>;

    /// Puts a message (the core's frame) into the conversation. Whether it steers the running turn or waits for the
    /// next is the harness's own choice.
    fn send<'a>(
        &'a self,
        conversation: &'a Conversation,
        message: &'a Value,
    ) -> BoxFuture<'a, Sent>;

    /// How the conversation is now.
    fn state<'a>(&'a self, conversation: &'a Conversation) -> BoxFuture<'a, State>;

    /// Which of `ids`, messages handed to the conversation, the agent now has.
    fn received<'a>(
        &'a self,
        conversation: &'a Conversation,
        ids: &'a [String],
    ) -> BoxFuture<'a, Vec<String>>;
}

/// The harnesses, by id. A conversation of an id none claims is served by the fallback, which has no way in of its
/// own (the generic HTTP endpoint, when the agent names one, is used whatever the harness).
pub struct Registry {
    harnesses: Vec<Arc<dyn Harness>>,
    fallback: Arc<dyn Harness>,
}

impl Registry {
    pub fn new(harnesses: Vec<Arc<dyn Harness>>) -> Self {
        Self {
            harnesses,
            fallback: Arc::new(super::http::Http),
        }
    }

    /// The harnesses Sidevoice ships, in the order a call is identified: Cursor first, since a Cursor started from
    /// another agent's terminal inherits that agent's environment.
    pub fn standard(profile: &Profile) -> Self {
        Self::new(vec![
            Arc::new(super::cursor::Chats::default()),
            Arc::new(super::claude::Sessions::default()),
            Arc::new(super::codex::Threads::new(profile.clone())),
        ])
    }

    /// The conversation `call` comes from, asking each harness in turn and the fallback last.
    pub fn identify(&self, call: &CallContext) -> Result<Option<Conversation>> {
        for harness in self.all() {
            if let Some(conversation) = harness.identify(call)? {
                return Ok(Some(conversation));
            }
        }
        Ok(None)
    }

    /// The harness of `id`, or the fallback.
    pub fn get(&self, id: &str) -> &Arc<dyn Harness> {
        self.find(id).unwrap_or(&self.fallback)
    }

    fn all(&self) -> impl Iterator<Item = &Arc<dyn Harness>> {
        self.harnesses.iter().chain(std::iter::once(&self.fallback))
    }

    /// The harnesses Sidevoice ships, in the order the agent list shows them (not the fallback, which has no agent).
    pub fn harnesses(&self) -> impl Iterator<Item = &Arc<dyn Harness>> {
        self.harnesses.iter()
    }

    /// The shipped harness of `id`, if there is one.
    pub fn find(&self, id: &str) -> Option<&Arc<dyn Harness>> {
        self.harnesses.iter().find(|harness| harness.id() == id)
    }

    /// The directories the agents keep, named by environment variables: what the profile resolves for them.
    pub fn homes() -> Vec<AgentHome> {
        [super::claude::HOME, super::codex::HOME]
            .into_iter()
            .chain(super::cursor::HOMES)
            .collect()
    }

    /// A harness whose registration edits the agent's file, so it connects without its CLI: for tests.
    #[cfg(test)]
    pub fn without_cli() -> &'static str {
        super::cursor::Chats::default().id()
    }

    /// Prefixes of conversation ids that let a chat speak as that conversation: the log names those ids by a hash.
    pub fn private_id_prefixes() -> Vec<&'static str> {
        vec![super::cursor::EDITOR_PREFIX]
    }

    /// Whether the MCP client (its `clientInfo` and capabilities) serves several of its chats through one server,
    /// so a call that names no conversation may be any of them, and its conversations outlive the server.
    pub fn hosts_several_chats(client: &Value, client_capabilities: &Value) -> bool {
        super::cursor::is_cursor_client(client) && super::cursor::is_editor(client_capabilities)
    }
}

#[cfg(test)]
mod tests {
    use super::Registry;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Every Rust source of the connector, below `dir`.
    fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                sources(&path, found);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                found.push(path);
            }
        }
    }

    /// Only the adapters name a harness: every other source reaches one through the registry by id. Test files
    /// (`tests.rs`, and the files of a `tests` module's directory) may name them, as fixtures.
    #[test]
    fn no_harness_is_named_outside_the_adapters() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let adapters = src.join("adapters");
        let scratch = crate::test_support::temp_dir("harness-names");
        let profile = crate::profile::Profile::for_test(scratch.path(), &Registry::homes());
        let names: Vec<String> = Registry::standard(&profile)
            .harnesses()
            .map(|harness| harness.id().to_ascii_lowercase())
            .collect();
        assert!(!names.is_empty());
        let mut files = Vec::new();
        sources(&src, &mut files);
        let mut leaks = Vec::new();
        for file in files {
            if file.starts_with(&adapters)
                || file == src.join("adapters.rs")
                || file.file_name().is_some_and(|name| name == "tests.rs")
                || file
                    .strip_prefix(&src)
                    .unwrap()
                    .components()
                    .any(|part| part.as_os_str() == "tests")
            {
                continue;
            }
            let text = fs::read_to_string(&file).unwrap();
            for (number, line) in text.lines().enumerate() {
                let lower = line.to_ascii_lowercase();
                if let Some(name) = names.iter().find(|name| lower.contains(name.as_str())) {
                    leaks.push(format!(
                        "{}:{}: names {name}",
                        file.strip_prefix(&src).unwrap().display(),
                        number + 1
                    ));
                }
            }
        }
        assert!(
            leaks.is_empty(),
            "harnesses named outside adapters/:\n{}",
            leaks.join("\n")
        );
    }
}
