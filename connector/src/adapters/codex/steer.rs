//! Steering a running Codex turn: `turn/steer` on the shared local app-server daemon.
//!
//! Codex's app-server protocol has had `turn/steer` since 0.99 (`{threadId, expectedTurnId, input}`: the input joins
//! the turn already running instead of waiting behind it). It is reachable from outside only on a server that listens
//! on a socket. Since 0.157 a plain `codex` session runs on the shared local daemon by default, which listens on
//! `$CODEX_HOME/app-server-control/app-server-control.sock`. A thread Codex owns privately (the desktop app's stdio
//! server, or `--no-daemon`) is not on that socket, and `codex queue` stays the only way in.
//!
//! Every outcome that is not an accepted steer is an error the caller answers by queueing.
use crate::platform::foreign::{Foreign, ForeignStream, Foreigners};
use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

/// The first Codex that hosts sessions on the shared daemon by default (0.157.0, "Enable automatic daemon startup by
/// default", openai/codex#47179), so the first where steering reaches a person's running turn from outside.
pub const MIN_VERSION: (u64, u64, u64) = (0, 157, 0);

const SOCKET: &str = "app-server-control/app-server-control.sock";
const TIMEOUT: Duration = Duration::from_secs(10);

/// `(major, minor, patch)` from `codex --version` output (`codex-cli 0.157.0`, `codex-cli 0.164.0-alpha.3`).
pub fn parse_version(output: &str) -> Option<(u64, u64, u64)> {
    let token = output
        .split_whitespace()
        .find(|word| word.starts_with(|c: char| c.is_ascii_digit()))?;
    let core = token.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|part| part.parse::<u64>());
    let version = (
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
    );
    parts.next().is_none().then_some(version)
}

pub fn supports_steer(version: Option<(u64, u64, u64)>) -> bool {
    version.is_some_and(|version| version >= MIN_VERSION)
}

pub fn socket_path(home: &Path) -> PathBuf {
    home.join(SOCKET)
}

type Socket = tokio_tungstenite::WebSocketStream<ForeignStream>;

/// Steers the active turn of `thread` with `text`, naming the message `message_id` (Codex's `clientUserMessageId`,
/// so a redelivery is the same message). Returns the id of the turn that took it.
pub async fn steer(socket: &Path, thread: &str, message_id: &str, text: &str) -> Result<String> {
    tokio::time::timeout(TIMEOUT, async {
        // Where Codex documents a local endpoint for the daemon (a Unix socket, today) this connects; where it does
        // not (Windows: nothing documented) the route is refused as unsupported and the caller queues instead.
        let route = socket.to_str().context("the Codex daemon socket path is not text")?;
        let stream = Foreigners::connect(route)
            .await
            .map_err(|error| anyhow!("no Codex daemon socket: {error}"))?;
        let (mut ws, _) = tokio_tungstenite::client_async("ws://localhost/", stream).await?;
        request(
            &mut ws,
            1,
            "initialize",
            json!({"clientInfo":{"name":"sidevoice-connector","version":env!("CARGO_PKG_VERSION")}}),
        )
        .await?;
        notify(&mut ws, "initialized").await?;
        let turns = request(
            &mut ws,
            2,
            "thread/turns/list",
            json!({"threadId":thread,"limit":1,"sortDirection":"desc","itemsView":"notLoaded"}),
        )
        .await?;
        let turn = active_turn(&turns).context("no running turn to steer")?;
        let answer = request(
            &mut ws,
            3,
            "turn/steer",
            json!({"threadId":thread,"expectedTurnId":turn,"clientUserMessageId":message_id,
                "input":[{"type":"text","text":text,"text_elements":[]}]}),
        )
        .await?;
        let _ = ws.close(None).await;
        answer
            .get("turnId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("steer answer without a turn id")
    })
    .await
    .map_err(|_| anyhow!("Codex steer timed out"))?
}

/// The id of the newest turn when it is still running.
fn active_turn(turns: &Value) -> Option<&str> {
    let newest = turns.get("data")?.as_array()?.first()?;
    (newest.get("status").and_then(Value::as_str) == Some("inProgress"))
        .then(|| newest.get("id").and_then(Value::as_str))
        .flatten()
}

async fn notify(ws: &mut Socket, method: &str) -> Result<()> {
    ws.send(Message::text(json!({"method":method}).to_string()))
        .await?;
    Ok(())
}

/// One request and its answer; notifications and requests the server sends meanwhile are not ours.
async fn request(ws: &mut Socket, id: u64, method: &str, params: Value) -> Result<Value> {
    ws.send(Message::text(
        json!({"id":id,"method":method,"params":params}).to_string(),
    ))
    .await?;
    while let Some(message) = ws.next().await {
        let Message::Text(text) = message? else {
            continue;
        };
        let Ok(frame) = serde_json::from_str::<Value>(text.as_str()) else {
            continue;
        };
        if frame.get("id") != Some(&json!(id)) || frame.get("method").is_some() {
            continue;
        }
        if let Some(error) = frame.get("error") {
            bail!("Codex {method} refused: {error}");
        }
        return frame
            .get("result")
            .cloned()
            .context("answer without result");
    }
    bail!("Codex daemon closed the connection")
}

/// A daemon that answers each request with the next canned answer and records what it was asked.
// A Unix socket of a test's own: Codex documents no daemon endpoint on Windows (the steer is unsupported there).
#[cfg(all(test, unix))]
#[allow(clippy::disallowed_types)]
pub(crate) async fn daemon_at(
    path: PathBuf,
    answers: Vec<Value>,
) -> tokio::task::JoinHandle<Vec<Value>> {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let mut seen = Vec::new();
        let mut answers = answers.into_iter();
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let frame: Value = serde_json::from_str(text.as_str()).unwrap();
            seen.push(frame.clone());
            let Some(id) = frame.get("id") else { continue };
            // A notification in the way, as a real server sends them.
            ws.send(Message::text(
                json!({"method":"thread/status/changed","params":{}}).to_string(),
            ))
            .await
            .unwrap();
            let Some(answer) = answers.next() else { break };
            let mut reply = json!({"id":id});
            let key = if answer.get("code").is_some() {
                "error"
            } else {
                "result"
            };
            reply[key] = answer;
            ws.send(Message::text(reply.to_string())).await.unwrap();
        }
        seen
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    async fn daemon(answers: Vec<Value>) -> (PathBuf, tokio::task::JoinHandle<Vec<Value>>) {
        // macOS's per-user temp dir can already consume most of SUN_LEN.
        let path = Path::new("/tmp")
            .join(format!("sv-steer-{}", uuid::Uuid::new_v4()))
            .join("daemon.sock");
        (path.clone(), daemon_at(path, answers).await)
    }

    #[test]
    fn versions_parse_from_codex_output() {
        assert_eq!(parse_version("codex-cli 0.157.0\n"), Some((0, 157, 0)));
        assert_eq!(
            parse_version("codex-cli 0.164.0-alpha.3"),
            Some((0, 164, 0))
        );
        assert_eq!(parse_version("0.99.1"), Some((0, 99, 1)));
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("codex-cli unknown"), None);
        assert_eq!(parse_version("codex-cli 0.157"), None);
    }

    #[test]
    fn steering_needs_the_minimum_version() {
        assert!(!supports_steer(None));
        assert!(!supports_steer(Some((0, 156, 9))));
        assert!(supports_steer(Some((0, 157, 0))));
        assert!(supports_steer(Some((0, 163, 0))));
        assert!(supports_steer(Some((1, 0, 0))));
    }

    #[test]
    fn only_a_running_newest_turn_is_steered() {
        let running =
            json!({"data":[{"id":"t2","status":"inProgress"},{"id":"t1","status":"completed"}]});
        assert_eq!(active_turn(&running), Some("t2"));
        for done in [
            json!({"data":[{"id":"t2","status":"completed"}]}),
            json!({"data":[{"id":"t2","status":"interrupted"}]}),
            json!({"data":[]}),
            json!({}),
        ] {
            assert_eq!(active_turn(&done), None, "{done}");
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn steers_the_running_turn_with_the_message_id() {
        let (path, daemon) = daemon(vec![
            json!({"userAgent":"x"}),
            json!({"data":[{"id":"turn-9","status":"inProgress"}]}),
            json!({"turnId":"turn-9"}),
        ])
        .await;
        let turn = steer(&path, "thread-1", "m-1", "hola").await.unwrap();
        assert_eq!(turn, "turn-9");
        let seen = daemon.await.unwrap();
        let methods: Vec<_> = seen.iter().map(|f| f["method"].as_str().unwrap()).collect();
        assert_eq!(
            methods,
            [
                "initialize",
                "initialized",
                "thread/turns/list",
                "turn/steer"
            ]
        );
        let params = &seen[3]["params"];
        assert_eq!(params["threadId"], "thread-1");
        assert_eq!(params["expectedTurnId"], "turn-9");
        assert_eq!(params["clientUserMessageId"], "m-1");
        assert_eq!(params["input"][0]["text"], "hola");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn an_idle_thread_is_not_steered() {
        let (path, daemon) = daemon(vec![
            json!({"userAgent":"x"}),
            json!({"data":[{"id":"turn-9","status":"completed"}]}),
        ])
        .await;
        assert!(steer(&path, "thread-1", "m-1", "hola").await.is_err());
        let seen = daemon.await.unwrap();
        assert!(seen.iter().all(|f| f["method"] != "turn/steer"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn a_refused_steer_is_an_error() {
        let (path, _daemon) = daemon(vec![
            json!({"userAgent":"x"}),
            json!({"data":[{"id":"turn-9","status":"inProgress"}]}),
            json!({"code":-32600,"message":"expected turn mismatch"}),
        ])
        .await;
        let error = steer(&path, "thread-1", "m-1", "hola").await.unwrap_err();
        assert!(error.to_string().contains("refused"), "{error}");
    }

    #[tokio::test]
    async fn no_socket_is_an_error() {
        let missing = std::env::temp_dir().join("sv-steer-missing/none.sock");
        assert!(steer(&missing, "t", "m", "x").await.is_err());
    }
}
