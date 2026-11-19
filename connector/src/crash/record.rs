//! The record's closed field list, its signature and the validation every record passes before it is kept.

use std::str::FromStr;

use chrono::{DateTime, Datelike, Utc};
use sentry::types::DebugId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{MAX_FRAMES, MAX_IMAGES, OWN_TARGETS, TAIL_LINES};

/// What stands in for a panic message that is not a literal (`docs/logging.md`, section 9).
pub const FORMATTED_MARKER: &str = "<formatted panic message omitted>";
const MAX_TEXT: usize = 200;
const MAX_FUNCTION: usize = 240;
const MAX_NAME: usize = 100;
const MAX_LINE: usize = 600;
/// How many of the first frames the signature takes.
const SIGNATURE_FRAMES: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exception {
    /// `panic`, or a stable error code.
    #[serde(rename = "type")]
    pub kind: String,
    /// A `&'static str` payload, or [`FORMATTED_MARKER`].
    pub message: String,
    /// With the marker: how long the formatted message was (its text is never kept).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub len: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub file: String,
    pub line: u32,
}

/// One frame of the stack, innermost first. The address is what Sentry symbolicates with the debug files of the release;
/// the name and place are what the binary itself could say (its symbol table, `strip = "debuginfo"`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    /// A bare file name, never a path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub addr: u64,
}

/// A loaded module that holds a frame of the stack, so that Sentry can match the frame to the debug file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    /// A bare file name, never a path.
    pub name: String,
    pub addr: u64,
    pub size: u64,
    pub vmaddr: u64,
    /// The debug id (`debugid`'s canonical text).
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_id: Option<String>,
}

/// The values of `os.name` the connector writes (`logfile::Resource`): any other is not ours and the record is not taken.
const OS_NAMES: [&str; 3] = ["macOS", "Windows", "Linux"];

/// One crash, or the several of one signature (`count`, `first_at`, `last_at`). Exactly these fields are stored and sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrashRecord {
    /// A random UUID v4, made when the crash happened: the Sentry `event_id`, so a send repeated after a failed delete is
    /// one event.
    pub id: String,
    pub version: String,
    pub os: String,
    /// OpenTelemetry's `os.name`, taken when the panic happened like `os` and `arch`.
    pub os_name: String,
    pub arch: String,
    /// RFC 3339, UTC, milliseconds.
    pub first_at: String,
    pub last_at: String,
    pub count: u32,
    /// Groups the crashes that are the same: a hash of the version, location and top frames.
    pub signature: String,
    pub exception: Exception,
    pub location: Location,
    pub backtrace: Vec<Frame>,
    pub images: Vec<Image>,
    /// The last warnings and errors of our own targets, masked where they were taken. Each line starts with its time as an
    /// offset from the crash (`-12.345s`, not a date) and names the ids it carries by tokens of this record (`d1`, `c2`),
    /// so that nothing in it correlates with another report or says when the machine was in use.
    pub log_tail: Vec<String>,
}

/// What a panic is, as the capture knows it.
pub struct Panic<'a> {
    pub version: &'a str,
    pub os: &'a str,
    pub os_name: &'a str,
    pub arch: &'a str,
    pub at: DateTime<Utc>,
    /// The payload when it is a `&'static str`; `None` for anything formatted.
    pub literal: Option<&'a str>,
    /// How long a formatted payload was; 0 when there was none.
    pub formatted_len: usize,
    pub file: &'a str,
    pub line: u32,
    pub frames: Vec<Frame>,
    pub images: Vec<Image>,
    /// Already rendered, with offsets from `at` (`tail::snapshot`).
    pub tail: Vec<String>,
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

impl CrashRecord {
    pub fn from_panic(panic: Panic<'_>, homes: &[String]) -> Self {
        let message = panic.literal.unwrap_or(FORMATTED_MARKER);
        let at = stamp(panic.at);
        let hide = |text: &str| mask_homes(text, homes);
        let mut record = Self {
            id: Uuid::new_v4().to_string(),
            version: panic.version.to_owned(),
            os: panic.os.to_owned(),
            os_name: panic.os_name.to_owned(),
            arch: panic.arch.to_owned(),
            first_at: at.clone(),
            last_at: at,
            count: 1,
            signature: String::new(),
            exception: Exception {
                kind: "panic".to_owned(),
                message: hide(message),
                len: u32::try_from(panic.formatted_len)
                    .ok()
                    .filter(|len| *len > 0 && panic.literal.is_none()),
            },
            location: Location {
                file: hide(panic.file),
                line: panic.line,
            },
            backtrace: panic.frames,
            images: panic.images,
            log_tail: panic.tail.iter().map(|line| hide(line)).collect(),
        };
        record.normalize();
        record
    }

    /// Reads a record from untrusted JSON (a file on disk may be corrupt): only the closed field list is taken, every text is
    /// bounded and cleaned, and the signature is made again rather than believed. The home directory is hidden again and
    /// the times are made UTC (milliseconds) and ordered. `None` when a required field is missing or malformed.
    pub fn from_untrusted(value: &Value, homes: &[String]) -> Option<Self> {
        let text = |value: &Value| value.as_str().map(str::to_owned);
        let list = |value: &Value| -> Vec<Value> { value.as_array().cloned().unwrap_or_default() };
        let mut record = Self {
            id: text(&value["id"])?,
            version: text(&value["version"])?,
            os: text(&value["os"])?,
            os_name: text(&value["os_name"])?,
            arch: text(&value["arch"])?,
            first_at: text(&value["first_at"])?,
            last_at: text(&value["last_at"])?,
            count: value["count"]
                .as_u64()
                .map_or(1, |count| count.clamp(1, u64::from(u32::MAX)) as u32),
            signature: String::new(),
            exception: Exception {
                kind: text(&value["exception"]["type"])?,
                message: text(&value["exception"]["message"])?,
                len: value["exception"]["len"]
                    .as_u64()
                    .and_then(|len| u32::try_from(len).ok()),
            },
            location: Location {
                file: text(&value["location"]["file"])?,
                line: u32::try_from(value["location"]["line"].as_u64()?).ok()?,
            },
            backtrace: list(&value["backtrace"])
                .into_iter()
                .filter_map(|frame| serde_json::from_value(frame).ok())
                .collect(),
            images: list(&value["images"])
                .into_iter()
                .filter_map(|image| serde_json::from_value(image).ok())
                .collect(),
            log_tail: list(&value["log_tail"])
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
        };
        let times = |text: &str| {
            DateTime::parse_from_rfc3339(text)
                .ok()
                .map(|at| at.with_timezone(&Utc))
        };
        let (first, last) = (times(&record.first_at)?, times(&record.last_at)?);
        // A year past 9999 would be written with a sign and a fifth digit.
        if first.max(last).year() > 9999 {
            return None;
        }
        record.first_at = stamp(first.min(last));
        record.last_at = stamp(first.max(last));
        record.exception.message = mask_homes(&record.exception.message, homes);
        record.location.file = mask_homes(&record.location.file, homes);
        for text in &mut record.log_tail {
            *text = mask_homes(text, homes);
        }
        if Uuid::parse_str(&record.id).ok()?.to_string() != record.id
            || !token(&record.version, 64, |c| {
                c.is_ascii_alphanumeric() || "._+-".contains(c)
            })
            || !token(&record.os, 32, |c| {
                c.is_ascii_alphanumeric() || "_-".contains(c)
            })
            || !OS_NAMES.contains(&record.os_name.as_str())
            || !token(&record.arch, 32, |c| {
                c.is_ascii_alphanumeric() || "_-".contains(c)
            })
            || !token(&record.exception.kind, 64, |c| {
                c.is_ascii_alphanumeric() || "._-".contains(c)
            })
        {
            return None;
        }
        record.normalize();
        Some(record)
    }

    /// Bounds every text, keeps only what has the shape of what we write, and makes the signature: the message is
    /// printable ASCII or the marker, the file a short relative path, a frame a symbol and a bare file name, an image a
    /// bare name and a debug id, a log line one of our own warnings or errors.
    fn normalize(&mut self) {
        let message = clean(&self.exception.message, MAX_TEXT);
        self.exception.message = if message.bytes().all(|b| (0x20..0x7f).contains(&b)) {
            message
        } else {
            FORMATTED_MARKER.to_owned()
        };
        if self.exception.message != FORMATTED_MARKER {
            self.exception.len = None;
        }
        self.location.file = relative_path(&self.location.file);
        self.backtrace = self
            .backtrace
            .iter()
            .filter(|frame| frame.addr != 0)
            .take(MAX_FRAMES)
            .map(frame)
            .collect();
        self.images = self
            .images
            .iter()
            .filter_map(image)
            .take(MAX_IMAGES)
            .collect();
        let lines: Vec<String> = self
            .log_tail
            .iter()
            .map(|line| clean(line, MAX_LINE))
            .filter(|line| is_ours(line))
            .collect();
        let skip = lines.len().saturating_sub(TAIL_LINES);
        self.log_tail = lines[skip..].to_vec();
        self.signature = self.make_signature();
    }

    /// Not the addresses: they change with every run.
    fn make_signature(&self) -> String {
        let mut hash = Sha256::new();
        for part in [
            &self.version,
            &self.location.file,
            &self.location.line.to_string(),
        ] {
            hash.update(part.as_bytes());
            hash.update([0]);
        }
        for frame in self.backtrace.iter().take(SIGNATURE_FRAMES) {
            for part in [
                frame.function.clone().unwrap_or_default(),
                frame.file.clone().unwrap_or_default(),
                frame.line.map(|line| line.to_string()).unwrap_or_default(),
            ] {
                hash.update(part.as_bytes());
                hash.update([0]);
            }
        }
        hash.finalize()[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

fn token(value: &str, max: usize, allowed: impl Fn(char) -> bool) -> bool {
    !value.is_empty() && value.len() <= max && value.chars().all(allowed)
}

/// The last component of a path, when it has the shape of a file name: so that no directory (a home, an install place) is
/// ever kept.
pub(super) fn bare(path: &str) -> Option<String> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    token(name, MAX_NAME, |c| {
        c.is_ascii_alphanumeric() || "_.-+".contains(c)
    })
    .then(|| name.to_owned())
}

/// A frame with its name bounded and shaped like a symbol, its file a bare name: what is not is dropped, the address stays.
fn frame(frame: &Frame) -> Frame {
    Frame {
        function: frame
            .function
            .as_deref()
            .map(|function| clean(function, MAX_FUNCTION))
            .filter(|function| {
                !function.is_empty()
                    && function
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "_:<>{}$.,&*()[]'+-@# ".contains(c))
            }),
        file: frame.file.as_deref().and_then(bare),
        line: frame.line,
        addr: frame.addr,
    }
}

/// An image with a bare name and a debug id `debugid` reads; `None` when it has neither.
fn image(image: &Image) -> Option<Image> {
    Some(Image {
        name: bare(&image.name)?,
        addr: image.addr,
        size: image.size,
        vmaddr: image.vmaddr,
        id: DebugId::from_str(&image.id).ok()?.to_string(),
        code_id: image
            .code_id
            .clone()
            .filter(|id| token(id, 128, |c| c.is_ascii_hexdigit())),
    })
}

/// A source path as a report may carry it: short, relative, of plain characters; anything else (an absolute path, a home, a
/// drive) is `unknown`.
pub(super) fn relative_path(path: &str) -> String {
    let file = clean(path, MAX_TEXT).replace('\\', "/");
    let relative = !file.starts_with('/')
        && file.split('/').all(|part| part != ".." && !part.is_empty())
        && file
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./-".contains(c));
    if relative {
        file
    } else {
        "unknown".to_owned()
    }
}

/// `text` with control characters as spaces and invisible format characters (bidi controls among them) removed, cut to
/// at most `max` characters.
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
pub(super) fn mask_homes(text: &str, homes: &[String]) -> String {
    let fold = cfg!(any(windows, target_os = "macos"));
    let text = homes
        .iter()
        .filter(|home| home.len() > 1)
        .fold(strip_format(text), |text, home| {
            mask_home(&text, &strip_format(home), fold)
        });
    mask_users(&text)
}

pub(super) fn mask_home(text: &str, home: &str, fold: bool) -> String {
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

/// Whether `time` is an offset as `tail` writes it: a sign, seconds, three digits of milliseconds and `s`.
fn is_offset(time: &str) -> bool {
    let Some(rest) = time
        .strip_prefix(['-', '+'])
        .and_then(|rest| rest.strip_suffix('s'))
    else {
        return false;
    };
    rest.split_once('.').is_some_and(|(seconds, millis)| {
        (1..=12).contains(&seconds.len())
            && seconds.bytes().all(|b| b.is_ascii_digit())
            && millis.len() == 3
            && millis.bytes().all(|b| b.is_ascii_digit())
    })
}

/// Whether `line` is a line of our own log at `warn` or `error`: `<offset> <LEVEL> [<span>: ]<target>: ...` (the offset
/// from the crash that `tail` writes), the target one of ours. A span the event was logged in (`name{fields}:` or `name:`,
/// as many as there are) comes before the target. A dependency's line, a debug line and the panic's own are none of these.
pub(super) fn is_ours(line: &str) -> bool {
    let mut parts = line.split_whitespace();
    let (Some(time), Some(level)) = (parts.next(), parts.next()) else {
        return false;
    };
    if !is_offset(time) || !matches!(level, "WARN" | "ERROR") {
        return false;
    }
    let rest = line.trim_start()[time.len()..].trim_start();
    let mut rest = rest[level.len()..].trim_start();
    loop {
        let Some((head, after)) = split_head(rest) else {
            return false;
        };
        if OWN_TARGETS.iter().any(|name| {
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
