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
/// The most lines of the log a record carries.
const TAIL_LINES: usize = 50;
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
    if let Ok(bytes) = serde_json::to_vec(&record) {
        let _ = Files::create_private(&inbox.join(format!("crash-{id}.json")), &bytes);
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

fn record(process: &Process<'_>, panic: &Panic<'_>, now: &str, id: &str) -> Value {
    let hide = |text: &str| {
        process
            .homes
            .iter()
            .filter(|home| home.len() > 1)
            .fold(text.to_owned(), |text, home| {
                text.replace(home.as_str(), "~")
            })
    };
    json!({
        "id": id,
        "piece": "connector",
        "version": process.version,
        "os": process.os,
        "arch": process.arch,
        "first_at": now,
        "last_at": now,
        "count": 1,
        "exception": {"type": "panic", "message": hide(panic.literal.unwrap_or(FORMATTED_MARKER))},
        "location": {"file": hide(panic.file), "line": panic.line},
        "backtrace": frames(&hide(panic.backtrace)),
        "log_tail": tail(process.log).iter().map(|line| hide(line)).collect::<Vec<_>>(),
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

/// The last lines of `log` without the backtrace events; empty when it cannot be read.
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
        let lines: Vec<String> = lines
            .into_iter()
            .filter(|line| !line.contains("crash_backtrace"))
            .map(str::to_owned)
            .collect();
        let skip = lines.len().saturating_sub(TAIL_LINES);
        Ok(lines.into_iter().skip(skip).collect())
    };
    read().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "INFO a\nERROR crash_backtrace: big\nWARN /home/canary-user-7f3a/x\n",
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
        assert_eq!(value["log_tail"], json!(["INFO a", "WARN ~/x"]));
        assert!(!value.to_string().contains("canary-user"));
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
}
