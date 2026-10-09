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
