//! Handled errors: what is read from an event, what is sent, how often, and never once the core says "off".

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::{Days, NaiveDate};
use tracing::{event, Level};
use tracing_subscriber::layer::SubscriberExt;

use super::*;
use crate::crash::handled::{
    event as handled_event, known, queueing, run, serve, Handled, Outcome, Worker, MAX_TYPES,
};
use crate::crash::record::relative_path;
use crate::crash::HANDLED_TAGS;
use crate::error::{codes, ConnectorError};
use crate::link_error::LinkError;

const SECRET: &str = "canary-secret-7f3a";
const WORDS: &str = "canary words 7f3a";
const HOME: &str = "/home/canary-user-7f3a";
const MACHINE: &str = "0123456789abcdef0123456789abcdef";

// A few of the connector's own codes, by what they stand for in a test.
const A: &str = "connector.io";
const B: &str = "connector.timeout";
const C: &str = "connector.json";
const D: &str = "connector.websocket";

/// What a subscriber with only the handled-errors layer queues while `emit` logs, with the core known to be off as `off` says, and how
/// many events had no code of the connector's own.
fn queued(off: bool, capacity: usize, emit: impl FnOnce()) -> (Vec<Handled>, u64) {
    static COUNTED: AtomicU64 = AtomicU64::new(0);
    // Each test takes the difference, so another test's events do not count.
    let before = COUNTED.load(Ordering::Relaxed);
    let (queue, waiting): (_, Receiver<Handled>) = sync_channel(capacity);
    let subscriber =
        tracing_subscriber::registry().with(queueing(move || off, queue, &COUNTED, || false));
    tracing::subscriber::with_default(subscriber, emit);
    (
        waiting.try_iter().collect(),
        COUNTED.load(Ordering::Relaxed) - before,
    )
}

fn kinds(events: &[Handled]) -> Vec<&str> {
    events.iter().map(|event| event.kind).collect()
}

fn error_of(kind: &'static str) -> Handled {
    Handled {
        kind,
        file: "connector/src/daemon.rs".to_owned(),
        line: 7,
        time: SystemTime::now(),
    }
}

fn today() -> NaiveDate {
    chrono::Utc::now().date_naive()
}

/// A context for a connector whose core said "on".
fn on(f: &Fixture) -> Context {
    let context = context(f);
    context.store.learned(true);
    context
}

fn yes() -> Option<bool> {
    Some(true)
}

/// `handle` with a core that always says on.
fn send(worker: &mut Worker<'_>, kind: &'static str, day: NaiveDate) -> Outcome {
    worker.handle(&error_of(kind), day, &yes)
}

fn saved(f: &Fixture) -> Value {
    serde_json::from_slice(&fs::read(f.data.join("connector-handled-errors.json")).unwrap())
        .unwrap()
}

// The codes.

#[test]
fn the_connectors_codes_are_what_an_error_type_must_be() {
    let all: Vec<&str> = codes().collect();
    assert!(all.len() > 30 && all.len() <= MAX_TYPES, "{}", all.len());
    for code in &all {
        assert_eq!(known(code), Some(*code));
    }
    // Both enums are in it, and the codes `ConnectorError::of` gives for a failure that travelled as an error.
    for code in [
        ConnectorError::Io.code(),
        LinkError::PongTimeout.code(),
        ConnectorError::CrashReportFailed.code(),
        ConnectorError::of(&anyhow::anyhow!("{HOME}"), ConnectorError::StateWrite),
        ConnectorError::of(
            &anyhow::Error::new(std::io::Error::other(HOME)),
            ConnectorError::StateWrite,
        ),
    ] {
        assert!(known(code).is_some(), "{code}");
    }
}

// Which events.

#[test]
fn only_error_events_of_our_own_targets_are_taken_and_never_the_panic_hooks() {
    let (events, untyped) = queued(false, 16, || {
        event!(target: "sidevoice_connector::daemon", Level::ERROR, error.type = A, "ours");
        event!(target: "sidevoice_connector", Level::ERROR, error.type = B, "the crate itself");
        event!(target: "sidevoice_platform::fs", Level::ERROR, error.type = C, "ours too");
        // The panic hook's own, and whatever is below them.
        event!(target: "panic", Level::ERROR, error.type = "connector.state-write", "the panic");
        event!(target: "panic::deeper", Level::ERROR, error.type = "connector.state-encode", "below the panic");
        event!(target: "crash_backtrace", Level::ERROR, error.type = "connector.link-closed", "the backtrace");
        event!(target: "crash_backtrace::deeper", Level::ERROR, error.type = "connector.link-unreachable", "below it");
        // Not ours: a dependency, another piece, a webview, a lookalike that only shares a prefix.
        event!(target: "ureq::run", Level::ERROR, error.type = "connector.core-unavailable", "a dependency");
        event!(target: "sidevoice_core", Level::ERROR, error.type = "connector.handler-capacity", "another piece");
        event!(target: "webview", Level::ERROR, error.type = "connector.core-already-running", "a webview");
        event!(target: "sidevoice_connectorx", Level::ERROR, error.type = "connector.invalid-log-filter", "a lookalike");
        event!(target: "panic_x", Level::ERROR, error.type = "connector.state-too-large", "another lookalike");
        // Ours, but not an error.
        event!(target: "sidevoice_connector::x", Level::WARN, error.type = "connector.binding-close-failed", "a warning");
        event!(target: "sidevoice_connector::x", Level::INFO, error.type = "connector.binding-replay-failed", "an info");
        event!(target: "sidevoice_connector::x", Level::DEBUG, error.type = "connector.agent-leave-replay-failed", "a debug");
        event!(target: "sidevoice_connector::x", Level::TRACE, error.type = "connector.agent-leave-confirm-failed", "a trace");
    });
    assert_eq!(kinds(&events), [A, B, C]);
    assert_eq!(untyped, 0, "the filter comes before the count");
}

#[test]
fn an_event_is_read_for_its_code_and_its_place_and_for_nothing_else() {
    let started = SystemTime::now();
    let line = line!() + 2;
    let (events, _) = queued(false, 16, || {
        event!(
            target: "sidevoice_connector::daemon",
            Level::ERROR,
            error.type = A,
            code.file.path = "/home/canary-user-7f3a/src/lie.rs",
            code.line.number = 99_999,
            secret = SECRET,
            path = HOME,
            words = %WORDS,
            detail = ?WORDS,
            kind = "canary",
            "the port canary words 7f3a was taken by canary-secret-7f3a"
        );
    });
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, A);
    // The callsite's, not what a field claims.
    assert_eq!(events[0].file, "connector/src/crash/tests/handled.rs");
    assert_eq!(events[0].line, line);
    assert!(events[0].time >= started && events[0].time <= SystemTime::now());
}

#[test]
fn an_error_type_that_is_not_one_of_the_connectors_codes_is_counted_and_never_taken() {
    let (events, untyped) = queued(false, 16, || {
        event!(target: "sidevoice_connector::x", Level::ERROR, "no field at all");
        event!(target: "sidevoice_connector::x", Level::ERROR, reason = A, "another field");
        // The code only counts when it is a string.
        event!(target: "sidevoice_connector::x", Level::ERROR, error.type = ?A, "recorded with Debug");
        event!(target: "sidevoice_connector::x", Level::ERROR, error.type = %A, "recorded with Display");
        event!(target: "sidevoice_connector::x", Level::ERROR, error.type = 7, "a number");
        // Having the shape of a code is not being one: a value a future call site logs by mistake does not leave.
        event!(target: "sidevoice_connector::x", Level::ERROR, error.type = "192.168.1.10", "an address");
        event!(target: "sidevoice_connector::x", Level::ERROR, error.type = "my-laptop.local", "a host name");
        event!(target: "sidevoice_connector::x", Level::ERROR, error.type = "canary-user-7f3a.home", "a user name");
        event!(target: "sidevoice_connector::x", Level::ERROR, error.type = "core.a", "another piece's shape");
        event!(target: "sidevoice_connector::x", Level::ERROR, error.type = "connector.not-a-code", "our shape, not our code");
        event!(target: "sidevoice_connector::x", Level::ERROR, error.type = "connector.io.more", "a code and more");
        event!(target: "sidevoice_connector::x", Level::ERROR, error.type = "connector.io ", "a code and a space");
        event!(target: "sidevoice_connector::x", Level::ERROR, error.type = "Connector.Io", "capitals");
        event!(target: "sidevoice_connector::x", Level::ERROR, error.type = "a sentence with spaces", "text");
        event!(target: "sidevoice_connector::x", Level::ERROR, error.type = HOME, "a path");
        event!(target: "sidevoice_connector::x", Level::ERROR, error.type = "", "empty");
        event!(target: "sidevoice_connector::x", Level::ERROR, error.type = "connector.timeoutaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "long");
        event!(target: "sidevoice_connector::x", Level::ERROR, error.type = B, "the one that is");
    });
    assert_eq!(kinds(&events), [B]);
    assert_eq!(untyped, 17);
}

#[test]
fn every_code_of_the_connector_is_one_that_leaves_and_leaves_as_its_own_text() {
    let all: Vec<&'static str> = codes().collect();
    let (events, untyped) = queued(false, 64, || {
        for code in &all {
            event!(target: "sidevoice_connector::x", Level::ERROR, error.type = *code, "a failure");
        }
        // As the call sites give them: from the enum, and from a failure that travelled as an error.
        event!(target: "sidevoice_connector::x", Level::ERROR,
            error.type = ConnectorError::of(&anyhow::anyhow!("{HOME}"), ConnectorError::StateWrite), "a failure");
    });
    assert_eq!(untyped, 0);
    assert_eq!(events.len(), all.len() + 1);
    for (event, code) in events.iter().zip(&all) {
        assert_eq!(event.kind, *code);
    }
    assert_eq!(events.last().unwrap().kind, "connector.state-write");
}

#[test]
fn the_place_is_a_short_relative_path_or_unknown() {
    assert_eq!(
        relative_path("connector/src/daemon.rs"),
        "connector/src/daemon.rs"
    );
    assert_eq!(
        relative_path("platform\\src\\fs.rs"),
        "platform/src/fs.rs",
        "a Windows build"
    );
    for path in [
        "",
        "/home/canary-user-7f3a/.cargo/registry/src/x/a.rs",
        "C:\\Users\\canary-user-7f3a\\a.rs",
        "~/a.rs",
        "../a.rs",
        "a b.rs",
    ] {
        assert_eq!(relative_path(path), "unknown", "{path}");
    }
}

// The layer: the core's answer, the hook and the queue.

#[test]
fn nothing_is_queued_only_once_the_core_is_known_to_be_off() {
    let emit = || event!(target: "sidevoice_connector::x", Level::ERROR, error.type = A, "failed");
    assert_eq!(
        queued(false, 16, emit).0.len(),
        1,
        "not off: unknown, or on"
    );
    assert!(queued(true, 16, emit).0.is_empty(), "known off");
    // What the daemon keeps of the core's answers, which the layer reads in the process: not knowing is neither.
    let f = fixture();
    assert!(!f.store.is_on() && !f.store.is_off(), "nothing learned yet");
    f.store.learned(true);
    assert!(f.store.is_on() && !f.store.is_off());
    f.store.learned(false);
    assert!(!f.store.is_on() && f.store.is_off());
    f.store.learned(true);
    assert!(f.store.is_on(), "on again");
}

#[test]
fn the_gate_follows_the_cached_answer_as_it_changes() {
    let answer = Arc::new(AtomicBool::new(false));
    let gate = answer.clone();
    static COUNTED: AtomicU64 = AtomicU64::new(0);
    let (queue, waiting) = sync_channel(16);
    let subscriber = tracing_subscriber::registry().with(queueing(
        move || gate.load(Ordering::SeqCst),
        queue,
        &COUNTED,
        || false,
    ));
    tracing::subscriber::with_default(subscriber, || {
        event!(target: "sidevoice_connector::x", Level::ERROR, error.type = A, "unknown");
        answer.store(true, Ordering::SeqCst);
        event!(target: "sidevoice_connector::x", Level::ERROR, error.type = B, "while off");
        answer.store(false, Ordering::SeqCst);
        event!(target: "sidevoice_connector::x", Level::ERROR, error.type = C, "on again");
    });
    let seen: Vec<&str> = waiting.try_iter().map(|event| event.kind).collect();
    assert_eq!(seen, [A, C], "queued while not off, not while off");
}

#[test]
fn a_sending_thread_and_the_panic_hook_never_feed_the_queue() {
    let emit = || event!(target: "sidevoice_connector::x", Level::ERROR, error.type = A, "failed");
    SENDING.set(true);
    let from_a_sender = queued(false, 16, emit).0;
    SENDING.set(false);
    assert!(from_a_sender.is_empty());
    static COUNTED: AtomicU64 = AtomicU64::new(0);
    let (queue, waiting) = sync_channel(16);
    let subscriber =
        tracing_subscriber::registry().with(queueing(|| false, queue, &COUNTED, || true));
    tracing::subscriber::with_default(subscriber, emit);
    assert!(waiting.try_iter().next().is_none(), "inside the hook");
}

#[test]
fn a_full_queue_drops_the_event_and_the_logging_thread_never_waits() {
    let (events, _) = queued(false, 2, || {
        for n in 0..50 {
            event!(target: "sidevoice_connector::x", Level::ERROR, error.type = A, n, "failed");
        }
    });
    assert_eq!(events.len(), 2);
}

// The event.

#[test]
fn the_event_carries_the_closed_field_list_and_no_text_of_the_event() {
    let f = fixture();
    let (events, _) = queued(false, 16, || {
        event!(
            target: "sidevoice_connector::daemon",
            Level::ERROR,
            error.type = "connector.state-write",
            secret = SECRET,
            path = HOME,
            words = %WORDS,
            "the file canary words 7f3a was taken by canary-secret-7f3a at /home/canary-user-7f3a"
        );
    });
    let value = serde_json::to_value(handled_event(&context(&f), &events[0], MACHINE)).unwrap();
    assert_eq!(
        keys(&value),
        [
            "event_id",
            "exception",
            "platform",
            "release",
            "tags",
            "timestamp"
        ],
        "no extra, breadcrumb, context, user, request, message or host name, and the default level: {value}"
    );
    // Sentry's default level is error, so the SDK's type leaves it out: the field is there only when it is not the default.
    assert!(value.get("level").is_none(), "{value}");
    assert_eq!(
        value["release"],
        format!("sidevoice-connector@{}", crate::identity::VERSION).as_str()
    );
    assert_eq!(keys(&value["tags"]), {
        let mut tags = HANDLED_TAGS.to_vec();
        tags.sort_unstable();
        tags
    });
    assert_eq!(value["tags"]["service.name"], "sidevoice-connector");
    assert_eq!(value["tags"]["os.type"], "linux");
    assert_eq!(value["tags"]["os.name"], "Linux");
    assert_eq!(value["tags"]["host.arch"], "amd64");
    assert_eq!(value["tags"]["installation.id"], MACHINE);
    let exception = &value["exception"]["values"];
    assert_eq!(exception.as_array().unwrap().len(), 1);
    assert_eq!(keys(&exception[0]), ["stacktrace", "type"], "no message");
    assert_eq!(exception[0]["type"], "connector.state-write");
    let frames = exception[0]["stacktrace"]["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(keys(&frames[0]), ["filename", "lineno"]);
    assert_eq!(
        frames[0]["filename"],
        "connector/src/crash/tests/handled.rs"
    );
    assert!(frames[0]["lineno"].as_u64().unwrap() > 0);
    // Random per event: nothing links two of them but the install id.
    let other = serde_json::to_value(handled_event(&context(&f), &events[0], MACHINE)).unwrap();
    assert_ne!(value["event_id"], other["event_id"]);

    let json = value.to_string();
    for canary in ["canary", "/home/"] {
        assert!(!json.contains(canary), "{canary} in {json}");
    }
    use crate::platform::user::{User, UserId};
    let host = UserId::host_name();
    if host.len() >= 6 {
        assert!(!json.contains(&host), "the host name is in {json}");
    }
}

#[test]
fn a_place_that_is_not_ours_to_say_is_unknown_in_the_event() {
    let f = fixture();
    let place = Handled {
        file: relative_path("/home/canary-user-7f3a/.cargo/registry/src/x/a.rs"),
        ..error_of(A)
    };
    let json = serde_json::to_string(&handled_event(&context(&f), &place, MACHINE)).unwrap();
    assert!(json.contains("\"filename\":\"unknown\""), "{json}");
    assert!(
        !json.contains("canary") && !json.contains("/home/"),
        "{json}"
    );
}

// The sender, against a Sentry that is this test's own socket.

#[test]
fn an_error_is_sent_once_a_day_per_type_and_a_new_day_or_a_clock_gone_back_is_a_new_day() {
    let f = fixture();
    let server = Sentry::start(200);
    let context = on(&f);
    let mut worker = Worker::new(&context, server.dsn());
    let day = today();
    let next = day.checked_add_days(Days::new(1)).unwrap();
    let before = day.checked_sub_days(Days::new(1)).unwrap();

    assert_eq!(send(&mut worker, A, day), Outcome::Sent);
    assert_eq!(send(&mut worker, A, day), Outcome::Capped);
    assert_eq!(send(&mut worker, A, day), Outcome::Capped);
    assert_eq!(send(&mut worker, B, day), Outcome::Sent);
    assert_eq!(server.requests().len(), 2, "one per type");

    assert_eq!(send(&mut worker, A, next), Outcome::Sent, "tomorrow");
    assert_eq!(send(&mut worker, A, next), Outcome::Capped);
    assert_eq!(server.requests().len(), 3);

    // The clock went back to a day the list is not for: a new day, and the list is that day's from now on.
    assert_eq!(send(&mut worker, A, before), Outcome::Sent, "clock back");
    assert_eq!(send(&mut worker, A, before), Outcome::Capped);
    assert_eq!(send(&mut worker, B, before), Outcome::Sent);
    assert_eq!(server.requests().len(), 5);
    assert_eq!(
        send(&mut worker, A, day),
        Outcome::Sent,
        "and forward again"
    );
    assert_eq!(server.requests().len(), 6);
}

#[test]
fn the_days_list_is_a_private_file_that_survives_a_restart_for_that_day_only() {
    let f = fixture();
    let server = Sentry::start(200);
    let context = on(&f);
    let day = today();
    {
        let mut worker = Worker::new(&context, server.dsn());
        assert_eq!(send(&mut worker, A, day), Outcome::Sent);
        assert_eq!(send(&mut worker, B, day), Outcome::Sent);
    }
    let file = f.data.join("connector-handled-errors.json");
    let value = saved(&f);
    assert_eq!(
        keys(&value),
        ["day", "sent"],
        "codes and the day, nothing else"
    );
    assert_eq!(value["day"], day.to_string().as_str());
    assert_eq!(value["sent"], json!([A, B]));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o077,
            0,
            "private"
        );
    }

    // Another run, the same day: the list is read back.
    let mut again = Worker::new(&context, server.dsn());
    assert_eq!(send(&mut again, A, day), Outcome::Capped);
    assert_eq!(send(&mut again, C, day), Outcome::Sent);
    assert_eq!(server.requests().len(), 3);

    // A file for another day (earlier or later), a file that is not ours, and one that is not a list: an empty list.
    for kept in [
        json!({"day": "2001-01-01", "sent": [A]}),
        json!({"day": "2999-01-01", "sent": [A]}),
        json!({"day": "not a day", "sent": [A]}),
        json!([A]),
    ] {
        Files::write_json(&file, &kept).unwrap();
        let mut fresh = Worker::new(&context, server.dsn());
        assert_eq!(send(&mut fresh, A, day), Outcome::Sent, "{kept}");
    }
    Files::write_private(&file, b"not json").unwrap();
    let mut fresh = Worker::new(&context, server.dsn());
    assert_eq!(send(&mut fresh, A, day), Outcome::Sent);
    assert_eq!(saved(&f)["sent"], json!([A]), "replaced by a good one");
}

#[test]
fn a_file_with_things_that_are_not_the_connectors_codes_is_read_as_its_codes_only() {
    let f = fixture();
    let server = Sentry::start(200);
    let context = on(&f);
    let day = today();
    Files::write_json(
        &f.data.join("connector-handled-errors.json"),
        &json!({"day": day.to_string(), "sent": [
            "not a code", HOME, A, A, "Connector.Io", "192.168.1.10", "core.a", "connector.not-a-code", 7, C
        ]}),
    )
    .unwrap();
    let mut worker = Worker::new(&context, server.dsn());
    assert_eq!(send(&mut worker, A, day), Outcome::Capped);
    assert_eq!(send(&mut worker, C, day), Outcome::Capped);
    assert_eq!(
        send(&mut worker, B, day),
        Outcome::Sent,
        "what is not a code says nothing about it"
    );
    assert_eq!(saved(&f)["sent"], json!([A, C, B]));
}

#[test]
fn the_days_list_and_so_the_days_sends_are_bounded() {
    let f = fixture();
    let server = Sentry::start(200);
    let context = on(&f);
    let day = today();
    // The connector's codes are fewer than the bound today; it holds for any number of them, and for a file with more.
    assert!(codes().count() <= MAX_TYPES);
    let file = f.data.join("connector-handled-errors.json");
    Files::write_json(&file, &json!({"day": day.to_string(), "sent": [A]})).unwrap();
    let mut worker = Worker::new(&context, server.dsn()).limited(2);
    assert_eq!(send(&mut worker, B, day), Outcome::Sent, "room for one");
    assert_eq!(
        send(&mut worker, C, day),
        Outcome::Capped,
        "a full list sends no more today, however many types come"
    );
    assert_eq!(send(&mut worker, D, day), Outcome::Capped);
    assert_eq!(server.requests().len(), 1);
    assert_eq!(saved(&f)["sent"].as_array().unwrap().len(), 2);

    // A file that holds more than the bound is cut to it when read, and is as full.
    let many: Vec<&str> = codes().take(6).collect();
    Files::write_json(&file, &json!({"day": day.to_string(), "sent": many})).unwrap();
    let mut read = Worker::new(&context, server.dsn()).limited(3);
    assert_eq!(send(&mut read, many[5], day), Outcome::Capped);
    assert_eq!(send(&mut read, many[1], day), Outcome::Capped);
    let tomorrow = day.checked_add_days(Days::new(1)).unwrap();
    assert_eq!(send(&mut read, many[5], tomorrow), Outcome::Sent);
}

// Under the core's answer.

#[test]
fn nothing_is_sent_or_made_once_the_core_is_known_to_be_off() {
    let f = fixture();
    let server = Sentry::start(200);
    let context = context(&f);
    let day = today();
    let mut worker = Worker::new(&context, server.dsn());
    let asked = Cell::new(0);
    let ask = || {
        asked.set(asked.get() + 1);
        Some(true)
    };
    context.store.learned(false);
    assert_eq!(worker.handle(&error_of(A), day, &ask), Outcome::Off);
    assert_eq!(asked.get(), 0, "known off: the core is not even asked");
    assert!(server.requests().is_empty());
    assert!(names(&f.data).is_empty(), "no id and no list were made");
}

#[test]
fn an_unknown_answer_is_asked_and_a_core_that_says_on_gets_the_event_sent() {
    let f = fixture();
    let server = Sentry::start(200);
    let context = context(&f);
    assert!(!context.store.is_on() && !context.store.is_off());
    let mut worker = Worker::new(&context, server.dsn());
    assert_eq!(send(&mut worker, A, today()), Outcome::Sent);
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn a_core_that_says_off_when_asked_erases_the_list_the_id_and_the_records_and_nothing_leaves() {
    let f = fixture();
    let server = Sentry::start(200);
    let context = on(&f);
    let day = today();
    let mut worker = Worker::new(&context, server.dsn());
    assert_eq!(send(&mut worker, A, day), Outcome::Sent);
    f.store.record(record(1)).unwrap();
    assert!(f.data.join("connector-handled-errors.json").exists());
    assert!(f.data.join("connector-crash-id.json").exists());

    assert_eq!(
        worker.handle(&error_of(B), day, &|| Some(false)),
        Outcome::Off
    );
    assert!(names(&f.data).is_empty(), "{:?}", names(&f.data));
    assert!(context.store.is_off(), "and none is queued after it");
    assert_eq!(server.requests().len(), 1, "nothing was posted for B");

    // Back on, as a core says it: what the worker remembered went with the list, so a type sent before is sent again.
    context.store.learned(true);
    assert_eq!(send(&mut worker, A, day), Outcome::Sent);
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn a_core_that_cannot_be_asked_sends_nothing_and_is_left_alone_until_the_retry() {
    let f = fixture();
    let server = Sentry::start(200);
    let context = on(&f);
    let day = today();
    let mut worker = Worker::new(&context, server.dsn());
    let asked = Cell::new(0);
    let silent = || {
        asked.set(asked.get() + 1);
        None
    };
    assert_eq!(worker.handle(&error_of(A), day, &silent), Outcome::Unknown);
    assert_eq!(worker.handle(&error_of(B), day, &silent), Outcome::Unknown);
    assert_eq!(worker.handle(&error_of(C), day, &yes), Outcome::Unknown);
    assert_eq!(asked.get(), 1, "one try, then left alone");
    assert!(server.requests().is_empty(), "never sent blind");
    assert!(names(&f.data).is_empty(), "no id and no list were made");
    assert!(
        !context.store.is_off(),
        "an answer that did not come is not off"
    );
}

/// A queue of `errors`, closed, for the sender.
fn queue_of(errors: Vec<Handled>) -> Receiver<Handled> {
    let (queue, waiting) = sync_channel(errors.len().max(1));
    for error in errors {
        queue.send(error).unwrap();
    }
    waiting
}

#[test]
fn errors_met_while_the_core_is_unknown_wait_and_leave_when_it_says_on_with_their_own_time() {
    let f = fixture();
    let server = Sentry::start(200);
    let context = context(&f);
    let past = SystemTime::now() - Duration::from_secs(3600);
    let error = Handled {
        time: past,
        ..error_of(A)
    };
    // The event as it will be sent carries the time it happened, not the time of the send.
    assert_eq!(handled_event(&context, &error, MACHINE).timestamp, past);
    let asked = Cell::new(0);
    // Not answered twice, then on.
    let ask = || {
        asked.set(asked.get() + 1);
        (asked.get() > 2).then_some(true)
    };
    let waiting = queue_of(vec![error, error_of(B)]);
    let worker = Worker::new(&context, server.dsn()).retrying(Duration::ZERO);
    run(worker, &waiting, &ask);
    assert_eq!(
        server.requests().len(),
        2,
        "both left once the core said on"
    );
    assert!(asked.get() >= 3);
    let body = String::from_utf8_lossy(&server.requests()[0].1).into_owned();
    // The time it happened (an hour ago), as the Sentry timestamp, not the time of the send.
    let secs = past
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(
        body.contains(&format!("\"timestamp\":{secs}.")),
        "{secs} not in {body}"
    );
    assert_eq!(saved(&f)["sent"], json!([A, B]));
}

#[test]
fn errors_waiting_for_a_core_that_then_says_off_are_discarded_with_the_queue_and_the_list() {
    let f = fixture();
    let server = Sentry::start(200);
    let context = context(&f);
    // A first send makes the list; then the core is unknown, then off.
    f.store.record(record(1)).unwrap();
    let mut worker = Worker::new(&context, server.dsn()).retrying(Duration::ZERO);
    assert_eq!(send(&mut worker, A, today()), Outcome::Sent);
    let answers = RefCell::new(vec![None, Some(false)].into_iter());
    let ask = || answers.borrow_mut().next().unwrap_or(Some(false));
    let waiting = queue_of(vec![error_of(B), error_of(C), error_of(D)]);
    run(worker, &waiting, &ask);
    assert_eq!(server.requests().len(), 1, "nothing left after the off");
    assert!(waiting.try_recv().is_err(), "the queue was cleared");
    assert!(names(&f.data).is_empty(), "{:?}", names(&f.data));
}

#[test]
fn the_wait_for_the_core_is_bounded_and_drops_what_does_not_fit() {
    let f = fixture();
    let server = Sentry::start(200);
    let context = context(&f);
    let kinds: Vec<&'static str> = codes().collect();
    assert!(
        kinds.len() > 16,
        "the test needs more codes than the wait holds"
    );
    // The core does not answer until the queue ended: the wait holds 16 of them and drops the rest.
    let asked = Cell::new(0);
    let total = kinds.len();
    let ask = || {
        asked.set(asked.get() + 1);
        (asked.get() > total).then_some(true)
    };
    let (queue, waiting) = sync_channel(total);
    for kind in &kinds {
        queue.send(error_of(kind)).unwrap();
    }
    drop(queue);
    let worker = Worker::new(&context, server.dsn()).retrying(Duration::ZERO);
    run(worker, &waiting, &ask);
    assert_eq!(server.requests().len(), 16);
}

#[test]
fn an_off_that_comes_after_the_send_leaves_neither_the_id_nor_the_list_behind() {
    let f = fixture();
    let server = Sentry::start(200);
    let context = on(&f);
    // On when the send is let go, off when the list and the id exist.
    let answers = RefCell::new(vec![Some(true), Some(false)]);
    let ask = || answers.borrow_mut().remove(0);
    let mut worker = Worker::new(&context, server.dsn());
    assert_eq!(worker.handle(&error_of(A), today(), &ask), Outcome::Sent);
    assert!(names(&f.data).is_empty(), "{:?}", names(&f.data));
    assert!(context.store.is_off());
    assert_eq!(server.requests().len(), 1);

    // An answer that does not come at that second look changes nothing: they stay for the next ask, which erases them.
    let f = fixture();
    let context = on(&f);
    let answers = RefCell::new(vec![Some(true), None]);
    let ask = || answers.borrow_mut().remove(0);
    let mut worker = Worker::new(&context, server.dsn());
    assert_eq!(worker.handle(&error_of(A), today(), &ask), Outcome::Sent);
    assert_eq!(
        names(&f.data),
        ["connector-crash-id.json", "connector-handled-errors.json"]
    );
    crate::crash::report::settle(&context, &|| Some(false));
    assert!(names(&f.data).is_empty(), "{:?}", names(&f.data));
}

#[test]
fn turning_off_erases_the_days_list_from_any_process() {
    let f = fixture();
    let file = f.data.join("connector-handled-errors.json");
    Files::write_json(&file, &json!({"day": today().to_string(), "sent": [A]})).unwrap();
    settings::machine_id(&f.data).unwrap();
    f.store.record(record(1)).unwrap();
    settings::erase(&f.data).unwrap();
    assert!(names(&f.data).is_empty(), "{:?}", names(&f.data));
    // And one that is not there is already gone.
    settings::erase(&f.data).unwrap();
}

#[test]
fn a_failed_send_is_not_counted_and_the_network_is_left_alone_for_a_while() {
    let f = fixture();
    let failing = Sentry::start(500);
    let context = on(&f);
    let day = today();
    let mut worker = Worker::new(&context, failing.dsn());
    assert_eq!(send(&mut worker, A, day), Outcome::Unsent);
    assert_eq!(send(&mut worker, B, day), Outcome::Quiet);
    assert_eq!(send(&mut worker, A, day), Outcome::Quiet);
    assert_eq!(failing.requests().len(), 1, "one try, then quiet");
    assert!(
        !f.data.join("connector-handled-errors.json").exists(),
        "nothing was sent"
    );

    // The same type, on a later run with the network back: it was never counted.
    let working = Sentry::start(200);
    let mut later = Worker::new(&context, working.dsn());
    assert_eq!(send(&mut later, A, day), Outcome::Sent);

    // A server that is not there is the same.
    let gone = {
        let server = Sentry::start(200);
        let dsn = server.dsn();
        drop(server);
        dsn
    };
    let mut lost = Worker::new(&context, gone);
    assert_eq!(send(&mut lost, D, day), Outcome::Unsent);
}

#[test]
fn what_reaches_the_server_is_the_closed_event_under_the_install_id() {
    let f = fixture();
    let server = Sentry::start(200);
    let context = on(&f);
    let (events, _) = queued(false, 16, || {
        event!(
            target: "sidevoice_connector::daemon",
            Level::ERROR,
            error.type = "connector.state-write",
            secret = SECRET,
            path = HOME,
            words = %WORDS,
            "the file canary words 7f3a was taken by canary-secret-7f3a at /home/canary-user-7f3a"
        );
    });
    let mut worker = Worker::new(&context, server.dsn());
    let sent = worker.handle(&events[0], today(), &yes);
    assert_eq!(sent, Outcome::Sent);
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let head = requests[0].0.to_ascii_lowercase();
    let body = String::from_utf8_lossy(&requests[0].1).into_owned();
    assert!(head.starts_with("post /api/42/envelope/ "), "{head}");
    assert!(head.contains("x-sentry-auth:"), "{head}");
    assert!(
        head.contains("content-type: application/x-sentry-envelope"),
        "{head}"
    );
    assert!(body.contains("connector.state-write"), "{body}");
    assert!(body.contains("sidevoice-connector@"), "{body}");
    // The install id is the connector's own, the one its crash reports use.
    assert!(
        body.contains(&settings::machine_id(&f.data).unwrap()),
        "{body}"
    );
    for canary in [
        "canary",
        "/home/",
        "breadcrumbs",
        "server_name",
        "\"user\"",
        "\"request\"",
    ] {
        assert!(!body.contains(canary), "{canary} in {body}");
        assert!(!head.contains(canary), "{canary} in {head}");
    }
    // Byte-decimal renderings too: what a `Debug` of the bytes would print.
    let decimal = format!("{:?}", SECRET.as_bytes());
    assert!(!body.contains(decimal.trim_matches(['[', ']'])));
    use crate::platform::user::{User, UserId};
    let host = UserId::host_name();
    if host.len() >= 6 {
        assert!(!body.contains(&host), "the host name is in {body}");
        assert!(!head.contains(&host), "the host name is in {head}");
    }
}

#[test]
fn the_sender_takes_what_is_queued_asks_the_core_and_survives_a_failure_inside_it() {
    let f = fixture();
    let server = Sentry::start(200);
    let context = on(&f);
    let (queue, waiting) = sync_channel(16);
    // The first event makes the ask itself panic: that costs that event and nothing else.
    let first = Cell::new(true);
    let ask = || {
        if first.replace(false) {
            panic!("a failure of the reporter");
        }
        Some(true)
    };
    queue.send(error_of(A)).unwrap();
    queue.send(error_of(B)).unwrap();
    queue.send(error_of(B)).unwrap();
    drop(queue);
    serve(&context, server.dsn(), &waiting, &ask);
    assert_eq!(
        server.requests().len(),
        1,
        "B once; A was lost with the failure"
    );
    assert_eq!(saved(&f)["sent"], json!([B]));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sender_asks_the_core_over_its_local_endpoint() {
    let f = fixture();
    let server = Sentry::start(200);
    let context = on(&f);
    let (on_core, off_core) = (fake_core(200, true), fake_core(200, false));
    // The sender is a thread of its own with a runtime of its own, as in the daemon: here a blocking task of this test's runtime.
    tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (queue, waiting) = sync_channel(16);
        queue.send(error_of(A)).unwrap();
        drop(queue);
        let ask = || runtime.block_on(on_core.profile.crash_reports()).ok();
        serve(&context, server.dsn(), &waiting, &ask);
        assert_eq!(server.requests().len(), 1);
        let asked = on_core.asked.lock().unwrap().clone();
        assert!(
            asked
                .iter()
                .any(|line| line.starts_with("GET /api/local/crash-reports ")),
            "{asked:?}"
        );

        // A core that says off: nothing is sent and what this connector kept is erased.
        let (queue, waiting) = sync_channel(16);
        queue.send(error_of(B)).unwrap();
        drop(queue);
        let ask = || runtime.block_on(off_core.profile.crash_reports()).ok();
        serve(&context, server.dsn(), &waiting, &ask);
        assert_eq!(server.requests().len(), 1, "B was not sent");
        assert!(
            names(&context.data).is_empty(),
            "{:?}",
            names(&context.data)
        );
    })
    .await
    .unwrap();
}
