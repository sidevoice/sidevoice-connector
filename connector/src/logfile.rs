//! The connector's log, `connector.log` in the data directory: one timestamped line per event, kept under 5 MB with
//! the two previous ones beside it (`.1`, `.2`).
//!
//! Rotated by copy-and-truncate, never by rename: a service manager holds the file open (`O_APPEND`) for as long as
//! the job runs, and a renamed file would keep receiving its output under the old name. Truncating in place moves
//! the next write to the start of the same file.
//!
//! A connector run by hand writes each line to stderr and to the file. Run by a service manager it writes the file
//! only: the manager already puts its stderr in that same file, so writing both would say everything twice. Before
//! [`init`] (the MCP server, the CLI) lines go to stderr only.

use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub const LOG_MAX: u64 = 5 * 1024 * 1024;
pub const LOG_KEPT: u32 = 2;

struct Sink {
    file: PathBuf,
    service: bool,
}

static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

/// From now on this process logs to `file` (and to stderr unless it runs as a service).
pub fn init(file: PathBuf, service: bool) {
    let _ = SINK.set(Mutex::new(Sink { file, service }));
}

/// Rotates `file` if it has grown past `max`: `.1` → `.2`, a copy of the file → `.1`, the file truncated in place.
/// Returns whether it rotated. Cheap enough to call before every write.
pub fn rotate(file: &Path, max: u64) -> bool {
    let Ok(metadata) = fs::metadata(file) else {
        return false;
    };
    if metadata.len() <= max {
        return false;
    }
    let numbered = |index: u32| {
        let mut name = file.as_os_str().to_owned();
        name.push(format!(".{index}"));
        PathBuf::from(name)
    };
    for index in (1..LOG_KEPT).rev() {
        let _ = fs::rename(numbered(index), numbered(index + 1));
    }
    let copied = (|| -> std::io::Result<()> {
        let mut source = fs::File::open(file)?;
        let mut target = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(numbered(1))?;
        std::io::copy(&mut source, &mut target)?;
        OpenOptions::new().write(true).open(file)?.set_len(0)
    })();
    copied.is_ok()
}

/// One line appended to a rotated log, created 0600.
pub fn append_line(file: &Path, line: &str) {
    rotate(file, LOG_MAX);
    let opened = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(file);
    if let Ok(mut opened) = opened {
        let mut text = line.to_owned();
        if !text.ends_with('\n') {
            text.push('\n');
        }
        let _ = opened.write_all(text.as_bytes());
    }
}

/// A Cursor editor conversation's id is what lets a chat speak as it: the log names it by a hash.
pub fn redact(line: &str) -> String {
    const PREFIX: &str = "cursor-editor-";
    const ID: usize = 36;
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find(PREFIX) {
        out.push_str(&rest[..at + PREFIX.len()]);
        let tail = &rest[at + PREFIX.len()..];
        let candidate = tail.get(..ID).filter(|id| {
            id.bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase() || byte == b'-')
        });
        match candidate {
            Some(id) => {
                let digest = hex::encode(Sha256::digest(format!("{PREFIX}{id}").as_bytes()));
                out.push_str("h:");
                out.push_str(&digest[..10]);
                rest = &tail[ID..];
            }
            None => rest = tail,
        }
    }
    out.push_str(rest);
    out
}

/// One event: timestamped and redacted, to the log file and/or stderr as [`init`] decided.
pub fn log(line: &str) {
    let stamped = format!("{} [sidevoice] {}", timestamp(), redact(line));
    match SINK.get().and_then(|sink| sink.lock().ok()) {
        Some(sink) => {
            if !sink.service {
                eprintln!("{stamped}");
            }
            append_line(&sink.file, &stamped);
        }
        None => eprintln!("{stamped}"),
    }
}

/// Now, in UTC, as RFC 3339 with milliseconds (`2026-10-06T17:35:00.123Z`).
pub fn timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format_utc(now.as_secs() as i64, now.subsec_millis())
}

fn format_utc(seconds: i64, millis: u32) -> String {
    let days = seconds.div_euclid(86400);
    let clock = seconds.rem_euclid(86400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        clock / 3600,
        clock % 3600 / 60,
        clock % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure_fs::tests::Scratch;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn a_log_past_its_limit_is_copied_aside_and_truncated_in_place() {
        let scratch = Scratch::new("log");
        let file = scratch.0.join("connector.log");
        append_line(&file, "first");
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
        // A writer holding the file open, as a service manager does.
        let mut held = OpenOptions::new().append(true).open(&file).unwrap();
        let inode = fs::metadata(&file).unwrap().ino();
        assert!(!rotate(&file, 1024), "under the limit");
        fs::write(&file, vec![b'a'; 64]).unwrap();
        assert!(rotate(&file, 32));
        assert_eq!(
            fs::read(scratch.0.join("connector.log.1")).unwrap().len(),
            64
        );
        assert_eq!(fs::metadata(&file).unwrap().len(), 0);
        assert_eq!(fs::metadata(&file).unwrap().ino(), inode, "never renamed");
        held.write_all(b"after\n").unwrap();
        assert_eq!(
            fs::read(&file).unwrap(),
            b"after\n",
            "the held writer continues in the same file"
        );

        fs::write(&file, vec![b'b'; 64]).unwrap();
        assert!(rotate(&file, 32));
        fs::write(&file, vec![b'c'; 64]).unwrap();
        assert!(rotate(&file, 32));
        assert_eq!(
            fs::read(scratch.0.join("connector.log.1")).unwrap()[0],
            b'c'
        );
        assert_eq!(
            fs::read(scratch.0.join("connector.log.2")).unwrap()[0],
            b'b'
        );
        assert!(
            !scratch.0.join("connector.log.3").exists(),
            "two previous logs are kept"
        );
    }

    #[test]
    fn editor_conversation_ids_are_logged_by_hash() {
        let id = "cursor-editor-0123abcd-0123-4567-89ab-0123456789ab";
        let line = redact(&format!("joined {id} and cursor-editor-short"));
        assert!(!line.contains("0123abcd"), "{line}");
        assert!(line.starts_with("joined cursor-editor-h:"), "{line}");
        assert!(line.ends_with(" and cursor-editor-short"), "{line}");
        assert_eq!(
            line,
            redact(&format!("joined {id} and cursor-editor-short"))
        );
    }

    #[test]
    fn timestamps_are_utc_rfc3339() {
        assert_eq!(format_utc(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_utc(951_782_400, 5), "2000-02-29T00:00:00.005Z");
        assert_eq!(format_utc(1_791_307_845, 123), "2026-10-06T17:30:45.123Z");
        assert_eq!(timestamp().len(), 24);
    }
}
