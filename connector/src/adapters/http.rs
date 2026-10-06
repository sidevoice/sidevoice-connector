use super::Identity;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::env;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

pub fn capabilities() -> Value {
    json!({"deliver":"supported","inspectInbound":"unsupported","working":"unsupported","endOfTurn":"unsupported","sessionIdentity":"supported"})
}

pub fn identity() -> Result<Option<Identity>> {
    let (Ok(thread), Ok(url)) = (
        env::var("SIDEVOICE_THREAD"),
        env::var("SIDEVOICE_DELIVERY_URL"),
    ) else {
        return Ok(None);
    };
    if thread.is_empty() || url.is_empty() {
        return Ok(None);
    }
    let harness = env::var("SIDEVOICE_HARNESS").unwrap_or_else(|_| "http".into());
    let mut identity = Identity::new(
        &harness,
        thread.clone(),
        json!({"kind":"http","url":url,"thread":thread}),
    );
    identity.capabilities = capabilities();
    Ok(Some(identity))
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
        "channel":event.get("channel").and_then(Value::as_str).unwrap_or("voice"),
    })
    .to_string();
    let mut child = tokio::process::Command::new("curl")
        .args([
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
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    child
        .stdin
        .take()
        .context("HTTP sender stdin unavailable")?
        .write_all(payload.as_bytes())
        .await?;
    let output = tokio::time::timeout(Duration::from_secs(32), child.wait_with_output()).await??;
    if !output.status.success() {
        bail!("HTTP receiver request failed");
    }
    let response = String::from_utf8_lossy(&output.stdout);
    let status = response
        .rsplit('\n')
        .next()
        .and_then(|last| last.parse::<u16>().ok())
        .unwrap_or(0);
    if !(200..300).contains(&status) {
        bail!("Harness delivery failed ({status})");
    }
    Ok(json!({"status":"accepted","detail":format!("receiver answered {status}")}))
}
