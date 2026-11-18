use std::fs;
use std::path::Path;

use chrono::{Duration, SecondsFormat, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use super::record::{mask_home, mask_homes, FORMATTED_MARKER};
use super::report::{drain, event, pass, Patience};
use super::*;
use crate::platform::fs::{Files, Fs};
use crate::profile::Profile;
use crate::test_support::temp_dir;

const ID: &str = "7b2f0b00-1c3e-4d5a-8f6b-0123456789ab";

fn at(days_ago: i64) -> String {
    (Utc::now() - Duration::days(days_ago)).to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// A line of the connector's own tail, at error, as `tail` renders it.
fn ours(n: impl std::fmt::Display) -> String {
    format!("-1.000s ERROR sidevoice_connector::x: line {n}")
}

fn frame(function: &str, addr: u64) -> Frame {
    Frame {
        function: Some(function.to_owned()),
        file: Some("a.rs".to_owned()),
        line: Some(1),
        addr,
    }
}

fn image(addr: u64, size: u64) -> Image {
    Image {
        name: "sidevoice-connector".to_owned(),
        addr,
        size,
        vmaddr: 0x40_0000,
        id: ID.to_owned(),
        code_id: Some("7b2f0b001c3e4d5a8f6b".to_owned()),
    }
}

fn panic<'a>(literal: Option<&'a str>, line: u32) -> Panic<'a> {
    Panic {
        version: "1.2.3",
        os: "linux",
        arch: "amd64",
        at: Utc::now(),
        literal,
        formatted_len: 0,
        file: "rust/server/x.rs",
        line,
        frames: vec![frame("a::b", 0x1010), frame("c", 0x1020)],
        images: vec![image(0x1000, 0x1000)],
        tail: vec![ours(1), ours(2)],
    }
}

fn record(line: u32) -> CrashRecord {
    CrashRecord::from_panic(panic(Some("boom"), line), &[])
}

struct Fixture {
    _root: tempfile::TempDir,
    data: std::path::PathBuf,
    store: CrashStore,
}

fn fixture() -> Fixture {
    let root = temp_dir("crash");
    let data = root.path().join("data");
    Files::create_private_dir(&data).unwrap();
    let store = CrashStore::new(&data, Vec::new());
    Fixture {
        _root: root,
        data,
        store,
    }
}

fn context(f: &Fixture) -> Context {
    Context {
        data: f.data.clone(),
        store: CrashStore::new(&f.data, Vec::new()),
        homes: Vec::new(),
        os: "linux",
        arch: "amd64",
        images: Vec::new(),
        redact: str::to_owned,
    }
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

// The record.

#[test]
fn a_record_holds_the_closed_field_list_and_a_literal_message() {
    let value = serde_json::to_value(record(7)).unwrap();
    assert_eq!(
        keys(&value),
        [
            "arch",
            "backtrace",
            "count",
            "exception",
            "first_at",
            "id",
            "images",
            "last_at",
            "location",
            "log_tail",
            "os",
            "signature",
            "version"
        ]
    );
    assert_eq!(
        value["exception"],
        json!({"type": "panic", "message": "boom"})
    );
    assert_eq!(
        value["location"],
        json!({"file": "rust/server/x.rs", "line": 7})
    );
    assert_eq!(
        value["backtrace"],
        json!([
            {"function": "a::b", "file": "a.rs", "line": 1, "addr": 0x1010},
            {"function": "c", "file": "a.rs", "line": 1, "addr": 0x1020}
        ])
    );
    assert_eq!(value["images"][0]["name"], "sidevoice-connector");
    assert_eq!(
        Uuid::parse_str(value["id"].as_str().unwrap())
            .unwrap()
            .get_version_num(),
        4
    );
}

#[test]
fn a_formatted_message_is_never_stored_only_its_length() {
    let canary = "canary-secret-7f3a";
    let mut formatted = panic(None, 1);
    formatted.formatted_len = canary.len();
    let stored = CrashRecord::from_panic(formatted, &[]);
    assert_eq!(stored.exception.message, FORMATTED_MARKER);
    assert_eq!(stored.exception.len, Some(canary.len() as u32));
    assert!(!serde_json::to_string(&stored).unwrap().contains(canary));
    // A literal has no length to say.
    let mut literal = panic(Some("boom"), 1);
    literal.formatted_len = 9;
    assert_eq!(CrashRecord::from_panic(literal, &[]).exception.len, None);
}

#[test]
fn the_home_directory_is_shown_as_a_tilde_and_no_file_keeps_a_directory() {
    let mut raw = panic(Some("x"), 1);
    raw.file = "/home/canary-user-7f3a/src/a.rs";
    raw.frames = vec![Frame {
        function: Some("f".to_owned()),
        file: Some("/home/canary-user-7f3a/b.rs".to_owned()),
        line: Some(3),
        addr: 1,
    }];
    raw.images = vec![Image {
        name: "/home/canary-user-7f3a/.local/bin/sidevoice-connector".to_owned(),
        ..image(0, 1)
    }];
    raw.tail = vec![format!("{} opened /home/canary-user-7f3a/x", ours(1))];
    let stored = CrashRecord::from_panic(raw, &["/home/canary-user-7f3a".to_owned()]);
    let json = serde_json::to_string(&stored).unwrap();
    assert!(!json.contains("canary-user"), "{json}");
    assert_eq!(stored.backtrace[0].file.as_deref(), Some("b.rs"));
    assert_eq!(stored.images[0].name, "sidevoice-connector");
    assert_eq!(stored.location.file, "unknown");
}

#[test]
fn the_signature_groups_by_version_location_and_top_frames_and_not_by_address() {
    let a = record(5);
    assert_eq!(a.signature, record(5).signature);
    assert_ne!(a.signature, record(6).signature, "location");
    let mut other = record(5);
    other.version = "1.2.4".to_owned();
    let other = CrashRecord::from_untrusted(&serde_json::to_value(&other).unwrap(), &[]).unwrap();
    assert_ne!(a.signature, other.signature, "version");
    let mut moved = panic(Some("boom"), 5);
    moved.frames = vec![frame("a::b", 0x9010), frame("c", 0x9020)];
    assert_eq!(
        a.signature,
        CrashRecord::from_panic(moved, &[]).signature,
        "a new address space is the same crash"
    );
    let mut deeper = panic(Some("boom"), 5);
    deeper.frames = vec![frame("a::other", 0x1010)];
    assert_ne!(
        a.signature,
        CrashRecord::from_panic(deeper, &[]).signature,
        "frames"
    );
    assert_ne!(a.id, record(5).id, "ids are random");
}

#[test]
fn a_record_is_cut_to_the_tail_frame_and_image_limits() {
    let mut big = serde_json::to_value(record(1)).unwrap();
    big["log_tail"] = json!((0..500).map(ours).collect::<Vec<_>>());
    big["backtrace"] = json!((0..500)
        .map(|n| json!({"function": format!("frame{n}"), "addr": n}))
        .collect::<Vec<_>>());
    big["images"] = json!((0..500)
        .map(|n| json!({"name": "x", "addr": n, "size": 1, "vmaddr": 0, "id": ID}))
        .collect::<Vec<_>>());
    let read = CrashRecord::from_untrusted(&big, &[]).unwrap();
    assert_eq!(read.log_tail.len(), TAIL_LINES);
    assert!(
        read.log_tail.last().unwrap().ends_with("line 499"),
        "the last lines"
    );
    assert_eq!(read.backtrace.len(), MAX_FRAMES);
    assert_eq!(read.images.len(), MAX_IMAGES);
}

#[test]
fn a_record_with_a_bad_field_is_not_taken() {
    let base = serde_json::to_value(record(1)).unwrap();
    for (field, value) in [
        ("id", json!("../../x")),
        ("version", json!("1 2")),
        ("os", json!("")),
        ("last_at", json!("yesterday")),
    ] {
        let mut bad = base.clone();
        bad[field] = value;
        assert!(CrashRecord::from_untrusted(&bad, &[]).is_none(), "{field}");
    }
    let mut dirty = base;
    dirty["exception"]["message"] = json!("a\u{1b}[31mred\nline");
    let clean = CrashRecord::from_untrusted(&dirty, &[]).unwrap();
    assert!(!clean.exception.message.chars().any(char::is_control));
}

#[test]
fn texts_that_are_not_what_we_write_are_replaced_or_dropped() {
    let mut raw = serde_json::to_value(record(1)).unwrap();
    raw["exception"]["message"] = json!("caf\u{e9} \u{202e}x");
    raw["location"]["file"] = json!("/etc/passwd");
    raw["backtrace"] = json!([
        {"function": "good::frame", "file": "a.rs", "line": 2, "addr": 1},
        {"function": "bad frame ?token=canary", "file": "/home/x/a b.rs", "addr": 2},
        {"file": "../../etc/passwd", "addr": 3},
        {"function": "no::address"},
        "not a frame",
    ]);
    raw["images"] = json!([
        {"name": "/opt/sidevoice/bin/sidevoice-connector", "addr": 1, "size": 2, "vmaddr": 0, "id": ID},
        {"name": "a b", "addr": 1, "size": 2, "vmaddr": 0, "id": ID},
        {"name": "ok", "addr": 1, "size": 2, "vmaddr": 0, "id": "not a debug id"},
    ]);
    let read = CrashRecord::from_untrusted(&raw, &[]).unwrap();
    assert_eq!(read.exception.message, FORMATTED_MARKER);
    assert_eq!(read.location.file, "unknown");
    assert_eq!(
        read.backtrace,
        [
            Frame {
                function: Some("good::frame".to_owned()),
                file: Some("a.rs".to_owned()),
                line: Some(2),
                addr: 1
            },
            Frame {
                function: None,
                file: None,
                line: None,
                addr: 2
            },
            Frame {
                function: None,
                file: Some("passwd".to_owned()),
                line: None,
                addr: 3
            },
        ]
    );
    assert_eq!(read.images.len(), 1);
    assert_eq!(read.images[0].name, "sidevoice-connector");
    for file in ["../a.rs", "a b.rs", "C:/a.rs", "~/a.rs"] {
        raw["location"]["file"] = json!(file);
        assert_eq!(
            CrashRecord::from_untrusted(&raw, &[])
                .unwrap()
                .location
                .file,
            "unknown",
            "{file}"
        );
    }
    raw["location"]["file"] = json!("rust\\server\\x.rs");
    assert_eq!(
        CrashRecord::from_untrusted(&raw, &[])
            .unwrap()
            .location
            .file,
        "rust/server/x.rs"
    );
}

#[test]
fn a_tail_line_is_cut_stripped_of_format_characters_and_only_ours() {
    let mut raw = serde_json::to_value(record(1)).unwrap();
    raw["log_tail"] = json!([
        format!("{} {}", ours(1), "z".repeat(900)),
        format!("{} \u{202e}reversed\u{200b}", ours(2)),
        "-1.000s ERROR sidevoice_core: a core line in a connector record",
        "-1.000s ERROR panic: the panic's own line",
        "-1.000s  WARN hyper_util::client: GET https://x/?token=canary",
        "-1.000s DEBUG sidevoice_connector::x: a debug line",
        "-1.000s  WARN sidevoice_connectorvoice: a lookalike target",
        "2026-11-15T10:00:00.123456Z ERROR sidevoice_connector::x: a dated line",
        "raw stderr of a service ?token=canary",
    ]);
    let read = CrashRecord::from_untrusted(&raw, &[]).unwrap();
    assert_eq!(read.log_tail.len(), 2, "{:?}", read.log_tail);
    assert!(
        read.log_tail[0].chars().count() <= 600 && read.log_tail[0].ends_with("zzzz"),
        "cut to the limit"
    );
    assert!(read.log_tail[1].ends_with("reversed"));
    assert!(!read.log_tail[1].contains('\u{202e}'));
}

#[test]
fn a_warning_inside_a_span_is_kept_and_a_dependency_inside_a_span_is_dropped() {
    let mut raw = serde_json::to_value(record(1)).unwrap();
    raw["log_tail"] = json!([
        "-1.000s  WARN binding{id=\"a: b}\" n=3}: sidevoice_connector::room: kept one",
        "-1.000s  WARN outer{a=1}: inner: sidevoice_connector::room: kept two",
        "-1.000s  WARN binding{id=1}: hyper_util::client: dropped one",
        "-1.000s  WARN binding{id=1}: sidevoice_connectorvoice: dropped two",
        "-1.000s  WARN binding{id=1} sidevoice_connector::room: dropped three",
    ]);
    let read = CrashRecord::from_untrusted(&raw, &[]).unwrap();
    assert_eq!(read.log_tail.len(), 2, "{:?}", read.log_tail);
    assert!(read.log_tail[1].contains("kept two"));
}

#[test]
fn times_are_made_utc_and_ordered_and_the_home_is_hidden_again() {
    let mut raw = serde_json::to_value(record(1)).unwrap();
    raw["first_at"] = json!("2026-11-15T12:00:00.000+02:00");
    raw["last_at"] = json!("2026-11-15T09:00:00.000Z");
    raw["log_tail"] = json!([format!("{} at /home/canary-user-7f3a/x", ours(1))]);
    let read = CrashRecord::from_untrusted(&raw, &["/home/canary-user-7f3a".to_owned()]).unwrap();
    assert_eq!(read.first_at, "2026-11-15T09:00:00.000Z");
    assert_eq!(read.last_at, "2026-11-15T10:00:00.000Z");
    assert!(!serde_json::to_string(&read)
        .unwrap()
        .contains("canary-user"));
    assert!(read.log_tail[0].ends_with("at ~/x"));
}

#[test]
fn format_characters_are_stripped_before_the_home_is_masked() {
    let mut raw = serde_json::to_value(record(1)).unwrap();
    raw["log_tail"] = json!([format!("{} at /data/al\u{200b}ice/x", ours(1))]);
    let read = CrashRecord::from_untrusted(&raw, &["/data/alice".to_owned()]).unwrap();
    assert!(read.log_tail[0].ends_with("at ~/x"), "{:?}", read.log_tail);
}

#[test]
fn a_date_past_year_9999_is_not_taken() {
    let mut raw = serde_json::to_value(record(1)).unwrap();
    raw["first_at"] = json!("9999-12-31T23:59:59-23:59");
    raw["last_at"] = json!("9999-12-31T23:59:59-23:59");
    assert!(CrashRecord::from_untrusted(&raw, &[]).is_none());
}

#[test]
fn the_home_is_matched_without_case_and_any_users_root_is_masked() {
    assert_eq!(
        mask_home(
            "D:\\Profiles\\\u{c9}LISE\\x",
            "d:\\profiles\\\u{e9}lise",
            true
        ),
        "~\\x"
    );
    assert_eq!(
        mask_homes("C:\\USERS\\ALICE~1\\x /Users/Bob/y /home/carol z", &[]),
        "~\\x ~/y ~ z"
    );
    assert_eq!(
        mask_homes("/usr/home/x /homes/y", &[]),
        "/usr/home/x /homes/y"
    );
}

// The store.

#[test]
fn the_same_signature_keeps_one_record_with_a_count_and_the_times() {
    let f = fixture();
    let first = record(5);
    f.store.record(first.clone()).unwrap();
    let mut again = record(5);
    again.last_at = at(-1);
    f.store.record(again).unwrap();
    f.store.record(record(5)).unwrap();
    let listed = f.store.pending().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, first.id, "the first record stays");
    assert_eq!(listed[0].count, 3);
    assert_eq!(listed[0].first_at, first.first_at);
    assert!(listed[0].last_at > first.last_at);
    assert_eq!(names(&f.data.join(store::STORE)).len(), 1);
}

#[test]
fn at_most_twenty_records_are_kept_and_the_oldest_go_first() {
    let f = fixture();
    for line in 0..25u32 {
        let mut r = record(line);
        r.first_at = at(i64::from(25 - line) / 2);
        r.last_at = r.first_at.clone();
        f.store.record(r).unwrap();
    }
    let listed = f.store.pending().unwrap();
    assert_eq!(listed.len(), MAX_RECORDS);
    let lines: Vec<u32> = listed.iter().map(|r| r.location.line).collect();
    assert!(lines.iter().all(|line| *line >= 5), "{lines:?}");
}

#[test]
fn a_record_older_than_thirty_days_is_deleted() {
    let f = fixture();
    let mut old = record(1);
    old.first_at = at(31);
    old.last_at = at(31);
    f.store.record(old).unwrap();
    f.store.record(record(2)).unwrap();
    let listed = f.store.pending().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].location.line, 2);
    assert_eq!(names(&f.data.join(store::STORE)).len(), 1);
}

#[test]
fn the_largest_possible_records_fit_the_caps() {
    let f = fixture();
    let worst = |line: u32| {
        let mut raw = serde_json::to_value(record(line)).unwrap();
        raw["log_tail"] = json!(vec![format!("{} {}", ours(0), "x".repeat(600)); TAIL_LINES]);
        raw["backtrace"] = json!(vec![
            json!({"function": "y".repeat(240), "file": "f".repeat(100), "line": u32::MAX, "addr": u64::MAX});
            MAX_FRAMES
        ]);
        raw["images"] = json!(vec![
            json!({"name": "n".repeat(100), "addr": u64::MAX, "size": u64::MAX, "vmaddr": u64::MAX,
                   "id": "7b2f0b00-1c3e-4d5a-8f6b-0123456789ab-ffffffff", "code_id": "a".repeat(128)});
            MAX_IMAGES
        ]);
        raw["location"]["line"] = json!(line);
        raw["last_at"] = json!(at(i64::from(line) % 3));
        CrashRecord::from_untrusted(&raw, &[]).unwrap()
    };
    for line in 0..MAX_RECORDS as u32 {
        f.store.record(worst(line)).unwrap();
    }
    let bytes: u64 = fs::read_dir(f.data.join(store::STORE))
        .unwrap()
        .flatten()
        .map(|entry| entry.metadata().unwrap().len())
        .sum();
    assert!(bytes <= MAX_BYTES, "{bytes}");
    assert_eq!(f.store.pending().unwrap().len(), MAX_RECORDS);
}

#[test]
fn corrupt_partial_oversized_and_foreign_files_are_removed_or_left_and_never_panic() {
    let f = fixture();
    f.store.record(record(1)).unwrap();
    let dir = f.data.join(store::STORE);
    let id = Uuid::new_v4().to_string();
    fs::write(dir.join(format!("crash-{id}.json")), b"{\"id\": \"trunc").unwrap();
    let other = Uuid::new_v4().to_string();
    fs::write(
        dir.join(format!("crash-{other}.json")),
        [0u8, 159, 146, 150],
    )
    .unwrap();
    let huge = Uuid::new_v4().to_string();
    fs::write(
        dir.join(format!("crash-{huge}.json")),
        vec![b' '; MAX_RECORD_BYTES as usize + 10],
    )
    .unwrap();
    // A valid record under the wrong name is not read as that name's.
    let misnamed = serde_json::to_vec(&record(9)).unwrap();
    fs::write(dir.join(format!("crash-{}.json", Uuid::new_v4())), misnamed).unwrap();
    fs::write(dir.join("notes.txt"), b"kept").unwrap();
    fs::create_dir(dir.join(format!("crash-{}.json", Uuid::new_v4()))).unwrap();
    let listed = f.store.pending().unwrap();
    assert_eq!(listed.len(), 1);
    let left = names(&dir);
    assert!(left.contains(&"notes.txt".to_owned()));
    assert!(
        left.iter()
            .filter(|name| name.starts_with("crash-"))
            .count()
            <= 2,
        "{left:?}"
    );
}

#[test]
fn removing_deletes_the_record_and_only_by_its_id() {
    let f = fixture();
    let kept = record(1);
    f.store.record(kept.clone()).unwrap();
    let outside = f.data.join(store::STORE).join("..").join("crash-x.json");
    fs::write(&outside, b"mine").unwrap();
    f.store.remove("../crash-x").unwrap();
    f.store.remove(&kept.id.to_uppercase()).unwrap();
    assert!(outside.exists(), "nothing but a UUID names a file");
    assert_eq!(f.store.pending().unwrap().len(), 1);
    f.store.remove(&kept.id).unwrap();
    f.store.remove(&kept.id).unwrap();
    assert!(f.store.pending().unwrap().is_empty());
}

// The id, and what the core's answer "off" erases.

#[test]
fn the_machine_id_is_random_kept_and_erased_with_the_records() {
    let f = fixture();
    f.store.record(record(1)).unwrap();
    let id = settings::machine_id(&f.data).unwrap();
    assert_eq!(id.len(), 32);
    assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(settings::machine_id(&f.data).unwrap(), id, "kept");
    assert_eq!(
        names(&f.data),
        ["connector-crash-id.json", "connector-crashes"]
    );

    erase(&f.data).unwrap();
    assert!(names(&f.data).is_empty(), "{:?}", names(&f.data));
    assert!(f.store.pending().unwrap().is_empty());
    erase(&f.data).unwrap();
    assert_ne!(settings::machine_id(&f.data).unwrap(), id, "a new id");
}

#[test]
fn the_machine_id_is_replaced_after_a_year() {
    let f = fixture();
    let id = settings::machine_id(&f.data).unwrap();
    settings::identity_made(&f.data, &at(364));
    assert_eq!(settings::machine_id(&f.data).unwrap(), "0".repeat(32));
    settings::identity_made(&f.data, &at(366));
    let next = settings::machine_id(&f.data).unwrap();
    assert_ne!(next, "0".repeat(32));
    assert_ne!(next, id);
    assert_eq!(settings::machine_id(&f.data).unwrap(), next, "and kept");
}

#[test]
fn an_id_file_that_is_not_ours_is_replaced() {
    let f = fixture();
    for bytes in [
        &b"not json"[..],
        br#"{"id": "short", "made": "2026-11-01T00:00:00Z"}"#,
        br#"{"id": "0123456789abcdef0123456789abcdeg", "made": "2026-11-01T00:00:00Z"}"#,
        br#"{"id": "0123456789abcdef0123456789abcdef", "made": "later"}"#,
    ] {
        Files::write_private(&f.data.join("connector-crash-id.json"), bytes).unwrap();
        let id = settings::machine_id(&f.data).unwrap();
        assert_ne!(id, "0123456789abcdef0123456789abcdef");
        assert_eq!(id.len(), 32);
    }
}

// What turning sending off races with.

#[test]
fn nothing_is_kept_once_this_process_learned_that_sending_is_off() {
    let f = fixture();
    f.store.learned(false);
    f.store.record(record(1)).unwrap();
    f.store.record_without_waiting(record(2));
    assert!(!f.data.join(store::STORE).exists(), "no record after off");
    f.store.learned(true);
    f.store.record(record(3)).unwrap();
    assert_eq!(f.store.pending().unwrap().len(), 1);
}

#[test]
fn erasing_a_store_that_is_being_written_into_is_retried_and_then_reported() {
    let f = fixture();
    for line in 0..MAX_RECORDS as u32 {
        f.store.record(record(line)).unwrap();
    }
    let writer = {
        let dir = f.data.join(store::STORE);
        std::thread::spawn(move || {
            for n in 0..200 {
                let _ = fs::write(dir.join(format!("noise-{n}")), b"x");
            }
        })
    };
    // A directory that stays written into through every retry is reported as `DirectoryNotEmpty` (and nothing else); the next
    // ask erases it again.
    for _ in 0..8 {
        if let Err(error) = erase(&f.data) {
            assert_eq!(error.kind(), std::io::ErrorKind::DirectoryNotEmpty);
        }
        f.store.record(record(1)).unwrap();
    }
    writer.join().unwrap();
    // Once nobody writes, it erases it all and says so.
    erase(&f.data).unwrap();
    assert!(f.store.pending().unwrap().is_empty());
}

#[test]
fn a_record_is_held_only_by_its_own_id() {
    let f = fixture();
    let kept = record(1);
    f.store.record(kept.clone()).unwrap();
    assert!(f.store.holds(&kept.id));
    assert!(!f.store.holds(&kept.id.to_uppercase()));
    assert!(!f.store.holds("../x"));
    f.store.remove(&kept.id).unwrap();
    assert!(!f.store.holds(&kept.id));
}

// The capture.

#[test]
fn a_capture_makes_a_record_with_the_frames_addresses() {
    // The capture reads the process's ring, which another test may be filling.
    let _serial = tail::testing::SERIAL
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let f = fixture();
    let context = context(&f);
    let capture = |literal| {
        write(
            &context,
            literal,
            0,
            "rust/server/x.rs",
            3,
            backtrace::Backtrace::new_unresolved(),
        );
    };
    // The store's lock is the process's: a capture that finds it held by another test drops its record, as it should.
    let mut records = Vec::new();
    for _ in 0..200 {
        capture(Some("boom"));
        records = f.store.pending().unwrap();
        if !records.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].exception.message, "boom");
    assert!(!records[0].backtrace.is_empty());
    assert!(
        records[0].backtrace.iter().all(|frame| frame.addr != 0),
        "{:#?}",
        records[0].backtrace
    );
    assert_eq!(records[0].version, crate::identity::VERSION);
}

#[test]
fn the_tail_goes_through_the_log_backstop_before_it_is_stored() {
    use tracing_subscriber::layer::SubscriberExt;
    let _serial = tail::testing::SERIAL
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let f = fixture();
    let mut context = context(&f);
    // The log's other backstop (a private conversation id named by a hash) is applied to the tail as to the file.
    context.redact = |line| line.replace("claude-session-1", "h:0123456789ab");
    tail::testing::clear();
    let subscriber = tracing_subscriber::registry().with(tail::layer(|| false));
    tracing::subscriber::with_default(subscriber, || {
        tracing::warn!(target: "sidevoice_connector::x", "lost claude-session-1 somewhere");
    });
    let mut records = Vec::new();
    for _ in 0..200 {
        write(
            &context,
            Some("boom"),
            0,
            "src/x.rs",
            3,
            backtrace::Backtrace::new_unresolved(),
        );
        records = f.store.pending().unwrap();
        if !records.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    tail::testing::clear();
    assert_eq!(records.len(), 1);
    let tail = records[0].log_tail.join("\n");
    assert!(
        tail.contains("h:0123456789ab") && !tail.contains("claude-session-1"),
        "{tail}"
    );
}

#[test]
fn frames_start_at_the_code_that_panicked_and_images_are_the_ones_that_hold_a_frame() {
    let f = |function: &str, addr| frame(function, addr);
    let trimmed = trace::trim(vec![
        f("backtrace::capture::Backtrace::new_unresolved", 1),
        f(
            "sidevoice_connector::runtime::logging::panic_hook::{{closure}}",
            2,
        ),
        f("std::panicking::rust_panic_with_hook", 3),
        f("std::sys::backtrace::__rust_end_short_backtrace", 4),
        f("rust_begin_unwind", 5),
        f("core::panicking::panic_fmt", 6),
        f("sidevoice_connector::x::boom", 7),
        f("std::panicking::try::do_call", 8),
    ]);
    let functions: Vec<_> = trimmed
        .iter()
        .map(|frame| frame.function.clone().unwrap())
        .collect();
    assert_eq!(
        functions,
        [
            "sidevoice_connector::x::boom",
            "std::panicking::try::do_call"
        ]
    );
    assert_eq!(
        trace::trim(vec![f("a", 1), f("b", 2)]).len(),
        2,
        "no machinery, no cut"
    );

    let images = [image(0x1000, 0x1000), image(0x9000, 0x100)];
    let held = trace::holding(&images, &[frame("a", 0x1ff0), frame("b", 0x20_0000)]);
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].addr, 0x1000);
    assert!(
        trace::holding(&images, &[frame("a", 0x2000)]).is_empty(),
        "past the end"
    );

    // A real backtrace: frames with an address each (the unwinder's terminal frame, address 0, is not one), and names.
    let mut here = backtrace::Backtrace::new();
    here.resolve();
    let frames = trace::frames(&here);
    assert!(!frames.is_empty());
    assert!(frames.iter().all(|frame| frame.addr != 0), "{frames:#?}");
    assert!(
        frames.iter().any(|frame| frame.function.is_some()),
        "{frames:#?}"
    );
    // The program's own module is listed with a bare name, when it has a debug id.
    assert!(trace::loaded()
        .iter()
        .all(|image| !image.name.contains(['/', '\\'])));
}

#[test]
fn a_frame_without_an_address_is_never_kept_or_sent() {
    let mut raw = panic(Some("boom"), 1);
    raw.frames = vec![frame("a", 0x1010), frame("end of the stack", 0)];
    let stored = CrashRecord::from_panic(raw, &[]);
    assert_eq!(stored.backtrace.len(), 1);
    // Nor when it is read back from a file that has one.
    let mut json = serde_json::to_value(&stored).unwrap();
    json["backtrace"] = json!([
        {"function": "a", "addr": 0x1010},
        {"function": "z", "addr": 0}
    ]);
    let read = CrashRecord::from_untrusted(&json, &[]).unwrap();
    assert_eq!(read.backtrace.len(), 1);
    let sent = serde_json::to_string(&event(&read, "00")).unwrap();
    assert!(!sent.contains("\"z\"") && !sent.contains("0x0\""), "{sent}");
}

// The event.

fn everything() -> CrashRecord {
    let mut raw = panic(None, 7);
    raw.formatted_len = 42;
    raw.file = "/home/canary-user-7f3a/.cargo/registry/src/x/a.rs";
    raw.frames = vec![
        Frame {
            function: Some("a::b".to_owned()),
            file: Some("/home/canary-user-7f3a/src/a.rs".to_owned()),
            line: Some(3),
            addr: 0x1010,
        },
        Frame {
            function: None,
            file: None,
            line: None,
            addr: 0x1020,
        },
    ];
    raw.images = vec![Image {
        name: "/home/canary-user-7f3a/.local/share/sidevoice/bin/sidevoice-connector".to_owned(),
        ..image(0x1000, 0x1000)
    }];
    raw.tail = vec![format!(
        "-0.250s ERROR sidevoice_connector::x: failed device=d1 path=/home/canary-user-7f3a/x {}",
        "ok"
    )];
    CrashRecord::from_panic(raw, &["/home/canary-user-7f3a".to_owned()])
}

#[test]
fn the_event_carries_the_closed_field_list_and_nothing_else() {
    let record = everything();
    let machine = "0123456789abcdef0123456789abcdef";
    let value = serde_json::to_value(event(&record, machine)).unwrap();
    assert_eq!(
        keys(&value),
        [
            "debug_meta",
            "event_id",
            "exception",
            "extra",
            "level",
            "platform",
            "release",
            "tags",
            "timestamp"
        ],
        "{value}"
    );
    assert!(
        value.get("server_name").is_none(),
        "never the host name: {value}"
    );
    assert_eq!(value["event_id"], record.id.replace('-', "").as_str());
    assert_eq!(value["level"], "fatal");
    assert_eq!(value["release"], "sidevoice-connector@1.2.3");
    assert_eq!(keys(&value["tags"]), {
        let mut tags = TAGS.to_vec();
        tags.sort_unstable();
        tags
    });
    assert_eq!(value["tags"]["service.name"], "sidevoice-connector");
    assert_eq!(value["tags"]["installation.id"], machine);
    let mut extras = EXTRAS.to_vec();
    extras.sort_unstable();
    assert_eq!(keys(&value["extra"]), extras);
    assert_eq!(value["extra"]["len"], 42);
    for local in ["count", "first_at"] {
        assert!(
            value["extra"].get(local).is_none(),
            "{local} stays on the machine"
        );
    }
    assert_eq!(value["extra"]["location"], "unknown:7");
    let exception = &value["exception"]["values"][0];
    assert_eq!(keys(exception), ["stacktrace", "type", "value"]);
    assert_eq!(exception["type"], "panic");
    assert_eq!(exception["value"], FORMATTED_MARKER);
    let frames = exception["stacktrace"]["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 2);
    for frame in frames {
        assert!(
            keys(frame)
                .iter()
                .all(|key| ["filename", "function", "instruction_addr", "lineno"].contains(key)),
            "{frame}"
        );
    }
    // Outermost first, as Sentry reads it.
    assert_eq!(frames[1]["function"], "a::b");
    assert_eq!(frames[1]["filename"], "a.rs");
    assert_eq!(frames[1]["instruction_addr"], "0x1010");
    let images = value["debug_meta"]["images"].as_array().unwrap();
    assert_eq!(images.len(), 1);
    assert!(
        keys(&images[0]).iter().all(|key| [
            "arch",
            "code_id",
            "id",
            "image_addr",
            "image_size",
            "image_vmaddr",
            "name",
            "type"
        ]
        .contains(key)),
        "{}",
        images[0]
    );
    assert_eq!(images[0]["name"], "sidevoice-connector");
    assert_eq!(images[0]["id"], ID);
    // Without a formatted message there is no length.
    let literal = serde_json::to_value(event(&record_with_literal(), machine)).unwrap();
    assert!(literal["extra"].get("len").is_none());
}

fn record_with_literal() -> CrashRecord {
    record(1)
}

#[test]
fn no_home_or_install_path_reaches_the_event() {
    let json = serde_json::to_string(&event(&everything(), "00")).unwrap();
    for canary in ["canary-user", "/home/", ".cargo", ".local", "registry"] {
        assert!(!json.contains(canary), "{canary} in {json}");
    }
    // Nor the name of the machine (a short one could be a word of the event's own).
    use crate::platform::user::{User, UserId};
    let host = UserId::host_name();
    if host.len() >= 6 {
        assert!(!json.contains(&host), "the host name is in {json}");
    }
}

// The sender, against a Sentry that is this test's own socket.

/// What a request to the fake Sentry was: its head and its body.
type Request = (String, Vec<u8>);

/// A server that answers every request with `status` and keeps what it was sent.
struct Sentry {
    address: std::net::SocketAddr,
    requests: std::sync::Arc<std::sync::Mutex<Vec<Request>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Sentry {
    fn start(status: u16) -> Self {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::sync::atomic::Ordering;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (seen, done) = (requests.clone(), stop.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if done.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                    head.push_str(&line);
                }
                let mut body = vec![0; length];
                let _ = reader.read_exact(&mut body);
                seen.lock().unwrap().push((head, body));
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
            }
        });
        Self {
            address,
            requests,
            stop,
        }
    }

    fn dsn(&self) -> sentry::types::Dsn {
        format!("http://public@{}/42", self.address)
            .parse()
            .unwrap()
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Sentry {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(self.address);
    }
}

fn later() -> std::time::Instant {
    std::time::Instant::now() + std::time::Duration::from_secs(30)
}

const QUICK: Patience = Patience {
    first: std::time::Duration::from_millis(1),
    longest: std::time::Duration::from_millis(4),
    budget: std::time::Duration::from_millis(60),
};

#[test]
fn a_record_is_sent_under_its_own_id_and_deleted_once_sentry_took_it() {
    let f = fixture();
    let server = Sentry::start(200);
    let kept = record(1);
    f.store.record(kept.clone()).unwrap();
    f.store.record(record(2)).unwrap();
    assert_eq!(
        drain(&context(&f), &|| Some(true), &server.dsn(), later()),
        2
    );
    assert!(f.store.pending().unwrap().is_empty());
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let bodies: String = requests
        .iter()
        .map(|(_, body)| String::from_utf8_lossy(body).into_owned())
        .collect();
    assert!(bodies.contains(&kept.id.replace('-', "")), "{bodies}");
    assert!(bodies.contains("installation.id"));
    assert!(bodies.contains("sidevoice-connector@1.2.3"), "{bodies}");
    let head = requests[0].0.to_ascii_lowercase();
    assert!(head.starts_with("post /api/42/envelope/ "), "{head}");
    assert!(head.contains("x-sentry-auth:"), "{head}");
    assert!(
        head.contains("content-type: application/x-sentry-envelope"),
        "{head}"
    );
    let id = settings::machine_id(&f.data).unwrap();
    assert!(
        bodies.contains(&id),
        "the connector's own id is the one sent"
    );
}

#[test]
fn a_failed_send_keeps_the_record_and_ends_the_pass() {
    let f = fixture();
    let server = Sentry::start(500);
    f.store.record(record(1)).unwrap();
    f.store.record(record(2)).unwrap();
    assert_eq!(
        drain(&context(&f), &|| Some(true), &server.dsn(), later()),
        0
    );
    assert_eq!(f.store.pending().unwrap().len(), 2);
    assert_eq!(
        server.requests().len(),
        1,
        "one failure and no more tries this start"
    );
    // A server that is not there is the same: nothing panics, nothing is lost.
    let gone = {
        let server = Sentry::start(200);
        let dsn = server.dsn();
        drop(server);
        dsn
    };
    assert_eq!(drain(&context(&f), &|| Some(true), &gone, later()), 0);
    assert_eq!(f.store.pending().unwrap().len(), 2);
}

#[test]
fn a_core_that_cannot_be_asked_is_not_off_and_nothing_is_sent_blind() {
    let f = fixture();
    let server = Sentry::start(200);
    f.store.record(record(1)).unwrap();
    let asked = std::cell::Cell::new(0);
    let silent = || {
        asked.set(asked.get() + 1);
        None
    };
    assert_eq!(drain(&context(&f), &silent, &server.dsn(), later()), 0);
    pass(&context(&f), &silent, &server.dsn(), QUICK);
    assert!(asked.get() > 2, "the pass kept asking for a while");
    assert!(server.requests().is_empty());
    assert_eq!(f.store.pending().unwrap().len(), 1, "the record waits");
    assert!(
        !f.data.join("connector-crash-id.json").exists(),
        "no id is made when nothing is sent"
    );
}

#[test]
fn a_core_that_says_off_erases_the_records_and_the_id_and_nothing_leaves() {
    let f = fixture();
    let server = Sentry::start(200);
    f.store.record(record(1)).unwrap();
    settings::machine_id(&f.data).unwrap();
    let context = context(&f);
    pass(&context, &|| Some(false), &server.dsn(), QUICK);
    assert!(server.requests().is_empty());
    assert!(names(&f.data).is_empty(), "{:?}", names(&f.data));
    assert!(context.store.is_off(), "and none is made after it");
}

#[test]
fn the_core_is_asked_again_before_each_record() {
    let f = fixture();
    let server = Sentry::start(200);
    f.store.record(record(1)).unwrap();
    f.store.record(record(2)).unwrap();
    // On when the pass starts and for the whole of the first record (before its id and after it); off for the second.
    let answers = std::cell::RefCell::new(vec![Some(true), Some(true), Some(true), Some(false)]);
    let ask = || answers.borrow_mut().remove(0);
    pass(&context(&f), &ask, &server.dsn(), QUICK);
    assert_eq!(server.requests().len(), 1, "the second was never posted");
    assert!(names(&f.data).is_empty(), "{:?}", names(&f.data));
}

#[test]
fn an_off_that_comes_once_the_id_exists_leaves_no_id_and_posts_nothing() {
    let f = fixture();
    let server = Sentry::start(200);
    f.store.record(record(1)).unwrap();
    // On at the first look, off at the second: after the id was made and before the POST.
    let answers = std::cell::RefCell::new(vec![Some(true), Some(false)]);
    let ask = || answers.borrow_mut().remove(0);
    let context = context(&f);
    assert_eq!(drain(&context, &ask, &server.dsn(), later()), 0);
    assert!(server.requests().is_empty());
    assert!(names(&f.data).is_empty(), "{:?}", names(&f.data));
    assert!(context.store.is_off());
    // A core that cannot be asked at the second look: no id is kept either, and the record waits.
    context.store.learned(true);
    f.store.record(record(2)).unwrap();
    let answers = std::cell::RefCell::new(vec![Some(true), None]);
    let ask = || answers.borrow_mut().remove(0);
    assert_eq!(drain(&context, &ask, &server.dsn(), later()), 0);
    assert!(server.requests().is_empty());
    assert!(!f.data.join("connector-crash-id.json").exists());
    assert_eq!(f.store.pending().unwrap().len(), 1, "the record waits");
}

#[test]
fn a_record_that_an_off_deleted_meanwhile_is_not_posted() {
    let f = fixture();
    let server = Sentry::start(200);
    let doomed = record(1);
    f.store.record(doomed.clone()).unwrap();
    // The record's file goes between the list and the POST (an erasing from another thread): at the second look here.
    let gone = f
        .data
        .join(store::STORE)
        .join(format!("crash-{}.json", doomed.id));
    let looks = std::cell::Cell::new(0);
    let ask = || {
        looks.set(looks.get() + 1);
        if looks.get() == 2 {
            fs::remove_file(&gone).unwrap();
        }
        Some(true)
    };
    assert_eq!(drain(&context(&f), &ask, &server.dsn(), later()), 0);
    assert!(server.requests().is_empty(), "nothing was posted");
    assert!(f.store.pending().unwrap().is_empty());
}

#[test]
fn a_pass_waits_for_a_core_that_is_still_starting() {
    let f = fixture();
    let server = Sentry::start(200);
    f.store.record(record(1)).unwrap();
    let tries = std::cell::Cell::new(0);
    let ask = || {
        tries.set(tries.get() + 1);
        (tries.get() > 3).then_some(true)
    };
    pass(
        &context(&f),
        &ask,
        &server.dsn(),
        Patience {
            budget: std::time::Duration::from_secs(5),
            ..QUICK
        },
    );
    assert_eq!(server.requests().len(), 1);
    assert!(f.store.pending().unwrap().is_empty());
}

#[test]
fn the_watch_applies_an_off_at_once_and_a_core_that_does_not_answer_changes_nothing() {
    let f = fixture();
    let context = context(&f);
    f.store.record(record(1)).unwrap();
    settings::machine_id(&f.data).unwrap();
    // No answer, then on: nothing is erased, nothing is sent from here, and what was learned is on.
    let answers = std::cell::RefCell::new(vec![None, Some(true)]);
    let ask = || answers.borrow_mut().remove(0);
    let laps = std::cell::Cell::new(0);
    let mut rest = || {
        laps.set(laps.get() + 1);
        laps.get() <= 2
    };
    report::watch(&context, &ask, &mut rest);
    assert_eq!(laps.get(), 3);
    assert_eq!(f.store.pending().unwrap().len(), 1);
    assert!(f.data.join("connector-crash-id.json").exists());
    assert!(!context.store.is_off());
    // Off, set elsewhere while the connector runs: the next look erases the records and the id, and no more are made.
    report::settle(&context, &|| Some(false));
    assert!(names(&f.data).is_empty(), "{:?}", names(&f.data));
    assert!(context.store.is_off());
    context.store.record(record(2)).unwrap();
    context.store.record_without_waiting(record(3));
    assert!(names(&f.data).is_empty(), "no record is made after it");
    // Back on: records are made again.
    report::settle(&context, &|| Some(true));
    context.store.record(record(4)).unwrap();
    assert_eq!(context.store.pending().unwrap().len(), 1);
    // And asking for another look with no sender thread is nothing.
    recheck();
}

#[test]
fn a_pass_stops_when_its_time_is_spent() {
    let f = fixture();
    let server = Sentry::start(200);
    f.store.record(record(1)).unwrap();
    let spent = std::time::Instant::now();
    assert_eq!(drain(&context(&f), &|| Some(true), &server.dsn(), spent), 0);
    assert!(server.requests().is_empty());
    assert_eq!(f.store.pending().unwrap().len(), 1);
}

#[test]
fn a_store_that_cannot_be_read_is_one_log_line_and_no_panic() {
    let f = fixture();
    let server = Sentry::start(200);
    // A file where the records' directory should be.
    fs::write(f.data.join(store::STORE), b"not a directory").unwrap();
    assert_eq!(
        drain(&context(&f), &|| Some(true), &server.dsn(), later()),
        0
    );
    assert!(server.requests().is_empty());
    pass(&context(&f), &|| Some(true), &server.dsn(), QUICK);
}

// What the core is asked, and what the command forwards.

/// A core's local endpoint: its health, as `Profile::health` identifies the core, and the crash report setting, whose
/// value is `send` and which a PUT changes. `answers` counts what was asked.
struct FakeCore {
    profile: Profile,
    asked: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    _root: tempfile::TempDir,
}

fn fake_core(endpoint_for_crash: u16, send: bool) -> FakeCore {
    use crate::platform::ipc::{Address, Channel, Channels, Endpoint};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let root = temp_dir("fake-core");
    let profile = Profile::for_test(root.path(), &[]);
    let core = profile.core_ready.parent().unwrap().to_path_buf();
    let endpoint = Endpoint::new(&core, "local");
    Files::write_json(
        &profile.core_ready,
        &json!({"pid": std::process::id(), "launch_id": "launch-1", "endpoint": endpoint.to_string(),
            "connector_id": "c", "token": "t", "connector_protocols": [3]}),
    )
    .unwrap();
    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = asked.clone();
    let listener = Channels::listen(&endpoint).unwrap();
    // The listener lives on a runtime of its own, as the core does in its process.
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let mut send = send;
            loop {
                let Ok(mut stream) = listener.accept().await else {
                    return;
                };
                let mut request = vec![0u8; 4096];
                let n = stream.read(&mut request).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&request[..n]).into_owned();
                let first = text.lines().next().unwrap_or("").to_owned();
                seen.lock().unwrap().push(first.clone());
                let (status, body) = if first.starts_with("GET /api/local/health ") {
                    (
                        200,
                        json!({"launch_id": "launch-1", "pid": std::process::id()}).to_string(),
                    )
                } else if endpoint_for_crash != 200 {
                    (endpoint_for_crash, String::new())
                } else if first.starts_with("PUT /api/local/crash-reports ") {
                    let body = text.split_once("\r\n\r\n").map_or("", |(_, body)| body);
                    send = serde_json::from_str::<Value>(body)
                        .ok()
                        .and_then(|body| body["send"].as_bool())
                        .unwrap_or(send);
                    (200, json!({"send": send}).to_string())
                } else if first.starts_with("GET /api/local/crash-reports ") {
                    (200, json!({"send": send}).to_string())
                } else {
                    (404, String::new())
                };
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
    });
    FakeCore {
        profile,
        asked,
        _root: root,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_connector_asks_the_core_and_never_keeps_the_answer() {
    let on = fake_core(200, true);
    assert!(on.profile.crash_reports().await.unwrap());
    let asked = on.asked.lock().unwrap().clone();
    assert!(
        asked
            .iter()
            .any(|line| line.starts_with("GET /api/local/health ")),
        "the core is identified first: {asked:?}"
    );
    assert!(asked
        .iter()
        .any(|line| line.starts_with("GET /api/local/crash-reports ")));
    assert!(
        !on.profile.data.join("crash-reports.json").exists()
            && !on
                .profile
                .data
                .join("connector-crash-reports.json")
                .exists(),
        "no setting of the connector's own"
    );
    let off = fake_core(200, false);
    assert!(!off.profile.crash_reports().await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_core_with_no_such_setting_or_no_core_at_all_is_an_error_not_off() {
    let old = fake_core(404, true);
    assert!(old.profile.crash_reports().await.is_err());
    let broken = fake_core(500, true);
    assert!(broken.profile.crash_reports().await.is_err());
    let root = temp_dir("no-core");
    let none = Profile::for_test(root.path(), &[]);
    assert!(none.crash_reports().await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_command_forwards_to_the_core_and_off_erases_the_connectors_own() {
    let core = fake_core(200, true);
    let data = core.profile.data.clone();
    let store = CrashStore::new(&data, Vec::new());
    store.record(record(1)).unwrap();
    settings::machine_id(&data).unwrap();

    assert!(forward(&core.profile, None).await.unwrap());
    assert!(forward(&core.profile, Some(true)).await.unwrap());
    assert_eq!(
        store.pending().unwrap().len(),
        1,
        "on and status erase nothing"
    );

    assert!(!forward(&core.profile, Some(false)).await.unwrap());
    assert!(
        core.asked
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.starts_with("PUT /api/local/crash-reports ")),
        "the core's own setting is what changed"
    );
    assert!(store.pending().unwrap().is_empty());
    assert!(!data.join("connector-crash-id.json").exists());
    assert!(
        !forward(&core.profile, None).await.unwrap(),
        "the core says it"
    );

    // No core: a keyed failure, and nothing is erased on a guess.
    let lonely = temp_dir("lonely");
    let profile = Profile::for_test(lonely.path(), &[]);
    CrashStore::new(&profile.data, Vec::new())
        .record(record(1))
        .unwrap();
    let error = forward(&profile, Some(false)).await.unwrap_err();
    let keyed = crate::messages::keyed(&error);
    assert_eq!(keyed.key, "crash-reports.core-unavailable");
    assert_eq!(
        CrashStore::new(&profile.data, Vec::new())
            .pending()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_off_command_fails_when_what_the_connector_kept_cannot_be_erased() {
    let core = fake_core(200, true);
    let data = core.profile.data.clone();
    // A file where the records' directory should be: it cannot be removed as a directory.
    fs::write(data.join(store::STORE), b"not a directory").unwrap();
    settings::machine_id(&data).unwrap();
    let error = forward(&core.profile, Some(false)).await.unwrap_err();
    let keyed = crate::messages::keyed(&error);
    assert_eq!(keyed.key, "crash-reports.erase-failed", "{error}");
    assert_eq!(keyed.params["kind"], "NotADirectory");
    // The id went, and the core's setting is off whatever happened here; asking changes and erases nothing.
    assert!(!data.join("connector-crash-id.json").exists());
    assert!(!forward(&core.profile, None).await.unwrap());
    assert!(data.join(store::STORE).exists());
}
