//! The connector's own crash record, left for the core to take in.
//!
//! The core owns the store of crash records and the API paired apps read them through; the connector only writes. When
//! the connector daemon panics it drops one complete record, once and never changed, in a spool directory beside the
//! core's (`D/crash-inbox`, [`INBOX`]). The core takes each file in when it next reads its store, checks it again,
//! applies the caps and deletes the file. Nothing here reads a record, sends one, or reaches the core over the link: a
//! record is a file, so a crash loop of the connector or a core that is down loses nothing, and the link has no
//! message that could change a record.
//!
//! The fields are the core's closed list (`sidevoice-core`, `rust/crash.rs`), which is the contract: a reader there
//! takes nothing else and makes the signature itself. The panic message is a `&'static str` literal or the fixed
//! marker, never the formatted text (`docs/logging.md`, section 9).

use crate::platform::fs::{Files, Fs};
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// The spool's name in `D`; the same name the core takes in (it cannot be shared: the core's crate is not a dependency).
pub const INBOX: &str = "crash-inbox";
/// The most records waiting in the spool: a crash loop does not fill the disk while the core is down. The core applies
/// its own caps when it takes them in.
const MAX_WAITING: usize = 20;
/// The caps the core applies when it takes a record in (`sidevoice-core`, `rust/crash.rs`): a record within them is never
/// changed or dropped there.
const TAIL_LINES: usize = 20;
const MAX_FRAMES: usize = 64;
const MAX_TEXT: usize = 200;
const MAX_FRAME: usize = 240;
const MAX_LINE: usize = 600;
const MAX_RECORD_BYTES: usize = 64 * 1024;
/// How much of the end of the log is read to find them.
const TAIL_BYTES: u64 = 64 * 1024;
const FORMATTED_MARKER: &str = "<formatted panic message omitted>";

/// What a panic is.
pub struct Panic<'a> {
    /// The payload when it is a `&'static str`; `None` for a formatted or non-text payload.
    pub literal: Option<&'a str>,
    pub file: &'a str,
    pub line: u32,
    /// `Backtrace`'s `Display`.
    pub backtrace: &'a str,
}

/// Where this process runs.
pub struct Process<'a> {
    pub version: &'a str,
    pub os: &'a str,
    pub arch: &'a str,
    /// The connector's log, whose last lines the record carries.
    pub log: &'a Path,
    /// Every spelling of the home directory, each shown as `~`.
    pub homes: &'a [String],
}

/// Leaves the record of `panic` in the spool beside `log`. Best effort, silent: the hook that calls it is panicking and
/// the panic is already in the log.
pub fn leave(process: &Process<'_>, panic: &Panic<'_>, now: &str) {
    let Some(data) = process.log.parent() else {
        return;
    };
    let inbox = data.join(INBOX);
    if Files::ensure_private_dir(&inbox).is_err() || waiting(&inbox) >= MAX_WAITING {
        return;
    }
    let id = uuid::Uuid::new_v4().to_string();
    let record = record(process, panic, now, &id);
    if let Some(bytes) = fit(record) {
        // Staged under a name the core ignores (`.crash-*.json.<random>`) and renamed whole: a core reading now sees the
        // record complete and with one name, or not at all.
        let _ = Files::write_private(&inbox.join(format!("crash-{id}.json")), &bytes);
    }
}

fn waiting(inbox: &Path) -> usize {
    fs::read_dir(inbox).map_or(0, |entries| {
        entries
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("crash-"))
            .count()
    })
}

/// The record as bytes, within [`MAX_RECORD_BYTES`]: the oldest tail lines go first, then the last frames.
fn fit(mut record: Value) -> Option<Vec<u8>> {
    loop {
        let bytes = serde_json::to_vec(&record).ok()?;
        if bytes.len() <= MAX_RECORD_BYTES {
            return Some(bytes);
        }
        let key = if record["log_tail"].as_array().is_some_and(|l| !l.is_empty()) {
            "log_tail"
        } else if record["backtrace"]
            .as_array()
            .is_some_and(|l| !l.is_empty())
        {
            "backtrace"
        } else {
            return None;
        };
        let items = record[key].as_array_mut()?;
        if key == "log_tail" {
            items.remove(0);
        } else {
            items.pop();
        }
    }
}

fn record(process: &Process<'_>, panic: &Panic<'_>, now: &str, id: &str) -> Value {
    let hide = |text: &str| mask_homes(text, process.homes);
    let message = clean(&hide(panic.literal.unwrap_or(FORMATTED_MARKER)), MAX_TEXT);
    let message = if message.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        message
    } else {
        FORMATTED_MARKER.to_owned()
    };
    let file = clean(&hide(panic.file), MAX_TEXT).replace('\\', "/");
    let relative = !file.starts_with('/')
        && file.split('/').all(|part| part != ".." && !part.is_empty())
        && file
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./-".contains(c));
    let backtrace: Vec<String> = frames(&hide(panic.backtrace))
        .iter()
        .map(|frame| clean(frame, MAX_FRAME))
        .filter(|frame| is_frame(frame))
        .take(MAX_FRAMES)
        .collect();
    let lines: Vec<String> = tail(process.log)
        .iter()
        .map(|line| clean(&hide(line), MAX_LINE))
        .filter(|line| is_ours(line))
        .collect();
    let skip = lines.len().saturating_sub(TAIL_LINES);
    json!({
        "id": id,
        "piece": "connector",
        "version": process.version,
        "os": process.os,
        "arch": process.arch,
        "first_at": now,
        "last_at": now,
        "count": 1,
        "exception": {"type": "panic", "message": message},
        "location": {"file": if relative { file.as_str() } else { "unknown" }, "line": panic.line},
        "backtrace": backtrace,
        "log_tail": lines.into_iter().skip(skip).collect::<Vec<_>>(),
    })
}

/// The frames of a `Backtrace`'s `Display`: `   3: symbol` and, under it, `at file:line`, one string each.
fn frames(backtrace: &str) -> Vec<String> {
    let mut frames: Vec<String> = Vec::new();
    for line in backtrace.lines().map(str::trim) {
        if let Some((index, symbol)) = line.split_once(": ") {
            if !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()) {
                frames.push(symbol.to_owned());
                continue;
            }
        }
        if let (Some(place), Some(last)) = (line.strip_prefix("at "), frames.last_mut()) {
            last.push_str(" at ");
            last.push_str(place);
        }
    }
    frames
}

/// `text` with control characters as spaces and invisible format characters (bidi controls among them) removed, cut to
/// at most `max` characters. The core does the same when it takes a record in.
fn clean(text: &str, max: usize) -> String {
    text.chars()
        .filter(|c| !is_format(*c))
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max)
        .collect()
}

/// Unicode format characters (category Cf): they change how a text is shown without being seen.
fn is_format(c: char) -> bool {
    matches!(c,
        '\u{ad}' | '\u{600}'..='\u{605}' | '\u{61c}' | '\u{6dd}' | '\u{70f}' | '\u{180e}'
        | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{206f}' | '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{110bd}'
        | '\u{1d173}'..='\u{1d17a}' | '\u{e0000}'..='\u{e007f}')
}

/// `text` with the invisible format characters removed, so one inside a name cannot keep it from being found.
fn strip_format(text: &str) -> String {
    text.chars().filter(|c| !is_format(*c)).collect()
}

/// `text` with the home directory shown as `~`: the format characters are removed first, every spelling of the home is
/// matched (without regard to case on Windows and macOS, whose default file systems ignore it), and then the user
/// segment of any path under a users root (`/Users/<name>`, `/home/<name>`, `C:\Users\<name>`, which also covers a
/// Windows 8.3 short name) is hidden whatever the home says.
fn mask_homes(text: &str, homes: &[String]) -> String {
    let fold = cfg!(any(windows, target_os = "macos"));
    let text = homes
        .iter()
        .filter(|home| home.len() > 1)
        .fold(strip_format(text), |text, home| {
            mask_home(&text, &strip_format(home), fold)
        });
    mask_users(&text)
}

fn mask_home(text: &str, home: &str, fold: bool) -> String {
    if !fold {
        return text.replace(home, "~");
    }
    let needle: Vec<char> = home.chars().flat_map(char::to_lowercase).collect();
    // The length in `text` of what matches `needle` when both are lowercased, if the text starts with it.
    let matches = |text: &str| {
        let (mut seen, mut len) = (0, 0);
        for c in text.chars() {
            if seen == needle.len() {
                break;
            }
            for lower in c.to_lowercase() {
                if needle.get(seen) != Some(&lower) {
                    return None;
                }
                seen += 1;
            }
            len += c.len_utf8();
        }
        (seen == needle.len()).then_some(len)
    };
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(c) = text[at..].chars().next() {
        if let Some(len) = matches(&text[at..]) {
            out.push('~');
            at += len;
        } else {
            out.push(c);
            at += c.len_utf8();
        }
    }
    out
}

/// `text` with the user segment of every path under a users root replaced, with the root, by `~`.
fn mask_users(text: &str) -> String {
    const ROOTS: [(&str, bool); 4] = [
        ("/users/", false),
        ("/home/", false),
        (":\\users\\", true),
        (":/users/", true),
    ];
    // ASCII lowercasing keeps every byte offset.
    let low = text.to_ascii_lowercase();
    let bytes = low.as_bytes();
    let word = |b: u8| b.is_ascii_alphanumeric() || b"_.-/~".contains(&b);
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    let mut i = 0;
    while i < bytes.len() {
        let hit = ROOTS.iter().find_map(|(root, drive)| {
            if !bytes[i..].starts_with(root.as_bytes()) {
                return None;
            }
            let start = if *drive {
                (i > 0 && bytes[i - 1].is_ascii_alphabetic()).then(|| i - 1)?
            } else {
                i
            };
            if start < at || (start > 0 && word(bytes[start - 1])) {
                return None;
            }
            let from = i + root.len();
            let len = bytes[from..]
                .iter()
                .position(|b| b"/\\\"',:)] \t\r\n".contains(b))
                .unwrap_or(bytes.len() - from);
            (len > 0).then_some((start, from + len))
        });
        if let Some((start, end)) = hit {
            out.push_str(&text[at..start]);
            out.push('~');
            at = end;
            i = end;
        } else {
            i += 1;
        }
    }
    out.push_str(&text[at..]);
    out
}

/// A frame as [`frames`] makes it: a symbol, then optionally ` at <path>:<line>[:<column>]`.
fn is_frame(frame: &str) -> bool {
    let (symbol, place) = match frame.rsplit_once(" at ") {
        Some((symbol, place)) => (symbol, Some(place)),
        None => (frame, None),
    };
    fn number(text: &str) -> Option<&str> {
        text.rsplit_once(':')
            .filter(|(_, n)| !n.is_empty() && n.len() <= 9 && n.bytes().all(|b| b.is_ascii_digit()))
            .map(|(rest, _)| rest)
    }
    let place_ok = place.is_none_or(|place| {
        number(place).is_some_and(|path| {
            let path = number(path).unwrap_or(path);
            !path.is_empty()
                && path
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_./\\~:+-".contains(c))
        })
    });
    !symbol.is_empty()
        && symbol
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_:<>{}$.,&*()[]'+-@# ".contains(c))
        && place_ok
}

/// Whether `line` is a line of our own log at `warn` or `error`: `<RFC 3339 time> <LEVEL> [<span>: ]<target>: ...`, the
/// target one of ours. A span the event was logged in (`name{fields}:` or `name:`, as many as there are) comes before
/// the target. A dependency's line, a debug line and the service's raw stderr are none of these.
fn is_ours(line: &str) -> bool {
    let mut parts = line.split_whitespace();
    let (Some(time), Some(level)) = (parts.next(), parts.next()) else {
        return false;
    };
    let is_time = time.len() >= 20
        && time.ends_with('Z')
        && time.as_bytes()[4] == b'-'
        && time.as_bytes()[10] == b'T'
        && time
            .bytes()
            .all(|b| b.is_ascii_digit() || b"-T:.Z".contains(&b));
    if !is_time || !matches!(level, "WARN" | "ERROR") {
        return false;
    }
    let rest = line.trim_start()[time.len()..].trim_start();
    let rest = rest[level.len()..].trim_start();
    let own_targets = ["sidevoice_connector", "sidevoice_platform", "panic"];
    let mut rest = rest;
    loop {
        let Some((head, after)) = split_head(rest) else {
            return false;
        };
        if own_targets.iter().any(|name| {
            head == *name
                || head
                    .strip_prefix(name)
                    .is_some_and(|rest| rest.starts_with("::"))
        }) {
            return true;
        }
        let ident = |text: &str| {
            !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        };
        if !(ident(head) || head.contains('{') && head.ends_with('}')) {
            return false;
        }
        rest = after.trim_start();
    }
}

/// The first `target:` or `span{fields}:` of `rest` and what follows it.
fn split_head(rest: &str) -> Option<(&str, &str)> {
    let name = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    if name > 0 && rest[name..].starts_with('{') {
        let (mut quoted, mut escaped) = (false, false);
        for (at, c) in rest.char_indices().skip_while(|(at, _)| *at <= name) {
            match c {
                _ if escaped => escaped = false,
                '\\' if quoted => escaped = true,
                '"' => quoted = !quoted,
                '}' if !quoted => {
                    return rest[at + 1..]
                        .strip_prefix(':')
                        .map(|after| (&rest[..=at], after));
                }
                _ => {}
            }
        }
        return None;
    }
    rest.split_once(": ")
}

/// The lines in the last [`TAIL_BYTES`] of `log` without the backtrace events; empty when it cannot be read. What is
/// ours is picked out in [`record`].
fn tail(log: &Path) -> Vec<String> {
    let read = || -> std::io::Result<Vec<String>> {
        let mut file = Files::open_read(log)?;
        let length = file.metadata()?.len();
        file.seek(SeekFrom::Start(length.saturating_sub(TAIL_BYTES)))?;
        let mut bytes = Vec::new();
        file.take(TAIL_BYTES).read_to_end(&mut bytes)?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let mut lines: Vec<&str> = text.lines().collect();
        if length > TAIL_BYTES && !lines.is_empty() {
            // The first line may be cut in the middle.
            lines.remove(0);
        }
        Ok(lines
            .into_iter()
            .filter(|line| !line.contains("crash_backtrace"))
            .map(str::to_owned)
            .collect())
    };
    read().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "2026-11-15T10:00:00.123456Z";

    #[test]
    fn frames_pair_each_symbol_with_its_place() {
        assert_eq!(
            frames("stack backtrace:\n   0: one\n             at f.rs:1:1\n  10: two\n"),
            ["one at f.rs:1:1", "two"]
        );
    }

    #[test]
    fn a_record_has_the_closed_field_list_and_hides_the_home() {
        let scratch = crate::test_support::temp_dir("crash");
        let log = scratch.path().join("connector.log");
        fs::write(
            &log,
            format!(
                "{T}  INFO sidevoice_connector: a\n{T} ERROR crash_backtrace: big\n{T}  WARN sidevoice_connector: /home/canary-user-7f3a/x\n\
                 {T}  WARN hyper_util::client: GET /?token=canary\n{T} DEBUG sidevoice_connector: d\nraw stderr ?token=canary\n\
                 \u{202e}{T}  WARN sidevoice_connector: \u{202e}rev\n"
            ),
        )
        .unwrap();
        let homes = ["/home/canary-user-7f3a".to_owned()];
        let process = Process {
            version: "1.0.0",
            os: "linux",
            arch: "amd64",
            log: &log,
            homes: &homes,
        };
        let panic = Panic {
            literal: None,
            file: "/home/canary-user-7f3a/a.rs",
            line: 4,
            backtrace: "   0: f\n             at /home/canary-user-7f3a/b.rs:1:1\n",
        };
        let value = record(&process, &panic, "2026-11-15T10:00:00.000Z", "id");
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "arch",
                "backtrace",
                "count",
                "exception",
                "first_at",
                "id",
                "last_at",
                "location",
                "log_tail",
                "os",
                "piece",
                "version"
            ]
        );
        assert_eq!(value["exception"]["message"], FORMATTED_MARKER);
        assert_eq!(
            value["log_tail"],
            json!([
                format!("{T}  WARN sidevoice_connector: ~/x"),
                format!("{T}  WARN sidevoice_connector: rev")
            ])
        );
        let all = value.to_string();
        assert!(!all.contains("canary"), "{all}");
        assert!(!all.contains('\u{202e}'));
    }

    #[test]
    fn the_spool_never_holds_more_than_its_limit() {
        let scratch = crate::test_support::temp_dir("crash-spool");
        let log = scratch.path().join("connector.log");
        let homes: [String; 0] = [];
        let process = Process {
            version: "1.0.0",
            os: "linux",
            arch: "amd64",
            log: &log,
            homes: &homes,
        };
        let panic = Panic {
            literal: Some("boom"),
            file: "x.rs",
            line: 1,
            backtrace: "",
        };
        for _ in 0..=MAX_WAITING {
            leave(&process, &panic, "2026-11-15T10:00:00.000Z");
        }
        assert_eq!(waiting(&scratch.path().join(INBOX)), MAX_WAITING);
    }

    #[test]
    fn a_record_is_within_the_caps_the_core_applies_and_left_whole_with_one_name() {
        let scratch = crate::test_support::temp_dir("crash-caps");
        let log = scratch.path().join("connector.log");
        let mut text = String::new();
        for n in 0..100 {
            text.push_str(&format!(
                "{T} ERROR sidevoice_connector: {n} {}\n",
                "x".repeat(900)
            ));
        }
        fs::write(&log, text).unwrap();
        let homes: [String; 0] = [];
        let process = Process {
            version: "1.0.0",
            os: "linux",
            arch: "amd64",
            log: &log,
            homes: &homes,
        };
        let backtrace: String = (0..200)
            .map(|n| format!("  {n}: frame{n}\n             at src/a.rs:{n}:1\n"))
            .collect();
        let panic = Panic {
            literal: Some("caf\u{e9}"),
            file: "/abs/x.rs",
            line: 1,
            backtrace: &backtrace,
        };
        leave(&process, &panic, "2026-11-15T10:00:00.000Z");
        let inbox = scratch.path().join(INBOX);
        let names: Vec<_> = fs::read_dir(&inbox)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(names[0].starts_with("crash-"), "no staging name is left");
        let bytes = fs::read(inbox.join(&names[0])).unwrap();
        assert!(bytes.len() <= MAX_RECORD_BYTES);
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["log_tail"].as_array().unwrap().len(), TAIL_LINES);
        assert!(value["log_tail"].as_array().unwrap().iter().all(|line| line
            .as_str()
            .unwrap()
            .chars()
            .count()
            <= MAX_LINE));
        assert_eq!(value["backtrace"].as_array().unwrap().len(), MAX_FRAMES);
        assert_eq!(value["exception"]["message"], FORMATTED_MARKER);
        assert_eq!(value["location"]["file"], "unknown");
    }

    #[test]
    fn format_characters_are_stripped_before_the_home_is_masked() {
        let homes = ["/data/alice".to_owned()];
        assert_eq!(mask_homes("at /data/al\u{200b}ice/x", &homes), "at ~/x");
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

    #[test]
    fn a_warning_inside_a_span_is_kept_and_a_dependency_inside_a_span_is_dropped() {
        assert!(is_ours(&format!(
            "{T}  WARN binding{{id=\"a: b}}\" n=3}}: sidevoice_connector::room: kept"
        )));
        assert!(is_ours(&format!(
            "{T}  WARN outer{{a=1}}: inner: sidevoice_platform::x: kept"
        )));
        assert!(!is_ours(&format!(
            "{T}  WARN binding{{id=1}}: hyper_util::client: dropped"
        )));
        assert!(!is_ours(&format!(
            "{T}  WARN binding{{id=1}}: sidevoice_connectorx: dropped"
        )));
        assert!(!is_ours(&format!(
            "{T}  WARN binding{{id=1}} sidevoice_connector::x: dropped"
        )));
    }
}
