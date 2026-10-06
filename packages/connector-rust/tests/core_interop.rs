//! The connector against the real core (sidevoice-core's published nightly), end to end, in a private profile:
//! the connector links to a core it did not start, over the core's own socket; a conversation joins through the
//! MCP server; what the person says in a call reaches the conversation; the conversation's reply reaches the core;
//! and when the core restarts the connector links to the new one by itself.

mod support;

use serde_json::json;
use std::time::Duration;
use support::*;

#[test]
fn a_conversation_talks_to_the_real_core_through_the_connector() {
    let Some(core) = core_dir() else { return };
    let profile = Profile::new("interop");
    let mut first_core = profile.start_core(&core, "interop-1");
    let _connector = profile.start_connector();
    let linked = profile.wait_linked("interop-1");
    assert_eq!(linked["protocol"], 3, "{linked}");
    assert_eq!(linked["core_pid"], first_core.pid(), "{linked}");

    let call = Presentation::open(&profile.core_socket());
    let receiver = http_receiver();
    let thread = "connector-interop-thread";
    let mut command = profile.connector(&["mcp"]);
    command
        .env("SIDEVOICE_THREAD", thread)
        .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
    let mut mcp = Mcp::start(command);

    let initialized = mcp.initialize("connector-interop");
    assert!(
        initialized["instructions"]
            .as_str()
            .is_some_and(|text| text.contains("Sidevoice connects")),
        "{initialized}"
    );
    let listed = mcp.request("tools/list", json!({}));
    let mut tools: Vec<&str> = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    tools.sort();
    assert_eq!(
        tools,
        [
            "voice_connect",
            "voice_disconnect",
            "voice_pair",
            "voice_pair_device",
            "voice_say",
            "voice_status"
        ]
    );

    let joined = mcp.tool("voice_connect", json!({"title": "Connector interop"}));
    assert_eq!(joined["conversation"], thread, "{joined}");
    assert!(
        joined["status"] == "joined" || joined["status"] == "joining",
        "{joined}"
    );
    let binding = joined["binding_id"].as_str().unwrap_or("");
    assert!(
        !binding.is_empty() && !binding.starts_with("local-"),
        "{joined}"
    );

    // The person speaks in the call (as text): the core hands it to the connector, which delivers it.
    let socket = profile.core_socket();
    let selected = core_json(
        &socket,
        "POST",
        "/api/presentation/select",
        Some(&json!({"session_id": call.session, "thread_id": thread})),
        Some(&call.token),
    );
    let message_id = uuid::Uuid::new_v4().to_string();
    let accepted = core_json(
        &socket,
        "POST",
        "/api/presentation/text",
        Some(
            &json!({"text": "Hello from the call", "session_id": call.session, "thread_id": thread,
                     "binding_id": selected["binding"]["binding_id"], "message_id": message_id}),
        ),
        Some(&call.token),
    );
    assert_eq!(accepted["accepted"], true, "{accepted}");
    let delivered = receiver
        .bodies
        .recv_timeout(Duration::from_secs(30))
        .expect("the conversation received nothing");
    assert_eq!(delivered["thread_id"], thread, "{delivered}");
    assert_eq!(delivered["message_id"], message_id, "{delivered}");
    assert!(
        delivered["text"]
            .as_str()
            .is_some_and(|text| text.contains("Hello from the call")),
        "{delivered}"
    );

    // The conversation answers: the reply is saved in the core.
    let status = mcp.tool("voice_status", json!({}));
    assert_eq!(status["joined"], true, "{status}");
    assert_eq!(status["binding_id"], binding, "{status}");
    let said = mcp.tool(
        "voice_say",
        json!({"session_id": delivered["session_id"], "revision": delivered["revision"],
               "text": "A reply from the conversation"}),
    );
    assert_eq!(said["text_saved"], true, "{said}");

    // The core restarts: the connector links to the new one without anybody asking.
    first_core.stop();
    drop(call);
    let _second_core = profile.start_core(&core, "interop-2");
    profile.wait_linked("interop-2");
    until("the conversation sees the core again", 30, || {
        let status = mcp.tool("voice_status", json!({}));
        (status["connector"]["connected"] == true).then_some(())
    });
    mcp.stop();
}

/// The trust boundary is the OS user: the profile's directories, the connector's socket and the core's are this
/// user's alone (another user can neither enter the directories nor connect to the sockets).
#[test]
fn the_connector_and_core_sockets_are_this_users_alone() {
    use std::os::unix::fs::PermissionsExt;
    let Some(core) = core_dir() else { return };
    let profile = Profile::new("modes");
    let _core = profile.start_core(&core, "modes-1");
    let _connector = profile.start_connector();
    profile.wait_linked("modes-1");
    for path in [
        profile.data(),
        profile.core_data(),
        profile.connector_socket(),
        profile.core_socket(),
        profile.data().join("proof.json"),
    ] {
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o077,
            0,
            "{} is open to others: {mode:o}",
            path.display()
        );
    }
}
