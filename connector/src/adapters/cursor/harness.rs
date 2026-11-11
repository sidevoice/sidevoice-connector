//! Cursor as a harness: the CLI's chats (found by the open chat database, typed into through tmux, followed through
//! their transcript) and the editor's chats, reached through Cursor's Desktop Bridge once it knows which chat a
//! conversation is, their transcript found the same way.

use super::engine::editor_engine_in;
use super::{
    bridge_instances, bridge_running, composer_holding, composer_holding_for_delivery,
    deliver_editor, editor_transcript_chat, engine, find_chat, is_cursor_client, is_editor,
    log_bridge, log_correlation, persist_session, send_cli, transcript_path, BridgeInstance,
    CLI_UNSUPPORTED, EDITOR_NOTE, EDITOR_PREFIX, NOT_PERSISTED,
};
use crate::adapters::harness::{
    Agent, CallContext, Conversation, Harness, Inbound, Limit, Sent, State,
};
use crate::adapters::http::ENDPOINT_VAR;
use crate::adapters::transcript::{self, Observed, TranscriptReader};
use crate::adapters::watch::{Watch, Watches};
use anyhow::{bail, Result};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio::time::Duration;

/// How many messages handed to an editor chat are remembered to find its transcript by.
const SENT_KEPT: usize = 64;

/// Which Cursor chat an editor conversation is: the one the Desktop Bridge holds (`bridge`, which authenticates it),
/// the one whose transcript is followed, and the messages handed to it, which its transcript shows once it has them.
#[derive(Default)]
pub struct Editor {
    bridge: Mutex<Option<String>>,
    transcript: Mutex<Option<String>>,
    bridge_changed: Notify,
    sent: Mutex<VecDeque<String>>,
}

type EditorWatch = Watch<Editor>;

fn bridge(watch: &EditorWatch) -> Option<String> {
    watch.extra.bridge.lock().unwrap().clone()
}

fn set_bridge(watch: &EditorWatch, chat: String) {
    *watch.extra.bridge.lock().unwrap() = Some(chat);
    watch.extra.bridge_changed.notify_one();
}

fn followed(watch: &EditorWatch) -> Option<String> {
    watch.extra.transcript.lock().unwrap().clone()
}

fn set_followed(watch: &EditorWatch, chat: String) {
    *watch.extra.transcript.lock().unwrap() = Some(chat);
}

/// Wait for the next discovery scan, unless delivery already identified this chat.
async fn wait_for_bridge_or_scan(watch: &EditorWatch, scan_ms: u64) {
    // notify_one retains a permit if delivery identifies the chat between this check and wait.
    let changed = watch.extra.bridge_changed.notified();
    if bridge(watch).is_some() {
        return;
    }
    tokio::select! {
        _ = changed => {},
        _ = tokio::time::sleep(Duration::from_millis(scan_ms)) => {},
    }
}

/// Cursor's chats, as the connector follows them.
#[derive(Default)]
pub struct Chats {
    watches: Watches<Editor>,
}

impl Chats {
    fn watch(&self, conversation: &Conversation) -> Arc<EditorWatch> {
        self.watches.get(&conversation.id, |watch| {
            match conversation.id.strip_prefix(EDITOR_PREFIX) {
                Some(marker) => {
                    watch_engine(watch.clone(), None);
                    watch_editor(watch.clone(), marker.to_owned());
                }
                None => {
                    let chat = conversation.id.clone();
                    if persist_session(&chat).is_none() {
                        watch.set_inbound(Inbound::Blocked(Limit::NoRoute(NOT_PERSISTED.into())));
                    }
                    watch_engine(watch.clone(), Some(chat.clone()));
                    transcript::follow(watch.clone(), Reader { chat, editor: None });
                }
            }
        })
    }
}

impl Harness for Chats {
    fn id(&self) -> &'static str {
        "cursor"
    }

    /// Cursor names itself in its MCP client info. Its editor declares MCP Apps support, and each of its chats that
    /// joins gets a conversation id of Sidevoice's own; a CLI chat is the chat database `cursor-agent` has open. A
    /// chat that names its own conversation and HTTP endpoint is the generic one's.
    fn identify(&self, call: &CallContext) -> Result<Option<Conversation>> {
        if !is_cursor_client(call.client)
            || (call.var("SIDEVOICE_THREAD").is_some() && call.var(ENDPOINT_VAR).is_some())
        {
            return Ok(None);
        }
        let id = if is_editor(call.client_capabilities) {
            format!("{EDITOR_PREFIX}{}", uuid::Uuid::new_v4())
        } else {
            match find_chat() {
                Ok(Some(found)) => found.chat,
                Ok(None) => bail!("{EDITOR_NOTE}"),
                Err(error) => bail!("{CLI_UNSUPPORTED} ({error})."),
            }
        };
        Ok(Some(Conversation {
            harness: self.id().into(),
            id,
            agent: Agent::default(),
        }))
    }

    fn send<'a>(
        &'a self,
        conversation: &'a Conversation,
        message: &'a Value,
    ) -> BoxFuture<'a, Sent> {
        Box::pin(async move {
            match conversation.id.strip_prefix(EDITOR_PREFIX) {
                Some(marker) => send_to_editor(&self.watch(conversation), marker, message).await,
                None => send_cli(&conversation.id, message).await,
            }
        })
    }

    fn state<'a>(&'a self, conversation: &'a Conversation) -> BoxFuture<'a, State> {
        Box::pin(async move {
            let watch = self.watch(conversation);
            let mut state = watch.state();
            if conversation.id.starts_with(EDITOR_PREFIX) {
                state.inbound = editor_inbound(bridge(&watch).is_some(), bridge_running);
            }
            state
        })
    }

    fn received<'a>(
        &'a self,
        conversation: &'a Conversation,
        ids: &'a [String],
    ) -> BoxFuture<'a, Vec<String>> {
        Box::pin(async move { self.watch(conversation).received(ids) })
    }
}

/// Whether messages get into an editor chat: once the Desktop Bridge knows which chat it is; else why not yet.
fn editor_inbound(chat_known: bool, bridge_running: impl FnOnce() -> bool) -> Inbound {
    if chat_known {
        Inbound::Open
    } else if !bridge_running() {
        Inbound::Blocked(Limit::BridgeNotConnected)
    } else {
        Inbound::Blocked(Limit::ChatNotFound)
    }
}

/// The engine of the chat: a CLI chat's own, an editor chat's once the Desktop Bridge and its transcript name the same
/// chat.
fn watch_engine(watch: Arc<EditorWatch>, cli_chat: Option<String>) {
    tokio::spawn(async move {
        while !watch.stopped() {
            let chat = cli_chat.clone().or_else(|| {
                bridge(&watch).filter(|chat| followed(&watch).as_deref() == Some(chat))
            });
            if let Some(chat) = chat {
                if let Some(found) = engine(&chat).await {
                    let still_current = cli_chat.is_some()
                        || (bridge(&watch).as_deref() == Some(chat.as_str())
                            && followed(&watch).as_deref() == Some(chat.as_str()));
                    if still_current {
                        watch.set_engine(found);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
}

/// An editor chat, only by its own id through the Desktop Bridge. Until the bridge knows which chat it is, the
/// message is not sent and the answer says why.
async fn send_to_editor(watch: &EditorWatch, marker: &str, message: &Value) -> Sent {
    log_bridge("delivery", "attempt", Some(marker), None, None);
    if let Some(id) = message.get("message_id").and_then(Value::as_str) {
        let mut sent = watch.extra.sent.lock().unwrap();
        sent.push_back(id.to_owned());
        while sent.len() > SENT_KEPT {
            sent.pop_front();
        }
    }
    let composer = match bridge(watch) {
        Some(composer) => {
            log_bridge("lookup", "cached_chat", Some(&composer), None, None);
            Some(composer)
        }
        None => composer_holding_for_delivery(marker).await,
    };
    let Some(composer) = composer else {
        log_bridge("delivery", "chat_unresolved", Some(marker), None, None);
        return match editor_inbound(false, bridge_running) {
            Inbound::Blocked(limit) => Sent::Refused(limit),
            Inbound::Open => unreachable!("an unknown chat is never open"),
        };
    };
    set_bridge(watch, composer.clone());
    let result = deliver_editor(&json!({"marker":marker,"composer":composer}), message).await;
    let (sent, outcome) = match result.as_ref().and_then(|result| result.get("status")) {
        Some(status) if status == "accepted" => (Sent::Accepted, "accepted"),
        // The bridge may or may not have sent it: handed over, so it is never typed in twice.
        Some(status) if status == "unknown" => (Sent::Accepted, "unconfirmed"),
        _ => (
            Sent::Refused(Limit::Failed(
                result
                    .as_ref()
                    .and_then(|result| result.get("detail"))
                    .and_then(Value::as_str)
                    .unwrap_or("Not sent: Cursor's Desktop Bridge did not take the message (it may not be running).")
                    .to_owned(),
            )),
            "bridge_failed",
        ),
    };
    log_bridge("delivery", outcome, Some(marker), None, None);
    sent
}

/// Finds which Cursor chat an editor conversation is (by the Desktop Bridge, or by a delivered message showing up
/// in a transcript), then follows that chat's transcript.
fn watch_editor(watch: Arc<EditorWatch>, marker: String) {
    tokio::spawn(async move {
        let lookup_at = std::env::var("SIDEVOICE_CURSOR_LOOKUP_AT")
            .ok()
            .map(|value| {
                value
                    .split(',')
                    .filter_map(|part| part.trim().parse::<u64>().ok())
                    .map(Duration::from_millis)
                    .collect::<Vec<_>>()
            })
            .filter(|times| !times.is_empty())
            .unwrap_or_else(|| {
                [3_000, 15_000, 35_000, 70_000]
                    .into_iter()
                    .map(Duration::from_millis)
                    .collect()
            });
        let started = tokio::time::Instant::now();
        let mut lookup_index = 0;
        let mut last_bridge_lookup: Option<tokio::time::Instant> = None;
        loop {
            if watch.stopped() {
                return;
            }
            if let Some(chat) = bridge(&watch) {
                if followed(&watch).as_deref() != Some(chat.as_str()) {
                    follow_transcript(&watch, chat.clone());
                }
                follow_editor_engine(watch.clone(), chat, bridge_instances());
                return;
            }

            let expected = watch
                .extra
                .sent
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .collect::<Vec<_>>();
            if let Some(chat) = editor_transcript_chat(&expected) {
                if followed(&watch).as_deref() != Some(chat.as_str()) {
                    follow_transcript(&watch, chat.clone());
                }
                log_correlation("mapping", "transcript_message", Some(&marker), &chat, None);
                set_bridge(&watch, chat);
                continue;
            }

            let lookup_due = if lookup_index < lookup_at.len() {
                started.elapsed() >= lookup_at[lookup_index]
            } else {
                last_bridge_lookup.is_some_and(|last| last.elapsed() >= Duration::from_secs(5))
            };
            if lookup_due {
                if lookup_index < lookup_at.len() {
                    lookup_index += 1;
                }
                last_bridge_lookup = Some(tokio::time::Instant::now());
                if let Some(chat) = composer_holding(&marker).await {
                    set_bridge(&watch, chat);
                    continue;
                }
            }
            let scan_ms = std::env::var("SIDEVOICE_CURSOR_SCAN_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(2_000)
                .max(10);
            wait_for_bridge_or_scan(&watch, scan_ms).await;
        }
    });
}

fn follow_transcript(watch: &Arc<EditorWatch>, chat: String) {
    set_followed(watch, chat.clone());
    transcript::follow(
        watch.clone(),
        Reader {
            chat,
            editor: Some(watch.clone()),
        },
    );
}

/// Observe authenticated editor metadata independently of transcript creation and replay.
fn follow_editor_engine(
    watch: Arc<EditorWatch>,
    chat: String,
    instances: Vec<BridgeInstance>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if watch.stopped()
                || bridge(&watch).as_deref() != Some(chat.as_str())
                || followed(&watch).as_deref() != Some(chat.as_str())
            {
                return;
            }
            if let Some(engine) = editor_engine_in(&chat, &instances).await {
                // The mapping may change while SQLite answers. Never attribute its model to a
                // chat that is no longer authenticated for this conversation.
                if bridge(&watch).as_deref() == Some(chat.as_str())
                    && followed(&watch).as_deref() == Some(chat.as_str())
                {
                    watch.set_engine(engine);
                }
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    })
}

/// A Cursor chat's transcript: working from its turns, and the messages read. An editor chat's (`editor`: its watch)
/// is only followed while it is the conversation's, and only authenticated once the Desktop Bridge names the same
/// chat.
struct Reader {
    chat: String,
    editor: Option<Arc<EditorWatch>>,
}

impl TranscriptReader for Reader {
    fn path(&self) -> Option<PathBuf> {
        transcript_path(&self.chat)
    }

    fn current(&self) -> bool {
        self.editor
            .as_ref()
            .is_none_or(|watch| followed(watch).as_deref() == Some(self.chat.as_str()))
    }

    fn authenticated(&self) -> bool {
        self.editor
            .as_ref()
            .is_none_or(|watch| bridge(watch).as_deref() == Some(self.chat.as_str()))
    }

    fn rewritten_in_place(&self) -> bool {
        true
    }

    fn item(&mut self, item: &Value) -> Vec<Observed> {
        let mut observed = Vec::new();
        let text = transcript::user_text(item, false);
        if item.get("type") == Some(&json!("turn_ended")) {
            observed.push(Observed::Working(false));
        } else if item.get("role") == Some(&json!("user")) {
            observed.push(Observed::Working(true));
        }
        if let Some(text) = text.as_deref() {
            observed.extend(
                transcript::voice_headers(text)
                    .into_iter()
                    .map(Observed::Read),
            );
        }
        observed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::harness::Status;
    use crate::profile::Profile;
    use std::fs;

    fn editor_caps() -> Value {
        json!({"extensions":{"io.modelcontextprotocol/ui":{"mimeTypes":["text/html;profile=mcp-app"]}}})
    }

    fn editor_call(chats: &Chats) -> Conversation {
        let env = |_: &str| None;
        chats
            .identify(&CallContext {
                env: &env,
                meta: &json!({}),
                client: &json!({"name":"cursor-vscode"}),
                client_capabilities: &editor_caps(),
            })
            .unwrap()
            .unwrap()
    }

    /// A delivery can identify the editor chat while the periodic transcript scan sleeps.
    /// Its retained wakeup must let the watcher attach without waiting for that scan.
    #[tokio::test]
    async fn bridge_identification_wakes_working_monitor() {
        let watch = Arc::new(Watch::<Editor>::default_for_test());
        let waiter = tokio::spawn({
            let watch = watch.clone();
            async move { wait_for_bridge_or_scan(&watch, 2_000).await }
        });
        tokio::task::yield_now().await;
        set_bridge(&watch, "chat-1".into());
        tokio::time::timeout(Duration::from_millis(500), waiter)
            .await
            .expect("delivery must wake the monitor without a scan interval")
            .unwrap();
        assert_eq!(bridge(&watch).as_deref(), Some("chat-1"));

        // A bridge set just before the wait is also observed without sleeping.
        tokio::time::timeout(
            Duration::from_millis(500),
            wait_for_bridge_or_scan(&watch, 2_000),
        )
        .await
        .expect("a known bridge must not wait for another scan");
    }

    #[test]
    fn transcript_fixture_reports_busy_until_turn_ends() {
        let mut reader = Reader {
            chat: "chat-1".into(),
            editor: None,
        };
        let fixture = [
            r#"{"role":"user","message":{"content":"hello"}}"#,
            r#"{"role":"assistant","message":{"content":"working"}}"#,
            r#"{"type":"turn_ended"}"#,
        ];
        let events = fixture
            .iter()
            .map(|line| {
                reader
                    .item(&serde_json::from_str::<Value>(line).unwrap())
                    .into_iter()
                    .filter_map(|event| match event {
                        Observed::Working(value) => Some(value),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(events, vec![vec![true], vec![], vec![false]]);
    }

    /// Why an editor chat is not reached yet: the bridge, then Cursor recording the join.
    #[test]
    fn an_unregistered_chat_says_what_it_waits_for() {
        assert_eq!(editor_inbound(true, || false), Inbound::Open);
        assert_eq!(
            editor_inbound(false, || false),
            Inbound::Blocked(Limit::BridgeNotConnected)
        );
        assert_eq!(
            editor_inbound(false, || true),
            Inbound::Blocked(Limit::ChatNotFound)
        );
    }

    /// Each editor chat that joins gets a conversation of its own, whose id carries the marker the Desktop Bridge finds
    /// it by; a chat naming its own conversation and endpoint is not Cursor's to identify.
    #[test]
    fn an_editor_chat_gets_an_id_of_its_own_that_carries_its_marker() {
        let chats = Chats::default();
        let first = editor_call(&chats);
        let second = editor_call(&chats);
        assert_eq!(first.harness, "cursor");
        assert_ne!(first.id, second.id);
        let marker = first.id.strip_prefix(EDITOR_PREFIX).unwrap();
        assert!(super::super::valid_marker(marker), "{marker}");

        let env = |name: &str| match name {
            "SIDEVOICE_THREAD" => Some("t".to_owned()),
            ENDPOINT_VAR => Some("http://127.0.0.1:1/".to_owned()),
            _ => None,
        };
        let generic = chats
            .identify(&CallContext {
                env: &env,
                meta: &json!({}),
                client: &json!({"name":"cursor-vscode"}),
                client_capabilities: &editor_caps(),
            })
            .unwrap();
        assert_eq!(generic, None);
    }

    /// Until the Desktop Bridge knows which chat it is, nothing is sent, and both the delivery and the state say why.
    #[tokio::test]
    async fn an_unregistered_editor_chat_is_waiting_not_reached_another_way() {
        let chats = Chats::default();
        let conversation = editor_call(&chats);
        let frame = json!({"channel":"voice","session_id":"s","revision":1,"epoch":0,"message_id":"m","text":"hola"});
        let waiting = editor_inbound(false, bridge_running);
        let Inbound::Blocked(limit) = waiting.clone() else {
            panic!("an unknown chat is never open");
        };
        assert_eq!(
            chats.send(&conversation, &frame).await,
            Sent::Refused(limit)
        );
        let state = chats.state(&conversation).await;
        assert_eq!(state.inbound, waiting);
        assert_eq!(state.status, Status::Idle);

        set_bridge(&chats.watch(&conversation), "chat-1".into());
        assert_eq!(chats.state(&conversation).await.inbound, Inbound::Open);
    }

    #[tokio::test]
    async fn authenticated_editor_reports_engine_without_a_transcript_and_retries_missing_metadata()
    {
        let scratch = crate::test_support::temp_dir("cursor-engine-before-transcript");
        let user_data = scratch.path().join("Cursor");
        let storage = user_data.join("User/globalStorage");
        fs::create_dir_all(&storage).unwrap();
        let database = storage.join("state.vscdb");
        super::super::state_db::fixture(
            &database,
            "CREATE TABLE cursorDiskKV(key TEXT, value TEXT);",
        );
        let watch = Arc::new(Watch::<Editor>::default_for_test());
        let chat = "chat-model".to_owned();
        set_followed(&watch, chat.clone());
        let instance = BridgeInstance {
            socket: PathBuf::new(),
            token: String::new(),
            user_data,
            created_at: 0,
        };
        // A known transcript path is deliberately absent throughout this test.
        let unauthenticated =
            follow_editor_engine(watch.clone(), chat.clone(), vec![instance.clone()]);
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(watch.engine().is_none(), "unbound chat reported an engine");
        unauthenticated.await.unwrap();
        set_bridge(&watch, "chat-model".into());
        let watcher = follow_editor_engine(watch.clone(), chat, vec![instance]);
        tokio::time::sleep(Duration::from_millis(450)).await;
        assert!(watch.engine().is_none(), "missing metadata was guessed");
        super::super::state_db::fixture(&database, "INSERT INTO cursorDiskKV VALUES ('composerData:chat-model', '{\"modelConfig\":{\"modelName\":\"gpt-5\",\"effort\":\"high\"}}');");
        tokio::time::timeout(Duration::from_secs(2), async {
            while watch.engine().is_none() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("authenticated metadata was not reported without a transcript");
        assert_eq!(
            watch.engine(),
            Some(json!({"model":"gpt-5","effort":"high","thinking":null}))
        );
        watch.stop();
        tokio::time::timeout(Duration::from_secs(1), watcher)
            .await
            .unwrap()
            .unwrap();
    }

    /// Waits for the followed state to say working, `expected`.
    async fn wait_for_working<S>(watch: &Watch<S>, expected: Status, within: Duration) {
        tokio::time::timeout(within, async {
            while watch.state().status != expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("not {expected:?} within {within:?}"));
    }

    /// Working is only told once a populated history is caught up, by the real Cursor and Codex followers.
    #[tokio::test]
    async fn populated_cursor_and_codex_history_delays_initial_working() {
        use std::io::Write;
        let root = std::env::temp_dir().join(format!("sv-history-watch-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let previous_data = std::env::var_os("CURSOR_DATA_DIR");
        std::env::set_var("CURSOR_DATA_DIR", &root);
        for large in [false, true] {
            let chat = if large {
                "history-large-chat"
            } else {
                "history-small-chat"
            };
            let path = root.join(format!(
                "projects/project/agent-transcripts/{chat}/{chat}.jsonl"
            ));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let history = format!(
                "{}\n",
                json!({"role":"assistant","message":{"content":"x".repeat(1024)}})
            );
            let mut content = if large {
                history.repeat(500)
            } else {
                String::new()
            };
            content.push_str("{\"role\":\"user\",\"message\":{\"content\":\"hello\"}}\n");
            fs::write(&path, &content).unwrap();
            let watch = Arc::new(Watch::<Editor>::default_for_test());
            watch_editor(watch.clone(), uuid::Uuid::new_v4().to_string());
            tokio::time::sleep(Duration::from_millis(50)).await;
            let start = tokio::time::Instant::now();
            set_bridge(&watch, chat.to_owned());
            wait_for_working(&watch, Status::Working, Duration::from_secs(6)).await;
            let elapsed = start.elapsed();
            println!(
                "History Cursor large={large} bytes={} first_working_ms={}",
                content.len(),
                elapsed.as_millis()
            );
            assert!(elapsed < Duration::from_secs(2));
            for _ in 0..10 {
                set_bridge(&watch, chat.to_owned());
            }
            let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(file, "{{\"type\":\"turn_ended\"}}").unwrap();
            wait_for_working(&watch, Status::Idle, Duration::from_millis(850)).await;
            watch.stop();
            tokio::time::sleep(Duration::from_millis(450)).await;
        }
        let profile = Profile::for_test(&root.join("codex"), &[crate::adapters::codex::HOME]);
        let home = profile.agent_home("CODEX_HOME").to_owned();
        let path = home.join("sessions/2026/10/15/rollout-history-codex.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let history = format!(
            "{}\n",
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"x".repeat(1024)}]}})
        );
        let mut content = history.repeat(500);
        content.push_str(
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"t1\"}}\n",
        );
        fs::write(&path, &content).unwrap();
        let watch = Arc::new(Watch::<()>::default_for_test());
        let start = tokio::time::Instant::now();
        crate::adapters::codex::follow_rollout(watch.clone(), home, "history-codex".into());
        wait_for_working(&watch, Status::Working, Duration::from_secs(6)).await;
        println!(
            "History Codex bytes={} first_working_ms={}",
            content.len(),
            start.elapsed().as_millis()
        );
        assert!(start.elapsed() >= Duration::from_secs(2));
        watch.stop();
        tokio::time::sleep(Duration::from_millis(450)).await;
        if let Some(value) = previous_data {
            std::env::set_var("CURSOR_DATA_DIR", value);
        } else {
            std::env::remove_var("CURSOR_DATA_DIR");
        }
        fs::remove_dir_all(root).unwrap();
    }
}
