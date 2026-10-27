//! Sidevoice with no service manager (`SIDEVOICE_SERVICE_MANAGER=none`: a container, an `su` shell, Linux without
//! a user bus), with the real core and an installation staged in a private profile: a conversation's MCP server
//! starts the connector, the connector starts the core and links to it; a core that dies is started again; the
//! person's retry ends the core and a new one comes; a person's stop ends both and holds, so a conversation is told
//! instead of starting anything, until `service start`. Where the core cannot run on demand (Windows: it would end
//! with its caller's Job Object), starting it with no core job is refused instead.

mod support;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream, UdpSocket};
use std::process::Stdio;

use serde_json::json;
use sidevoice_connector::platform::service::{Service, Services};
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
    // Nothing starts at login with no manager: not known, and not settable.
    assert!(idle["start_at_login"].is_null(), "{idle}");
    let refused = service(&profile, &extra, "login off");
    assert_eq!(refused["error"]["key"], "service.no-manager", "{refused}");

    if !Services::DETACHED_OUTLIVES_CALLER {
        // Here a core started detached would end with whoever asked for it (on Windows, its Job Object), so with no
        // core job nothing starts one: each way of asking is refused before anything runs, with the remedy.
        for verb in ["start", "restart"] {
            let refused = service(&profile, &extra, verb);
            assert_eq!(
                refused["error"]["key"], "service.core-needs-service",
                "{verb}: {refused}"
            );
        }
        let output = command(&profile, &extra, &["pair-device", "--json"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let paired: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(!output.status.success(), "{paired}");
        assert_eq!(
            paired["error"]["key"], "service.core-needs-service",
            "{paired}"
        );
        let after = service(&profile, &extra, "status");
        assert_eq!(
            (after["state"].as_str(), after["reachable"].as_bool()),
            (Some("not-installed"), Some(false)),
            "{after}"
        );
        assert!(
            !endpoint_exists(&profile.connector_endpoint()),
            "pair-device started a connector"
        );
        // A connector already running (an agent's MCP server started it) refuses a pairing code on its socket with
        // the same key, beside the text every version reads, instead of waiting for a core that will not come.
        let connector = Process(
            command(&profile, &extra, &["connector"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        until("the connector's socket", 30, || {
            endpoint_exists(&profile.connector_endpoint()).then_some(())
        });
        let refused = connector_answer(&profile.connector_endpoint(), "pair_device").unwrap();
        assert_eq!(refused["ok"], false, "{refused}");
        assert_eq!(
            refused["keyed_error"]["key"], "service.core-needs-service",
            "{refused}"
        );
        assert_eq!(
            refused["error"], refused["keyed_error"]["message"],
            "{refused}"
        );
        drop(connector);
        return;
    }

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
    let port = read_json(&profile.core_data().join("core.json")).unwrap()["port"]
        .as_u64()
        .unwrap();
    // Select this host's nonloopback IPv4 route without sending a packet to the probe address.
    // The installed connector must actually start a core that answers on that interface.
    let route = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
    route.connect(("192.0.2.1", 9)).unwrap();
    let ip = route.local_addr().unwrap().ip();
    assert!(
        !ip.is_loopback(),
        "test runner needs a nonloopback IPv4 route"
    );
    let mut stream = TcpStream::connect((ip, port as u16)).unwrap();
    stream
        .write_all(
            b"GET /api/rendezvous HTTP/1.1\r\nHost: 192.168.1.4:10829\r\nConnection: close\r\n\r\n",
        )
        .unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    assert!(
        answer.starts_with("HTTP/1.1 200 "),
        "installed core must accept a LAN IP Host: {answer}"
    );
    let joined = mcp.tool("voice_connect", json!({"title": "On demand"}));
    assert_eq!(joined["conversation"], "on-demand-thread", "{joined}");

    // The core dies: the connector that started it starts another.
    kill(first);
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
        !endpoint_exists(&profile.connector_endpoint()),
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
