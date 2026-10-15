//! The connector against the real core (the sidevoice-core release pinned in core.pin), end to end, in a private profile:
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
    assert_eq!(linked["pid"], first_core.pid(), "{linked}");

    let call = Presentation::open(&profile.core_socket());

    // The machine as the room will list it (asked as the device the call paired): what this machine actually is,
    // not a placeholder.
    let listed = core_json(
        &profile.core_socket(),
        "GET",
        "/api/connectors",
        None,
        Some(&call.token),
    );
    let machine = listed["connectors"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["connected"] == true))
        .unwrap_or_else(|| panic!("no connected connector: {listed}"))
        .clone();
    let host = machine["host"].as_str().unwrap_or("");
    assert!(!host.is_empty() && host != "rust-proof", "{machine}");
    let platform = machine["platform"].as_str().unwrap_or("");
    assert!(
        platform.starts_with("macOS ") || platform.starts_with("Linux "),
        "{machine}"
    );
    assert!(
        machine["version"].as_str().is_some_and(|v| !v.is_empty()),
        "{machine}"
    );
    // The profile keeps homes for Claude Code, Codex and Cursor; nothing else is claimed.
    assert_eq!(
        machine["harnesses"],
        json!(["claude", "codex", "cursor"]),
        "{machine}"
    );
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
            "get_messages",
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
        json!({"session_id": delivered["session_id"], "revision": delivered["revision"], "epoch": delivered["epoch"],
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
        profile.data().join("connector.lock"),
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

/// A device code, from the real core through the connector: `pair-device --json` gives the fields the desktop app
/// reads, the text gives the code, its QR and where it works, and the MCP tool says the same text.
#[test]
fn pair_device_gives_the_cores_code_with_its_qr() {
    let Some(core) = core_dir() else { return };
    let profile = Profile::new("pairdev");
    let _core = profile.start_core(&core, "pairdev-1");
    let _connector = profile.start_connector();
    profile.wait_linked("pairdev-1");

    let output = profile
        .connector(&["pair-device", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let answer: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(answer["ok"], true, "{answer}");
    let code = answer["code"].as_str().unwrap_or("");
    assert!(code.starts_with("SV1."), "{answer}");
    assert!(
        answer["expires_in"].as_u64().is_some_and(|s| s > 0),
        "{answer}"
    );
    // The test's core has only its loopback address and no room.
    assert_eq!(answer["reach"], "local-only", "{answer}");
    assert!(answer["payload"]["urls"].is_array(), "{answer}");

    let output = profile.connector(&["pair-device"]).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.starts_with("One-time code to pair a device with this machine"),
        "{text}"
    );
    let printed = text.lines().nth(2).unwrap_or("");
    assert!(
        printed.starts_with("SV1.") && printed != code,
        "a fresh code: {text}"
    );
    assert!(text.contains('█') && text.contains('▀'), "{text}");
    assert!(text.contains("only works on this computer"), "{text}");

    let mut mcp = Mcp::start(profile.connector(&["mcp"]));
    mcp.initialize("connector-pair-device");
    let result = mcp.request(
        "tools/call",
        json!({"name": "voice_pair_device", "arguments": {}}),
    );
    assert_ne!(result["isError"], true, "{result}");
    let said = result["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        said.starts_with("One-time code to pair a device with this machine")
            && said.contains("\n\nSV1.")
            && said.contains('█'),
        "{result}"
    );
    mcp.stop();
}

#[test]
fn t16_pull_claim_ack_redelivery_and_mode_handover_use_the_real_core() {
    let Some(core) = core_dir() else { return };
    let profile = Profile::new("turns-pull");
    let _core = profile.start_core(&core, "turns-pull-core");
    let _connector = profile.start_connector();
    profile.wait_linked("turns-pull-core");
    let call = Presentation::open(&profile.core_socket());
    let receiver = http_receiver();
    let thread = "turns-pull-thread";
    let mut command = profile.connector(&["mcp"]);
    command
        .env("SIDEVOICE_THREAD", thread)
        .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
    let mut mcp = Mcp::start(command);
    mcp.initialize("turns-pull-client");
    let joined = mcp.tool("voice_connect", json!({"input_mode":"pull"}));
    assert_eq!(joined["delivery"], "pull", "{joined}");
    assert_eq!(joined["initial_reply_context"]["epoch"], 0, "{joined}");
    let selected = core_json(
        &profile.core_socket(),
        "POST",
        "/api/presentation/select",
        Some(&json!({"session_id":call.session,"thread_id":thread})),
        Some(&call.token),
    );
    let id = uuid::Uuid::new_v4().to_string();
    let posted = core_json(
        &profile.core_socket(),
        "POST",
        "/api/presentation/text",
        Some(
            &json!({"text":"Pull words","session_id":call.session,"thread_id":thread,
            "binding_id":selected["binding"]["binding_id"],"message_id":id}),
        ),
        Some(&call.token),
    );
    assert_eq!(posted["accepted"], true, "{posted}");
    assert!(receiver
        .bodies
        .recv_timeout(Duration::from_millis(250))
        .is_err());
    let check = mcp.tool("get_messages", json!({"operation":"check"}));
    assert_eq!(check["count"], 1, "{check}");
    let first = mcp.tool("get_messages", json!({}));
    assert_eq!(first["messages"][0]["message_id"], id, "{first}");
    assert_eq!(first["messages"][0]["epoch"], 0, "{first}");
    assert!(first["messages"][0]["envelope"]
        .as_str()
        .unwrap()
        .contains("Pull words"));
    let again = mcp.tool("get_messages", json!({}));
    assert_eq!(again["messages"][0]["message_id"], id, "{again}");
    mcp.stop();
    let mut reconnected_command = profile.connector(&["mcp"]);
    reconnected_command
        .env("SIDEVOICE_THREAD", thread)
        .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
    let mut mcp = Mcp::start(reconnected_command);
    mcp.initialize("turns-pull-reconnected");
    let rejoined = mcp.tool("voice_connect", json!({"input_mode":"pull"}));
    assert_eq!(rejoined["delivery"], "pull", "{rejoined}");
    let recovered = mcp.tool("get_messages", json!({}));
    assert_eq!(recovered["messages"][0]["message_id"], id, "{recovered}");
    assert_eq!(
        recovered["messages"][0]["envelope"],
        first["messages"][0]["envelope"]
    );
    let ack = mcp.tool("get_messages", json!({"ack_ids":[id]}));
    assert_eq!(ack["acknowledged"], 1, "{ack}");
    assert_eq!(ack["count"], 0, "{ack}");
    let push = mcp.tool("voice_connect", json!({"input_mode":"push"}));
    assert_eq!(push["delivery"], "push", "{push}");
    let id2 = uuid::Uuid::new_v4().to_string();
    let posted = core_json(
        &profile.core_socket(),
        "POST",
        "/api/presentation/text",
        Some(
            &json!({"text":"Push words","session_id":call.session,"thread_id":thread,
            "binding_id":selected["binding"]["binding_id"],"message_id":id2}),
        ),
        Some(&call.token),
    );
    assert_eq!(posted["accepted"], true, "{posted}");
    let pushed = receiver
        .bodies
        .recv_timeout(Duration::from_secs(15))
        .unwrap();
    assert_eq!(pushed["message_id"], id2, "{pushed}");
    assert_eq!(pushed["epoch"], 0, "{pushed}");
    assert!(pushed["envelope"].as_str().unwrap().contains("Push words"));
    mcp.stop();
}

#[test]
fn t02c_t02d_open_turn_refusal_is_terminal_and_empty_notice_is_pullable() {
    let Some(core) = core_dir() else { return };
    let profile = Profile::new("turns-refusal");
    let _core = profile.start_core(&core, "turns-refusal-core");
    let _connector = profile.start_connector();
    profile.wait_linked("turns-refusal-core");
    let call = Presentation::open(&profile.core_socket());
    let receiver = http_receiver();
    let thread = "turns-refusal-thread";
    let mut command = profile.connector(&["mcp"]);
    command
        .env("SIDEVOICE_THREAD", thread)
        .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
    let mut mcp = Mcp::start(command);
    mcp.initialize("turns-refusal-client");
    let joined = mcp.tool("voice_connect", json!({"input_mode":"pull"}));
    assert_eq!(joined["delivery"], "pull", "{joined}");
    core_json(
        &profile.core_socket(),
        "POST",
        "/api/presentation/select",
        Some(&json!({"session_id":call.session,"thread_id":thread})),
        Some(&call.token),
    );
    call.report_turn("started", "open", None);
    let started = call.turn_started("open");
    assert_eq!(started["epoch"], 1, "{started}");
    let args = json!({"text":"Reply during the turn","session_id":call.session,
        "revision":started["revision"],"epoch":started["epoch"],
        "event_id":"refused-event","utterance_id":"refused-utterance"});
    let refused = mcp.tool("voice_say", args.clone());
    assert_eq!(refused["status"], "rejected", "{refused}");
    assert_eq!(refused["reason"], "user_speaking", "{refused}");
    assert!(refused["instruction"]
        .as_str()
        .unwrap()
        .contains("Wait for their message"));
    let outbox: serde_json::Value =
        serde_json::from_slice(&std::fs::read(profile.data().join("outbox.json")).unwrap())
            .unwrap();
    assert_eq!(outbox, json!([]), "refusal left queued audio: {outbox}");
    call.report_turn("finished", "open", Some(""));
    let note = until("empty-turn note in the pull queue", 15, || {
        let page = mcp.tool("get_messages", json!({}));
        page["messages"].as_array().and_then(|messages| {
            messages
                .iter()
                .find(|message| message["notice"]["kind"] == "turn_empty")
                .cloned()
        })
    });
    assert_eq!(note["channel"], "note", "{note}");
    assert_eq!(note["text"], "", "{note}");
    assert!(note["envelope"]
        .as_str()
        .unwrap()
        .contains("person sent no words"));
    let after = mcp.tool("voice_say", args);
    assert_eq!(after["status"], "rejected", "{after}");
    assert_eq!(after["reason"], "user_speaking", "{after}");
    let history = core_json(
        &profile.core_socket(),
        "GET",
        &format!("/api/presentation/history?thread_id={thread}"),
        None,
        Some(&call.token),
    );
    assert!(
        history["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["role"] != "assistant"),
        "refused speech created a reply bubble: {history}"
    );
    mcp.stop();
}

#[test]
fn t04_t05_non_editor_can_speak_first_in_push_and_pull() {
    let Some(core) = core_dir() else { return };
    for mode in ["push", "pull"] {
        let profile = Profile::new(&format!("speak-first-{mode}"));
        let _core = profile.start_core(&core, &format!("speak-first-core-{mode}"));
        let _connector = profile.start_connector();
        profile.wait_linked(&format!("speak-first-core-{mode}"));
        let thread = format!("speak-first-thread-{mode}");
        let mut command = profile.connector(&["mcp"]);
        command.env("SIDEVOICE_THREAD", &thread);
        let mut mcp = Mcp::start(command);
        mcp.initialize("speak-first-client");
        let joined = mcp.tool("voice_connect", json!({"input_mode":mode}));
        let context = &joined["initial_reply_context"];
        assert_eq!(context["revision"], 0, "{joined}");
        assert_eq!(context["epoch"], 0, "{joined}");
        let said = mcp.tool(
            "voice_say",
            json!({"conversation":joined["conversation"],"text":format!("Hello in {mode}")}),
        );
        assert_eq!(said["text_saved"], true, "{mode}: {said}");
        mcp.stop();
    }
}

#[test]
fn t07_retry_identifiers_are_a_pair_and_one_pair_creates_one_reply() {
    let Some(core) = core_dir() else { return };
    let profile = Profile::new("retry-ids");
    let _core = profile.start_core(&core, "retry-ids-core");
    let _connector = profile.start_connector();
    profile.wait_linked("retry-ids-core");
    let call = Presentation::open(&profile.core_socket());
    let thread = "retry-ids-thread";
    let mut command = profile.connector(&["mcp"]);
    command.env("SIDEVOICE_THREAD", thread);
    let mut mcp = Mcp::start(command);
    mcp.initialize("retry-ids-client");
    let joined = mcp.tool("voice_connect", json!({"input_mode":"pull"}));
    let conversation = joined["conversation"].clone();
    for single in [
        json!({"event_id":"event-only"}),
        json!({"utterance_id":"utterance-only"}),
    ] {
        let mut args = json!({"conversation":conversation,"text":"One reply"});
        args.as_object_mut()
            .unwrap()
            .extend(single.as_object().unwrap().clone());
        let refused = mcp.request("tools/call", json!({"name":"voice_say","arguments":args}));
        assert_eq!(refused["isError"], true, "{refused}");
        assert!(refused["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("event_id and utterance_id together"));
    }
    let paired = json!({"conversation":conversation,"text":"One reply",
        "event_id":"paired-event","utterance_id":"paired-utterance"});
    let first = mcp.tool("voice_say", paired.clone());
    let again = mcp.tool("voice_say", paired);
    assert_eq!(first["text_saved"], true, "{first}");
    assert_eq!(again["text_saved"], true, "{again}");
    let history = core_json(
        &profile.core_socket(),
        "GET",
        &format!("/api/presentation/history?thread_id={thread}"),
        None,
        Some(&call.token),
    );
    let rows = history["messages"].as_array().unwrap();
    assert_eq!(
        rows.iter().filter(|row| row["role"] == "assistant").count(),
        1,
        "{history}"
    );
    mcp.stop();
}
