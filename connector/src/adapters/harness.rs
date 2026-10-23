//! The harness adapter: everything the connector knows about one agent, behind one trait, found by its id.

use super::conversation::{Conversation, Reporter};
use super::registration::Registration;
use super::Identity;
use crate::profile::{AgentHome, Profile};
use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::{json, Map, Value};
use std::any::Any;
use std::sync::{Arc, Weak};

/// What a harness makes of the delivery route a façade names for a conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// A route this harness knows, and it reaches that conversation.
    Accepted,
    /// A route this harness knows, naming somewhere that is not that conversation: the registration is refused.
    Refused,
    /// A kind this harness does not know (a newer façade's): the conversation joins with no route in.
    Unknown,
}

impl Route {
    /// `Accepted` when `matches`, else `Refused`: for a kind the harness knows.
    pub fn known(matches: bool) -> Self {
        if matches {
            Self::Accepted
        } else {
            Self::Refused
        }
    }
}

/// One harness: how its conversations are reached and followed, and how Sidevoice is registered with it.
pub trait Harness: Send + Sync {
    /// The id conversations and the agent list name it by (`claude`, `codex`, …).
    fn id(&self) -> &'static str;

    /// What its conversations can do when the client declares nothing. Harnesses with an
    /// injection route declare `deliver: supported`; the default uses get_messages.
    fn capabilities(&self) -> Value {
        json!({"deliver":"unsupported"})
    }

    /// The directories the agent keeps, named by environment variables: what the profile resolves for it.
    fn homes(&self) -> &'static [AgentHome] {
        &[]
    }

    /// Registering Sidevoice with the agent itself; None when there is nothing to register with.
    fn registration(&self) -> Option<&dyn Registration> {
        None
    }

    /// Whether voice reaches its conversation `thread` by `delivery`, for a route kind this harness knows; a kind it
    /// does not know is `Route::Unknown`, never refused.
    fn accepts(&self, delivery: &Value, thread: &str) -> Route;

    /// The harness's own state for one conversation, kept in `Conversation::state`.
    fn conversation_state(&self) -> Box<dyn Any + Send + Sync> {
        Box::new(())
    }

    /// Starts what the harness keeps for all its conversations on this machine, once the core link exists.
    fn attach(&self, _profile: &Profile, _reporter: Weak<dyn Reporter>) -> Result<()> {
        Ok(())
    }

    /// Readies a conversation's route as it joins; what it returns goes back to the façade as `prepared`.
    fn prepare<'a>(
        &'a self,
        _conversation: &'a Conversation,
    ) -> BoxFuture<'a, Result<Option<Value>>> {
        Box::pin(async { Ok(None) })
    }

    /// Attempts to put a message into the conversation. The default leaves it in the journal
    /// for get_messages.
    fn deliver<'a>(
        &'a self,
        _conversation: &'a Conversation,
        _frame: &'a Value,
        _profile: &'a Profile,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async { Ok(json!({"status":"unsupported"})) })
    }

    /// Releases what `prepare` held, when the conversation leaves.
    fn close<'a>(&'a self, _conversation: &'a Conversation) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    /// Fields about its route that `status` lists beside the conversation.
    fn status<'a>(
        &'a self,
        _conversation: &'a Conversation,
    ) -> BoxFuture<'a, Option<Map<String, Value>>> {
        Box::pin(async { None })
    }

    /// Whether a detached conversation may be taken by another façade: any of this harness (`None`), or the one
    /// named `client_ref`.
    fn adopts(&self, _client_ref: Option<&str>) -> bool {
        false
    }

    /// Prefixes of this harness's conversation ids that let a chat speak as it: the log names those ids by a hash.
    fn private_id_prefixes(&self) -> &'static [&'static str] {
        &[]
    }

    /// Whether `client` (an MCP client's `clientInfo`) is this harness: the one whose hooks below serve it.
    fn recognises_client(&self, _client: &Value) -> bool {
        false
    }

    /// Whether this harness's client (with capabilities `client_caps`) serves several of its chats at once, so a call
    /// that names no conversation may be any of them.
    fn hosts_several_chats(&self, _client_caps: &Value) -> bool {
        false
    }

    /// Refines a conversation's identity as it joins, with the title the agent gave.
    fn refine_identity<'a>(
        &'a self,
        _identity: &'a mut Identity,
        _title: &'a str,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    /// What the agent is told once about an experimental `capability` of this conversation.
    fn experimental_note(&self, _capability: &str, _identity: &Identity) -> Option<&'static str> {
        None
    }

    /// Fields a join answer to this harness's client carries, from the conversation's identity and the connector's
    /// `registered` answer.
    fn joined_fields(&self, _identity: &Identity, _registered: &Value) -> Map<String, Value> {
        Map::new()
    }

    /// Fields a status answer to this harness's client carries for a joined conversation: from its identity and the
    /// connector's `listed` binding.
    fn status_fields(&self, _identity: &Identity, _listed: Option<&Value>) -> Map<String, Value> {
        Map::new()
    }

    /// The identity and title of conversation `name`, which another façade of this harness's client left and this
    /// one adopted (`adopted`: the connector's answer).
    fn adopted_identity(&self, _name: &str, _adopted: &Value) -> Option<(Identity, String)> {
        None
    }

    /// Follows what the harness records about the conversation (transcript, rollout, session state), reporting it
    /// to the core until the conversation stops.
    fn watch(&self, _conversation: Arc<Conversation>, _reporter: Arc<dyn Reporter>) {}
}

/// Unsupported delivery, as a delivery result.
pub fn no_route(thread: &str) -> Value {
    json!({"status":"unsupported","detail":format!("{} has no delivery route", thread)})
}

/// The harnesses, by id. A conversation of an id none claims is served by the fallback (the generic HTTP route).
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

    /// The harnesses Sidevoice ships.
    pub fn standard() -> Self {
        Self::new(vec![
            Arc::new(super::claude::Claude),
            Arc::new(super::codex::Codex),
            Arc::new(super::cursor::Cursor),
        ])
    }

    /// The harness whose MCP client `client` is, if one recognises it.
    pub fn client(&self, client: &Value) -> Option<&Arc<dyn Harness>> {
        self.harnesses
            .iter()
            .find(|harness| harness.recognises_client(client))
    }

    /// The harness of `id`, or the fallback.
    pub fn get(&self, id: &str) -> &Arc<dyn Harness> {
        self.find(id).unwrap_or(&self.fallback)
    }

    /// The harness of `id`, if one claims it.
    pub fn find(&self, id: &str) -> Option<&Arc<dyn Harness>> {
        self.harnesses.iter().find(|harness| harness.id() == id)
    }

    /// Every harness, in the order the agent list shows them.
    pub fn all(&self) -> impl Iterator<Item = &Arc<dyn Harness>> {
        self.harnesses.iter().chain(std::iter::once(&self.fallback))
    }

    /// Every harness's homes, in registry order.
    pub fn homes(&self) -> Vec<AgentHome> {
        self.all()
            .flat_map(|harness| harness.homes().iter().copied())
            .collect()
    }

    /// Every harness's private conversation-id prefixes.
    pub fn private_id_prefixes(&self) -> Vec<&'static str> {
        self.all()
            .flat_map(|harness| harness.private_id_prefixes().iter().copied())
            .collect()
    }

    /// The harnesses with an agent to register with, in the order the agent list shows them.
    pub fn agents(&self) -> impl Iterator<Item = (&'static str, &dyn Registration)> {
        self.harnesses.iter().filter_map(|harness| {
            harness
                .registration()
                .map(|registration| (harness.id(), registration))
        })
    }

    /// The registration of the agent `id`.
    pub fn agent(&self, id: &str) -> Option<(&'static str, &dyn Registration)> {
        self.agents().find(|(agent, _)| *agent == id)
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
        let names: Vec<String> = Registry::standard()
            .agents()
            .map(|(id, _)| id.to_ascii_lowercase())
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
