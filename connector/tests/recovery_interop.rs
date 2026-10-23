//! A voluntary leave while Core is unavailable, exercised with real Core, daemon and MCP processes.
//! Runs against the released Core pinned in core.pin.
mod support;

use serde_json::json;
use support::*;

#[test]
fn agent_leave_while_core_is_down_stays_closed_after_restart_until_explicit_rejoin() {
    let core = core_dir().expect("released pinned Core required");
    let profile = Profile::new("recoverleave");
    let mut first_core = profile.start_core(&core, "before-leave");
    let _connector = profile.start_connector();
    profile.wait_linked("before-leave");
    let call = Presentation::open(&profile.core_endpoint());
    let receiver = http_receiver();
    let thread = "agent-offline-leave";
    let mut command = profile.connector(&["mcp"]);
    command
        .env("SIDEVOICE_THREAD", thread)
        .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
    let mut mcp = Mcp::start(command);
    mcp.initialize("agent-offline-leave");
    let joined = mcp.tool("voice_connect", json!({"title":"Recovery lifecycle"}));
    let original = joined["binding_id"].as_str().unwrap().to_owned();
    let before = core_json(
        &profile.core_endpoint(),
        "GET",
        "/api/presentation/participants",
        None,
        Some(&call.token),
    );
    assert_eq!(
        before["participants"].as_array().unwrap().len(),
        1,
        "{before}"
    );

    first_core.stop();
    let left = mcp.tool("voice_disconnect", json!({"conversation":thread}));
    assert_eq!(left["status"], "left", "{left}");
    let _second_core = profile.start_core(&core, "after-leave");
    profile.wait_linked("after-leave");
    let after = until("durable agent closure reaches Core", 30, || {
        let view = core_json(
            &profile.core_endpoint(),
            "GET",
            "/api/presentation/participants",
            None,
            Some(&call.token),
        );
        (view["closed_threads"]
            .as_array()?
            .iter()
            .any(|item| item == thread))
        .then_some(view)
    });
    assert!(
        after["participants"].as_array().unwrap().is_empty(),
        "{after}"
    );
    until(
        "Core acknowledged and Connector compacted the durable leave",
        15,
        || {
            let saved: serde_json::Value = serde_json::from_slice(
                &std::fs::read(profile.data().join("conversation-state.json")).ok()?,
            )
            .ok()?;
            saved["left_by_agent"].get(thread).is_none().then_some(())
        },
    );
    let fresh = mcp.tool("voice_connect", json!({"title":"Explicit rejoin"}));
    assert_ne!(fresh["binding_id"], original, "{fresh}");
    let recovered = until("one newly joined conversation", 15, || {
        let view = core_json(
            &profile.core_endpoint(),
            "GET",
            "/api/presentation/participants",
            None,
            Some(&call.token),
        );
        (view["participants"].as_array()?.len() == 1).then_some(view)
    });
    assert_eq!(recovered["participants"][0]["thread_id"], thread);
    assert_eq!(recovered["closed_threads"], json!([]));
    mcp.stop();
}
