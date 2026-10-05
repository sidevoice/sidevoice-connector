use super::{cursor, decide, pre_tool_use, protocol};
use serde_json::json;

fn waiting(fresh: u64) -> serde_json::Value {
    json!({"connected":true,"pending":true,"count":2,"fresh":fresh})
}

#[test]
fn pre_tool_use_denies_once_for_new_messages_and_never_the_retrieval_tools() {
    let protocol = &pre_tool_use::PreToolUse;
    let input = json!({"session_id":"s","tool_name":"Bash"});
    let decision = decide(protocol, &input, Some(&waiting(2))).unwrap();
    assert_eq!(decision["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(decision["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .contains("voice_get_messages"));
    // Fetched but not yet acknowledged: the agent already has them, no second denial.
    assert!(decide(protocol, &input, Some(&waiting(0))).is_none());
    for tool in [
        "mcp__sidevoice__voice_get_messages",
        "mcp__sidevoice__voice_say",
        "mcp__other__voice_get_messages",
        "ToolSearch",
    ] {
        let input = json!({"session_id":"s","tool_name":tool});
        assert!(decide(protocol, &input, Some(&waiting(2))).is_none());
    }
    // No conversation, not joined, or no answer from the connector: the call goes through.
    assert!(decide(protocol, &json!({"tool_name":"Bash"}), Some(&waiting(2))).is_none());
    let absent = json!({"connected":false,"pending":false,"count":0,"fresh":0});
    assert!(decide(protocol, &input, Some(&absent)).is_none());
    assert!(decide(protocol, &input, None).is_none());
}

#[test]
fn cursor_denies_with_agent_and_user_messages_and_spares_its_mcp_retrieval() {
    let protocol = &cursor::PreToolUse;
    let input = json!({"conversation_id":"c","tool_name":"Shell","tool_input":{"command":"ls"}});
    let decision = decide(protocol, &input, Some(&waiting(1))).unwrap();
    assert_eq!(decision["permission"], "deny");
    assert!(decision["agent_message"]
        .as_str()
        .unwrap()
        .contains("voice_get_messages"));
    assert!(decision["user_message"].as_str().unwrap().contains('1'));
    let fetch = json!({"conversation_id":"c","tool_name":"MCP",
        "tool_input":{"name":"voice_get_messages","arguments":{}}});
    assert!(decide(protocol, &fetch, Some(&waiting(1))).is_none());
}

#[test]
fn hooks_exist_only_for_known_harness_events() {
    for (harness, event) in [
        ("claude", "pre-tool-use"),
        ("codex", "pre-tool-use"),
        ("cursor", "pre-tool-use"),
    ] {
        assert!(protocol(harness, event).is_ok());
    }
    assert!(protocol("claude", "stop").is_err());
    assert!(protocol("other", "pre-tool-use").is_err());
}
