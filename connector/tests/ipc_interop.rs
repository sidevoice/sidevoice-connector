//! The IPC over `connector.sock` as a façade of another version speaks it, against the daemon with its real harnesses
//! (sidevoice/sidevoice-core#112): fields this daemon does not know (an older façade's route and capabilities) are
//! ignored, and an optional field sent as `null` reads as absent.
// A façade of another version speaks the wire raw, so it opens the local endpoint itself.
#![allow(clippy::disallowed_types)]

mod support;

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use support::*;

/// One façade's connection to the daemon.
struct Ipc {
    reader: BufReader<LocalStream>,
    serial: u64,
}

impl Ipc {
    fn open(profile: &Profile) -> Self {
        let stream =
            LocalStream::connect(&profile.connector_endpoint()).expect("the connector's endpoint");
        Self {
            reader: BufReader::new(stream),
            serial: 0,
        }
    }

    /// The daemon's whole answer to one command: `{"id","ok","result"}` or `{"id","ok":false,"error"}`.
    fn ask(&mut self, method: &str, params: Value) -> Value {
        self.serial += 1;
        let request = json!({"id": self.serial, "method": method, "params": params});
        writeln!(self.reader.get_mut(), "{request}").unwrap();
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

/// A façade of another version sends a route, capabilities and experimental names: the daemon reads only the
/// conversation (its harness and id), asks the harness how it is, and the speech still goes out.
#[test]
fn route_fields_of_another_version_are_ignored_and_the_conversation_joins_by_its_id() {
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
    assert!(listed["inbound"]["ok"].is_boolean(), "{listed}");
    for field in ["delivery", "route", "capabilities", "experimental"] {
        assert!(listed.get(field).is_none(), "{field} is not read: {listed}");
    }

    let said = ipc.ask(
        "publish",
        json!({"client_ref":"t-1","session_id":"binding:t-1","revision":0,"epoch":0,"text":"Still speaking"}),
    );
    assert_eq!(said["ok"], true, "{said}");
    assert_eq!(said["result"]["status"], "queued", "{said}");
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
            "endpoint":null,"engine":null}),
    );
    assert_eq!(joined["ok"], true, "{joined}");
    let status = ipc.ask("status", json!({}));
    let listed = binding(&status, "t-1");
    assert!(
        listed["inbound"].is_object(),
        "a null endpoint reads as absent: {listed}"
    );

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
