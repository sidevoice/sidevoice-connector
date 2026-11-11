//! Claude Code as a harness: a session is found by the environment Claude Code starts its MCP server with (the session,
//! where Claude Code keeps its state, its own process), reached through the inbox the process's session record names,
//! and followed through that record and its transcript.

use super::{agent_home, agent_process, assistant_engine, assistant_model, Session};
use crate::adapters::harness::{Agent, CallContext, Conversation, Harness, Sent, State};
use crate::adapters::registration::Registration;
use crate::adapters::transcript::{self, Observed, TranscriptReader};
use crate::adapters::watch::{Watch, Watches};
use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::time::Duration;

/// How long the record may say idle while the transcript holds a turn open before the turn is taken as dead (an
/// interrupted or crashed one leaves no closing entry).
const OPEN_TURN_MS: u64 = 30_000;
/// How long not working must hold before it is reported: the record flickers idle between tool calls.
const IDLE_DEBOUNCE: Duration = Duration::from_secs(2);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(1, |d| d.as_millis() as u64)
}

/// When the last entry of a session's open turn was written, in epoch milliseconds; 0 when none is open.
#[derive(Default)]
pub struct OpenTurn(AtomicU64);

/// Claude Code's sessions, as the connector follows them.
#[derive(Default)]
pub struct Sessions {
    watches: Watches<OpenTurn>,
}

impl Sessions {
    fn watch(&self, conversation: &Conversation) -> Arc<Watch<OpenTurn>> {
        let session = Session::of(conversation);
        self.watches.get(&conversation.id, |watch| {
            if let Some(engine) = session.engine() {
                watch.set_engine(engine);
            }
            follow(watch.clone(), session.clone());
        })
    }
}

impl Harness for Sessions {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn registration(&self) -> &dyn Registration {
        &super::Claude
    }

    /// Claude Code names the session, and that it has an inbox for messages, in its MCP server's environment.
    fn identify(&self, call: &CallContext) -> Result<Option<Conversation>> {
        let (Some(id), Some(_)) = (
            call.var("CLAUDE_CODE_SESSION_ID"),
            call.var("CLAUDE_CODE_MESSAGING_SOCKET"),
        ) else {
            return Ok(None);
        };
        let env = |name: &str| call.var(name);
        let home = agent_home(env);
        let pid = home.as_deref().and_then(|home| agent_process(home, env));
        Ok(Some(Conversation {
            harness: self.id().into(),
            id,
            agent: Agent { pid, home },
        }))
    }

    fn send<'a>(
        &'a self,
        conversation: &'a Conversation,
        message: &'a Value,
    ) -> BoxFuture<'a, Sent> {
        Box::pin(async move { super::send(&Session::of(conversation), message).await })
    }

    fn state<'a>(&'a self, conversation: &'a Conversation) -> BoxFuture<'a, State> {
        Box::pin(async move {
            // Asked afresh each time: a session whose settings were fixed takes messages at once.
            let watch = self.watch(conversation);
            watch.set_inbound(Session::of(conversation).inbound());
            watch.state()
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

/// Follows the session: its transcript, followed again when the process moves to another session (`/clear`), and its
/// working state.
///
/// Working state: the session record says busy, or the transcript holds a turn open (a message in, no final answer
/// yet) and was written to lately. The record alone reads idle between a message arriving and the turn starting, and
/// between tool calls. Working at once, idle once it has held for [`IDLE_DEBOUNCE`].
fn follow(watch: Arc<Watch<OpenTurn>>, session: Session) {
    tokio::spawn(async move {
        let mut idle_since = Instant::now();
        let mut followed: Option<String> = None;
        while !watch.stopped() {
            let current = session
                .record()
                .and_then(|record| record.session_id().map(str::to_owned))
                .unwrap_or_else(|| session.id().to_owned());
            if followed.as_deref() != Some(current.as_str()) {
                transcript::follow(
                    watch.clone(),
                    Reader {
                        session: session.clone(),
                        followed: current.clone(),
                        watch: watch.clone(),
                    },
                );
                followed = Some(current);
            }
            let open = watch.extra.0.load(Ordering::Relaxed);
            let working = session.working() == Some(true)
                || (open > 0 && now_ms().saturating_sub(open) < OPEN_TURN_MS);
            if working {
                idle_since = Instant::now();
            }
            watch.set_working(working || idle_since.elapsed() < IDLE_DEBOUNCE);
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    });
}

/// What a transcript entry says of the turn: `Some(true)` a turn is open, `Some(false)` it ended.
fn turn_signal(item: &Value) -> Option<bool> {
    if item.get("isSidechain") == Some(&json!(true)) {
        return None;
    }
    match item.get("type").and_then(Value::as_str)? {
        "assistant" => match item
            .pointer("/message/stop_reason")
            .and_then(Value::as_str)?
        {
            "end_turn" | "stop_sequence" | "max_tokens" | "refusal" => Some(false),
            _ => Some(true),
        },
        "user" => {
            let interrupted = transcript::user_text(item, true)
                .is_some_and(|text| text.starts_with("[Request interrupted by user"));
            Some(!interrupted)
        }
        "attachment" => (item.pointer("/attachment/type")? == "queued_command").then_some(true),
        _ => None,
    }
}

/// The transcript of the session the process runs (`followed`): the engine of each assistant entry, the open turn, and
/// the messages read. Followed while the process runs that session.
struct Reader {
    session: Session,
    followed: String,
    watch: Arc<Watch<OpenTurn>>,
}

impl TranscriptReader for Reader {
    fn path(&self) -> Option<PathBuf> {
        self.session.transcript(&self.followed)
    }

    fn current(&self) -> bool {
        self.session
            .record()
            .is_none_or(|record| record.session_id().is_none_or(|id| id == self.followed))
    }

    fn item(&mut self, item: &Value) -> Vec<Observed> {
        let mut observed = Vec::new();
        match turn_signal(item) {
            Some(true) => self.watch.extra.0.store(now_ms(), Ordering::Relaxed),
            Some(false) => self.watch.extra.0.store(0, Ordering::Relaxed),
            None => {}
        }
        if assistant_model(item).is_some() {
            let launch = self.session.engine().unwrap_or_else(|| json!({}));
            if let Some(engine) = assistant_engine(item, &launch) {
                observed.push(Observed::Engine(engine));
            }
        }
        if let Some(text) = transcript::user_text(item, true) {
            observed.extend(
                transcript::voice_headers(&text)
                    .into_iter()
                    .map(Observed::Read),
            );
        }
        observed
    }
}

#[cfg(test)]
mod tests {
    use super::turn_signal;
    use serde_json::json;

    #[test]
    fn a_turn_is_open_from_the_message_to_the_final_answer() {
        let user = json!({"type":"user","message":{"role":"user","content":"hi"}});
        let tool = json!({"type":"assistant","message":{"stop_reason":"tool_use"}});
        let end = json!({"type":"assistant","message":{"stop_reason":"end_turn"}});
        let side =
            json!({"type":"assistant","isSidechain":true,"message":{"stop_reason":"end_turn"}});
        let stop = json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}});
        let queued =
            json!({"type":"attachment","attachment":{"type":"queued_command","prompt":"x"}});
        assert_eq!(turn_signal(&user), Some(true));
        assert_eq!(turn_signal(&tool), Some(true));
        assert_eq!(turn_signal(&queued), Some(true));
        assert_eq!(turn_signal(&end), Some(false));
        assert_eq!(turn_signal(&stop), Some(false));
        assert_eq!(turn_signal(&side), None);
    }

    /// Issue #330: a headless Claude Code session (`--input-format stream-json --output-format stream-json`, no TTY),
    /// as Paseo runs it, is joined, sent a message, seen working and seen to have the message. Shapes as Claude Code
    /// 2.1.318 writes them in that mode: the record (`kind` `interactive`, `entrypoint` `sdk-cli`, `status` written
    /// from the SDK session state), the inbox that answers nothing when it takes a message, and the transcript, which
    /// first logs the message as enqueued and then as a user entry from a peer, the envelope inside the text Claude
    /// Code wraps it in.
    ///
    /// The agent's home here is a scratch directory, never this process's own `CLAUDE_CONFIG_DIR`: the daemon is
    /// shared, and the agent that started it may have a home of its own (0.16.0 read the daemon's, and followed
    /// nothing of any session whose home was another).
    #[tokio::test]
    async fn a_headless_session_is_seen_working_and_its_message_received() {
        use super::Sessions;
        use crate::adapters::harness::{CallContext, Harness, Inbound, Sent, Status};
        use crate::platform::ipc::{Address, Channel, Channels, Endpoint};
        use std::fs;
        use std::io::Write;
        use std::time::Duration;
        use tokio::io::AsyncBufReadExt;

        let scratch = crate::test_support::temp_dir("claude-headless");
        // A Unix socket is only made in a directory open to this user alone.
        #[cfg(unix)]
        fs::set_permissions(
            scratch.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let home = scratch.path().join("agent-home");
        let session_id = "1094c68b-6039-4953-b701-911511511cb0";
        let pid = std::process::id();
        // Where Claude Code binds its inbox on this OS: a Unix socket, a named pipe on Windows.
        let socket = Endpoint::new(scratch.path(), "inbox").to_string();
        let transcript = home
            .join("projects")
            .join("-work")
            .join(format!("{session_id}.jsonl"));
        fs::create_dir_all(home.join("sessions")).unwrap();
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        let record = |status: &str| {
            fs::write(
                home.join("sessions").join(format!("{pid}.json")),
                json!({"pid":pid,"sessionId":session_id,"cwd":"/work","version":"2.1.318","peerProtocol":1,
                    "kind":"interactive","entrypoint":"sdk-cli","messagingSocketPath":socket,
                    "name":"work-39","nameSource":"derived","status":status})
                .to_string(),
            )
            .unwrap();
        };
        let append = |entries: &[serde_json::Value]| {
            let mut file = fs::File::options()
                .create(true)
                .append(true)
                .open(&transcript)
                .unwrap();
            for entry in entries {
                writeln!(file, "{entry}").unwrap();
            }
        };
        record("idle");
        // The key file: native Windows takes no message without its token.
        fs::write(
            home.join("sessions").join(format!("{pid}.k.key")),
            json!({"peerToken":"t"}).to_string(),
        )
        .unwrap();
        append(&[]);

        // Claude Code's inbox: an auth line, then the message; it answers nothing and keeps the connection open.
        let listener = Channels::listen(&socket.parse().unwrap()).unwrap();
        let (taken, mut inbox) = tokio::sync::mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            let mut lines = tokio::io::BufReader::new(stream).lines();
            let _auth = lines.next_line().await.unwrap();
            let message = lines.next_line().await.unwrap().unwrap();
            taken.send(message).unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let env_home = home.to_string_lossy().into_owned();
        let env_socket = socket.clone();
        let env_pid = pid.to_string();
        let env = move |name: &str| match name {
            "CLAUDE_CODE_SESSION_ID" => Some(session_id.to_owned()),
            "CLAUDE_CODE_MESSAGING_SOCKET" => Some(env_socket.clone()),
            "CLAUDE_CONFIG_DIR" => Some(env_home.clone()),
            "CLAUDE_PID" => Some(env_pid.clone()),
            "CLAUDE_CODE_ENTRYPOINT" => Some("sdk-cli".to_owned()),
            _ => None,
        };
        let sessions = Sessions::default();
        let conversation = sessions
            .identify(&CallContext {
                env: &env,
                meta: &json!({}),
                client: &json!({"name":"claude-code"}),
                client_capabilities: &json!({}),
            })
            .unwrap()
            .expect("a headless Claude Code session is one of Claude Code's");
        assert_eq!(conversation.agent.pid, Some(pid));
        assert_eq!(conversation.agent.home.as_deref(), Some(home.as_path()));

        let state = sessions.state(&conversation).await;
        assert_eq!(state.inbound, Inbound::Open);
        assert_eq!(state.status, Status::Idle);

        let message = json!({"channel":"voice","session_id":"s1","revision":1,"epoch":1,
            "message_id":"m-330","text":"hello from voice"});
        assert_eq!(
            sessions.send(&conversation, &message).await,
            Sent::Accepted,
            "an inbox that answers nothing took the message"
        );
        let line: serde_json::Value = serde_json::from_str(&inbox.recv().await.unwrap()).unwrap();
        let content = line
            .pointer("/message/content")
            .and_then(serde_json::Value::as_str)
            .unwrap()
            .to_owned();

        // Enqueued is not in the conversation yet.
        append(&[
            json!({"type":"queue-operation","operation":"enqueue","sessionId":session_id,
            "content":content}),
        ]);
        let ids = ["m-330".to_owned()];
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(sessions.received(&conversation, &ids).await.is_empty());

        // The turn starts: the record says busy, the message is a user entry from a peer.
        record("busy");
        append(&[
            json!({"type":"queue-operation","operation":"dequeue","sessionId":session_id}),
            json!({"parentUuid":null,"isSidechain":false,"type":"user","isMeta":true,
                "message":{"role":"user","content":format!(
                    "Another Claude session sent a message:\n{content}\n\nThis came from another Claude session \u{2014} not typed by your user, but very likely working on their behalf.")},
                "origin":{"kind":"peer","from":"unknown"},"promptSource":"system","turnOrigin":"peer",
                "entrypoint":"sdk-cli","sessionId":session_id,"version":"2.1.318"}),
        ]);
        let (sessions, conversation, ids) = (&sessions, &conversation, &ids);
        let until = |what: &'static str| async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
            while tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let status = sessions.state(conversation).await.status;
                let received = sessions.received(conversation, ids).await == *ids;
                if what == "working" && status == Status::Working && received {
                    return true;
                }
                if what == "idle" && status == Status::Idle {
                    return true;
                }
            }
            false
        };
        assert!(until("working").await, "busy, and the message received");

        // The turn ends.
        append(&[json!({"type":"assistant","isSidechain":false,
            "message":{"model":"claude-opus-5-5","role":"assistant","stop_reason":"end_turn",
                "content":[{"type":"text","text":"done"}]},"sessionId":session_id})]);
        record("idle");
        assert!(until("idle").await, "idle once the turn ended");
        assert_eq!(
            sessions
                .state(conversation)
                .await
                .model
                .map(|model| model.name),
            Some("claude-opus-5-5".to_owned())
        );
    }
}
