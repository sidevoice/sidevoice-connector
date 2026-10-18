//! The protocol evolves like a production API (sidevoice/sidevoice-core#112): a core, a façade or a daemon of another
//! version may send fields and enum values this one does not know. Each is read for what it knows: unknown fields are
//! ignored, an unknown value gets its defined default, and nothing is refused for them.

use super::Daemon;
use crate::adapters::{prepare_pull_page, PULL_POSITION_FIELD};
use crate::adapters::{Conversation, Harness, Registry, Route};
use crate::link::Link;
use crate::profile::Profile;
use crate::secure_fs::tests::Scratch;
use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio_tungstenite::tungstenite::Message;

/// A harness that takes every delivery on its route and keeps the frames it was handed.
struct Recorder {
    frames: Arc<Mutex<Vec<Value>>>,
}

impl Harness for Recorder {
    fn id(&self) -> &'static str {
        "recorder"
    }

    fn capabilities(&self) -> Value {
        json!({"deliver":"supported"})
    }

    fn accepts(&self, delivery: &Value, thread: &str) -> Route {
        match delivery["kind"].as_str() {
            Some("recorder") => Route::known(delivery["thread"] == thread),
            _ => Route::Unknown,
        }
    }

    fn deliver<'a>(
        &'a self,
        _: &'a Conversation,
        frame: &'a Value,
        _: &'a Profile,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            self.frames.lock().unwrap().push(frame.clone());
            Ok(json!({"status":"accepted"}))
        })
    }

    fn adopts(&self, _: Option<&str>) -> bool {
        true
    }
}

fn daemon(scratch: &Scratch, link: Arc<Link>) -> (Arc<Daemon>, Arc<Mutex<Vec<Value>>>) {
    let profile = Profile::for_test(&scratch.0, &Registry::standard().homes());
    let frames = Arc::new(Mutex::new(Vec::new()));
    let registry = Registry::new(vec![Arc::new(Recorder {
        frames: frames.clone(),
    })]);
    let (replay_tx, _replay_rx) = tokio::sync::mpsc::channel(1);
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let daemon = Daemon::new(profile, link, replay_tx, registry, false, shutdown).unwrap();
    (daemon, frames)
}

/// A registration as a newer façade sends it: fields this daemon does not know at the top and inside its objects.
fn registration(client_ref: &str) -> Value {
    json!({"client_ref":client_ref,"thread":client_ref,"title":"Future","harness":"recorder",
        "delivery":{"kind":"recorder","thread":client_ref,"future_route_hint":{"socket":"/nowhere"}},
        "engine":{"model":"m-1","effort":"high","thinking":null,"future_engine_field":[1, 2]},
        "detachable":true,"future_field":"ignored","future_object":{"nested":{"deeper":true}}})
}

/// Plays the core's side of the link: every request is answered as a newer core would, with fields this connector
/// does not know and, for speech, a status it has never heard of.
fn play_core(link: Arc<Link>, mut frames: tokio::sync::mpsc::Receiver<Message>) {
    tokio::spawn(async move {
        while let Some(Message::Text(text)) = frames.recv().await {
            let frame: Value = serde_json::from_str(text.as_str()).unwrap();
            let Some(id) = frame.get("id").cloned() else {
                continue;
            };
            let params = &frame["params"];
            let result = match frame["method"].as_str() {
                Some("binding.register") => {
                    json!({"binding_id":format!("core-{}", params["client_ref"].as_str().unwrap()),
                    "error":null,"epoch":0,"initial_reply_context":{"thread_id":params["thread"],"session_id":format!("binding:core-{}", params["client_ref"].as_str().unwrap()),"revision":0,"epoch":0},"status":"registered_somewhere_new","future":{"nested":{"x":1}}})
                }
                Some("speech.publish") => {
                    if params["text"] == "during turn" {
                        json!({"event_id":params["event_id"],"utterance_id":params["utterance_id"],
                            "status":"rejected","terminal":true,"reason":"user_speaking",
                            "instruction":"The person is speaking; wait for their message."})
                    } else {
                        json!({"event_id":params["event_id"],"utterance_id":params["utterance_id"],
                        "text_saved":true,"status":"a_status_from_the_future","reason":"new_reason",
                        "future":{"nested":[1, 2]}})
                    }
                }
                Some("input.pull") => {
                    let after = params["after"].as_u64().unwrap_or(0);
                    let next = after + 1;
                    let messages = if params["operation"] == "get" && next <= 620 {
                        let mut message = json!({"message_id":format!("pull-{next}"),"session_id":"s",
                            "revision":next,"epoch":next,"channel":"voice","text":"words",
                            "arrival_time":1792000000});
                        message[PULL_POSITION_FIELD] = json!(next);
                        vec![message]
                    } else {
                        Vec::new()
                    };
                    let count = messages.len();
                    let mut page = json!({"messages":messages,"count":count,"more":false,
                        "acknowledged":params["ack_ids"].as_array().map_or(0,Vec::len)});
                    page[PULL_POSITION_FIELD] = json!(if count == 0 { after } else { next });
                    page
                }
                _ => json!({}),
            };
            link.answer(
                json!({"jsonrpc":"2.0","id":id,"result":result,"future_envelope":{"trace":"t"}}),
            )
            .await
            .unwrap();
        }
    });
}

#[tokio::test]
async fn v1_speech_fails_closed_against_an_older_core() {
    let scratch = Scratch::new("v1-old-core");
    let link = Link::new();
    let core = link.attached().await;
    play_core(link.clone(), core); // register response intentionally has no voice_contract echo
    let (daemon, _) = daemon(&scratch, link);
    let joined = daemon
        .command(1, "register", registration("t-1"))
        .await
        .unwrap();
    assert_eq!(joined["voice_contract"], false);
    let failure = daemon
        .command(
            1,
            "publish",
            json!({"voice_contract":1,"client_ref":"t-1",
        "session_id":"s","revision":0,"epoch":0,"text":"Do not publish",
        "event_id":"e-v1","utterance_id":"u-v1"}),
        )
        .await
        .unwrap_err();
    assert!(failure.to_string().contains("contract_unavailable"));
    assert!(daemon.outbox.lock().await.is_empty());
    for method in ["get_reply", "replay"] {
        let failure = daemon
            .command(
                1,
                method,
                json!({"client_ref":"t-1",
            "utterance_id":"u-v1","replay_id":"r-v1","session_id":"s",
            "revision":0,"epoch":0,"context_message_id":"m-v1"}),
            )
            .await
            .unwrap_err();
        assert!(
            failure.to_string().contains("contract_unavailable"),
            "{method}"
        );
    }
}

#[tokio::test]
async fn v1_ids_cannot_re_admit_after_both_processes_restart() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn mock_v1(
        link: Arc<Link>,
        mut frames: tokio::sync::mpsc::Receiver<Message>,
        instance: &'static str,
        publications: Arc<AtomicUsize>,
    ) {
        tokio::spawn(async move {
            while let Some(Message::Text(text)) = frames.recv().await {
                let frame: Value = serde_json::from_str(text.as_str()).unwrap();
                let Some(id) = frame.get("id") else { continue };
                let params = &frame["params"];
                let result = match frame["method"].as_str() {
                    Some("binding.register") => json!({"binding_id":"core-t-1","voice_contract":1,
                        "voice_contract_instance":instance,"initial_reply_context":{
                            "session_id":format!("start-{instance}"),"revision":0,"epoch":0}}),
                    Some("speech.publish") => {
                        publications.fetch_add(1, Ordering::Relaxed);
                        assert_eq!(params["voice_contract_instance"], instance);
                        json!({"voice_contract":1,"status":"accepted","playback":"queued",
                            "event_id":params["event_id"],"utterance_id":params["utterance_id"],
                            "attempt_id":"attempt-1"})
                    }
                    Some("speech.replay") => {
                        publications.fetch_add(1, Ordering::Relaxed);
                        assert_eq!(params["voice_contract_instance"], instance);
                        json!({"voice_contract":1,"status":"accepted","playback":"queued",
                            "utterance_id":params["utterance_id"],"replay_id":params["replay_id"],
                            "attempt_id":"attempt-2"})
                    }
                    Some("speech.get_reply") => json!({"voice_contract":1,"status":"available",
                            "utterance_id":params["utterance_id"],"text":"Original words"}),
                    _ => json!({}),
                };
                link.answer(json!({"jsonrpc":"2.0","id":id,"result":result}))
                    .await
                    .unwrap();
            }
        });
    }

    let scratch = Scratch::new("v1-restart-identity");
    let first_link = Link::new();
    let first_calls = Arc::new(AtomicUsize::new(0));
    mock_v1(
        first_link.clone(),
        first_link.attached().await,
        "inc-a",
        first_calls.clone(),
    );
    let (first, _) = daemon(&scratch, first_link);
    first
        .command(1, "register", registration("t-1"))
        .await
        .unwrap();
    let first_say = json!({"voice_contract":1,"client_ref":"t-1","session_id":"start-inc-a",
        "revision":0,"epoch":0,"text":"Original words","event_id":"e-1","utterance_id":"u-1"});
    let answer = first
        .command(1, "publish", first_say.clone())
        .await
        .unwrap();
    assert_eq!(answer["status"], "accepted");
    assert!(
        first.outbox.lock().await.is_empty(),
        "accepted outbox was cleared"
    );
    assert_eq!(
        first
            .command(1, "publish", first_say.clone())
            .await
            .unwrap(),
        answer
    );
    assert_eq!(first_calls.load(Ordering::Relaxed), 1);
    let mut conflict = first_say.clone();
    conflict["text"] = json!("Changed words");
    assert_eq!(
        first.command(1, "publish", conflict).await.unwrap()["reason"],
        "identity_conflict"
    );

    let first_replay = json!({"client_ref":"t-1","utterance_id":"u-1","replay_id":"r-1",
        "mode":"resume","session_id":"start-inc-a","revision":0,"epoch":0,
        "context_message_id":"m-1"});
    let replayed = first
        .command(1, "replay", first_replay.clone())
        .await
        .unwrap();
    assert_eq!(replayed["status"], "accepted");
    assert_eq!(
        first.command(1, "replay", first_replay).await.unwrap(),
        replayed
    );
    assert_eq!(first_calls.load(Ordering::Relaxed), 2);

    first.link.detached().await;
    assert_eq!(
        first
            .command(1, "publish", first_say.clone())
            .await
            .unwrap()["status"],
        "admission_unknown",
        "a disconnected Core cannot prove that the old binding is still owned"
    );
    let mut not_sent = first_say.clone();
    not_sent["event_id"] = json!("e-new");
    not_sent["utterance_id"] = json!("u-new");
    let unavailable = first.command(1, "publish", not_sent).await.unwrap_err();
    assert!(
        unavailable.to_string().contains("core_unavailable"),
        "{unavailable}"
    );
    assert!(first.outbox.lock().await.is_empty());
    assert!(
        first
            .command(
                1,
                "replay",
                json!({"client_ref":"t-1",
            "utterance_id":"u-1","replay_id":"r-1","mode":"resume",
            "session_id":"start-inc-a","revision":0,"epoch":0,
            "context_message_id":"m-1"})
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("indeterminate"),
        "a settled replay retry cannot claim a proven owner while disconnected"
    );

    drop(first);
    let second_link = Link::new();
    let second_calls = Arc::new(AtomicUsize::new(0));
    mock_v1(
        second_link.clone(),
        second_link.attached().await,
        "inc-b",
        second_calls.clone(),
    );
    let (second, _) = daemon(&scratch, second_link);
    second
        .command(1, "register", registration("t-1"))
        .await
        .unwrap();
    let mut old_say = first_say;
    old_say["session_id"] = json!("start-inc-b"); // omitted context silently refreshed by MCP
    assert_eq!(
        second.command(1, "publish", old_say).await.unwrap()["reason"],
        "context_stale"
    );
    let old_replay = json!({"client_ref":"t-1","utterance_id":"u-1","replay_id":"r-1",
        "mode":"resume","session_id":"start-inc-b","revision":0,"epoch":0,
        "context_message_id":"m-2"});
    assert_eq!(
        second.command(1, "replay", old_replay).await.unwrap()["reason"],
        "context_stale"
    );
    assert_eq!(
        second_calls.load(Ordering::Relaxed),
        0,
        "old IDs never reached Core again"
    );
}

#[tokio::test]
async fn uncertain_replay_keeps_its_original_request_across_a_replacement_binding() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let scratch = Scratch::new("v1-replay-rebinding");
    let link = Link::new();
    let mut frames = link.attached().await;
    let answered = link.clone();
    let registrations = Arc::new(AtomicUsize::new(0));
    let count = registrations.clone();
    tokio::spawn(async move {
        while let Some(Message::Text(text)) = frames.recv().await {
            let frame: Value = serde_json::from_str(text.as_str()).unwrap();
            let Some(id) = frame.get("id") else { continue };
            let result = match frame["method"].as_str() {
                Some("binding.register") => {
                    let number = count.fetch_add(1, Ordering::Relaxed);
                    json!({"binding_id":if number == 0 { "original" } else { "replacement" },
                        "voice_contract":1,"voice_contract_instance":"same-core-incarnation",
                        "initial_reply_context":{"session_id":"start","revision":0,"epoch":0}})
                }
                Some("speech.replay") => {
                    assert_eq!(frame["params"]["binding_id"], "original");
                    assert_eq!(frame["params"]["recovery_binding_id"], "replacement");
                    assert_eq!(
                        frame["params"]["voice_contract_instance"],
                        "same-core-incarnation"
                    );
                    assert!(matches!(
                        frame["params"]["replay_id"].as_str(),
                        Some("r-lost" | "r-fresh")
                    ));
                    json!({"voice_contract":1,"status":"accepted","playback":"queued",
                        "utterance_id":"u-old","replay_id":frame["params"]["replay_id"],
                        "attempt_id":if frame["params"]["replay_id"] == "r-lost" {
                            "unchanged-attempt" } else { "fresh-attempt" }})
                }
                Some("speech.get_reply") => {
                    assert_eq!(frame["params"]["binding_id"], "original");
                    assert_eq!(frame["params"]["recovery_binding_id"], "replacement");
                    assert_eq!(
                        frame["params"]["voice_contract_instance"],
                        "same-core-incarnation"
                    );
                    json!({"voice_contract":1,"status":"available","utterance_id":"u-old","text":"Original"})
                }
                _ => json!({}),
            };
            answered
                .answer(json!({"jsonrpc":"2.0","id":id,"result":result}))
                .await
                .unwrap();
        }
    });
    let (daemon, _) = daemon(&scratch, link);
    daemon
        .command(1, "register", registration("t-1"))
        .await
        .unwrap();
    let mut record = Daemon::identity_record(
        "replay",
        "t-1",
        "r-lost",
        "u-old",
        "same-core-incarnation",
        "original",
        json!({"client_ref":"t-1","utterance_id":"u-old",
            "replay_id":"r-lost","mode":"resume","session_id":"s","revision":1,
            "epoch":0,"context_message_id":"m"}),
    )
    .unwrap();
    record["request"] = json!({"voice_contract":1,"binding_id":"original",
        "voice_contract_instance":"same-core-incarnation","utterance_id":"u-old",
        "replay_id":"r-lost","mode":"resume","session_id":"s","revision":1,
        "epoch":0,"context_message_id":"m"});
    assert_eq!(
        daemon.remember_identity(record).await.unwrap(),
        super::IdentityCheck::Fresh
    );
    let mut original = Daemon::identity_record(
        "say",
        "t-1",
        "e-old",
        "u-old",
        "same-core-incarnation",
        "original",
        json!({"original":"fixture"}),
    )
    .unwrap();
    original["binding_id"] = json!("original");
    assert_eq!(
        daemon.remember_identity(original).await.unwrap(),
        super::IdentityCheck::Fresh
    );
    daemon
        .command(1, "register", registration("t-1"))
        .await
        .unwrap();
    let recovered = daemon
        .command(
            1,
            "replay",
            json!({"client_ref":"t-1",
        "utterance_id":"u-old","replay_id":"r-lost","mode":"resume",
        "session_id":"s","revision":1,"epoch":0,"context_message_id":"m"}),
        )
        .await
        .unwrap();
    assert_eq!(recovered["status"], "accepted", "{recovered}");
    assert_eq!(recovered["attempt_id"], "unchanged-attempt");
    let lookup = daemon
        .command(
            1,
            "get_reply",
            json!({"client_ref":"t-1",
        "utterance_id":"u-old"}),
        )
        .await
        .unwrap();
    assert_eq!(lookup["status"], "available", "{lookup}");
    let fresh = daemon
        .command(
            1,
            "replay",
            json!({"client_ref":"t-1",
        "utterance_id":"u-old","replay_id":"r-fresh","mode":"restart",
        "session_id":"s","revision":1,"epoch":0,"context_message_id":"m"}),
        )
        .await
        .unwrap();
    assert_eq!(fresh["attempt_id"], "fresh-attempt", "{fresh}");
}

#[tokio::test]
async fn settled_ids_need_live_owner_proof_after_replacement_and_retire_on_leave() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let scratch = Scratch::new("v1-binding-lifetimes");
    let link = Link::new();
    let mut frames = link.attached().await;
    let answered = link.clone();
    let registrations = Arc::new(AtomicUsize::new(0));
    let publications = Arc::new(AtomicUsize::new(0));
    let replays = Arc::new(AtomicUsize::new(0));
    let proof_available = Arc::new(AtomicBool::new(true));
    let (registered, published, replayed) =
        (registrations.clone(), publications.clone(), replays.clone());
    let proof = proof_available.clone();
    tokio::spawn(async move {
        while let Some(Message::Text(text)) = frames.recv().await {
            let frame: Value = serde_json::from_str(text.as_str()).unwrap();
            let Some(id) = frame.get("id") else { continue };
            let params = &frame["params"];
            let result = match frame["method"].as_str() {
                Some("binding.register") => {
                    let number = registered.fetch_add(1, Ordering::Relaxed);
                    json!({"binding_id":format!("binding-{number}"),"voice_contract":1,
                        "voice_contract_instance":"same-instance",
                        "initial_reply_context":{"session_id":"start","revision":0,"epoch":0}})
                }
                Some("speech.publish") => {
                    published.fetch_add(1, Ordering::Relaxed);
                    assert!(matches!(
                        params["binding_id"].as_str(),
                        Some("binding-0" | "binding-2")
                    ));
                    json!({"voice_contract":1,"status":"accepted","playback":"queued",
                        "event_id":params["event_id"],"utterance_id":params["utterance_id"],
                        "attempt_id":"say-attempt"})
                }
                Some("speech.replay") => {
                    replayed.fetch_add(1, Ordering::Relaxed);
                    assert!(matches!(
                        params["binding_id"].as_str(),
                        Some("binding-0" | "binding-2")
                    ));
                    json!({"voice_contract":1,"status":"accepted","playback":"queued",
                        "replay_id":params["replay_id"],"utterance_id":params["utterance_id"],
                        "attempt_id":"replay-attempt"})
                }
                Some("speech.get_reply") => {
                    assert!(matches!(
                        params["binding_id"].as_str(),
                        Some("binding-0" | "binding-2")
                    ));
                    if params.get("recovery_binding_id").is_some() {
                        assert!(matches!(
                            params["recovery_binding_id"].as_str(),
                            Some("binding-1" | "binding-3")
                        ));
                    }
                    if proof.load(Ordering::Relaxed) {
                        json!({"voice_contract":1,"status":"available","utterance_id":params["utterance_id"],
                            "text":"Original"})
                    } else {
                        json!({"voice_contract":1,"status":"unavailable","utterance_id":"u-one"})
                    }
                }
                _ => json!({}),
            };
            answered
                .answer(json!({"jsonrpc":"2.0","id":id,"result":result}))
                .await
                .unwrap();
        }
    });
    let (daemon, _) = daemon(&scratch, link);
    daemon
        .command(1, "register", registration("t-one"))
        .await
        .unwrap();
    let say = json!({"voice_contract":1,"client_ref":"t-one","session_id":"start",
        "revision":0,"epoch":0,"text":"Original","event_id":"e-one",
        "utterance_id":"u-one"});
    let original = daemon.command(1, "publish", say.clone()).await.unwrap();
    let replay = json!({"client_ref":"t-one","utterance_id":"u-one",
        "replay_id":"r-one","mode":"resume","session_id":"start",
        "revision":0,"epoch":0,"context_message_id":"m-one"});
    let replay_result = daemon.command(1, "replay", replay.clone()).await.unwrap();
    daemon
        .command(1, "register", registration("t-one"))
        .await
        .unwrap();
    assert_eq!(
        daemon.command(1, "publish", say.clone()).await.unwrap(),
        original
    );
    assert_eq!(
        daemon.command(1, "replay", replay.clone()).await.unwrap(),
        replay_result
    );
    assert_eq!(publications.load(Ordering::Relaxed), 1);
    assert_eq!(replays.load(Ordering::Relaxed), 1);

    proof_available.store(false, Ordering::Relaxed);
    let unproven = daemon.command(1, "publish", say.clone()).await.unwrap();
    assert_eq!(unproven["status"], "admission_unknown", "{unproven}");
    assert!(daemon
        .command(1, "replay", replay.clone())
        .await
        .unwrap_err()
        .to_string()
        .contains("indeterminate"));
    proof_available.store(true, Ordering::Relaxed);

    daemon
        .queue(json!({"voice_contract":1,"client_ref":"t-one",
        "event_id":"pending-event","utterance_id":"pending-utterance",
        "binding_id":"binding-1","text":"uncertain","attempted":true}))
        .await
        .unwrap();

    daemon
        .command(1, "unregister", json!({"client_ref":"t-one"}))
        .await
        .unwrap();
    assert!(
        daemon.outbox.lock().await.is_empty(),
        "a deliberate leave discards old uncertain speech without recovering it under a rejoin"
    );
    daemon
        .command(1, "register", registration("t-one"))
        .await
        .unwrap();
    let denied = daemon.command(1, "publish", say).await.unwrap();
    assert_eq!(denied["reason"], "context_stale", "{denied}");
    let denied = daemon.command(1, "replay", replay).await.unwrap();
    assert_eq!(denied["reason"], "context_stale", "{denied}");
    assert_eq!(publications.load(Ordering::Relaxed), 1);
    assert_eq!(replays.load(Ordering::Relaxed), 1);

    // A room-initiated close is another deliberate lifetime boundary, even though the
    // Core can authorize same-owner recovery for an ordinary replacement binding.
    let second_say = json!({"voice_contract":1,"client_ref":"t-one",
        "session_id":"start","revision":0,"epoch":0,"text":"Second original",
        "event_id":"e-two","utterance_id":"u-two"});
    let second_replay = json!({"client_ref":"t-one","utterance_id":"u-two",
        "replay_id":"r-two","mode":"resume","session_id":"start",
        "revision":0,"epoch":0,"context_message_id":"m-two"});
    assert_eq!(
        daemon
            .command(1, "publish", second_say.clone())
            .await
            .unwrap()["status"],
        "accepted"
    );
    assert_eq!(
        daemon
            .command(1, "replay", second_replay.clone())
            .await
            .unwrap()["status"],
        "accepted"
    );
    assert_eq!(
        daemon
            .handle_incoming(
                "binding.close",
                &json!({"binding_id":"binding-2","reason":"room_closed"})
            )
            .await,
        json!({})
    );
    daemon
        .command(1, "register", registration("t-one"))
        .await
        .unwrap();
    let denied = daemon.command(1, "publish", second_say).await.unwrap();
    assert_eq!(denied["reason"], "context_stale", "{denied}");
    let denied = daemon.command(1, "replay", second_replay).await.unwrap();
    assert_eq!(denied["reason"], "context_stale", "{denied}");
    assert_eq!(publications.load(Ordering::Relaxed), 2);
    assert_eq!(replays.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn a_closed_registration_on_welcome_retires_old_ids_before_a_fresh_join() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let scratch = Scratch::new("v1-closed-reregistration");
    let link = Link::new();
    let mut frames = link.attached().await;
    let answered = link.clone();
    let closed = Arc::new(AtomicBool::new(false));
    let publications = Arc::new(AtomicUsize::new(0));
    let replays = Arc::new(AtomicUsize::new(0));
    let (closed_core, published, replayed) =
        (closed.clone(), publications.clone(), replays.clone());
    tokio::spawn(async move {
        while let Some(Message::Text(text)) = frames.recv().await {
            let frame: Value = serde_json::from_str(text.as_str()).unwrap();
            let Some(id) = frame.get("id") else { continue };
            let p = &frame["params"];
            let result = match frame["method"].as_str() {
                Some("binding.register")
                    if p["binding_id"] == "old-binding" && closed_core.load(Ordering::Relaxed) =>
                {
                    json!({"status":"closed","reason":"binding_closed",
                        "binding_id":"old-binding","voice_contract":1})
                }
                Some("binding.register") => {
                    json!({"binding_id":if p["binding_id"] == "old-binding" {
                        "old-binding" } else if closed_core.load(Ordering::Relaxed) {
                        "new-binding" } else { "old-binding" },
                        "voice_contract":1,"voice_contract_instance":"same-instance",
                        "initial_reply_context":{"session_id":"start","revision":0,"epoch":0}})
                }
                Some("speech.publish") => {
                    published.fetch_add(1, Ordering::Relaxed);
                    json!({"voice_contract":1,"status":"accepted","playback":"queued",
                        "event_id":p["event_id"],"utterance_id":p["utterance_id"],
                        "attempt_id":"first-attempt"})
                }
                Some("speech.replay") => {
                    replayed.fetch_add(1, Ordering::Relaxed);
                    json!({"voice_contract":1,"status":"accepted","playback":"queued",
                        "replay_id":p["replay_id"],"utterance_id":p["utterance_id"],
                        "attempt_id":"replay-attempt"})
                }
                Some("speech.get_reply") => json!({"voice_contract":1,"status":"available",
                    "utterance_id":p["utterance_id"],"text":"Original"}),
                _ => json!({}),
            };
            answered
                .answer(json!({"jsonrpc":"2.0","id":id,"result":result}))
                .await
                .unwrap();
        }
    });
    let (daemon, _) = daemon(&scratch, link);
    daemon
        .command(1, "register", registration("t-1"))
        .await
        .unwrap();
    let say = json!({"voice_contract":1,"client_ref":"t-1","session_id":"start",
        "revision":0,"epoch":0,"text":"Original","event_id":"old-event",
        "utterance_id":"old-utterance"});
    let replay = json!({"client_ref":"t-1","utterance_id":"old-utterance",
        "replay_id":"old-replay","mode":"resume","session_id":"start",
        "revision":0,"epoch":0,"context_message_id":"m-1"});
    assert_eq!(
        daemon.command(1, "publish", say.clone()).await.unwrap()["status"],
        "accepted"
    );
    assert_eq!(
        daemon.command(1, "replay", replay.clone()).await.unwrap()["status"],
        "accepted"
    );
    daemon
        .queue(json!({"voice_contract":1,"client_ref":"t-1",
        "event_id":"uncertain-event","utterance_id":"uncertain-utterance",
        "binding_id":"old-binding","text":"uncertain","attempted":true}))
        .await
        .unwrap();
    closed.store(true, Ordering::Relaxed);
    daemon.on_welcome().await;
    assert!(daemon.bindings.lock().await.is_empty());
    assert!(daemon.outbox.lock().await.is_empty());
    assert_eq!(daemon.closed_by_room.lock().await["t-1"], "binding_closed");
    let new_join = daemon
        .command(1, "register", registration("t-1"))
        .await
        .unwrap();
    assert_eq!(new_join["binding_id"], "new-binding");
    assert_eq!(
        daemon.command(1, "publish", say).await.unwrap()["reason"],
        "context_stale"
    );
    assert_eq!(
        daemon.command(1, "replay", replay).await.unwrap()["reason"],
        "context_stale"
    );
    assert_eq!(publications.load(Ordering::Relaxed), 1);
    assert_eq!(replays.load(Ordering::Relaxed), 1);
}

#[test]
fn t16_complete_pull_pages_fit_ipc_and_keep_the_next_position() {
    let source: Vec<Value> = (1..=32)
        .map(|n| {
            let mut message = json!({"message_id":format!("m-{n}"),"session_id":"s","revision":n,
                "epoch":0,"channel":"voice","text":"\"".repeat(12_000),
                "arrival_time":1792000000});
            message[PULL_POSITION_FIELD] = json!(n);
            message
        })
        .collect();
    let raw = json!({"messages":source.clone(),"count":32,"more":false});
    assert!(serde_json::to_vec(&raw).unwrap().len() < 1 << 20);
    let mut delivered = Vec::new();
    let mut position = 0;
    while delivered.len() < 32 {
        let mut page = json!({"messages":source.iter().filter(|m| m[PULL_POSITION_FIELD].as_u64().unwrap() > position).cloned().collect::<Vec<_>>(),
            "count":32-delivered.len(),"more":false});
        prepare_pull_page(&mut page, position).unwrap();
        let messages = page["messages"].as_array().unwrap();
        assert!(!messages.is_empty());
        assert!(
            serde_json::to_vec(&json!({"id":u64::MAX,"ok":true,"result":page.clone()}))
                .unwrap()
                .len()
                < crate::IPC_FRAME_LIMIT
        );
        for message in messages {
            assert!(message["envelope"]
                .as_str()
                .unwrap()
                .contains(&"\"".repeat(12_000)));
            delivered.push(message["message_id"].as_str().unwrap().to_owned());
        }
        position = page[PULL_POSITION_FIELD].as_u64().unwrap();
        assert_eq!(page["more"], delivered.len() < 32);
    }
    assert_eq!(
        delivered,
        (1..=32).map(|n| format!("m-{n}")).collect::<Vec<_>>()
    );
}

/// The core link: the frames a newer core sends this daemon (`connector.welcome`, `input.deliver`,
/// `node.rendezvous`, `binding.close`, a method it has never heard of) and its answers to the daemon's own requests
/// (`binding.register`, `speech.publish`), all with extra fields, nested too, and enum values it does not know.
#[tokio::test]
async fn core_link_frames_with_unknown_fields_and_values_are_read_for_what_they_know() {
    let scratch = Scratch::new("contract-link");
    let link = Link::new();
    let core = link.attached().await;
    play_core(link.clone(), core);
    let (daemon, frames) = daemon(&scratch, link);

    // binding.register answered with `"error": null` and fields of its own: the binding is the core's.
    let joined = daemon
        .command(1, "register", registration("t-1"))
        .await
        .unwrap();
    assert_eq!(joined["binding_id"], "core-t-1", "{joined}");
    assert_eq!(joined["connected"], true, "{joined}");

    let welcome = daemon
        .handle_incoming(
            "connector.welcome",
            &json!({"core":{"version":"9.9.9"},"future":[1]}),
        )
        .await;
    assert_eq!(welcome, json!({}));

    // input.deliver with extra keys, nested ones in `unheard`, and a channel this connector does not know: delivered,
    // and the agent reads it as voice, the default.
    let frame = json!({"binding_id":"core-t-1","message_id":"m-1","session_id":"s","revision":3,"epoch":1,
        "text":"hello","channel":"a_channel_from_the_future","future_field":{"nested":true},
        "unheard":{"count":1,"replies":[{"text":"earlier","future_reply_field":1}],"future":"x"}});
    let delivered = daemon.handle_incoming("input.deliver", &frame).await;
    assert_eq!(delivered["status"], "accepted", "{delivered}");
    assert_eq!(
        frames.lock().unwrap().as_slice(),
        std::slice::from_ref(&frame)
    );
    assert_eq!(
        daemon.handle_incoming("input.deliver", &frame).await["status"],
        "accepted"
    );
    let mut conflict = frame.clone();
    conflict["text"] = json!("different words under the same ID");
    assert_eq!(
        daemon.handle_incoming("input.deliver", &conflict).await["detail"],
        "identity_conflict"
    );
    assert_eq!(frames.lock().unwrap().len(), 1);
    let envelope = crate::adapters::envelope(&frame).unwrap();
    let header: Value = serde_json::from_str(envelope.lines().next().unwrap()).unwrap();
    assert_eq!(
        header,
        json!({"channel":"voice","origin":"spoken_room","session_id":"s","revision":3,"epoch":1,
            "message_id":"m-1","hearing_changes":[],
            "playback_at_turn_start":{"knowledge":"unknown","last_played":null,"recent":[],"truncated":false}})
    );
    assert!(envelope.ends_with("\n\nhello"));
    assert!(!envelope.contains("[Sidevoice]"));

    // speech.publish answered with a status and fields this connector does not know: saved is saved.
    let said = daemon
        .command(
            1,
            "publish",
            json!({"client_ref":"t-1","session_id":"s","revision":3,"epoch":1,"text":"reply",
                "future_publish_field":{"nested":1}}),
        )
        .await
        .unwrap();
    assert_eq!(said["text_saved"], true, "{said}");
    assert_eq!(said["status"], "a_status_from_the_future", "{said}");
    assert!(
        daemon.outbox.lock().await.is_empty(),
        "the saved speech left the outbox"
    );
    let refused = daemon
        .command(
            1,
            "publish",
            json!({"client_ref":"t-1","session_id":"s",
        "revision":3,"epoch":1,"text":"during turn"}),
        )
        .await
        .unwrap();
    assert_eq!(refused["status"], "rejected", "{refused}");
    assert_eq!(refused["reason"], "user_speaking", "{refused}");
    assert!(
        daemon.outbox.lock().await.is_empty(),
        "a terminal refusal cannot retry as speech"
    );

    // node.rendezvous with fields of its own and no refusal: nothing is refused.
    let rendezvous = daemon
        .handle_incoming(
            "node.rendezvous",
            &json!({"refused":null,"reachable":true,"future":{"nested":{"x":1}}}),
        )
        .await;
    assert_eq!(rendezvous, json!({}));
    let status = daemon.command(1, "status", json!({})).await.unwrap();
    assert_eq!(status["refused"], Value::Null, "{status}");
    assert_eq!(status["bindings"][0]["binding_id"], "core-t-1", "{status}");

    // A method this connector has never heard of, asked with an ID: a defined answer, and the link stays.
    let unknown = daemon
        .handle_incoming("a.method.from.the.future", &json!({"x":1}))
        .await;
    assert_eq!(unknown, json!({"error":{"key":"connector.unknown-method"}}));

    // binding.close with fields of its own and a reason this connector does not know: closed, with that reason.
    let closed = daemon
        .handle_incoming(
            "binding.close",
            &json!({"binding_id":"core-t-1","reason":"a_reason_from_the_future","future":{"by":"room"}}),
        )
        .await;
    assert_eq!(closed, json!({}));
    let status = daemon.command(1, "status", json!({})).await.unwrap();
    assert_eq!(status["bindings"], json!([]), "{status}");
    assert_eq!(
        status["closed_reasons"]["t-1"], "a_reason_from_the_future",
        "{status}"
    );
}

#[tokio::test]
async fn unresolved_other_conversation_does_not_hide_live_refusal() {
    let scratch = Scratch::new("outbox-other-conversation");
    let link = Link::new();
    let core = link.attached().await;
    play_core(link.clone(), core);
    let (daemon, _) = daemon(&scratch, link);
    daemon
        .command(1, "register", registration("a"))
        .await
        .unwrap();
    daemon
        .command(2, "register", registration("b"))
        .await
        .unwrap();
    daemon
        .queue(
            json!({"client_ref":"a","binding_id":"core-a","event_id":"event-a",
        "utterance_id":"utterance-a","session_id":"s","revision":1,"epoch":0,
        "text":"unresolved","history_only":false,"attempted":true}),
        )
        .await
        .unwrap();
    let refused = daemon
        .command(
            2,
            "publish",
            json!({"client_ref":"b","session_id":"s",
        "revision":1,"epoch":0,"text":"during turn"}),
        )
        .await
        .unwrap();
    assert_eq!(refused["status"], "rejected", "{refused}");
    assert_eq!(refused["reason"], "user_speaking", "{refused}");
    assert!(refused["instruction"].as_str().unwrap().contains("wait"));
    let outbox = daemon.outbox.lock().await;
    assert_eq!(outbox.len(), 1, "{outbox:?}");
    assert_eq!(outbox[0]["event_id"], "event-a");
}

#[tokio::test]
async fn uncertain_publication_keeps_its_fingerprint_and_unsent_work_is_suppressed() {
    let scratch = Scratch::new("outbox-immutable");
    let link = Link::new();
    let core = link.attached().await;
    play_core(link.clone(), core);
    let (daemon, _) = daemon(&scratch, link);
    daemon
        .command(1, "register", registration("a"))
        .await
        .unwrap();
    let base = json!({"client_ref":"a","binding_id":"core-a","session_id":"s",
        "revision":1,"epoch":0,"text":"reply","history_only":false});
    let mut attempted = base.clone();
    attempted["event_id"] = json!("event-a");
    attempted["utterance_id"] = json!("utterance-a");
    attempted["attempted"] = json!(true);
    daemon.queue(attempted).await.unwrap();
    let mut unsent = base;
    unsent["event_id"] = json!("event-b");
    unsent["utterance_id"] = json!("utterance-b");
    daemon.queue(unsent).await.unwrap();
    daemon.retire_held_audio().await.unwrap();
    let outbox = daemon.outbox.lock().await.clone();
    assert_eq!(outbox[0]["history_only"], false);
    assert_eq!(outbox[0]["attempted"], true);
    assert_eq!(outbox[1]["history_only"], true);
    daemon.flush_outbox().await;
    let outbox = daemon.outbox.lock().await.clone();
    assert_eq!(outbox.len(), 1, "{outbox:?}");
    assert_eq!(outbox[0]["event_id"], "event-a");
}

#[tokio::test]
async fn paired_retry_ids_share_one_pending_publication_and_reject_conflicting_reuse() {
    let scratch = Scratch::new("pending-retry-ids");
    let (daemon, _) = daemon(&scratch, Link::new());
    daemon
        .command(1, "register", registration("a"))
        .await
        .unwrap();
    let args = json!({"client_ref":"a","session_id":"s","revision":1,"epoch":0,
        "text":"same reply","event_id":"same-event","utterance_id":"same-utterance"});
    let first = daemon.command(1, "publish", args.clone()).await.unwrap();
    let second = daemon.command(1, "publish", args.clone()).await.unwrap();
    assert_eq!(first["status"], "queued", "{first}");
    assert_eq!(second["status"], "queued", "{second}");
    assert_eq!(daemon.outbox.lock().await.len(), 1);
    let mut changed = args;
    changed["text"] = json!("different reply");
    assert!(daemon
        .command(1, "publish", changed)
        .await
        .unwrap_err()
        .to_string()
        .contains("conflict"));
    assert_eq!(daemon.outbox.lock().await.len(), 1);
}

#[tokio::test]
async fn lost_admission_recovers_original_and_does_not_bar_new_audio() {
    for outcome in ["saved", "refused", "missing"] {
        let scratch = Scratch::new(&format!("lost-admission-{outcome}"));
        let link = Link::new();
        let mut frames = link.attached().await;
        let (published_tx, mut published_rx) = tokio::sync::mpsc::unbounded_channel();
        let answering_link = link.clone();
        tokio::spawn(async move {
            while let Some(Message::Text(text)) = frames.recv().await {
                let frame: Value = serde_json::from_str(text.as_str()).unwrap();
                if frame["method"] == "binding.register" {
                    answering_link.answer(json!({"jsonrpc":"2.0","id":frame["id"],
                        "result":{"binding_id":"core-a","initial_reply_context":{"session_id":"s","revision":1,"epoch":0}}})).await.unwrap();
                } else if frame["method"] == "speech.publish" {
                    published_tx.send(frame["params"].clone()).unwrap();
                    if frame["params"]["event_id"] == "next-event" {
                        answering_link
                            .answer(json!({"jsonrpc":"2.0","id":frame["id"],
                            "result":{"event_id":"next-event","utterance_id":"next-utterance",
                                "status":"published","text_saved":true}}))
                            .await
                            .unwrap();
                    }
                } else if frame["method"] == "speech.recover" {
                    let params = &frame["params"];
                    assert_eq!(params["recovery_binding_id"], "core-a");
                    assert_eq!(params["history_only"], false);
                    assert!(params.get("attempted").is_none());
                    let result = if outcome == "refused" {
                        json!({"event_id":params["event_id"],"utterance_id":params["utterance_id"],
                            "status":"rejected","terminal":true,"reason":"user_speaking"})
                    } else {
                        json!({"event_id":params["event_id"],"utterance_id":params["utterance_id"],
                            "status":if outcome == "missing" { "text_only" } else { "published" },
                            "reason":if outcome == "missing" { "connector_disconnected" } else { "" },
                            "text_saved":true})
                    };
                    answering_link
                        .answer(json!({"jsonrpc":"2.0","id":frame["id"],
                        "result":result}))
                        .await
                        .unwrap();
                }
            }
        });
        let (daemon, _) = daemon(&scratch, link);
        daemon
            .command(1, "register", registration("a"))
            .await
            .unwrap();
        let args = json!({"client_ref":"a","session_id":"s",
            "revision":1,"epoch":0,"text":format!("{outcome} reply"),
            "event_id":format!("{outcome}-event"),"utterance_id":format!("{outcome}-utterance")});
        let answer = daemon.command(1, "publish", args.clone()).await.unwrap();
        assert_eq!(answer["status"], "admission_unknown", "{answer}");
        let sent = published_rx.try_recv().unwrap();
        assert_eq!(sent["history_only"], false, "{sent}");
        assert!(
            sent.get("attempted").is_none(),
            "local state leaked to core: {sent}"
        );
        daemon.retire_held_audio().await.unwrap();
        let recovered = daemon.command(1, "publish", args).await.unwrap();
        assert_eq!(
            recovered["status"],
            if outcome == "refused" {
                "rejected"
            } else if outcome == "missing" {
                "text_only"
            } else {
                "published"
            },
            "{recovered}"
        );
        let retained = daemon.outbox.lock().await.clone();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0]["resolved"], recovered);
        let next = daemon
            .command(
                1,
                "publish",
                json!({"client_ref":"a","session_id":"s",
            "revision":1,"epoch":1,"text":"fresh answer","event_id":"next-event",
            "utterance_id":"next-utterance"}),
            )
            .await
            .unwrap();
        assert_eq!(next["status"], "published", "{next}");
        let sent_next = published_rx.try_recv().unwrap();
        assert_eq!(sent_next["event_id"], "next-event");
        assert_eq!(sent_next["history_only"], false);
        assert!(
            published_rx.try_recv().is_err(),
            "uncertain first admission was replayed"
        );
    }
}

#[tokio::test]
async fn recovery_re_registers_an_unknown_binding_without_changing_original_fields() {
    let scratch = Scratch::new("recover-unknown-binding");
    let link = Link::new();
    let mut frames = link.attached().await;
    let answering_link = link.clone();
    tokio::spawn(async move {
        let mut registrations = 0;
        let mut recoveries = 0;
        while let Some(Message::Text(text)) = frames.recv().await {
            let frame: Value = serde_json::from_str(text.as_str()).unwrap();
            if frame["method"] == "binding.register" {
                registrations += 1;
                let id = if registrations == 1 {
                    "core-a"
                } else {
                    "core-a-new"
                };
                answering_link
                    .answer(json!({"jsonrpc":"2.0","id":frame["id"],
                    "result":{"binding_id":id,"initial_reply_context":{}}}))
                    .await
                    .unwrap();
            } else if frame["method"] == "speech.recover" {
                recoveries += 1;
                let params = &frame["params"];
                assert_eq!(params["binding_id"], "core-a");
                assert_eq!(params["history_only"], false);
                assert_eq!(
                    params["recovery_binding_id"],
                    if recoveries == 1 {
                        "core-a"
                    } else {
                        "core-a-new"
                    }
                );
                let result = if recoveries == 1 {
                    json!({"status":"unknown_binding","event_id":params["event_id"],
                        "utterance_id":params["utterance_id"]})
                } else {
                    json!({"status":"text_only","reason":"connector_disconnected","text_saved":true,
                        "event_id":params["event_id"],"utterance_id":params["utterance_id"]})
                };
                answering_link
                    .answer(json!({"jsonrpc":"2.0","id":frame["id"],"result":result}))
                    .await
                    .unwrap();
            } else if frame["method"] == "speech.publish" {
                panic!("uncertain first admission was blindly retried");
            }
        }
    });
    let (daemon, _) = daemon(&scratch, link);
    daemon
        .command(1, "register", registration("a"))
        .await
        .unwrap();
    daemon
        .queue(
            json!({"client_ref":"a","binding_id":"core-a","event_id":"event-a",
        "utterance_id":"utterance-a","session_id":"s","revision":1,"epoch":0,
        "text":"reply","language":null,"history_only":false,"attempted":true}),
        )
        .await
        .unwrap();
    let result = daemon
        .command(
            1,
            "publish",
            json!({"client_ref":"a","session_id":"s",
        "revision":1,"epoch":0,"text":"reply","event_id":"event-a",
        "utterance_id":"utterance-a"}),
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "text_only", "{result}");
    assert_eq!(result["reason"], "connector_disconnected", "{result}");
    let retained = daemon.outbox.lock().await.clone();
    assert_eq!(retained[0]["binding_id"], "core-a");
    assert_eq!(retained[0]["resolved"], result);
}

/// The façade↔daemon IPC: every command a newer `sidevoice mcp` sends, with fields this daemon does not know at the
/// top and nested, is answered for what it knows. A command it has never heard of gets a defined refusal.
#[tokio::test]
async fn ipc_commands_with_unknown_fields_are_read_for_what_they_know() {
    let scratch = Scratch::new("contract-ipc");
    let (daemon, _) = daemon(&scratch, Link::new());

    let joined = daemon
        .command(7, "register", registration("t-2"))
        .await
        .unwrap();
    assert_eq!(joined["thread"], "t-2", "{joined}");
    let binding_id = joined["binding_id"].as_str().unwrap().to_owned();

    let status = daemon
        .command(7, "status", json!({"future":{"verbose":true}}))
        .await
        .unwrap();
    let listed = &status["bindings"][0];
    assert_eq!(listed["client_ref"], "t-2", "{status}");
    assert_eq!(listed["engine"]["model"], "m-1", "{status}");
    assert_eq!(
        listed["delivery"]["kind"], "recorder",
        "the route is kept as it was sent: {status}"
    );

    // No core: the speech is queued, whatever else the call carried.
    let said = daemon
        .command(
            7,
            "publish",
            json!({"client_ref":"t-2","session_id":"s","revision":1,"epoch":0,"text":"queued",
                "language":"a_language_from_the_future","future":{"nested":[1]}}),
        )
        .await
        .unwrap();
    assert_eq!(said["status"], "queued", "{said}");
    let second = daemon
        .command(
            7,
            "publish",
            json!({"client_ref":"t-2","session_id":"s","revision":1,"epoch":0,
                "text":"queued second"}),
        )
        .await
        .unwrap();
    assert_eq!(second["status"], "queued", "{second}");
    let outbox = daemon.outbox.lock().await.clone();
    assert_eq!(outbox.len(), 2);
    assert_eq!(outbox[0]["text"], "queued");
    assert_eq!(outbox[1]["text"], "queued second");
    assert!(outbox.iter().all(|item| item["history_only"] == true));
    let persisted: Value =
        serde_json::from_slice(&std::fs::read(scratch.0.join("sidevoice/outbox.json")).unwrap())
            .unwrap();
    assert_eq!(
        persisted,
        json!(outbox),
        "publication order must survive restart"
    );

    // A detachable conversation another façade adopts, asked with fields of its own.
    let left = daemon
        .command(
            7,
            "unregister",
            json!({"binding_id":binding_id,"client_ref":"t-2","future":1}),
        )
        .await
        .unwrap();
    assert_eq!(left["left"], true, "{left}");
    daemon
        .command(7, "register", registration("t-3"))
        .await
        .unwrap();
    daemon.bindings.lock().await["t-3"]
        .owner
        .store(0, std::sync::atomic::Ordering::Relaxed);
    let adopted = daemon
        .command(
            8,
            "adopt",
            json!({"client_ref":"t-3","future":{"nested":true}}),
        )
        .await
        .unwrap();
    assert_eq!(adopted["client_ref"], "t-3", "{adopted}");

    let identity = daemon
        .command(8, "identity", json!({"future":true}))
        .await
        .unwrap();
    assert_eq!(identity["pid"], std::process::id(), "{identity}");

    let unknown = daemon
        .command(8, "a_command_from_the_future", json!({"x":1}))
        .await
        .unwrap_err();
    assert_eq!(unknown.to_string(), "unknown IPC method");
}

#[tokio::test]
async fn a_detached_pull_binding_reconnects_under_its_original_client_ref() {
    let scratch = Scratch::new("pull-reconnect");
    let link = Link::new();
    let core = link.attached().await;
    play_core(link.clone(), core);
    let (daemon, _) = daemon(&scratch, link);
    let mut params = registration("pull-thread");
    params["input_mode"] = json!("pull");
    let first = daemon.command(7, "register", params.clone()).await.unwrap();
    let binding = daemon.bindings.lock().await["pull-thread"].clone();
    binding.owner.store(0, std::sync::atomic::Ordering::Relaxed);
    let second = daemon.command(8, "register", params).await.unwrap();
    assert_eq!(second["binding_id"], first["binding_id"]);
    assert_eq!(
        second["initial_reply_context"],
        first["initial_reply_context"]
    );
    assert_eq!(binding.owner.load(std::sync::atomic::Ordering::Relaxed), 8);
    assert_eq!(*binding.input_mode.lock().await, "pull");
}

#[tokio::test]
async fn acknowledged_pull_pages_keep_recent_routing_bounded_across_rejoin() {
    let scratch = Scratch::new("pull-context-bound");
    let link = Link::new();
    let core = link.attached().await;
    play_core(link.clone(), core);
    let (daemon, _) = daemon(&scratch, link);
    let mut params = registration("pull-bound");
    params["input_mode"] = json!("pull");
    daemon.command(7, "register", params.clone()).await.unwrap();
    let binding = daemon.bindings.lock().await["pull-bound"].clone();
    for after in 0..620 {
        let ack_ids = if after == 0 {
            vec![]
        } else {
            vec![format!("pull-{after}")]
        };
        let page = daemon
            .command(
                7,
                "get_messages",
                json!({"client_ref":"pull-bound","after":after,"ack_ids":ack_ids}),
            )
            .await
            .unwrap();
        assert_eq!(
            page["messages"][0]["message_id"],
            format!("pull-{}", after + 1)
        );
    }
    daemon
        .command(
            7,
            "get_messages",
            json!({"client_ref":"pull-bound","after":620,
        "ack_ids":["pull-620"]}),
        )
        .await
        .unwrap();
    assert!(binding.input_context.lock().await.is_empty());
    assert_eq!(binding.recent_inputs.lock().await.len(), 512);
    assert_eq!(binding.turns.lock().await.len(), 512);
    assert!(!binding.turns.lock().await.contains_key("s:1"));
    binding.owner.store(0, std::sync::atomic::Ordering::Relaxed);
    daemon.command(8, "register", params).await.unwrap();
    assert!(binding.input_context.lock().await.is_empty());
    assert_eq!(binding.recent_inputs.lock().await.len(), 512);
}
