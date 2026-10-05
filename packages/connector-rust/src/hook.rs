//! Mechanical hook checks: a harness runs this as a command hook, and it asks the local
//! connector whether the hook's own conversation, joined for pulled voice input, has voice
//! messages nobody fetched yet. No model is involved and no message text leaves the connector.

use crate::proof::{verify_socket, Profile};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::{timeout, Duration};

/// Tools the guard never stops: the agent must always be able to fetch what it is told about.
fn exempt(tool: &str) -> bool {
    tool.starts_with("mcp__sidevoice__")
        || tool.ends_with("voice_get_messages")
        || tool.ends_with("voice_has_pending")
}

/// Claude Code `PreToolUse`: the conversation the hook names (its `session_id`) and the
/// connector's answer for it decide whether the attempted tool call is denied, once per
/// batch of new messages. Anything unexpected lets the call through.
pub fn claude_pre_tool_use(input: &Value, check: Option<&Value>) -> Option<Value> {
    let tool = input.get("tool_name").and_then(Value::as_str).unwrap_or("");
    if exempt(tool) {
        return None;
    }
    let fresh = check?.get("fresh").and_then(Value::as_u64).unwrap_or(0);
    if fresh == 0 {
        return None;
    }
    let reason = format!(
        "Sidevoice: {fresh} new voice message{} from the user wait for this conversation. Call voice_get_messages, handle them as its result says, then retry this tool call.",
        if fresh == 1 { "" } else { "s" }
    );
    Some(json!({"hookSpecificOutput":{"hookEventName":"PreToolUse",
        "permissionDecision":"deny","permissionDecisionReason":reason}}))
}

async fn pull_check(profile: &Profile, harness: &str, thread: &str) -> Result<Value> {
    verify_socket(&profile.socket)?;
    let stream = timeout(Duration::from_secs(2), UnixStream::connect(&profile.socket)).await??;
    let (read, mut write) = stream.into_split();
    let request =
        json!({"id":1,"method":"pull_check","params":{"harness":harness,"thread":thread}});
    write.write_all(format!("{request}\n").as_bytes()).await?;
    let mut reader = BufReader::new(read);
    let line = timeout(
        Duration::from_secs(6),
        crate::bounded_line(&mut reader, 1 << 16),
    )
    .await??
    .context("connector closed the check")?;
    let reply: Value = serde_json::from_str(&line)?;
    if reply.get("ok") != Some(&json!(true)) {
        bail!(
            "{}",
            reply
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("refused")
        );
    }
    Ok(reply.get("result").cloned().unwrap_or(Value::Null))
}

pub async fn run(profile: Profile, event: &str) -> Result<()> {
    if event != "claude-pre-tool-use" {
        bail!("unknown hook event");
    }
    let mut stdin = String::new();
    BufReader::new(tokio::io::stdin().take(1 << 20))
        .read_to_string(&mut stdin)
        .await?;
    let input: Value = serde_json::from_str(&stdin).unwrap_or(Value::Null);
    let tool = input.get("tool_name").and_then(Value::as_str).unwrap_or("");
    let thread = input
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    if exempt(tool) || thread.is_empty() {
        return Ok(());
    }
    let check = match pull_check(&profile, "claude", thread).await {
        Ok(check) => Some(check),
        Err(error) => {
            // Never block the agent because Sidevoice is away: say why, let the call through.
            eprintln!(
                "{}",
                crate::agents::message(
                    "hook.check-unavailable",
                    &json!({"detail":error.to_string()})
                )
            );
            None
        }
    };
    if let Some(decision) = claude_pre_tool_use(&input, check.as_ref()) {
        println!("{decision}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_once_for_new_messages_and_never_the_retrieval_tools() {
        let input = json!({"session_id":"s","tool_name":"Bash"});
        let waiting = json!({"connected":true,"pending":true,"count":2,"fresh":2});
        let decision = claude_pre_tool_use(&input, Some(&waiting)).unwrap();
        assert_eq!(decision["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(decision["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains("voice_get_messages"));
        // Fetched but not yet acknowledged: the agent already has them, no second denial.
        let held = json!({"connected":true,"pending":true,"count":2,"fresh":0});
        assert!(claude_pre_tool_use(&input, Some(&held)).is_none());
        for tool in [
            "mcp__sidevoice__voice_get_messages",
            "mcp__sidevoice__voice_say",
            "mcp__other__voice_get_messages",
        ] {
            let input = json!({"session_id":"s","tool_name":tool});
            assert!(claude_pre_tool_use(&input, Some(&waiting)).is_none());
        }
        // Not joined, or the connector could not answer: the call goes through.
        let absent = json!({"connected":false,"pending":false,"count":0,"fresh":0});
        assert!(claude_pre_tool_use(&input, Some(&absent)).is_none());
        assert!(claude_pre_tool_use(&input, None).is_none());
    }
}
