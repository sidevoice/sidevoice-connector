//! The negotiated voice contract through real Core, Connector and MCP child processes.

mod support;

use serde_json::json;
use std::process::Stdio;
use std::time::{Duration, Instant};
use support::*;

/// Keep the daemon alive while its Core link is displaced. The guard resumes it even
/// if a test assertion fails before the room close is sent.
struct PausedConnector(u32);

impl PausedConnector {
    fn new(pid: u32) -> Self {
        assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGSTOP) }, 0);
        Self(pid)
    }
}

impl Drop for PausedConnector {
    fn drop(&mut self) {
        unsafe { libc::kill(self.0 as i32, libc::SIGCONT) };
    }
}

/// An authenticated, short-lived second connector socket displaces the real daemon's
/// link; closing it leaves that daemon offline until PausedConnector is dropped.
fn displace_connector_link(profile: &Profile) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let ready = read_json(&profile.core_data().join("core.json")).expect("Core ready file");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let stream = tokio::net::UnixStream::connect(profile.core_socket())
            .await
            .unwrap();
        let (mut peer, _) =
            tokio_tungstenite::client_async("ws://localhost/api/connectors/v3", stream)
                .await
                .unwrap();
        peer.send(Message::Text(
            json!({"jsonrpc":"2.0","id":"test-hello","method":"connector.hello",
                "params":{"protocol":3,"connector_id":ready["connector_id"],
                    "token":ready["token"]}})
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
        let hello = peer.next().await.unwrap().unwrap();
        let hello: serde_json::Value = serde_json::from_str(hello.to_text().unwrap()).unwrap();
        assert_eq!(hello["result"]["protocol"], 3, "replacement hello refused");
        peer.close(None).await.unwrap();
    });
}

#[test]
fn admitted_original_lookup_replay_and_old_ids_after_both_processes_restart() {
    let Some(core) = core_dir() else { return };
    let profile = Profile::new("v1restart");
    let mut first_core = profile.start_core(&core, "v1-first");
    let mut first_connector = profile.start_connector();
    profile.wait_linked("v1-first");
    let call = Presentation::open(&profile.core_socket());
    let receiver = http_receiver();
    let thread = "v1-real-restart-thread";
    let mut command = profile.connector(&["mcp"]);
    command
        .env("SIDEVOICE_THREAD", thread)
        .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
    let mut mcp = Mcp::start(command);
    mcp.initialize("v1-restart-client");
    let joined = mcp.tool("voice_connect", json!({"title":"V1 restart"}));
    assert_eq!(joined["voice_contract"], true, "{joined}");
    assert!(
        joined["initial_reply_context"]["session_id"].is_string(),
        "{joined}"
    );
    // No room event exists yet, so the MCP tool uses the binding's negotiated initial context.
    let say = json!({"conversation":thread,"event_id":"v1-event-1",
        "utterance_id":"v1-original-1","text":"The exact original reply."});
    let accepted = mcp.tool("voice_say", say.clone());
    assert_eq!(accepted["status"], "accepted", "{accepted}");
    assert_eq!(accepted["playback"], "text_only", "{accepted}");
    assert_eq!(mcp.tool("voice_say", say.clone()), accepted);
    let original = mcp.tool(
        "voice_get_reply",
        json!({"conversation":thread,
        "utterance_id":"v1-original-1"}),
    );
    assert_eq!(original["status"], "available", "{original}");
    assert_eq!(original["text"], "The exact original reply.", "{original}");
    let initial = &joined["initial_reply_context"];
    let explicit_initial = mcp.tool(
        "voice_say",
        json!({"conversation":thread,
        "event_id":"v1-event-2","utterance_id":"v1-original-2",
        "text":"Another exact original.","session_id":initial["session_id"],
        "revision":initial["revision"],"epoch":initial["epoch"]}),
    );
    assert_eq!(explicit_initial["status"], "accepted", "{explicit_initial}");
    assert_eq!(
        explicit_initial["playback"], "text_only",
        "{explicit_initial}"
    );

    let selected = core_json(
        &profile.core_socket(),
        "POST",
        "/api/presentation/select",
        Some(&json!({"session_id":call.session,"thread_id":thread})),
        Some(&call.token),
    );
    assert_eq!(selected["binding"]["thread_id"], thread, "{selected}");

    let message_id = uuid::Uuid::new_v4().to_string();
    let typed = core_json(
        &profile.core_socket(),
        "POST",
        "/api/presentation/text",
        Some(
            &json!({"text":"Please repeat that.","session_id":call.session,
            "thread_id":thread,"binding_id":selected["binding"]["binding_id"],
            "message_id":message_id}),
        ),
        Some(&call.token),
    );
    assert_eq!(typed["accepted"], true, "{typed}");
    let deadline = Instant::now() + Duration::from_secs(15);
    let delivered = loop {
        let event = receiver
            .bodies
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        if event["message_id"] == message_id {
            break event;
        }
    };
    assert_eq!(delivered["message_id"], message_id, "{delivered}");
    assert_eq!(delivered["origin"], "typed_room", "{delivered}");
    assert!(delivered["envelope"]
        .as_str()
        .unwrap()
        .contains("Please repeat that."));
    let replay = json!({"conversation":thread,"utterance_id":"v1-original-1",
        "replay_id":"v1-replay-1","mode":"restart","session_id":delivered["session_id"],
        "revision":delivered["revision"],"epoch":delivered["epoch"],
        "context_message_id":delivered["message_id"]});
    let replayed = mcp.tool("voice_replay", replay.clone());
    assert_eq!(replayed["status"], "accepted", "{replayed}");
    assert!(replayed["attempt_id"].is_string(), "{replayed}");
    assert_eq!(mcp.tool("voice_replay", replay.clone()), replayed);

    // Both processes stop after the accepted outbox has been cleared. A new MCP process
    // supplies the new initial context, but old caller IDs remain fenced by the private ledger.
    let outbox = read_json(&profile.data().join("outbox.json")).unwrap_or(json!([]));
    assert_eq!(
        outbox,
        json!([]),
        "accepted outbox was not cleared: {outbox}"
    );
    mcp.stop();
    first_connector.stop();
    first_core.stop();
    drop(call);
    let _second_core = profile.start_core(&core, "v1-second");
    let _second_connector = profile.start_connector();
    profile.wait_linked("v1-second");
    let mut command = profile.connector(&["mcp"]);
    command
        .env("SIDEVOICE_THREAD", thread)
        .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
    let mut mcp = Mcp::start(command);
    mcp.initialize("v1-restart-client");
    let rejoined = mcp.tool("voice_connect", json!({"title":"V1 restart"}));
    assert_eq!(rejoined["voice_contract"], true, "{rejoined}");
    assert_ne!(
        rejoined["initial_reply_context"]["session_id"],
        joined["initial_reply_context"]["session_id"]
    );
    let stale_say = mcp.tool("voice_say", say);
    assert_eq!(stale_say["status"], "rejected", "{stale_say}");
    assert_eq!(stale_say["reason"], "context_stale", "{stale_say}");
    let stale_replay = mcp.tool("voice_replay", replay);
    assert_eq!(stale_replay["status"], "rejected", "{stale_replay}");
    assert_eq!(stale_replay["reason"], "context_stale", "{stale_replay}");
    mcp.stop();
}

#[test]
fn an_offline_room_close_cannot_resurrect_a_cached_binding_on_real_reconnect() {
    let Some(core) = core_dir() else { return };
    let profile = Profile::new("v1offlineclose");
    let _core = profile.start_core(&core, "offline-close");
    let connector = profile.start_connector();
    profile.wait_linked("offline-close");
    let call = Presentation::open(&profile.core_socket());
    let receiver = http_receiver();
    let thread = "v1-offline-close-thread";
    let mut command = profile.connector(&["mcp"]);
    command
        .env("SIDEVOICE_THREAD", thread)
        .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
    let mut mcp = Mcp::start(command);
    mcp.initialize("v1-offline-close-client");
    let joined = mcp.tool("voice_connect", json!({"title":"Offline close"}));
    let original_binding = joined["binding_id"].as_str().unwrap().to_owned();
    core_json(
        &profile.core_socket(),
        "POST",
        "/api/presentation/select",
        Some(&json!({"session_id":call.session,"thread_id":thread})),
        Some(&call.token),
    );
    let original = json!({"conversation":thread,"event_id":"closed-event",
        "utterance_id":"closed-utterance","text":"Reply before close."});
    let first = mcp.tool("voice_say", original.clone());
    assert_eq!(first["status"], "accepted", "{first}");
    assert_eq!(first["playback"], "queued", "{first}");
    let first_audio = call.next_frame("voice-reply", 15);
    assert_eq!(first_audio["data"]["utterance_id"], "closed-utterance");

    let paused = PausedConnector::new(connector.pid());
    displace_connector_link(&profile);
    let ready = read_json(&profile.core_data().join("core.json")).unwrap();
    until("the replacement socket is gone", 15, || {
        let listed = core_json(
            &profile.core_socket(),
            "GET",
            "/api/connectors",
            None,
            Some(&call.token),
        );
        listed["connectors"]
            .as_array()?
            .iter()
            .find(|row| row["id"] == ready["connector_id"])
            .and_then(|row| (row["connected"] == false).then_some(()))
    });
    let closed = core_json(
        &profile.core_socket(),
        "POST",
        "/api/presentation/close",
        Some(&json!({"thread_id":thread})),
        Some(&call.token),
    );
    assert_eq!(closed["binding_id"], original_binding, "{closed}");
    drop(paused);

    let status = until("closed binding retired after real reconnect", 30, || {
        let status = connector_ipc(&profile.connector_socket(), "status")?;
        (status["connected"] == true
            && status["bindings"].as_array().is_some_and(Vec::is_empty)
            && status["closed_by_room"]
                .as_array()
                .is_some_and(|rows| rows.iter().any(|row| row == thread)))
        .then_some(status)
    });
    assert_eq!(
        status["closed_reasons"][thread], "binding_closed",
        "{status}"
    );
    let view = core_json(
        &profile.core_socket(),
        "GET",
        &format!("/api/presentation?session_id={}", call.session),
        None,
        Some(&call.token),
    );
    assert!(view["binding"].is_null(), "{view}");
    let current = mcp.tool("voice_status", json!({"conversation":thread}));
    assert_eq!(current["joined"], false, "{current}");
    assert_eq!(current["closed_by_room"], true, "{current}");

    // A deliberate new join is possible, but cannot recover the old accepted IDs.
    let rejoined = mcp.tool("voice_connect", json!({"title":"Rejoined"}));
    assert_ne!(rejoined["binding_id"], original_binding, "{rejoined}");
    let old = mcp.tool("voice_say", original);
    assert_eq!(old["status"], "rejected", "{old}");
    assert_eq!(old["reason"], "context_stale", "{old}");
    assert!(
        !call.saw_frame_within("voice-reply", Duration::from_secs(1)),
        "room sent new audio after the old IDs were retried"
    );
    mcp.stop();
}

#[test]
fn lost_identity_ledger_after_cleared_acceptance_blocks_a_real_process_restart() {
    let Some(core) = core_dir() else { return };
    let profile = Profile::new("v1lostledger");
    let mut first_core = profile.start_core(&core, "ledger-first");
    let mut first_connector = profile.start_connector();
    profile.wait_linked("ledger-first");
    let thread = "v1-ledger-thread";
    let receiver = http_receiver();
    let mut command = profile.connector(&["mcp"]);
    command
        .env("SIDEVOICE_THREAD", thread)
        .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
    let mut mcp = Mcp::start(command);
    mcp.initialize("v1-ledger-client");
    let joined = mcp.tool("voice_connect", json!({"title":"Ledger loss"}));
    assert_eq!(joined["voice_contract"], true, "{joined}");
    let accepted = mcp.tool(
        "voice_say",
        json!({"conversation":thread,
        "event_id":"ledger-event","utterance_id":"ledger-utterance",
        "text":"Original accepted reply."}),
    );
    assert_eq!(accepted["status"], "accepted", "{accepted}");
    assert_eq!(
        read_json(&profile.data().join("outbox.json")).unwrap_or(json!([])),
        json!([])
    );
    let ledger = profile.data().join("speech-identities.json");
    let guard = profile.data().join("speech-identities.guard");
    assert!(ledger.is_file() && guard.is_file());
    mcp.stop();
    first_connector.stop();
    first_core.stop();
    std::fs::remove_file(&ledger).unwrap();
    let _second_core = profile.start_core(&core, "ledger-second");
    let log_path = profile.root.join("connector-ledger-restart.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut second_connector = profile
        .connector(&["connector"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .unwrap();
    let status = until("connector must refuse lost ledger", 15, || {
        second_connector.try_wait().ok().flatten()
    });
    assert!(
        !status.success(),
        "lost ledger unexpectedly started a connector"
    );
    let text = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        text.contains("speech identity ledger missing after initialization"),
        "{text}"
    );
    assert!(connector_ipc(&profile.connector_socket(), "status").is_none());
}

#[test]
fn spoken_turn_arrives_as_a_canonical_push_and_is_current_reply_context() {
    let Some(core) = core_dir() else { return };
    let profile = Profile::new("v1spoken");
    let _core = profile.start_core(&core, "v1-spoken");
    let _connector = profile.start_connector();
    profile.wait_linked("v1-spoken");
    let call = Presentation::open(&profile.core_socket());
    let receiver = http_receiver();
    let thread = "v1-spoken-push-thread";
    let mut command = profile.connector(&["mcp"]);
    command
        .env("SIDEVOICE_THREAD", thread)
        .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
    let mut mcp = Mcp::start(command);
    mcp.initialize("v1-spoken-client");
    let joined = mcp.tool("voice_connect", json!({"input_mode":"push"}));
    assert_eq!(joined["voice_contract"], true, "{joined}");
    let selected = core_json(
        &profile.core_socket(),
        "POST",
        "/api/presentation/select",
        Some(&json!({"session_id":call.session,"thread_id":thread})),
        Some(&call.token),
    );
    assert_eq!(selected["binding"]["thread_id"], thread, "{selected}");
    call.report_turn("started", "spoken-1", None);
    call.report_turn("finished", "spoken-1", Some("Yes, go ahead."));
    let deadline = Instant::now() + Duration::from_secs(15);
    let delivered = loop {
        let event = receiver
            .bodies
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        if event["text"] == "Yes, go ahead." {
            break event;
        }
    };
    assert_eq!(delivered["channel"], "voice", "{delivered}");
    assert_eq!(delivered["origin"], "spoken_room", "{delivered}");
    let envelope = delivered["envelope"].as_str().unwrap();
    let (header, words) = envelope.split_once("\n\n").unwrap();
    let header: serde_json::Value = serde_json::from_str(header).unwrap();
    assert_eq!(header["message_id"], delivered["message_id"]);
    assert_eq!(header["origin"], "spoken_room");
    assert_eq!(words, "Yes, go ahead.");
    assert!(header["playback_at_turn_start"].is_object());
    let said = mcp.tool(
        "voice_say",
        json!({"conversation":thread,
        "event_id":"v1-spoken-event","utterance_id":"v1-spoken-original",
        "text":"I heard your reply.","session_id":delivered["session_id"],
        "revision":delivered["revision"],"epoch":delivered["epoch"],
        "context_message_id":delivered["message_id"]}),
    );
    assert_eq!(said["status"], "accepted", "{said}");
    assert_eq!(said["playback"], "queued", "{said}");
    mcp.stop();
}

#[test]
fn heard_a_then_unplayed_b_crosses_real_spoken_onset_in_push_and_pull() {
    let Some(core) = core_dir() else { return };
    for mode in ["push", "pull"] {
        let profile = Profile::new(&format!("v1heard-a-unplayed-b-{mode}"));
        let launch = format!("v1-heard-a-unplayed-b-{mode}");
        let _core = profile.start_core(&core, &launch);
        let _connector = profile.start_connector();
        profile.wait_linked(&launch);
        let call = Presentation::open(&profile.core_socket());
        let receiver = http_receiver();
        let thread = format!("v1-heard-a-unplayed-b-thread-{mode}");
        let mut command = profile.connector(&["mcp"]);
        command
            .env("SIDEVOICE_THREAD", &thread)
            .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
        let mut mcp = Mcp::start(command);
        mcp.initialize("v1-causal-client");
        let joined = mcp.tool("voice_connect", json!({"input_mode":mode}));
        assert_eq!(joined["voice_contract"], true, "{joined}");
        let selected = core_json(
            &profile.core_socket(),
            "POST",
            "/api/presentation/select",
            Some(&json!({"session_id":call.session,"thread_id":thread})),
            Some(&call.token),
        );
        assert_eq!(selected["binding"]["thread_id"], thread, "{selected}");

        let a_text = "May I archive fixture A?";
        let a = mcp.tool(
            "voice_say",
            json!({"conversation":thread,"event_id":"causal-event-a",
        "utterance_id":"causal-utterance-a","text":a_text}),
        );
        assert_eq!(a["status"], "accepted", "{a}");
        assert_eq!(a["playback"], "queued", "{a}");
        let a_frame = call.next_frame("voice-reply", 15);
        assert_eq!(
            a_frame["data"]["utterance_id"], "causal-utterance-a",
            "{a_frame}"
        );
        call.report_contract_playback(
            "causal-utterance-a",
            a["attempt_id"].as_str().unwrap(),
            "heard",
            "complete",
            a_text.chars().count() as u64,
        );
        let a_heard = json!({"utterance_id":"causal-utterance-a","hearing":"complete",
        "heard_chars":a_text.chars().count()});
        let snapshot = json!({"knowledge":"known","last_played":a_heard,
        "recent":[a_heard],"truncated":false});

        call.report_turn_with_evidence("started", "causal-turn", None, &snapshot, &json!([]));
        call.turn_started("causal-turn");
        let b = mcp.tool(
            "voice_say",
            json!({"conversation":thread,"event_id":"causal-event-b",
        "utterance_id":"causal-utterance-b","text":"May I archive fixture B?"}),
        );
        assert_eq!(b["status"], "accepted", "{b}");
        assert_eq!(b["playback"], "pending", "{b}");
        let certified_b = json!([{"utterance_id":"causal-utterance-b",
        "hearing":"not_played","heard_chars":0}]);
        call.report_turn_with_evidence(
            "finished",
            "causal-turn",
            Some("Yes, go ahead."),
            &snapshot,
            &certified_b,
        );
        let delivered = if mode == "push" {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                let event = receiver
                    .bodies
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap();
                if event["text"] == "Yes, go ahead." {
                    break event;
                }
            }
        } else {
            until("spoken turn in the pull journal", 15, || {
                let page = mcp.tool("get_messages", json!({}));
                page["messages"].as_array().and_then(|messages| {
                    messages
                        .iter()
                        .find(|event| event["text"] == "Yes, go ahead.")
                        .cloned()
                })
            })
        };
        assert_eq!(delivered["origin"], "spoken_room", "{delivered}");
        assert_eq!(
            delivered["playback_at_turn_start"]["last_played"]["utterance_id"],
            "causal-utterance-a",
            "{delivered}"
        );
        assert_eq!(
            delivered["not_played_at_turn_start"][0]["utterance_id"], "causal-utterance-b",
            "{delivered}"
        );
        let envelope = delivered["envelope"].as_str().unwrap();
        let (header, words) = envelope.split_once("\n\n").unwrap();
        let header: serde_json::Value = serde_json::from_str(header).unwrap();
        assert_eq!(words, "Yes, go ahead.");
        assert_eq!(
            header["playback_at_turn_start"],
            delivered["playback_at_turn_start"]
        );
        assert_eq!(
            header["not_played_at_turn_start"],
            delivered["not_played_at_turn_start"]
        );
        if let Ok(path) = std::env::var("SIDEVOICE_TRIAL_ENVELOPE_PATH") {
            std::fs::write(path, serde_json::to_vec_pretty(&delivered).unwrap()).unwrap();
        }
        mcp.stop();
    }
}
