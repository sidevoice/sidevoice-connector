//! Mechanical hook checks for pulled voice input. A harness runs `hook <harness> <event>` as a
//! command hook; the common part asks the local connector whether the hook's own conversation,
//! joined for pulled input, has voice messages nobody fetched yet, and an adapter per hook
//! protocol reads the harness's input and writes its decision. No model is involved and no
//! message text leaves the connector.

mod check;
mod cursor;
mod pre_tool_use;
#[cfg(test)]
mod tests;

use crate::proof::Profile;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, BufReader};

/// What a protocol adapter reads from the harness's hook input.
pub(crate) struct Attempt<'a> {
    /// The conversation the harness names, which is the binding's thread.
    pub thread: &'a str,
    /// Whether the attempted tool is one the agent needs to read its messages.
    pub exempt: bool,
}

/// A hook protocol: how one harness describes the attempted call and expects a denial.
pub(crate) trait Protocol {
    fn attempt<'a>(&self, input: &'a Value) -> Attempt<'a>;
    fn deny(&self, fresh: u64) -> Value;
}

/// The harness's name as its bindings are registered, and the protocol of its hook.
fn protocol(harness: &str, event: &str) -> Result<&'static dyn Protocol> {
    match (harness, event) {
        ("claude" | "codex", "pre-tool-use") => Ok(&pre_tool_use::PreToolUse),
        ("cursor", "pre-tool-use") => Ok(&cursor::PreToolUse),
        _ => bail!("unsupported hook: {harness} {event}"),
    }
}

/// Decide for one attempted call. Anything unexpected (no conversation, no answer from the
/// connector, nothing new) lets the call through.
pub(crate) fn decide(
    protocol: &dyn Protocol,
    input: &Value,
    check: Option<&Value>,
) -> Option<Value> {
    let attempt = protocol.attempt(input);
    if attempt.exempt || attempt.thread.is_empty() {
        return None;
    }
    let fresh = check?.get("fresh").and_then(Value::as_u64).unwrap_or(0);
    (fresh > 0).then(|| protocol.deny(fresh))
}

/// The reason the agent reads when its call is denied (agent-facing, English).
pub(crate) fn agent_reason(fresh: u64) -> String {
    format!(
        "Sidevoice: {fresh} new voice message{} from the user wait for this conversation. Call voice_get_messages, handle them as its result says, then retry this tool call.",
        if fresh == 1 { "" } else { "s" }
    )
}

pub async fn run(profile: Profile, harness: &str, event: &str) -> Result<()> {
    let protocol = protocol(harness, event)?;
    let mut stdin = String::new();
    BufReader::new(tokio::io::stdin().take(1 << 20))
        .read_to_string(&mut stdin)
        .await?;
    let input: Value = serde_json::from_str(&stdin).unwrap_or(Value::Null);
    let attempt = protocol.attempt(&input);
    if attempt.exempt || attempt.thread.is_empty() {
        return Ok(());
    }
    let check = match check::pending(&profile, harness, attempt.thread).await {
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
    if let Some(decision) = decide(protocol, &input, check.as_ref()) {
        println!("{decision}");
    }
    Ok(())
}
