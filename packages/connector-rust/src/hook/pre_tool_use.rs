//! The `PreToolUse` command-hook protocol shared by Claude Code and Codex: `session_id` and
//! `tool_name` on stdin; a `hookSpecificOutput` permission decision on stdout.

use super::{agent_reason, Attempt, Protocol};
use serde_json::{json, Value};

pub(super) struct PreToolUse;

impl Protocol for PreToolUse {
    fn attempt<'a>(&self, input: &'a Value) -> Attempt<'a> {
        let tool = input.get("tool_name").and_then(Value::as_str).unwrap_or("");
        Attempt {
            thread: input
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or(""),
            // ToolSearch loads deferred MCP tool schemas, voice_get_messages among them.
            exempt: tool == "ToolSearch"
                || tool.starts_with("mcp__sidevoice__")
                || tool.ends_with("voice_get_messages")
                || tool.ends_with("voice_has_pending"),
        }
    }

    fn deny(&self, fresh: u64) -> Value {
        json!({"hookSpecificOutput":{"hookEventName":"PreToolUse",
            "permissionDecision":"deny","permissionDecisionReason":agent_reason(fresh)}})
    }
}
