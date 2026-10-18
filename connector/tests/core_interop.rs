//! The connector against the real core (the sidevoice-core release pinned in core.pin), end to end, in a private profile:
//! the connector links to a core it did not start, over the core's own socket; a conversation joins through the
//! MCP server; what the person says in a call reaches the conversation; the conversation's reply reaches the core;
//! and when the core restarts the connector links to the new one by itself.

mod support;

use serde_json::json;
use serde_json::Value;
use std::time::Duration;
use support::*;

fn assistant_history_id(
    profile: &Profile,
    call: &Presentation,
    thread: &str,
    utterance_id: &str,
) -> String {
    let suffix = format!(":voice:{utterance_id}");
    until("assistant history row", 15, || {
        let history = core_json(
            &profile.core_socket(),
            "GET",
            &format!("/api/presentation/history?thread_id={thread}"),
            None,
            Some(&call.token),
        );
        history["messages"]
            .as_array()?
            .iter()
            .find_map(|row| row["id"].as_str().filter(|id| id.ends_with(&suffix)))
            .map(str::to_owned)
    })
}

fn confirm_hearing(
    profile: &Profile,
    call: &Presentation,
    thread: &str,
    utterance_id: &str,
    outcome: &str,
    heard_chars: Option<u64>,
) {
    let history_id = assistant_history_id(profile, call, thread, utterance_id);
    call.report_hearing(&history_id, utterance_id, outcome, heard_chars);
}

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
            "voice_get_reply",
            "voice_pair",
            "voice_pair_device",
            "voice_replay",
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
               "context_message_id":delivered["message_id"],
               "event_id":"interop-event","utterance_id":"interop-utterance",
               "text": "A reply from the conversation"}),
    );
    assert_eq!(said["status"], "accepted", "{said}");

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
    assert!(text.contains("no address for another device"), "{text}");
    assert!(text.contains("IP address and port"), "{text}");

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
    // Let core settle the acknowledgement while its MCP answer is never consumed by the caller.
    mcp.request_without_response(
        "tools/call",
        json!({"name":"get_messages",
        "arguments":{"ack_ids":[id]}}),
    );
    until("lost-answer acknowledgement settled", 15, || {
        let check = mcp.tool("get_messages", json!({"operation":"check"}));
        (check["count"] == 0).then_some(())
    });
    mcp.stop();
    let mut resumed_command = profile.connector(&["mcp"]);
    resumed_command
        .env("SIDEVOICE_THREAD", thread)
        .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
    let mut mcp = Mcp::start(resumed_command);
    mcp.initialize("turns-pull-after-lost-ack");
    mcp.tool("voice_connect", json!({"input_mode":"pull"}));
    let duplicate = mcp.tool("get_messages", json!({"ack_ids":[id]}));
    assert_eq!(duplicate["acknowledged"], 0, "{duplicate}");
    assert_eq!(duplicate["count"], 0, "{duplicate}");
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
fn t02c_t02d_open_turn_admission_is_pending_then_resumes_after_empty_turn() {
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
    let args = json!({"conversation":thread,"text":"Reply during the turn",
        "event_id":"refused-event","utterance_id":"refused-utterance"});
    let pending = mcp.tool("voice_say", args.clone());
    assert_eq!(pending["status"], "accepted", "{pending}");
    assert_eq!(pending["playback"], "pending", "{pending}");
    assert!(!call.saw_frame_within("voice-reply", Duration::from_millis(250)));
    let outbox: serde_json::Value =
        serde_json::from_slice(&std::fs::read(profile.data().join("outbox.json")).unwrap())
            .unwrap();
    assert_eq!(
        outbox,
        json!([]),
        "accepted admission did not settle outbox: {outbox}"
    );
    call.report_turn("finished", "open", Some(""));
    let resumed = call.next_frame("voice-reply", 15);
    assert_eq!(
        resumed["data"]["utterance_id"], "refused-utterance",
        "{resumed}"
    );
    let after = mcp.tool("voice_say", args);
    assert_eq!(after, pending, "same IDs must retain original admission");
    let history = core_json(
        &profile.core_socket(),
        "GET",
        &format!("/api/presentation/history?thread_id={thread}"),
        None,
        Some(&call.token),
    );
    assert_eq!(
        history["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["role"] == "assistant")
            .count(),
        1,
        "{history}"
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
        let receiver = http_receiver();
        let mut command = profile.connector(&["mcp"]);
        command
            .env("SIDEVOICE_THREAD", &thread)
            .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
        let mut mcp = Mcp::start(command);
        mcp.initialize("speak-first-client");
        let joined = mcp.tool("voice_connect", json!({"input_mode":mode}));
        let context = &joined["initial_reply_context"];
        assert_eq!(context["revision"], 0, "{joined}");
        assert_eq!(context["epoch"], 0, "{joined}");
        let said = mcp.tool(
            "voice_say",
            json!({"conversation":joined["conversation"],"text":format!("Hello in {mode}"),
                "event_id":format!("hello-{mode}-event"),
                "utterance_id":format!("hello-{mode}-utterance")}),
        );
        assert_eq!(said["status"], "accepted", "{mode}: {said}");
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
    let receiver = http_receiver();
    let mut command = profile.connector(&["mcp"]);
    command
        .env("SIDEVOICE_THREAD", thread)
        .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
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
            .contains("requires a nonempty stable"));
    }
    let paired = json!({"conversation":conversation,"text":"One reply",
        "event_id":"paired-event","utterance_id":"paired-utterance"});
    let first = mcp.tool("voice_say", paired.clone());
    let again = mcp.tool("voice_say", paired);
    assert_eq!(first["status"], "accepted", "{first}");
    assert_eq!(again["status"], "accepted", "{again}");
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

#[test]
fn t07_recovered_attempt_allows_a_new_context_to_play_with_real_core() {
    let Some(core) = core_dir() else { return };
    let profile = Profile::new("v1-same-core-replacement");
    let _core = profile.start_core(&core, "v1-same-core-replacement");
    let mut connector = profile.start_connector();
    profile.wait_linked("v1-same-core-replacement");
    let call = Presentation::open(&profile.core_socket());
    let receiver = http_receiver();
    let thread = "same-core-replacement-thread";
    let mut command = profile.connector(&["mcp"]);
    command
        .env("SIDEVOICE_THREAD", thread)
        .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
    let mut mcp = Mcp::start(command);
    mcp.initialize("replacement-first");
    let first_join = mcp.tool("voice_connect", json!({}));
    core_json(
        &profile.core_socket(),
        "POST",
        "/api/presentation/select",
        Some(&json!({"session_id":call.session,"thread_id":thread})),
        Some(&call.token),
    );
    let original = json!({"conversation":thread,"text":"First answer",
        "event_id":"recover-a-event","utterance_id":"recover-a-utterance"});
    let first = mcp.tool("voice_say", original.clone());
    assert_eq!(first["status"], "accepted", "{first}");
    assert_eq!(first["playback"], "queued", "{first}");
    assert_eq!(
        call.next_frame("voice-reply", 15)["data"]["utterance_id"],
        "recover-a-utterance"
    );
    assert_eq!(
        read_json(&profile.data().join("outbox.json")),
        Some(json!([]))
    );
    mcp.stop();
    connector.stop();

    let _replacement_connector = profile.start_connector();
    profile.wait_linked("v1-same-core-replacement");
    let mut command = profile.connector(&["mcp"]);
    command
        .env("SIDEVOICE_THREAD", thread)
        .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
    let mut mcp = Mcp::start(command);
    mcp.initialize("replacement-second");
    let replacement = mcp.tool("voice_connect", json!({}));
    assert_eq!(replacement["voice_contract"], true, "{replacement}");
    assert!(replacement["binding_id"]
        .as_str()
        .is_some_and(|id| !id.is_empty()));
    assert_eq!(first_join["conversation"], replacement["conversation"]);
    let recovered = mcp.tool("voice_say", original);
    assert_eq!(recovered["status"], "rejected", "{recovered}");
    assert_eq!(recovered["reason"], "identity_conflict", "{recovered}");
    assert!(!call.saw_frame_within("voice-reply", Duration::from_millis(250)));

    call.report_turn("started", "recover-next-turn", None);
    call.turn_started("recover-next-turn");
    call.report_turn("finished", "recover-next-turn", Some("A new question"));
    let words = until("new words after original recovery", 15, || {
        receiver
            .bodies
            .recv_timeout(Duration::from_millis(100))
            .ok()
            .filter(|message| message["text"] == "A new question")
    });
    let next = mcp.tool(
        "voice_say",
        json!({"conversation":thread,"text":"Fresh answer",
        "session_id":words["session_id"],"revision":words["revision"],
        "epoch":words["epoch"],"context_message_id":words["message_id"],
        "event_id":"recover-b-event","utterance_id":"recover-b-utterance"}),
    );
    assert_eq!(next["status"], "accepted", "{next}");
    assert_eq!(next["playback"], "queued", "{next}");
    mcp.stop();
}

#[test]
fn t16_large_valid_pull_page_arrives_in_complete_cursor_order() {
    let Some(core) = core_dir() else { return };
    let profile = Profile::new("pull-large-page");
    let _core = profile.start_core(&core, "pull-large-core");
    let _connector = profile.start_connector();
    profile.wait_linked("pull-large-core");
    let call = Presentation::open(&profile.core_socket());
    let thread = "pull-large-thread";
    let receiver = http_receiver();
    let mut command = profile.connector(&["mcp"]);
    command
        .env("SIDEVOICE_THREAD", thread)
        .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
    let mut mcp = Mcp::start(command);
    mcp.initialize("pull-large-client");
    mcp.tool("voice_connect", json!({"input_mode":"pull"}));
    let selected = core_json(
        &profile.core_socket(),
        "POST",
        "/api/presentation/select",
        Some(&json!({"session_id":call.session,"thread_id":thread})),
        Some(&call.token),
    );
    let words = "\"".repeat(12_000);
    let mut ids = Vec::new();
    for _ in 0..32 {
        let id = uuid::Uuid::new_v4().to_string();
        let posted = core_json(
            &profile.core_socket(),
            "POST",
            "/api/presentation/text",
            Some(
                &json!({"text":words,"session_id":call.session,"thread_id":thread,
                "binding_id":selected["binding"]["binding_id"],"message_id":id}),
            ),
            Some(&call.token),
        );
        assert_eq!(posted["accepted"], true, "{posted}");
        ids.push(id);
    }
    let check = mcp.tool("get_messages", json!({"operation":"check"}));
    assert_eq!(check["count"], 32, "{check}");
    let mut cursor = 0;
    let mut received = Vec::new();
    while received.len() < ids.len() {
        let page = mcp.tool("get_messages", json!({"after":cursor}));
        let messages = page["messages"].as_array().unwrap();
        assert!(!messages.is_empty(), "{page}");
        for message in messages {
            assert_eq!(message["text"], words);
            assert!(message["envelope"].as_str().unwrap().contains(&words));
            received.push(message["message_id"].as_str().unwrap().to_owned());
        }
        cursor = page["cursor"].as_u64().unwrap();
        assert_eq!(page["more"], received.len() < ids.len(), "{page}");
    }
    assert_eq!(received, ids);
    let ack = mcp.tool("get_messages", json!({"ack_ids":ids,"after":cursor}));
    assert_eq!(ack["acknowledged"], 32, "{ack}");
    assert_eq!(ack["count"], 0, "{ack}");
    mcp.stop();
}

fn actual_notice(
    core: &std::path::Path,
    mode: &str,
    label: &str,
    kind: &str,
    produce: impl FnOnce(&Profile, &Presentation, &mut Mcp, &Value),
) -> Value {
    let profile = Profile::new(&format!("notice-{label}-{mode}"));
    let launch = format!("notice-core-{label}-{mode}");
    let _core = profile.start_core(core, &launch);
    let _connector = profile.start_connector();
    profile.wait_linked(&launch);
    let call = Presentation::open(&profile.core_socket());
    let receiver = http_receiver();
    let thread = format!("notice-thread-{label}-{mode}");
    let mut command = profile.connector(&["mcp"]);
    command
        .env("SIDEVOICE_THREAD", &thread)
        .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
    let mut mcp = Mcp::start(command);
    mcp.initialize("notice-client");
    let joined = mcp.tool("voice_connect", json!({"input_mode":mode}));
    core_json(
        &profile.core_socket(),
        "POST",
        "/api/presentation/select",
        Some(&json!({"session_id":call.session,"thread_id":thread})),
        Some(&call.token),
    );
    produce(&profile, &call, &mut mcp, &joined);
    let matches_kind = |message: &Value| match kind {
        "reply_not_heard" => message["hearing_changes"]
            .as_array()
            .is_some_and(|changes| !changes.is_empty()),
        "user_returned" => {
            message["event"] == "user_returned" || message["notice"]["kind"] == "user_returned"
        }
        "turn_empty" => {
            message["notice"]["kind"] == "turn_empty" || message["event"] == "turn_empty"
        }
        _ => message["notice"]["kind"] == kind,
    };
    let message = if mode == "push" {
        until(&format!("{kind} injected"), 20, || {
            receiver
                .bodies
                .recv_timeout(Duration::from_millis(150))
                .ok()
                .filter(|message| matches_kind(message))
        })
    } else {
        until(&format!("{kind} pulled"), 20, || {
            let page = mcp.tool("get_messages", json!({}));
            page["messages"].as_array().and_then(|messages| {
                messages
                    .iter()
                    .find(|message| matches_kind(message))
                    .cloned()
            })
        })
    };
    let envelope = message["envelope"].as_str().unwrap();
    let (header, words) = if message["channel"] == "note" {
        (envelope, None)
    } else {
        let (header, words) = envelope.split_once("\n\n").unwrap();
        (header, Some(words))
    };
    let header: Value = serde_json::from_str(header).unwrap();
    assert_eq!(header["message_id"], message["message_id"], "{message}");
    assert_eq!(
        header["hearing_changes"], message["hearing_changes"],
        "{message}"
    );
    if let Some(words) = words {
        assert_eq!(words, message["text"].as_str().unwrap(), "{message}");
    }
    mcp.stop();
    message
}

#[test]
fn t01_t02a_t02b_real_notices_reach_push_and_pull_with_complete_evidence() {
    let Some(core) = core_dir() else { return };
    for mode in ["push", "pull"] {
        for (label, playback, words, kind) in [
            (
                "interrupted",
                "interrupted",
                Some("Wait"),
                "reply_not_heard",
            ),
            (
                "unstarted-words",
                "unplayed",
                Some("Actually"),
                "reply_not_heard",
            ),
        ] {
            let message = actual_notice(&core, mode, label, kind, |profile, call, mcp, joined| {
                let said = mcp.tool(
                    "voice_say",
                    json!({"conversation":joined["conversation"],
                    "text":"é🐙 original","event_id":"event-1","utterance_id":"utterance-1"}),
                );
                assert_eq!(said["status"], "accepted", "{said}");
                if playback == "interrupted" {
                    call.report_playback("utterance-1", "playing", None, None);
                    call.report_playback(
                        "utterance-1",
                        "interrupted",
                        Some("user_interrupted"),
                        Some(2),
                    );
                }
                confirm_hearing(
                    profile,
                    call,
                    joined["conversation"].as_str().unwrap(),
                    "utterance-1",
                    if playback == "interrupted" {
                        "partial"
                    } else {
                        "not_heard"
                    },
                    (playback == "interrupted").then_some(2),
                );
                call.report_turn("started", "turn-1", None);
                call.turn_started("turn-1");
                if playback == "unplayed" {
                    call.report_playback("utterance-1", "unplayed", Some("newer_turn"), None);
                }
                call.report_turn("finished", "turn-1", words);
            });
            let change = message["hearing_changes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|change| change["utterance_id"] == "utterance-1")
                .unwrap_or_else(|| panic!("missing correlated hearing change: {message}"));
            if playback == "interrupted" {
                assert_eq!(change["hearing"], "partial", "{message}");
                assert_eq!(change["heard_chars"], 2, "{message}");
            } else {
                assert_eq!(change["hearing"], "not_played", "{message}");
                assert_eq!(change["heard_chars"], 0, "{message}");
            }
            assert!(message["envelope"]
                .as_str()
                .unwrap()
                .contains("hearing_changes"));
            assert!(!message["envelope"]
                .as_str()
                .unwrap()
                .contains("é🐙 original"));
            if let Some(words) = words {
                assert_eq!(message["text"], words, "{message}");
            } else {
                assert_eq!(message["channel"], "note", "{message}");
                assert_eq!(message["text"], "", "{message}");
            }
        }
    }
}

#[test]
fn t12_local_stop_exposes_only_semantic_partial_hearing_in_push_and_pull() {
    let Some(core) = core_dir() else { return };
    for mode in ["push", "pull"] {
        let note = actual_notice(
            &core,
            mode,
            "local-stop",
            "reply_not_heard",
            |profile, call, mcp, joined| {
                let said = mcp.tool(
                    "voice_say",
                    json!({"conversation":joined["conversation"],
                "text":"Partially delivered","event_id":"stop-event","utterance_id":"stop-utterance"}),
                );
                assert_eq!(said["status"], "accepted", "{said}");
                call.report_playback("stop-utterance", "interrupted", None, Some(2));
                confirm_hearing(
                    profile,
                    call,
                    joined["conversation"].as_str().unwrap(),
                    "stop-utterance",
                    "partial",
                    Some(2),
                );
                call.report_turn("started", "after-stop", None);
                call.turn_started("after-stop");
                call.report_turn("finished", "after-stop", Some("Continue"));
            },
        );
        let change = note["hearing_changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|change| change["utterance_id"] == "stop-utterance")
            .unwrap_or_else(|| panic!("missing correlated partial hearing: {note}"));
        assert_eq!(change["hearing"], "partial", "{note}");
        assert_eq!(change["heard_chars"], 2, "{note}");
        assert!(change["attempt_id"].is_string(), "{note}");
        assert!(!note["envelope"]
            .as_str()
            .unwrap()
            .contains("Partially delivered"));
    }
}

#[test]
fn t03_stale_initial_context_is_refused_after_new_words_in_push_and_pull() {
    let Some(core) = core_dir() else { return };
    for mode in ["push", "pull"] {
        let label = format!("stale-context-{mode}");
        let profile = Profile::new(&label);
        let _core = profile.start_core(&core, &label);
        let _connector = profile.start_connector();
        profile.wait_linked(&label);
        let call = Presentation::open(&profile.core_socket());
        let receiver = http_receiver();
        let thread = format!("thread-{label}");
        let mut command = profile.connector(&["mcp"]);
        command
            .env("SIDEVOICE_THREAD", &thread)
            .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
        let mut mcp = Mcp::start(command);
        mcp.initialize("stale-context-client");
        let joined = mcp.tool("voice_connect", json!({"input_mode":mode}));
        core_json(
            &profile.core_socket(),
            "POST",
            "/api/presentation/select",
            Some(&json!({"session_id":call.session,"thread_id":thread})),
            Some(&call.token),
        );
        call.report_turn("started", "new-turn", None);
        call.turn_started("new-turn");
        call.report_turn("finished", "new-turn", Some("New question"));
        let words = if mode == "push" {
            until("new words injected", 15, || {
                receiver
                    .bodies
                    .recv_timeout(Duration::from_millis(100))
                    .ok()
                    .filter(|row| row["text"] == "New question")
            })
        } else {
            until("new words pulled", 15, || {
                let page = mcp.tool("get_messages", json!({}));
                page["messages"].as_array().and_then(|rows| {
                    rows.iter()
                        .find(|row| row["text"] == "New question")
                        .cloned()
                })
            })
        };
        assert_eq!(words["origin"], "spoken_room", "{words}");
        let initial = &joined["initial_reply_context"];
        let refused = mcp.request(
            "tools/call",
            json!({"name":"voice_say","arguments":{"conversation":thread,
                "text":"Late old answer","session_id":initial["session_id"],
                "revision":initial["revision"],"epoch":initial["epoch"],
                "event_id":"stale-event","utterance_id":"stale-utterance"}}),
        );
        assert_eq!(refused["isError"], false, "{refused}");
        let disposition: Value =
            serde_json::from_str(refused["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(disposition["status"], "rejected", "{disposition}");
        assert_eq!(disposition["reason"], "context_stale", "{disposition}");
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
            "{history}"
        );
        mcp.stop();
    }
}

#[test]
fn text_only_written_hearing_is_confirmed_without_an_audio_target_in_both_modes() {
    let Some(core) = core_dir() else { return };
    for mode in ["push", "pull"] {
        let label = format!("written-hearing-{mode}");
        let profile = Profile::new(&label);
        let _core = profile.start_core(&core, &label);
        let _connector = profile.start_connector();
        profile.wait_linked(&label);
        let call = Presentation::open(&profile.core_socket());
        let receiver = http_receiver();
        let thread = format!("thread-{label}");
        let mut command = profile.connector(&["mcp"]);
        command
            .env("SIDEVOICE_THREAD", &thread)
            .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
        let mut mcp = Mcp::start(command);
        mcp.initialize("written-hearing-client");
        let joined = mcp.tool("voice_connect", json!({"input_mode":mode}));
        core_json(
            &profile.core_socket(),
            "POST",
            "/api/presentation/select",
            Some(&json!({"session_id":call.session,"thread_id":thread})),
            Some(&call.token),
        );
        call.report_speech(false, Some("voice_off"));
        let said = mcp.tool(
            "voice_say",
            json!({"conversation":joined["conversation"],
            "text":"Read in text","event_id":"written-event","utterance_id":"written-utterance"}),
        );
        assert_eq!(said["playback"], "text_only", "{said}");
        assert!(said.get("reason").is_none(), "{said}");
        confirm_hearing(&profile, &call, &thread, "written-utterance", "heard", None);
        let history_id = assistant_history_id(&profile, &call, &thread, "written-utterance");
        let history = core_json(
            &profile.core_socket(),
            "GET",
            &format!("/api/presentation/history?thread_id={thread}"),
            None,
            Some(&call.token),
        );
        let row = history["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == history_id)
            .unwrap();
        assert_eq!(row["hearing"], "heard", "{row}");
        assert_eq!(row["hearing_unconfirmed"], false, "{row}");
        call.report_speech(true, None);
        call.report_turn("started", "after-reading", None);
        call.turn_started("after-reading");
        call.report_turn("finished", "after-reading", Some("Next topic"));
        let message = if mode == "push" {
            until("written receipt next words injected", 15, || {
                receiver
                    .bodies
                    .recv_timeout(Duration::from_millis(100))
                    .ok()
                    .filter(|message| message["text"] == "Next topic")
            })
        } else {
            until("written receipt next words pulled", 15, || {
                let page = mcp.tool("get_messages", json!({}));
                page["messages"]
                    .as_array()?
                    .iter()
                    .find(|message| message["text"] == "Next topic")
                    .cloned()
            })
        };
        assert_eq!(message["text"], "Next topic", "{message}");
        assert!(message["unheard"].is_null(), "{message}");
        mcp.stop();
    }
}

#[test]
fn t05_checkpoint_interruption_keeps_only_unheard_stage_in_both_modes() {
    let Some(core) = core_dir() else { return };
    for mode in ["push", "pull"] {
        let message = actual_notice(
            &core,
            mode,
            "checkpoint",
            "reply_not_heard",
            |profile, call, mcp, joined| {
                for (event, stage) in [
                    ("ack-event", "ack"),
                    ("checkpoint-event", "checkpoint"),
                    ("result-event", "result"),
                ] {
                    let said = mcp.tool(
                        "voice_say",
                        json!({"conversation":joined["conversation"],
                    "text":stage,"event_id":event,"utterance_id":stage}),
                    );
                    assert_eq!(said["status"], "accepted", "{said}");
                }
                call.report_playback("ack", "heard", None, None);
                confirm_hearing(
                    profile,
                    call,
                    joined["conversation"].as_str().unwrap(),
                    "ack",
                    "heard",
                    None,
                );
                call.report_playback("checkpoint", "playing", None, None);
                call.report_playback(
                    "checkpoint",
                    "interrupted",
                    Some("user_interrupted"),
                    Some(4),
                );
                confirm_hearing(
                    profile,
                    call,
                    joined["conversation"].as_str().unwrap(),
                    "checkpoint",
                    "partial",
                    Some(4),
                );
                call.report_turn("started", "next-turn", None);
                call.turn_started("next-turn");
                call.report_turn("finished", "next-turn", Some("Stop there"));
            },
        );
        let changes = message["hearing_changes"].as_array().unwrap();
        assert!(
            changes.iter().any(|change| {
                change["utterance_id"] == "ack" && change["hearing"] == "complete"
            }),
            "{message}"
        );
        assert!(
            changes.iter().any(|change| {
                change["utterance_id"] == "checkpoint"
                    && change["hearing"] == "partial"
                    && change["heard_chars"] == 4
            }),
            "{message}"
        );
        assert!(
            changes
                .iter()
                .all(|change| change["utterance_id"] != "result"),
            "{message}"
        );
        assert_eq!(message["text"], "Stop there", "{message}");
    }
}

#[test]
fn t06d_real_return_notice_is_delivered_once_in_push_and_pull() {
    let Some(core) = core_dir() else { return };
    for mode in ["push", "pull"] {
        let notice = actual_notice(
            &core,
            mode,
            "return",
            "user_returned",
            |profile, call, mcp, joined| {
                call.report_speech(false, Some("voice_off"));
                let said = mcp.tool("voice_say", json!({"conversation":joined["conversation"],
                "text":"Missed update","event_id":"return-event","utterance_id":"return-utterance"}));
                assert_eq!(said["status"], "accepted", "{said}");
                confirm_hearing(
                    profile,
                    call,
                    joined["conversation"].as_str().unwrap(),
                    "return-utterance",
                    "not_heard",
                    None,
                );
                call.report_speech(true, None);
            },
        );
        assert_eq!(notice["channel"], "note", "{notice}");
        assert!(
            notice["hearing_changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| {
                    change["utterance_id"] == "return-utterance"
                        && change["hearing"] == "not_played"
                        && change["heard_chars"] == 0
                }),
            "{notice}"
        );
        assert!(!notice["envelope"]
            .as_str()
            .unwrap()
            .contains("Missed update"));
    }
}

#[test]
fn t02c_t02d_pending_admission_keeps_words_or_empty_input_in_both_modes() {
    let Some(core) = core_dir() else { return };
    for mode in ["push", "pull"] {
        for (suffix, words) in [("words", Some("Person's words")), ("empty", None)] {
            let label = format!("refusal-{mode}-{suffix}");
            let profile = Profile::new(&label);
            let _core = profile.start_core(&core, &label);
            let _connector = profile.start_connector();
            profile.wait_linked(&label);
            let call = Presentation::open(&profile.core_socket());
            let receiver = http_receiver();
            let thread = format!("thread-{label}");
            let mut command = profile.connector(&["mcp"]);
            command
                .env("SIDEVOICE_THREAD", &thread)
                .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
            let mut mcp = Mcp::start(command);
            mcp.initialize("refusal-client");
            mcp.tool("voice_connect", json!({"input_mode":mode}));
            core_json(
                &profile.core_socket(),
                "POST",
                "/api/presentation/select",
                Some(&json!({"session_id":call.session,"thread_id":thread})),
                Some(&call.token),
            );
            call.report_turn("started", "open", None);
            let _started = call.turn_started("open");
            let pending = mcp.tool(
                "voice_say",
                json!({"conversation":thread,"text":"During open turn",
                "event_id":"refused-event","utterance_id":"refused-utterance"}),
            );
            assert_eq!(pending["status"], "accepted", "{pending}");
            assert_eq!(pending["playback"], "pending", "{pending}");
            assert!(!call.saw_frame_within("voice-reply", Duration::from_millis(250)));
            call.report_turn("finished", "open", words);
            if let Some(words) = words {
                let message = if mode == "push" {
                    until("spoken words injected", 15, || {
                        receiver
                            .bodies
                            .recv_timeout(Duration::from_millis(100))
                            .ok()
                            .filter(|item| item["text"] == words)
                    })
                } else {
                    until("spoken words pulled", 15, || {
                        let page = mcp.tool("get_messages", json!({}));
                        page["messages"].as_array().and_then(|items| {
                            items.iter().find(|item| item["text"] == words).cloned()
                        })
                    })
                };
                assert_eq!(message["text"], words, "{message}");
                assert_eq!(message["channel"], "voice", "{message}");
            } else {
                let resumed = call.next_frame("voice-reply", 15);
                assert_eq!(
                    resumed["data"]["utterance_id"], "refused-utterance",
                    "{resumed}"
                );
            }
            let history = core_json(
                &profile.core_socket(),
                "GET",
                &format!("/api/presentation/history?thread_id={thread}"),
                None,
                Some(&call.token),
            );
            assert_eq!(
                history["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|row| row["role"] == "assistant")
                    .count(),
                1,
                "{history}"
            );
            mcp.stop();
        }
    }
}

#[test]
fn t04a_multiple_publications_keep_core_playback_fifo_in_both_modes() {
    let Some(core) = core_dir() else { return };
    for mode in ["push", "pull"] {
        let profile = Profile::new(&format!("fifo-{mode}"));
        let launch = format!("fifo-core-{mode}");
        let _core = profile.start_core(&core, &launch);
        let _connector = profile.start_connector();
        profile.wait_linked(&launch);
        let call = Presentation::open(&profile.core_socket());
        let receiver = http_receiver();
        let thread = format!("fifo-thread-{mode}");
        let mut command = profile.connector(&["mcp"]);
        command
            .env("SIDEVOICE_THREAD", &thread)
            .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
        let mut mcp = Mcp::start(command);
        mcp.initialize("fifo-client");
        let joined = mcp.tool("voice_connect", json!({"input_mode":mode}));
        core_json(
            &profile.core_socket(),
            "POST",
            "/api/presentation/select",
            Some(&json!({"session_id":call.session,"thread_id":thread})),
            Some(&call.token),
        );
        for (n, stage) in ["first", "second", "third"].iter().enumerate() {
            let said = mcp.tool(
                "voice_say",
                json!({"conversation":joined["conversation"],
                "text":stage,"event_id":format!("fifo-event-{n}"),"utterance_id":stage}),
            );
            assert_eq!(said["status"], "accepted", "{said}");
        }
        let played: Vec<_> = (0..3)
            .map(|_| {
                call.next_frame("voice-reply", 15)["data"]["utterance_id"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(played, ["first", "second", "third"]);
        for id in played {
            call.report_playback(&id, "heard", None, None);
            confirm_hearing(&profile, &call, &thread, &id, "heard", None);
        }
        mcp.stop();
    }
}

#[test]
fn t04b_eight_withdrawn_replies_retain_evidence_and_ninth_is_capacity_full() {
    let Some(core) = core_dir() else { return };
    for mode in ["push", "pull"] {
        let profile = Profile::new(&format!("batches-{mode}"));
        let launch = format!("batches-core-{mode}");
        let _core = profile.start_core(&core, &launch);
        let _connector = profile.start_connector();
        profile.wait_linked(&launch);
        let call = Presentation::open(&profile.core_socket());
        let receiver = http_receiver();
        let thread = format!("batches-thread-{mode}");
        let mut command = profile.connector(&["mcp"]);
        command
            .env("SIDEVOICE_THREAD", &thread)
            .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
        let mut mcp = Mcp::start(command);
        mcp.initialize("batches-client");
        let joined = mcp.tool("voice_connect", json!({"input_mode":mode}));
        core_json(
            &profile.core_socket(),
            "POST",
            "/api/presentation/select",
            Some(&json!({"session_id":call.session,"thread_id":thread})),
            Some(&call.token),
        );
        for n in 0..8 {
            let said = mcp.tool(
                "voice_say",
                json!({"conversation":joined["conversation"],
                "text":format!("Reply {n}"),"event_id":format!("batch-event-{n}"),
                "utterance_id":format!("batch-utterance-{n}")}),
            );
            assert_eq!(said["status"], "accepted", "{said}");
        }
        let ninth = mcp.tool(
            "voice_say",
            json!({"conversation":joined["conversation"],
            "text":"Reply 8","event_id":"batch-event-8","utterance_id":"batch-utterance-8"}),
        );
        assert_eq!(ninth["status"], "rejected", "{ninth}");
        assert_eq!(ninth["reason"], "capacity_full", "{ninth}");
        for _ in 0..8 {
            assert_eq!(call.next_frame("voice-reply", 15)["type"], "voice-reply");
        }
        call.report_turn("started", "turn", None);
        call.turn_started("turn");
        for n in 0..8 {
            call.report_playback(
                &format!("batch-utterance-{n}"),
                "unplayed",
                Some("newer_turn"),
                None,
            );
            confirm_hearing(
                &profile,
                &call,
                &thread,
                &format!("batch-utterance-{n}"),
                "not_heard",
                None,
            );
        }
        call.report_turn("finished", "turn", Some("New request"));
        let mut evidence = std::collections::BTreeMap::new();
        let mut seen = std::collections::HashSet::new();
        let mut after = 0;
        until("all eight hearing changes in real delivery", 20, || {
            let messages = if mode == "push" {
                receiver
                    .bodies
                    .recv_timeout(Duration::from_millis(100))
                    .ok()
                    .into_iter()
                    .collect::<Vec<_>>()
            } else {
                let page = mcp.tool("get_messages", json!({"after":after}));
                after = page["cursor"].as_u64().unwrap_or(after);
                page["messages"].as_array().cloned().unwrap_or_default()
            };
            for message in messages {
                if !seen.insert(message["message_id"].as_str().unwrap().to_owned()) {
                    continue;
                }
                let changes = message["hearing_changes"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if !changes.is_empty() {
                    assert!(changes.len() <= 8, "{message}");
                    for change in changes {
                        let id = change["utterance_id"].as_str().unwrap().to_owned();
                        assert_eq!(change["hearing"], "not_played", "{message}");
                        assert_eq!(change["heard_chars"], 0, "{message}");
                        evidence.insert(id, change);
                    }
                }
            }
            (evidence.len() >= 8).then_some(())
        });
        assert_eq!(
            evidence.keys().cloned().collect::<Vec<_>>(),
            (0..8)
                .map(|n| format!("batch-utterance-{n}"))
                .collect::<Vec<_>>()
        );
        mcp.stop();
    }
}

#[test]
fn t06e_no_return_without_a_miss_but_per_listener_miss_is_reported() {
    let Some(core) = core_dir() else { return };
    for mode in ["push", "pull"] {
        let profile = Profile::new(&format!("no-return-{mode}"));
        let launch = format!("no-return-core-{mode}");
        let _core = profile.start_core(&core, &launch);
        let _connector = profile.start_connector();
        profile.wait_linked(&launch);
        let call = Presentation::open(&profile.core_socket());
        let receiver = http_receiver();
        let thread = format!("no-return-thread-{mode}");
        let mut command = profile.connector(&["mcp"]);
        command
            .env("SIDEVOICE_THREAD", &thread)
            .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
        let mut mcp = Mcp::start(command);
        mcp.initialize("no-return-client");
        let joined = mcp.tool("voice_connect", json!({"input_mode":mode}));
        core_json(
            &profile.core_socket(),
            "POST",
            "/api/presentation/select",
            Some(&json!({"session_id":call.session,"thread_id":thread})),
            Some(&call.token),
        );
        call.report_speech(false, Some("voice_off"));
        call.report_speech(true, None);
        std::thread::sleep(Duration::from_secs(6));
        if mode == "pull" {
            assert_eq!(
                mcp.tool("get_messages", json!({"operation":"check"}))["count"],
                0
            );
        } else {
            assert!(receiver
                .bodies
                .recv_timeout(Duration::from_millis(100))
                .is_err());
        }
        let other = Presentation::open_second(&profile);
        core_json(
            &profile.core_socket(),
            "POST",
            "/api/presentation/select",
            Some(&json!({"session_id":other.session,"thread_id":thread})),
            Some(&other.token),
        );
        let said = mcp.tool(
            "voice_say",
            json!({"conversation":joined["conversation"],
            "text":"Heard elsewhere","event_id":"heard-event","utterance_id":"heard-utterance"}),
        );
        assert_eq!(said["status"], "accepted", "{said}");
        other.report_playback("heard-utterance", "heard", None, None);
        confirm_hearing(&profile, &other, &thread, "heard-utterance", "heard", None);
        std::thread::sleep(Duration::from_millis(150));
        call.report_speech(false, Some("voice_off"));
        call.report_speech(true, None);
        let returned = if mode == "pull" {
            until("per-listener return note pulled", 15, || {
                let page = mcp.tool("get_messages", json!({}));
                page["messages"].as_array().and_then(|messages| {
                    messages
                        .iter()
                        .find(|row| row["event"] == "user_returned")
                        .cloned()
                })
            })
        } else {
            until("per-listener return note pushed", 15, || {
                receiver
                    .bodies
                    .recv_timeout(Duration::from_millis(100))
                    .ok()
                    .filter(|row| row["event"] == "user_returned")
            })
        };
        assert_eq!(returned["channel"], "note", "{returned}");
        assert!(
            returned["hearing_changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|fact| fact["utterance_id"] == "heard-utterance"
                    && fact["hearing"] != "complete"),
            "{returned}"
        );
        mcp.stop();
    }
}

#[test]
fn t14_real_two_device_corrections_keep_the_original_notice_frozen() {
    let Some(core) = core_dir() else { return };
    for mode in ["push", "pull"] {
        for (fate, status, reason) in [
            ("partial", "interrupted", Some("user_interrupted")),
            ("heard", "heard", None),
        ] {
            let label = format!("correction-{mode}-{fate}");
            let profile = Profile::new(&label);
            let _core = profile.start_core(&core, &label);
            let _connector = profile.start_connector();
            profile.wait_linked(&label);
            let call = Presentation::open(&profile.core_socket());
            let other = Presentation::open_second(&profile);
            let receiver = http_receiver();
            let thread = format!("thread-{label}");
            let mut command = profile.connector(&["mcp"]);
            command
                .env("SIDEVOICE_THREAD", &thread)
                .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
            let mut mcp = Mcp::start(command);
            mcp.initialize("correction-client");
            let joined = mcp.tool("voice_connect", json!({"input_mode":mode}));
            for device in [&call, &other] {
                core_json(
                    &profile.core_socket(),
                    "POST",
                    "/api/presentation/select",
                    Some(&json!({"session_id":device.session,"thread_id":thread})),
                    Some(&device.token),
                );
            }
            let said = mcp.tool("voice_say", json!({"conversation":joined["conversation"],
                "text":"Two-device reply","event_id":"correction-event","utterance_id":"correction-utterance"}));
            assert_eq!(said["status"], "accepted", "{said}");
            call.report_playback("correction-utterance", "playing", None, None);
            call.report_playback(
                "correction-utterance",
                "interrupted",
                Some("user_interrupted"),
                Some(2),
            );
            other.report_playback(
                "correction-utterance",
                "interrupted",
                Some("user_interrupted"),
                Some(1),
            );
            let history_id = assistant_history_id(&profile, &call, &thread, "correction-utterance");
            call.report_hearing(&history_id, "correction-utterance", "partial", Some(2));
            other.report_hearing(&history_id, "correction-utterance", "partial", Some(1));
            call.report_turn("started", "new-turn", None);
            call.turn_started("new-turn");
            call.report_turn("finished", "new-turn", Some("What happened?"));
            let initial = if mode == "push" {
                until("initial words with partial hearing injected", 15, || {
                    receiver
                        .bodies
                        .recv_timeout(Duration::from_millis(100))
                        .ok()
                        .filter(|message| message["text"] == "What happened?")
                })
            } else {
                until("initial words with partial hearing pulled", 15, || {
                    let page = mcp.tool("get_messages", json!({}));
                    page["messages"].as_array().and_then(|items| {
                        items
                            .iter()
                            .find(|message| message["text"] == "What happened?")
                            .cloned()
                    })
                })
            };
            assert!(
                initial["hearing_changes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|change| {
                        change["utterance_id"] == "correction-utterance"
                            && change["hearing"] == "partial"
                    }),
                "{initial}"
            );
            let frozen = initial["envelope"].clone();
            other.report_playback(
                "correction-utterance",
                status,
                reason,
                (fate == "partial").then_some(3),
            );
            other.report_hearing(
                &history_id,
                "correction-utterance",
                fate,
                (fate == "partial").then_some(3),
            );
            call.report_turn("started", "correction-next-turn", None);
            call.turn_started("correction-next-turn");
            call.report_turn("finished", "correction-next-turn", Some("Another update"));
            let correction = if mode == "push" {
                until("later words with correction injected", 15, || {
                    receiver
                        .bodies
                        .recv_timeout(Duration::from_millis(100))
                        .ok()
                        .filter(|message| message["text"] == "Another update")
                })
            } else {
                until("later words with correction pulled", 15, || {
                    let page = mcp.tool("get_messages", json!({}));
                    page["messages"].as_array().and_then(|items| {
                        items
                            .iter()
                            .find(|message| message["text"] == "Another update")
                            .cloned()
                    })
                })
            };
            assert_ne!(
                correction["message_id"], initial["message_id"],
                "{correction}"
            );
            assert!(
                correction["hearing_changes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|change| {
                        change["utterance_id"] == "correction-utterance"
                            && change["hearing"]
                                == if fate == "heard" {
                                    "complete"
                                } else {
                                    "partial"
                                }
                            && change["heard_chars"] == if fate == "partial" { 3 } else { 16 }
                    }),
                "{correction}"
            );
            if mode == "pull" {
                let again = mcp.tool("get_messages", json!({}));
                let original = again["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|item| item["message_id"] == initial["message_id"])
                    .unwrap();
                assert_eq!(original["envelope"], frozen);
            }
            mcp.stop();
        }
    }
}

#[test]
fn t16_failed_injection_and_pull_expose_identical_real_notice_fields() {
    let Some(core) = core_dir() else { return };
    let profile = Profile::new("notice-parity");
    let _core = profile.start_core(&core, "notice-parity-core");
    let _connector = profile.start_connector();
    profile.wait_linked("notice-parity-core");
    let call = Presentation::open(&profile.core_socket());
    let other = Presentation::open_second(&profile);
    let receiver = http_receiver_with_status(503);
    let thread = "notice-parity-thread";
    let mut command = profile.connector(&["mcp"]);
    command
        .env("SIDEVOICE_THREAD", thread)
        .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
    let mut mcp = Mcp::start(command);
    mcp.initialize("notice-parity-client");
    let joined = mcp.tool("voice_connect", json!({"input_mode":"push"}));
    core_json(
        &profile.core_socket(),
        "POST",
        "/api/presentation/select",
        Some(&json!({"session_id":call.session,"thread_id":thread})),
        Some(&call.token),
    );
    core_json(
        &profile.core_socket(),
        "POST",
        "/api/presentation/select",
        Some(&json!({"session_id":other.session,"thread_id":thread})),
        Some(&other.token),
    );
    let said = mcp.tool(
        "voice_say",
        json!({"conversation":joined["conversation"],
        "text":"Full parity reply","event_id":"parity-event","utterance_id":"parity-utterance"}),
    );
    assert_eq!(said["status"], "accepted", "{said}");
    call.report_playback("parity-utterance", "playing", None, None);
    call.report_playback(
        "parity-utterance",
        "interrupted",
        Some("user_interrupted"),
        Some(4),
    );
    other.report_playback(
        "parity-utterance",
        "interrupted",
        Some("user_interrupted"),
        Some(1),
    );
    let history_id = assistant_history_id(&profile, &call, thread, "parity-utterance");
    call.report_hearing(&history_id, "parity-utterance", "partial", Some(4));
    other.report_hearing(&history_id, "parity-utterance", "partial", Some(1));
    call.report_turn("started", "parity-turn", None);
    call.turn_started("parity-turn");
    call.report_turn("finished", "parity-turn", Some("New words"));
    let injected = until("failed injected words", 15, || {
        receiver
            .bodies
            .recv_timeout(Duration::from_millis(100))
            .ok()
            .filter(|message| message["text"] == "New words")
    });
    let switched = mcp.tool("voice_connect", json!({"input_mode":"pull"}));
    assert_eq!(switched["delivery"], "pull", "{switched}");
    let pulled = until("same notice in pull", 15, || {
        let page = mcp.tool("get_messages", json!({}));
        page["messages"].as_array().and_then(|items| {
            items
                .iter()
                .find(|item| item["message_id"] == injected["message_id"])
                .cloned()
        })
    });
    // Push adds thread_id and pull adds cursor/arrival_time. Every shared agent fact must
    // exist on both routes and match exactly, including the issued epoch and hearing evidence.
    assert_eq!(injected["thread_id"], thread, "{injected}");
    for field in [
        "message_id",
        "session_id",
        "revision",
        "epoch",
        "channel",
        "origin",
        "text",
        "hearing_changes",
        "playback_outcomes",
        "playback_at_turn_start",
        "not_played_at_turn_start",
        "not_played_at_turn_start_truncated",
        "envelope",
    ] {
        assert!(injected.get(field).is_some(), "push {field}: {injected}");
        assert!(pulled.get(field).is_some(), "pull {field}: {pulled}");
        assert_eq!(
            injected[field], pulled[field],
            "{field}: injected={injected}, pulled={pulled}"
        );
    }
    assert_eq!(injected["epoch"], 1, "{injected}");
    assert!(
        pulled["hearing_changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| {
                change["utterance_id"] == "parity-utterance"
                    && change["hearing"] == "partial"
                    && change["heard_chars"] == 4
            }),
        "{pulled}"
    );
    assert!(!pulled["envelope"]
        .as_str()
        .unwrap()
        .contains("Full parity reply"));
    mcp.stop();
    let mut command = profile.connector(&["mcp"]);
    command
        .env("SIDEVOICE_THREAD", thread)
        .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
    let mut mcp = Mcp::start(command);
    mcp.initialize("notice-parity-after-claim-release");
    mcp.tool("voice_connect", json!({"input_mode":"pull"}));
    other.report_playback(
        "parity-utterance",
        "interrupted",
        Some("user_interrupted"),
        Some(6),
    );
    other.report_hearing(&history_id, "parity-utterance", "partial", Some(6));
    call.report_turn("started", "parity-later-turn", None);
    call.turn_started("parity-later-turn");
    call.report_turn("finished", "parity-later-turn", Some("Later words"));
    let (original, correction) = until("frozen notice and late partial correction", 15, || {
        let page = mcp.tool("get_messages", json!({}));
        let messages = page["messages"].as_array()?;
        let original = messages
            .iter()
            .find(|item| item["message_id"] == pulled["message_id"])?
            .clone();
        let correction = messages
            .iter()
            .find(|item| item["text"] == "Later words")?
            .clone();
        Some((original, correction))
    });
    assert_eq!(original["envelope"], pulled["envelope"]);
    assert_eq!(original["hearing_changes"], pulled["hearing_changes"]);
    assert_ne!(correction["message_id"], pulled["message_id"]);
    assert!(
        correction["hearing_changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| {
                change["utterance_id"] == "parity-utterance"
                    && change["hearing"] == "partial"
                    && change["heard_chars"] == 6
            }),
        "{correction}"
    );
    let acknowledged = mcp.tool(
        "get_messages",
        json!({"ack_ids":[pulled["message_id"], correction["message_id"]]}),
    );
    assert_eq!(acknowledged["acknowledged"], 2, "{acknowledged}");
    mcp.stop();
}

#[test]
fn t02c_t02d_backlogged_other_conversation_cannot_hide_pending_admission() {
    use std::os::unix::fs::PermissionsExt;
    let Some(core) = core_dir() else { return };
    for (suffix, words) in [("words", Some("New request")), ("empty", None)] {
        let label = format!("backlog-{suffix}");
        let profile = Profile::new(&label);
        let _core = profile.start_core(&core, &label);
        let held = json!([{"client_ref":"older-conversation","binding_id":"old-binding",
            "event_id":"old-event","utterance_id":"old-utterance","session_id":"old-session",
            "revision":0,"epoch":0,"text":"uncertain earlier work","history_only":false,"attempted":true}]);
        let outbox_path = profile.data().join("outbox.json");
        std::fs::write(&outbox_path, held.to_string()).unwrap();
        std::fs::set_permissions(&outbox_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let _connector = profile.start_connector();
        profile.wait_linked(&label);
        let call = Presentation::open(&profile.core_socket());
        let receiver = http_receiver();
        let thread = format!("thread-{label}");
        let mut command = profile.connector(&["mcp"]);
        command
            .env("SIDEVOICE_THREAD", &thread)
            .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
        let mut mcp = Mcp::start(command);
        mcp.initialize("backlog-client");
        mcp.tool("voice_connect", json!({"input_mode":"pull"}));
        core_json(
            &profile.core_socket(),
            "POST",
            "/api/presentation/select",
            Some(&json!({"session_id":call.session,"thread_id":thread})),
            Some(&call.token),
        );
        call.report_turn("started", "open", None);
        let _started = call.turn_started("open");
        let pending = mcp.tool(
            "voice_say",
            json!({"conversation":thread,"text":"Pending now",
            "event_id":"new-event","utterance_id":"new-utterance"}),
        );
        assert_eq!(pending["status"], "accepted", "{pending}");
        assert_eq!(pending["playback"], "pending", "{pending}");
        call.report_turn("finished", "open", words);
        if let Some(words) = words {
            let message = until("backlogged current words", 15, || {
                let page = mcp.tool("get_messages", json!({}));
                page["messages"]
                    .as_array()
                    .and_then(|items| items.iter().find(|item| item["text"] == words).cloned())
            });
            assert_eq!(message["text"], words, "{message}");
        } else {
            let resumed = call.next_frame("voice-reply", 15);
            assert_eq!(
                resumed["data"]["utterance_id"], "new-utterance",
                "{resumed}"
            );
        }
        let retained: Value =
            serde_json::from_slice(&std::fs::read(&outbox_path).unwrap()).unwrap();
        assert_eq!(retained.as_array().unwrap().len(), 1, "{retained}");
        assert_eq!(retained[0]["event_id"], "old-event", "{retained}");
        mcp.stop();
    }
}

#[test]
fn t02a_late_receipt_follows_real_words_with_related_message_id_in_both_modes() {
    let Some(core) = core_dir() else { return };
    for mode in ["push", "pull"] {
        let profile = Profile::new(&format!("late-followup-{mode}"));
        let launch = format!("late-followup-core-{mode}");
        let _core = profile.start_core(&core, &launch);
        let _connector = profile.start_connector();
        profile.wait_linked(&launch);
        let call = Presentation::open(&profile.core_socket());
        let receiver = http_receiver();
        let thread = format!("late-followup-thread-{mode}");
        let mut command = profile.connector(&["mcp"]);
        command
            .env("SIDEVOICE_THREAD", &thread)
            .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
        let mut mcp = Mcp::start(command);
        mcp.initialize("late-followup-client");
        let joined = mcp.tool("voice_connect", json!({"input_mode":mode}));
        core_json(
            &profile.core_socket(),
            "POST",
            "/api/presentation/select",
            Some(&json!({"session_id":call.session,"thread_id":thread})),
            Some(&call.token),
        );
        let said = mcp.tool(
            "voice_say",
            json!({"conversation":joined["conversation"],
            "text":"Late receipt reply","event_id":"late-event","utterance_id":"late-utterance"}),
        );
        assert_eq!(said["status"], "accepted", "{said}");
        call.report_turn("started", "new-turn", None);
        call.turn_started("new-turn");
        call.report_turn("finished", "new-turn", Some("New words"));
        let words = if mode == "push" {
            receiver
                .bodies
                .recv_timeout(Duration::from_secs(15))
                .unwrap()
        } else {
            until("new words pulled", 15, || {
                let page = mcp.tool("get_messages", json!({}));
                page["messages"].as_array().and_then(|items| {
                    items
                        .iter()
                        .find(|item| item["text"] == "New words")
                        .cloned()
                })
            })
        };
        assert_eq!(words["text"], "New words", "{words}");
        assert_eq!(words["origin"], "spoken_room", "{words}");
        call.report_playback("late-utterance", "unplayed", Some("newer_turn"), None);
        confirm_hearing(
            &profile,
            &call,
            &thread,
            "late-utterance",
            "not_heard",
            None,
        );
        call.report_turn("started", "later-turn", None);
        call.turn_started("later-turn");
        call.report_turn("finished", "later-turn", Some("Later words"));
        let followup = if mode == "push" {
            until("later words with hearing correction injected", 15, || {
                receiver
                    .bodies
                    .recv_timeout(Duration::from_millis(100))
                    .ok()
                    .filter(|message| message["text"] == "Later words")
            })
        } else {
            until("later words with hearing correction pulled", 15, || {
                let page = mcp.tool("get_messages", json!({}));
                page["messages"].as_array().and_then(|items| {
                    items
                        .iter()
                        .find(|item| item["text"] == "Later words")
                        .cloned()
                })
            })
        };
        assert_ne!(followup["message_id"], words["message_id"], "{followup}");
        assert_eq!(followup["origin"], "spoken_room", "{followup}");
        assert!(
            followup["hearing_changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| {
                    change["utterance_id"] == "late-utterance"
                        && change["hearing"] == "not_played"
                        && change["heard_chars"] == 0
                }),
            "{followup}"
        );
        assert!(!followup["envelope"]
            .as_str()
            .unwrap()
            .contains("Late receipt reply"));
        mcp.stop();
    }
}

#[test]
fn t07_t16_detachable_pull_claim_reaches_the_next_real_ipc_owner() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::Path;

    struct Ipc {
        reader: BufReader<UnixStream>,
        serial: u64,
    }
    impl Ipc {
        fn open(path: &Path) -> Self {
            let stream = UnixStream::connect(path).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(15)))
                .unwrap();
            Self {
                reader: BufReader::new(stream),
                serial: 0,
            }
        }
        fn call(&mut self, method: &str, params: Value) -> Value {
            self.serial += 1;
            writeln!(
                self.reader.get_mut(),
                "{}",
                json!({"id":self.serial,"method":method,"params":params})
            )
            .unwrap();
            self.reader.get_mut().flush().unwrap();
            let mut line = String::new();
            self.reader.read_line(&mut line).unwrap();
            let answer: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(answer["ok"], true, "{answer}");
            answer["result"].clone()
        }
    }

    let Some(core) = core_dir() else { return };
    let profile = Profile::new("detachable-claim");
    let _core = profile.start_core(&core, "detachable-core");
    let _connector = profile.start_connector();
    profile.wait_linked("detachable-core");
    let call = Presentation::open(&profile.core_socket());
    let thread = "detachable-thread";
    let receiver = http_receiver();
    let registration = json!({"client_ref":"detachable-ref","thread":thread,
        "title":"Detached pull","harness":"http","delivery":{"kind":"http","url":receiver.url,"thread":thread},
        "detachable":true,"input_mode":"pull"});
    let mut first = Ipc::open(&profile.connector_socket());
    let joined = first.call("register", registration.clone());
    assert!(joined["binding_id"].is_string(), "{joined}");
    let selected = core_json(
        &profile.core_socket(),
        "POST",
        "/api/presentation/select",
        Some(&json!({"session_id":call.session,"thread_id":thread})),
        Some(&call.token),
    );
    let binding_id = selected["binding"]["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let id = uuid::Uuid::new_v4().to_string();
    let posted = core_json(
        &profile.core_socket(),
        "POST",
        "/api/presentation/text",
        Some(
            &json!({"text":"Claim survives owner exit","session_id":call.session,
            "thread_id":thread,"binding_id":binding_id,"message_id":id}),
        ),
        Some(&call.token),
    );
    assert_eq!(posted["accepted"], true, "{posted}");
    let first_page = first.call("get_messages", json!({"client_ref":"detachable-ref"}));
    assert_eq!(first_page["messages"][0]["message_id"], id, "{first_page}");
    drop(first);
    let mut next = Ipc::open(&profile.connector_socket());
    let reclaimed = next.call("register", registration);
    assert!(reclaimed["binding_id"].is_string(), "{reclaimed}");
    let again = next.call("get_messages", json!({"client_ref":"detachable-ref"}));
    assert_eq!(again["messages"][0]["message_id"], id, "{again}");
    assert_eq!(
        again["messages"][0]["envelope"],
        first_page["messages"][0]["envelope"]
    );
    let acknowledged = next.call(
        "get_messages",
        json!({"client_ref":"detachable-ref","ack_ids":[id]}),
    );
    assert_eq!(acknowledged["acknowledged"], 1, "{acknowledged}");
    assert_eq!(acknowledged["count"], 0, "{acknowledged}");
}
