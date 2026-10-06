//! Sidevoice with no service manager (`SIDEVOICE_SERVICE_MANAGER=none`: a container, an `su` shell, Linux without
//! a user bus), with the real core and an installation staged in a private profile: a conversation's MCP server
//! starts the connector, the connector starts the core and links to it; a core that dies is started again; the
//! person's retry ends the core and a new one comes; a person's stop ends both and holds, so a conversation is told
//! instead of starting anything, until `service start`.

mod support;

use std::collections::BTreeMap;
use std::process::Stdio;

use serde_json::json;
use support::service::*;
use support::*;

struct Teardown<'a>(&'a Profile, &'a BTreeMap<String, String>);

impl Drop for Teardown<'_> {
    fn drop(&mut self) {
        let _ = service(self.0, self.1, "stop");
    }
}

#[test]
fn with_no_manager_a_conversation_starts_the_connector_and_the_connector_the_core() {
    let Some(core) = core_dir() else { return };
    let extra = BTreeMap::from([("SIDEVOICE_SERVICE_MANAGER".to_owned(), "none".to_owned())]);
    let profile = Profile::new("ondemand");
    stage_installation(&profile, &core);
    let _teardown = Teardown(&profile, &extra);

    let idle = service(&profile, &extra, "status");
    assert_eq!(
        (idle["state"].as_str(), idle["service"].as_str()),
        (Some("not-installed"), Some("none")),
        "{idle}"
    );
    assert_eq!(idle["reachable"], false, "{idle}");
    let refused = service(&profile, &extra, "install");
    assert_eq!(refused["error"]["key"], "service.no-manager", "{refused}");

    // A conversation's MCP server is what gets Sidevoice started; joining keeps its connector busy.
    let receiver = http_receiver();
    let mut conversation = command(&profile, &extra, &["mcp"]);
    conversation
        .env("SIDEVOICE_THREAD", "on-demand-thread")
        .env("SIDEVOICE_DELIVERY_URL", &receiver.url);
    let mut mcp = Mcp::start(conversation);
    mcp.initialize("on-demand");
    let _ = mcp.request(
        "tools/call",
        json!({"name": "voice_connect", "arguments": {"title": "On demand"}}),
    );
    let linked = wait_state(
        &profile,
        &extra,
        "the connector started the core and links to it",
        |status| status["reachable"] == true && status["connector"]["running"] == true,
    );
    assert_eq!(
        linked["state"], "not-installed",
        "no service: a reachable core is still not kept up at login"
    );
    let first = linked["core"]["pid"].as_u64().unwrap();
    profile.wait_linked(linked["core"]["launch_id"].as_str().unwrap());
    let joined = mcp.tool("voice_connect", json!({"title": "On demand"}));
    assert_eq!(joined["conversation"], "on-demand-thread", "{joined}");

    // The core dies: the connector that started it starts another.
    unsafe {
        libc::kill(first as i32, libc::SIGKILL);
    }
    let second = wait_state(
        &profile,
        &extra,
        "the connector started the core again",
        |status| {
            status["reachable"] == true
                && status["core"]["pid"]
                    .as_u64()
                    .is_some_and(|pid| pid != first)
        },
    );
    profile.wait_linked(second["core"]["launch_id"].as_str().unwrap());

    // The person's retry ends the core; the connector starts a new one.
    ok(&profile, &extra, "restart");
    let second_pid = second["core"]["pid"].as_u64().unwrap();
    wait_gone(second_pid, "restart ended the core");
    let third = wait_state(&profile, &extra, "a new core after restart", |status| {
        status["reachable"] == true
            && status["core"]["pid"]
                .as_u64()
                .is_some_and(|pid| pid != second_pid)
    });

    // A person's stop ends both, and holds.
    let stopped = ok(&profile, &extra, "stop");
    assert_eq!(
        (stopped["state"].as_str(), stopped["service"].as_str()),
        (Some("stopped-by-person"), Some("none")),
        "{stopped}"
    );
    wait_gone(
        third["core"]["pid"].as_u64().unwrap(),
        "the stopped core exited",
    );
    assert!(
        !profile.connector_socket().exists(),
        "the connector left its socket"
    );
    mcp.stop();
    let output = command(&profile, &extra, &["mcp"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "a stopped Sidevoice started an MCP server"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("stopped"), "{stderr}");
    // With no service there is no job to be stopped: the status says so (Node's order), and nothing answers.
    let after = service(&profile, &extra, "status");
    assert_eq!(
        (after["state"].as_str(), after["reachable"].as_bool()),
        (Some("not-installed"), Some(false)),
        "{after}"
    );

    // `start` lifts the stop; with no manager nothing runs until a conversation needs it.
    let started = ok(&profile, &extra, "start");
    assert_eq!(started["state"], "not-installed", "{started}");
    assert_eq!(service(&profile, &extra, "status")["reachable"], false);
}
