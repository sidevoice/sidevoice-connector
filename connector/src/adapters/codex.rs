use super::{envelope, Identity};
use crate::profile::Profile;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::env;
use std::process::Stdio;
use std::time::Duration;

pub fn capabilities() -> Value {
    json!({"deliver":"supported","inspectInbound":"unsupported","working":"supported","endOfTurn":"supported","sessionIdentity":"supported"})
}

pub fn identity(meta: &Value) -> Result<Option<Identity>> {
    let turn = meta.get("x-codex-turn-metadata").and_then(|value| {
        if let Some(text) = value.as_str() {
            serde_json::from_str::<Value>(text).ok()
        } else {
            Some(value.clone())
        }
    });
    let thread = [
        meta.get("openai/threadId"),
        meta.get("openai/thread_id"),
        meta.get("codexThreadId"),
        meta.get("codex_thread_id"),
        turn.as_ref().and_then(|value| value.get("thread_id")),
        meta.get("session_id"),
        meta.get("thread_id"),
    ]
    .into_iter()
    .flatten()
    .find_map(|value| {
        value
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    })
    .or_else(|| {
        env::var("CODEX_THREAD_ID")
            .ok()
            .filter(|text| !text.is_empty())
    });
    let Some(thread) = thread else {
        return Ok(None);
    };
    let delivery = if let Some(url) = env::var("SIDEVOICE_DELIVERY_URL")
        .ok()
        .filter(|url| !url.is_empty())
    {
        json!({"kind":"http","url":url,"thread":thread})
    } else {
        json!({"kind":"codex-queue","thread":thread})
    };
    Ok(Some(Identity::new("codex", thread, delivery)))
}

/// Queues `event` on the thread with the Codex CLI the agent scan found, not whatever `codex` a service manager's
/// PATH reaches (`agents::agent_command`).
pub async fn deliver(delivery: &Value, event: &Value, profile: &Profile) -> Result<Value> {
    if delivery.get("kind").and_then(Value::as_str) != Some("codex-queue") {
        bail!("Unsupported Codex delivery route");
    }
    let thread = delivery
        .get("thread")
        .and_then(Value::as_str)
        .context("Codex thread missing")?;
    let codex = crate::agents::agent_command(profile, "codex");
    let mut command = tokio::process::Command::new(&codex.program);
    if let Some(path) = &codex.path {
        command.env("PATH", path);
    }
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        command
            .env("CODEX_HOME", &profile.codex)
            .args(["queue", "--thread", thread, "--message"])
            .arg(envelope(event)?)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await;
    match result {
        Ok(Ok(status)) if status.success() => {
            Ok(json!({"status":"accepted","detail":"codex queue confirmed the thread"}))
        }
        _ => bail!("Codex queue failed"),
    }
}
