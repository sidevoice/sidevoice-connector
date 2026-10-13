//! The MCP server (`sidevoice-connector mcp`) as the agents that run it speak to it, with no core needed: a client of
//! MCP 2026-07-28 (current Claude Code) opens with `server/discover` and sends the protocol's per-request metadata,
//! and must get every list with its cache hints, which that version requires (`ttlMs`, `cacheScope`), or it rejects
//! the list and the conversation has no voice tools; a client of an older version opens with `initialize` and must
//! get the same tools.

mod support;

use serde_json::{json, Value};
use support::*;

const STATELESS: &str = "2026-07-28";
const OLDER: &str = "2025-11-25";

/// The per-request metadata a 2026-07-28 client sends with every request.
fn meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": STATELESS,
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {"name": "claude-code", "version": "2.1.292"},
    })
}

fn names(list: &Value) -> Vec<&str> {
    list.as_array()
        .unwrap_or_else(|| panic!("not a list: {list}"))
        .iter()
        .filter_map(|item| item["name"].as_str())
        .collect()
}

#[test]
fn a_client_that_opens_with_server_discover_gets_every_list_with_its_cache_hints() {
    let profile = Profile::new("discover");
    let mut mcp = Mcp::start(profile.connector(&["mcp"]));

    let discovered = mcp.request("server/discover", json!({"_meta": meta()}));
    let versions: Vec<_> = discovered["supportedVersions"]
        .as_array()
        .unwrap_or_else(|| panic!("server/discover: {discovered}"))
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(versions.contains(&STATELESS), "{discovered}");

    for (method, items) in [("tools/list", "tools"), ("prompts/list", "prompts")] {
        let list = mcp.request(method, json!({"_meta": meta()}));
        assert!(
            list["ttlMs"].is_u64(),
            "{method}: ttlMs must be a number: {list}"
        );
        assert!(
            matches!(list["cacheScope"].as_str(), Some("public" | "private")),
            "{method}: cacheScope must be public or private: {list}"
        );
        assert!(list[items].is_array(), "{method}: {list}");
        if method == "tools/list" {
            assert!(names(&list[items]).contains(&"voice_connect"), "{list}");
        }
    }
    mcp.stop();
}

#[test]
fn a_client_that_initializes_with_an_older_version_gets_the_tools() {
    let profile = Profile::new("initialize");
    let mut mcp = Mcp::start(profile.connector(&["mcp"]));
    let initialized = mcp.request(
        "initialize",
        json!({"protocolVersion": OLDER, "capabilities": {},
               "clientInfo": {"name": "older-agent", "version": "test"}}),
    );
    assert_eq!(initialized["protocolVersion"], OLDER, "{initialized}");
    mcp.notify("notifications/initialized", json!({}));
    let tools = mcp.request("tools/list", json!({}));
    assert!(names(&tools["tools"]).contains(&"voice_connect"), "{tools}");
    mcp.stop();
}

/// Codex, the CLI and the desktop app alike, runs the MCP server with a filtered environment that never holds the
/// conversation, and says which conversation is calling in each tool call's `_meta` (`x-codex-turn-metadata`, with
/// its `thread_id`): `voice_connect` joins that conversation. A call that names none is refused naming Codex and the
/// agents Sidevoice joins, and tells the agent nothing to set.
#[test]
fn codex_names_its_conversation_in_each_calls_meta() {
    let profile = Profile::new("codexmeta");
    let _connector = profile.start_connector();
    let mut mcp = Mcp::start(profile.connector(&["mcp"]));
    mcp.initialize("codex-mcp-client");

    let refused = mcp.request(
        "tools/call",
        json!({"name": "voice_connect", "arguments": {}}),
    );
    assert_eq!(refused["isError"], true, "{refused}");
    let text = refused["content"][0]["text"].as_str().unwrap_or("");
    for named in ["Codex", "Claude Code", "Cursor"] {
        assert!(text.contains(named), "{named} in {text}");
    }
    assert!(
        !text.contains("SIDEVOICE_"),
        "no settings for people: {text}"
    );

    let thread = "019a0000-0000-7000-8000-000000000001";
    let turn = json!({"session_id": thread, "thread_id": thread, "turn_id": "turn-1"}).to_string();
    let joined = mcp.request(
        "tools/call",
        json!({"name": "voice_connect", "arguments": {},
               "_meta": {"callId": "call-1", "x-codex-turn-metadata": turn}}),
    );
    assert_ne!(joined["isError"], true, "{joined}");
    let answer: Value = serde_json::from_str(joined["content"][0]["text"].as_str().unwrap_or(""))
        .unwrap_or_default();
    assert_eq!(answer["conversation"], thread, "{answer}");
    assert_eq!(answer["harness"], "codex", "{answer}");
}

/// The agents' health check starts the server as an agent would, with `SIDEVOICE_HEALTH_CHECK` set, and asks only
/// for the handshake and the tool list: the server answers both without a daemon, and starts none.
#[test]
fn a_health_check_gets_the_tools_and_starts_no_daemon() {
    let profile = Profile::new("health");
    let mut command = profile.connector(&["mcp"]);
    command.env("SIDEVOICE_HEALTH_CHECK", "1");
    let mut mcp = Mcp::start(command);
    let initialized = mcp.request(
        "initialize",
        json!({"protocolVersion": OLDER, "capabilities": {},
               "clientInfo": {"name": "sidevoice-health", "version": "test"}}),
    );
    assert_eq!(initialized["protocolVersion"], OLDER, "{initialized}");
    mcp.notify("notifications/initialized", json!({}));
    let tools = mcp.request("tools/list", json!({}));
    assert!(names(&tools["tools"]).contains(&"voice_connect"), "{tools}");
    mcp.stop();
    for file in ["connector.sock", "connector.lock"] {
        assert!(
            !profile.data().join(file).exists(),
            "{file}: a health check started the daemon"
        );
    }
}

/// An agent newer than this server may send arguments and `_meta` keys it does not know (sidevoice/sidevoice-core#112):
/// no tool schema refuses them, and each call is answered for what it knows.
#[test]
fn a_tool_call_with_arguments_this_server_does_not_know_is_answered_for_what_it_knows() {
    let profile = Profile::new("extraargs");
    let _connector = profile.start_connector();
    let mut mcp = Mcp::start(profile.connector(&["mcp"]));
    mcp.initialize("codex-mcp-client");

    let tools = mcp.request("tools/list", json!({}));
    for tool in tools["tools"].as_array().unwrap() {
        assert_eq!(
            tool["inputSchema"].get("additionalProperties"),
            None,
            "{tool}"
        );
    }

    let thread = "019a0000-0000-7000-8000-000000000002";
    let turn = json!({"session_id": thread, "thread_id": thread, "turn_id": "turn-1",
                      "future_turn_field": {"nested": true}})
    .to_string();
    let meta =
        json!({"callId": "call-1", "x-codex-turn-metadata": turn, "future-meta-key": [1, 2]});
    let joined = mcp.request(
        "tools/call",
        json!({"name": "voice_connect", "_meta": meta,
               "arguments": {"title": "Extra arguments", "future_argument": true,
                             "future_object": {"nested": {"deeper": 1}}}}),
    );
    assert_ne!(joined["isError"], true, "{joined}");
    let answer: Value = serde_json::from_str(joined["content"][0]["text"].as_str().unwrap_or(""))
        .unwrap_or_default();
    assert_eq!(answer["conversation"], thread, "{answer}");

    let status = mcp.request(
        "tools/call",
        json!({"name": "voice_status", "_meta": meta,
               "arguments": {"future_argument": "x", "verbose": {"nested": true}}}),
    );
    assert_ne!(status["isError"], true, "{status}");
    let status: Value = serde_json::from_str(status["content"][0]["text"].as_str().unwrap_or(""))
        .unwrap_or_default();
    assert_eq!(status["joined"], true, "{status}");
    assert_eq!(status["conversation"], thread, "{status}");

    let left = mcp.request(
        "tools/call",
        json!({"name": "voice_disconnect", "_meta": meta, "arguments": {"future_argument": 1}}),
    );
    assert_ne!(left["isError"], true, "{left}");
    mcp.stop();
}
