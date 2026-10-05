//! Cursor's `preToolUse` protocol: `conversation_id`, `tool_name` and `tool_input` on stdin;
//! `permission`, `agent_message` and `user_message` on stdout.

use super::{agent_reason, Attempt, Protocol};
use serde_json::{json, Value};

pub(super) struct PreToolUse;

impl Protocol for PreToolUse {
    fn attempt<'a>(&self, input: &'a Value) -> Attempt<'a> {
        // Cursor names every MCP call "MCP"; which tool it is lives in tool_input.
        let mcp_tool = input
            .get("tool_input")
            .map(Value::to_string)
            .unwrap_or_default();
        Attempt {
            thread: input
                .get("conversation_id")
                .and_then(Value::as_str)
                .unwrap_or(""),
            exempt: input.get("tool_name").and_then(Value::as_str) == Some("MCP")
                && (mcp_tool.contains("voice_get_messages")
                    || mcp_tool.contains("voice_has_pending")),
        }
    }

    fn deny(&self, fresh: u64) -> Value {
        json!({"permission":"deny","agent_message":agent_reason(fresh),
            "user_message":crate::agents::message("hook.voice-waiting", &json!({"count":fresh}))})
    }
}
