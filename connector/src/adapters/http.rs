use super::conversation::Conversation;
use super::harness::{no_route, Harness, Route};
use super::Identity;
use crate::platform::process::{ChildProcess, Processes};
use crate::profile::Profile;
use anyhow::{bail, Context, Result};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::env;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub fn capabilities() -> Value {
    json!({"deliver":"supported","inspectInbound":"unsupported","working":"unsupported","endOfTurn":"unsupported","sessionIdentity":"supported"})
}

pub fn identity() -> Result<Option<Identity>> {
    let Ok(thread) = env::var("SIDEVOICE_THREAD") else {
        return Ok(None);
    };
    if thread.is_empty() {
        return Ok(None);
    }
    let url = env::var("SIDEVOICE_DELIVERY_URL")
        .ok()
        .filter(|url| !url.is_empty());
    let harness = env::var("SIDEVOICE_HARNESS").unwrap_or_else(|_| "http".into());
    Ok(Some(identity_for(&harness, thread, url)))
}

fn identity_for(harness: &str, thread: String, url: Option<String>) -> Identity {
    let mut identity = Identity::new(
        harness,
        thread.clone(),
        url.as_ref().map_or_else(
            || json!({"kind":"none"}),
            |url| json!({"kind":"http","url":url,"thread":thread}),
        ),
    );
    identity.capabilities = capabilities();
    if url.is_none() {
        identity.capabilities["deliver"] = json!("unsupported");
    }
    identity
}

pub async fn deliver(delivery: &Value, event: &Value) -> Result<Value> {
    let url = delivery
        .get("url")
        .and_then(Value::as_str)
        .context("HTTP receiver URL missing")?;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        bail!("HTTP receiver URL must use http or https");
    }
    let payload = json!({
        "thread_id":delivery.get("thread").and_then(Value::as_str).unwrap_or(""),
        "text":event.get("text").and_then(Value::as_str).unwrap_or(""),
        "message_id":event.get("message_id").and_then(Value::as_str).unwrap_or(""),
        "session_id":event.get("session_id").and_then(Value::as_str).unwrap_or(""),
        "revision":event.get("revision").and_then(Value::as_i64).unwrap_or(0),
        "epoch":event.get("epoch").and_then(Value::as_u64).unwrap_or(0),
        "channel":event.get("channel").and_then(Value::as_str).unwrap_or("voice"),
        "notice":event.get("notice"),
        "unheard":event.get("unheard"),
        "envelope":super::envelope(event)?,
    })
    .to_string();
    let mut curl = tokio::process::Command::new("curl");
    curl.args([
        "--silent",
        "--show-error",
        "--location",
        "--max-time",
        "30",
        "--max-redirs",
        "20",
        "--proto",
        "=http,https",
        "--header",
        "content-type: application/json",
        "--data-binary",
        "@-",
        "--write-out",
        "\n%{http_code}",
        "--",
        url,
    ]);
    let (status, stdout) = send(curl, payload.as_bytes(), Duration::from_secs(32)).await?;
    if !status.success() {
        bail!("HTTP receiver request failed");
    }
    let response = String::from_utf8_lossy(&stdout);
    let code = response
        .rsplit('\n')
        .next()
        .and_then(|last| last.parse::<u16>().ok())
        .unwrap_or(0);
    if !(200..300).contains(&code) {
        bail!("Harness delivery failed ({code})");
    }
    Ok(json!({"status":"accepted","detail":format!("receiver answered {code}")}))
}

/// Runs `curl` with `payload` on its stdin and gives its exit status and output, within `limit` after the payload is
/// written (then the program is stopped). Started through [`Processes::spawn_detached`], so that on Windows it has no
/// console window of its own: the daemon has none, and a console program started without one opens a visible window
/// for every message.
async fn send(
    mut curl: tokio::process::Command,
    payload: &[u8],
    limit: Duration,
) -> Result<(ExitStatus, Vec<u8>)> {
    curl.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = Processes::spawn_detached(curl).await?;
    child
        .stdin()
        .context("HTTP sender stdin unavailable")?
        .write_all(payload)
        .await?;
    let mut stdout = child.stdout().context("HTTP sender stdout unavailable")?;
    let finished = tokio::time::timeout(limit, async {
        let read = async {
            let mut output = Vec::new();
            stdout.read_to_end(&mut output).await?;
            std::io::Result::Ok(output)
        };
        let (status, output) = tokio::join!(child.wait(), read);
        Ok::<_, std::io::Error>((status?, output?))
    })
    .await;
    match finished {
        Ok(done) => Ok(done?),
        Err(elapsed) => {
            child.kill_tree().await;
            Err(elapsed.into())
        }
    }
}

/// Whether `delivery` is the HTTP route of conversation `thread`.
/// The HTTP route, which every harness knows: `Unknown` for any other kind.
pub fn accepts(delivery: &Value, thread: &str) -> Route {
    match delivery.get("kind").and_then(Value::as_str) {
        Some("none") => Route::Accepted,
        Some("http") => {
            Route::known(delivery.get("thread").and_then(Value::as_str) == Some(thread))
        }
        _ => Route::Unknown,
    }
}

/// Any agent whose conversations take voice by HTTP (`SIDEVOICE_DELIVERY_URL`): the harness of an id no other claims.
pub struct Http;

impl Harness for Http {
    fn id(&self) -> &'static str {
        "http"
    }

    fn capabilities(&self) -> Value {
        capabilities()
    }

    fn accepts(&self, delivery: &Value, thread: &str) -> Route {
        accepts(delivery, thread)
    }

    fn deliver<'a>(
        &'a self,
        conversation: &'a Conversation,
        frame: &'a Value,
        _profile: &'a Profile,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            match conversation.route_kind() {
                Some("http") => deliver(&conversation.delivery, frame).await,
                _ => Ok(no_route(&conversation.thread)),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_harness_without_an_injector_uses_get_messages() {
        let identity = identity_for("http", "thread".into(), None);
        assert_eq!(identity.delivery["kind"], "none");
        assert_eq!(identity.capabilities["deliver"], "unsupported");
        assert_eq!(accepts(&identity.delivery, "thread"), Route::Accepted);
        let with_injector = identity_for("http", "thread".into(), Some("http://local".into()));
        assert_eq!(with_injector.capabilities["deliver"], "supported");
    }

    #[tokio::test]
    async fn a_request_gives_its_status_and_output() {
        use crate::platform::stand_in::{StandIn, StandIns as _};
        use crate::platform::target::{Host, Target as _};
        let dir = crate::test_support::temp_dir("http-send");
        let program = dir.path().join(Host::exe_name("curl"));
        StandIn::make(&program, r#"{"play":"script","print":"ok\n201","exit":3}"#);
        // No payload: a stand-in that ends without reading its input would make writing a real one fail.
        let (status, stdout) = send(
            tokio::process::Command::new(&program),
            b"",
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        assert_eq!(status.code(), Some(3));
        assert_eq!(String::from_utf8_lossy(&stdout), "ok\n201");
    }

    #[tokio::test]
    async fn a_request_that_outlasts_its_limit_is_stopped() {
        use crate::platform::target::{Host, Target as _};
        use crate::test_support::stand_in::Script;
        let dir = crate::test_support::temp_dir("http-send-late");
        let program = dir.path().join(Host::exe_name("curl"));
        let pid_file = dir.path().join("pid");
        // Stays up a minute, far past the limit.
        Script {
            pid: Some(pid_file.clone()),
            sleep_ms: 60_000,
            ..Script::default()
        }
        .make(&program);
        let started = std::time::Instant::now();
        let late = send(
            tokio::process::Command::new(&program),
            b"",
            Duration::from_millis(1500),
        )
        .await;
        assert!(late.is_err(), "no answer in time");
        assert!(started.elapsed() < Duration::from_secs(30), "waited for it");
        // Where the program had not yet started to write its id, there is nothing left to be alive.
        if let Some(pid) = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|text| text.parse::<u32>().ok())
        {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while Processes::identity(pid).is_some() {
                assert!(std::time::Instant::now() < deadline, "it was left running");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}
