//! The IPC over `connector.sock` as a façade of another version speaks it, against the daemon with its real harnesses
//! (sidevoice/sidevoice-core#112): a route kind this daemon does not know joins the conversation with no route in,
//! instead of refusing it, and an optional field sent as `null` reads as absent.

mod support;

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use support::*;

/// One façade's connection to the daemon.
struct Ipc {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    serial: u64,
}

impl Ipc {
    fn open(profile: &Profile) -> Self {
        let stream =
            UnixStream::connect(profile.connector_socket()).expect("the connector's socket");
        let reader = BufReader::new(stream.try_clone().unwrap());
        Self {
            stream,
            reader,
            serial: 0,
        }
    }

    /// The daemon's whole answer to one command: `{"id","ok","result"}` or `{"id","ok":false,"error"}`.
    fn ask(&mut self, method: &str, params: Value) -> Value {
        self.serial += 1;
        let request = json!({"id": self.serial, "method": method, "params": params});
        writeln!(self.stream, "{request}").unwrap();
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        let answer: Value =
            serde_json::from_str(&line).unwrap_or_else(|_| panic!("{method}: {line}"));
        assert_eq!(answer["id"], self.serial, "{answer}");
        answer
    }
}

fn binding<'a>(status: &'a Value, client_ref: &str) -> &'a Value {
    status["result"]["bindings"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["client_ref"] == client_ref))
        .unwrap_or_else(|| panic!("{client_ref} not listed: {status}"))
}

/// A newer façade names a route kind Claude Code's harness does not know: the conversation joins with no route in
/// (deliver unsupported), so the core cannot push to it and its speech still goes out. A route kind the harness
/// knows, naming another conversation, is still refused.
#[test]
fn an_unknown_route_kind_joins_with_no_route_in_and_a_known_one_is_still_checked() {
    let profile = Profile::new("ipcroute");
    let _connector = profile.start_connector();
    let mut ipc = Ipc::open(&profile);

    let joined = ipc.ask(
        "register",
        json!({"client_ref":"t-1","thread":"t-1","title":"Future route","harness":"claude",
            "delivery":{"kind":"claude-route-from-the-future","thread":"t-1","socket":"/nowhere"},
            "route":"claude-future","capabilities":{"deliver":"supported","working":"unsupported"},
            "experimental":["deliver"]}),
    );
    assert_eq!(joined["ok"], true, "{joined}");
    let status = ipc.ask("status", json!({}));
    let listed = binding(&status, "t-1");
    assert_eq!(listed["harness"], "claude", "{listed}");
    assert_eq!(listed["delivery"], json!({"kind":"none"}), "{listed}");
    assert_eq!(listed["capabilities"]["deliver"], "unsupported", "{listed}");
    assert_eq!(listed["experimental"], json!([]), "{listed}");

    let said = ipc.ask(
        "publish",
        json!({"client_ref":"t-1","session_id":"binding:t-1","revision":0,"epoch":0,"text":"Still speaking"}),
    );
    assert_eq!(said["ok"], true, "{said}");
    assert_eq!(said["result"]["status"], "queued", "{said}");

    // Known kinds keep their identity checks: Codex's queue for another thread, Claude Code's socket missing.
    for (harness, delivery) in [
        (
            "codex",
            json!({"kind":"codex-queue","thread":"someone-else"}),
        ),
        ("claude", json!({"kind":"claude-uds"})),
        (
            "claude",
            json!({"kind":"http","thread":"someone-else","url":"http://127.0.0.1:9"}),
        ),
    ] {
        let refused = ipc.ask(
            "register",
            json!({"client_ref":"t-2","thread":"t-2","harness":harness,"delivery":delivery}),
        );
        assert_eq!(refused["ok"], false, "{refused}");
        assert_eq!(
            refused["error"], "harness identity does not match its delivery route",
            "{refused}"
        );
    }
}

/// `"client_refs": null` is the same as no `client_refs`: the speech goes to the conversation named, not through turn
/// routing, and only for the façade that owns it.
#[test]
fn a_null_optional_field_reads_as_absent() {
    let profile = Profile::new("ipcnull");
    let _connector = profile.start_connector();
    let mut ipc = Ipc::open(&profile);
    let joined = ipc.ask(
        "register",
        json!({"client_ref":"t-1","thread":"t-1","harness":"codex",
            "delivery":{"kind":"codex-queue","thread":"t-1"},"capabilities":null,"experimental":null}),
    );
    assert_eq!(joined["ok"], true, "{joined}");
    let status = ipc.ask("status", json!({}));
    let listed = binding(&status, "t-1");
    assert!(
        listed["capabilities"].is_object(),
        "null capabilities read as absent: {listed}"
    );
    assert_eq!(listed["experimental"], json!([]), "{listed}");

    let absent = json!({"client_ref":"t-1","session_id":"binding:t-1","revision":0,"epoch":0,"text":"absent"});
    let mut null = absent.clone();
    null["client_refs"] = Value::Null;
    null["adopt_orphans"] = Value::Null;
    null["text"] = json!("null");
    for params in [absent, null.clone()] {
        let said = ipc.ask("publish", params);
        assert_eq!(said["ok"], true, "{said}");
        assert_eq!(said["result"]["status"], "queued", "{said}");
    }

    // Ownership is checked as before: another façade cannot speak for it.
    let mut other = Ipc::open(&profile);
    let refused = other.ask("publish", null);
    assert_eq!(refused["ok"], false, "{refused}");
    assert_eq!(refused["error"], "unknown binding", "{refused}");
}
