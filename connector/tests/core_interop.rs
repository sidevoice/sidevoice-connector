//! The connector against the real core (the sidevoice-core release pinned in core.pin), end to end, in a private profile:
//! the connector links to a core it did not start, over the core's own socket; a conversation joins through the
//! MCP server; what the person says in a call reaches the conversation; the conversation's reply reaches the core;
//! and when the core restarts the connector links to the new one by itself.

mod support;

use serde_json::json;
use serde_json::Value;
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
    let message = if mode == "push" {
        until(&format!("{kind} injected"), 20, || {
            receiver
                .bodies
                .recv_timeout(Duration::from_millis(150))
                .ok()
                .filter(|body| body["notice"]["kind"] == kind)
        })
    } else {
        until(&format!("{kind} pulled"), 20, || {
            let page = mcp.tool("get_messages", json!({}));
            page["messages"].as_array().and_then(|messages| {
                messages
                    .iter()
                    .find(|message| message["notice"]["kind"] == kind)
                    .cloned()
            })
        })
    };
    assert_eq!(message["notice"]["kind"], kind, "{message}");
    let envelope = message["envelope"].as_str().unwrap();
    assert!(
        envelope.contains(&message["notice"].to_string()),
        "{message}"
    );
    assert!(
        envelope.contains(message["notice"]["instruction"].as_str().unwrap()),
        "{message}"
    );
    if !message["unheard"].is_null() {
        assert!(
            envelope.contains(&message["unheard"].to_string()),
            "{message}"
        );
    }
    if let Some(replies) = message["unheard"]["replies"].as_array() {
        for reply in replies {
            assert!(
                envelope.contains(reply["text"].as_str().unwrap()),
                "{message}"
            );
        }
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
                "reply_interrupted",
            ),
            (
                "unstarted-words",
                "unplayed",
                Some("Actually"),
                "reply_not_played",
            ),
            ("unstarted-empty", "unplayed", None, "turn_empty"),
        ] {
            let message = actual_notice(&core, mode, label, kind, |_, call, mcp, joined| {
                let said = mcp.tool(
                    "voice_say",
                    json!({"conversation":joined["conversation"],
                    "text":"é🐙 original","event_id":"event-1","utterance_id":"utterance-1"}),
                );
                assert_eq!(said["text_saved"], true, "{said}");
                if playback == "interrupted" {
                    call.report_playback("utterance-1", "playing", None, None);
                    call.report_playback(
                        "utterance-1",
                        "interrupted",
                        Some("user_interrupted"),
                        Some(2),
                    );
                }
                call.report_turn("started", "turn-1", None);
                call.turn_started("turn-1");
                if playback == "unplayed" {
                    call.report_playback("utterance-1", "unplayed", Some("newer_turn"), None);
                }
                call.report_turn("finished", "turn-1", words);
            });
            assert_eq!(
                message["unheard"]["replies"][0]["text"], "é🐙 original",
                "{message}"
            );
            if playback == "interrupted" {
                assert_eq!(
                    message["unheard"]["replies"][0]["heard_chars"], 2,
                    "{message}"
                );
            }
            if words.is_none() {
                assert_eq!(message["channel"], "note", "{message}");
                assert_eq!(message["text"], "", "{message}");
            } else {
                assert_eq!(message["text"], words.unwrap(), "{message}");
            }
        }
    }
}

#[test]
fn t12_real_skip_notice_keeps_choice_outside_unheard_in_push_and_pull() {
    let Some(core) = core_dir() else { return };
    for mode in ["push", "pull"] {
        let note = actual_notice(
            &core,
            mode,
            "skip",
            "reply_skipped",
            |_, call, mcp, joined| {
                let said = mcp.tool(
                    "voice_say",
                    json!({"conversation":joined["conversation"],
                "text":"Skip this","event_id":"skip-event","utterance_id":"skip-utterance"}),
                );
                assert_eq!(said["text_saved"], true, "{said}");
                call.report_playback(
                    "skip-utterance",
                    "interrupted",
                    Some("user_skipped"),
                    Some(2),
                );
            },
        );
        assert_eq!(note["notice"]["skipped"][0]["text"], "Skip this", "{note}");
        assert!(
            note.get("unheard").is_none() || note["unheard"].is_null(),
            "{note}"
        );
        assert!(note["notice"]["instruction"]
            .as_str()
            .unwrap()
            .contains("Do not repeat"));
    }
}

#[test]
fn t03_stale_reply_is_text_only_with_ordinary_notice_in_push_and_pull() {
    let Some(core) = core_dir() else { return };
    for mode in ["push", "pull"] {
        let note = actual_notice(
            &core,
            mode,
            "stale",
            "reply_not_played",
            |profile, call, mcp, joined| {
                call.report_turn("started", "new-turn", None);
                call.turn_started("new-turn");
                call.report_turn("finished", "new-turn", Some("New question"));
                let thread = joined["conversation"].as_str().unwrap();
                until("new words are in core history", 15, || {
                    let history = core_json(
                        &profile.core_socket(),
                        "GET",
                        &format!("/api/presentation/history?thread_id={thread}"),
                        None,
                        Some(&call.token),
                    );
                    history["messages"]
                        .as_array()
                        .is_some_and(|rows| rows.iter().any(|row| row["text"] == "New question"))
                        .then_some(())
                });
                let initial = &joined["initial_reply_context"];
                let said = mcp.tool("voice_say", json!({"text":"Late old answer",
                "session_id":initial["session_id"],"revision":initial["revision"],
                "epoch":initial["epoch"],"event_id":"stale-event","utterance_id":"stale-utterance"}));
                assert_eq!(said["audio"], "text_only", "{said}");
                assert_eq!(said["reason"], "newer_turn", "{said}");
            },
        );
        assert_eq!(note["channel"], "note", "{note}");
        assert_eq!(
            note["unheard"]["replies"][0]["text"], "Late old answer",
            "{note}"
        );
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
            "reply_interrupted",
            |_, call, mcp, joined| {
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
                    assert_eq!(said["text_saved"], true, "{said}");
                }
                call.report_playback("ack", "heard", None, None);
                call.report_playback("checkpoint", "playing", None, None);
                call.report_playback(
                    "checkpoint",
                    "interrupted",
                    Some("user_interrupted"),
                    Some(4),
                );
                call.report_turn("started", "next-turn", None);
                call.turn_started("next-turn");
                call.report_turn("finished", "next-turn", Some("Stop there"));
            },
        );
        assert_eq!(
            message["unheard"]["replies"][0]["utterance_id"], "checkpoint",
            "{message}"
        );
        assert_eq!(
            message["unheard"]["replies"][0]["heard_chars"], 4,
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
            |_, call, mcp, joined| {
                call.report_speech(false, Some("voice_off"));
                let said = mcp.tool("voice_say", json!({"conversation":joined["conversation"],
                "text":"Missed update","event_id":"return-event","utterance_id":"return-utterance"}));
                assert_eq!(said["text_saved"], true, "{said}");
                call.report_speech(true, None);
            },
        );
        assert_eq!(notice["channel"], "note", "{notice}");
        assert_eq!(
            notice["unheard"]["replies"][0]["text"], "Missed update",
            "{notice}"
        );
    }
}

#[test]
fn t02c_t02d_refusal_reaches_the_caller_and_words_or_empty_input_in_both_modes() {
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
            let started = call.turn_started("open");
            let refused = mcp.tool(
                "voice_say",
                json!({"text":"During open turn",
                "session_id":call.session,"revision":started["revision"],"epoch":started["epoch"],
                "event_id":"refused-event","utterance_id":"refused-utterance"}),
            );
            assert_eq!(refused["status"], "rejected", "{refused}");
            assert_eq!(refused["reason"], "user_speaking", "{refused}");
            assert!(refused["instruction"].as_str().unwrap().contains("Wait"));
            call.report_turn("finished", "open", words);
            let message = if mode == "push" {
                receiver
                    .bodies
                    .recv_timeout(Duration::from_secs(15))
                    .unwrap()
            } else {
                until("refusal outcome pulled", 15, || {
                    let page = mcp.tool("get_messages", json!({}));
                    page["messages"]
                        .as_array()
                        .and_then(|items| items.first().cloned())
                })
            };
            if let Some(words) = words {
                assert_eq!(message["text"], words, "{message}");
                assert_eq!(message["channel"], "voice", "{message}");
            } else {
                assert_eq!(message["text"], "", "{message}");
                assert_eq!(message["notice"]["kind"], "turn_empty", "{message}");
                assert!(message["envelope"]
                    .as_str()
                    .unwrap()
                    .contains("Resend what you meant"));
            }
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
            assert_eq!(said["text_saved"], true, "{said}");
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
        }
        mcp.stop();
    }
}

#[test]
fn t04b_nine_withdrawn_replies_reach_complete_ordered_real_core_batches() {
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
        for n in 0..9 {
            let said = mcp.tool(
                "voice_say",
                json!({"conversation":joined["conversation"],
                "text":format!("Reply {n}"),"event_id":format!("batch-event-{n}"),
                "utterance_id":format!("batch-utterance-{n}")}),
            );
            assert_eq!(said["text_saved"], true, "{said}");
        }
        call.report_turn("started", "turn", None);
        call.turn_started("turn");
        for n in 0..9 {
            call.report_playback(
                &format!("batch-utterance-{n}"),
                "unplayed",
                Some("newer_turn"),
                None,
            );
        }
        call.report_turn("finished", "turn", Some("New request"));
        let mut evidence = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut after = 0;
        until("all nine replies in real delivery batches", 20, || {
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
                let replies = message["unheard"]["replies"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if !replies.is_empty() {
                    assert!(replies.len() <= 8, "{message}");
                    let envelope = message["envelope"].as_str().unwrap();
                    assert!(
                        envelope.contains(&message["unheard"].to_string()),
                        "{message}"
                    );
                    evidence.extend(
                        replies
                            .into_iter()
                            .map(|reply| reply["text"].as_str().unwrap().to_owned()),
                    );
                }
            }
            (evidence.len() >= 9).then_some(())
        });
        assert_eq!(
            evidence,
            (0..9).map(|n| format!("Reply {n}")).collect::<Vec<_>>()
        );
        mcp.stop();
    }
}

#[test]
fn t06e_no_return_notice_without_a_miss_or_after_another_device_heard() {
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
        assert_eq!(said["text_saved"], true, "{said}");
        other.report_playback("heard-utterance", "heard", None, None);
        std::thread::sleep(Duration::from_millis(150));
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
        mcp.stop();
    }
}

#[test]
fn t14_real_two_device_corrections_keep_the_original_notice_frozen() {
    let Some(core) = core_dir() else { return };
    for mode in ["push", "pull"] {
        for (fate, status, reason) in [
            ("heard", "heard", None),
            ("skipped", "interrupted", Some("user_skipped")),
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
            assert_eq!(said["text_saved"], true, "{said}");
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
            call.report_turn("started", "new-turn", None);
            call.turn_started("new-turn");
            call.report_turn("finished", "new-turn", Some("What happened?"));
            let initial = if mode == "push" {
                until("initial interrupted notice injected", 15, || {
                    receiver
                        .bodies
                        .recv_timeout(Duration::from_millis(100))
                        .ok()
                        .filter(|message| message["notice"]["kind"] == "reply_interrupted")
                })
            } else {
                until("initial interrupted notice pulled", 15, || {
                    let page = mcp.tool("get_messages", json!({}));
                    page["messages"].as_array().and_then(|items| {
                        items
                            .iter()
                            .find(|message| message["notice"]["kind"] == "reply_interrupted")
                            .cloned()
                    })
                })
            };
            let frozen = initial["envelope"].clone();
            other.report_playback("correction-utterance", status, reason, Some(3));
            let correction = if mode == "push" {
                until("heard correction injected", 15, || {
                    receiver
                        .bodies
                        .recv_timeout(Duration::from_millis(100))
                        .ok()
                        .filter(|message| message["notice"]["kind"] == "reply_heard")
                })
            } else {
                until("heard correction pulled", 15, || {
                    let page = mcp.tool("get_messages", json!({}));
                    page["messages"].as_array().and_then(|items| {
                        items
                            .iter()
                            .find(|message| message["notice"]["kind"] == "reply_heard")
                            .cloned()
                    })
                })
            };
            assert_eq!(
                correction["notice"]["corrects_message_id"], initial["message_id"],
                "{correction}"
            );
            assert_eq!(
                correction["notice"]["heard"][0]["utterance_id"], "correction-utterance",
                "{correction}"
            );
            assert_eq!(
                correction["notice"]["heard"][0]["fate"], fate,
                "{correction}"
            );
            assert_eq!(
                correction["notice"]["heard"][0]["heard_chars"], 16,
                "{correction}"
            );
            assert!(correction["envelope"]
                .as_str()
                .unwrap()
                .contains(&correction["notice"].to_string()));
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
    let said = mcp.tool(
        "voice_say",
        json!({"conversation":joined["conversation"],
        "text":"Full parity reply","event_id":"parity-event","utterance_id":"parity-utterance"}),
    );
    assert_eq!(said["text_saved"], true, "{said}");
    call.report_playback("parity-utterance", "playing", None, None);
    call.report_playback(
        "parity-utterance",
        "interrupted",
        Some("user_interrupted"),
        Some(4),
    );
    call.report_turn("started", "parity-turn", None);
    call.turn_started("parity-turn");
    call.report_turn("finished", "parity-turn", Some("New words"));
    let injected = until("failed injected notice", 15, || {
        receiver
            .bodies
            .recv_timeout(Duration::from_millis(100))
            .ok()
            .filter(|message| message["notice"]["kind"] == "reply_interrupted")
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
    for field in [
        "thread_id",
        "message_id",
        "session_id",
        "revision",
        "epoch",
        "channel",
        "text",
        "notice",
        "unheard",
        "envelope",
    ] {
        if let (Some(injected_field), Some(pulled_field)) = (injected.get(field), pulled.get(field))
        {
            assert_eq!(
                injected_field, pulled_field,
                "{field}: injected={injected}, pulled={pulled}"
            );
        }
    }
    assert_eq!(pulled["unheard"]["replies"][0]["text"], "Full parity reply");
    let acknowledged = mcp.tool("get_messages", json!({"ack_ids":[pulled["message_id"]]}));
    assert_eq!(acknowledged["acknowledged"], 1, "{acknowledged}");
    mcp.stop();
}

#[test]
fn t02c_t02d_backlogged_other_conversation_cannot_hide_real_core_refusal() {
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
        let started = call.turn_started("open");
        let refused = mcp.tool(
            "voice_say",
            json!({"text":"Refuse now",
            "session_id":call.session,"revision":started["revision"],"epoch":started["epoch"],
            "event_id":"new-event","utterance_id":"new-utterance"}),
        );
        assert_eq!(refused["status"], "rejected", "{refused}");
        assert_eq!(refused["reason"], "user_speaking", "{refused}");
        assert!(refused["instruction"].as_str().unwrap().contains("Wait"));
        call.report_turn("finished", "open", words);
        let message = until("backlogged refusal outcome", 15, || {
            let page = mcp.tool("get_messages", json!({}));
            page["messages"]
                .as_array()
                .and_then(|items| items.first().cloned())
        });
        if let Some(words) = words {
            assert_eq!(message["text"], words, "{message}");
        } else {
            assert_eq!(message["notice"]["kind"], "turn_empty", "{message}");
        }
        let retained: Value =
            serde_json::from_slice(&std::fs::read(&outbox_path).unwrap()).unwrap();
        assert_eq!(retained.as_array().unwrap().len(), 1, "{retained}");
        assert_eq!(retained[0]["event_id"], "old-event", "{retained}");
        mcp.stop();
    }
}
