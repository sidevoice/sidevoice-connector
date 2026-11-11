//! Delivery to an editor chat whose Desktop Bridge is not there reaches the harness from a core frame, and is refused
//! with why.

use super::contract_tests::play_core;
use super::Daemon;
use crate::adapters::Registry;
use crate::link::Link;
use crate::profile::Profile;
use serde_json::json;
use tokio::sync::{mpsc, watch};

#[tokio::test]
async fn cursor_push_frame_reaches_bridge_adapter() {
    let scratch = crate::test_support::temp_dir("cursor-push-route");
    let profile = Profile::for_test(scratch.path(), &Registry::homes());
    let registry = Registry::standard(&profile);
    let link = Link::new();
    let core = link.attached().await;
    play_core(link.clone(), core);
    let (replay_tx, _replay_rx) = mpsc::channel(1);
    let (shutdown, _) = watch::channel(false);
    let daemon = Daemon::new(profile, link, replay_tx, registry, false, shutdown).unwrap();
    let id = format!("cursor-editor-{}", uuid::Uuid::new_v4());
    let joined = daemon
        .command(
            1,
            "register",
            json!({"client_ref":id,"harness":"cursor","thread":id}),
        )
        .await
        .unwrap();
    assert_eq!(joined["delivery"], "push", "{joined}");
    let answer = daemon
        .handle_incoming(
            "input.deliver",
            &json!({
                "binding_id":joined["binding_id"],"message_id":"m","session_id":"s",
                "revision":1,"epoch":1,"text":"hello"
            }),
        )
        .await;
    assert_eq!(answer["status"], "failed", "{answer}");
    assert!(
        answer["detail"]
            .as_str()
            .unwrap()
            .contains("Desktop Bridge"),
        "{answer}"
    );
}
