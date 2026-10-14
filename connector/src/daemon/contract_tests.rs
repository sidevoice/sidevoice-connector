//! The protocol evolves like a production API (sidevoice/sidevoice-core#112): a core, a façade or a daemon of another
//! version may send fields and enum values this one does not know. Each is read for what it knows: unknown fields are
//! ignored, an unknown value gets its defined default, and nothing is refused for them.

use super::Daemon;
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
        "unheard":{"count":1,"replies":[{"text":"earlier","cut":false,"future_reply_field":1}],"future":"x"}});
    let delivered = daemon.handle_incoming("input.deliver", &frame).await;
    assert_eq!(delivered["status"], "accepted", "{delivered}");
    assert_eq!(
        frames.lock().unwrap().as_slice(),
        std::slice::from_ref(&frame)
    );
    let envelope = crate::adapters::envelope(&frame).unwrap();
    let header: Value = serde_json::from_str(envelope.lines().next().unwrap()).unwrap();
    assert_eq!(
        header,
        json!({"channel":"voice","session_id":"s","revision":3,"epoch":1,"message_id":"m-1"})
    );
    assert!(envelope.contains("\n\nhello\n\n[Sidevoice] Reply with voice_say"));
    assert!(envelope.contains("Complete unheard evidence"));

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
