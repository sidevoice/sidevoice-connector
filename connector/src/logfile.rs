//! The connector's log, `connector.log` in the data directory: one `tracing` line per event, kept under 5 MB with
//! the two previous ones beside it (`.1`, `.2`). The rules for what a line may hold are the shared logging guideline's
//! (https://github.com/sidevoice/.github-private/blob/main/docs/logging.md); [`crate::log_safe`] has the types.
//!
//! Rotated by copy-and-truncate, never by rename: a service manager holds the file open (`O_APPEND`) for as long as
//! the job runs, and a renamed file would keep receiving its output under the old name. Truncating in place moves
//! the next write to the start of the same file.
//!
//! [`install`] sets the process's one subscriber, whose writer is [`LogWriter`]. A connector run by hand writes each
//! line to the file and, on a terminal, to stderr. Run by a service manager it writes the file only: the manager
//! already puts its stderr in that same file, so writing both would say everything twice. Before [`init`] (the MCP
//! server, the CLI) lines go to stderr only; those processes default to `warn`, so a new `info` event never mixes into
//! what a person reads. `SIDEVOICE_LOG` is the only variable that sets the level.

use crate::error::ConnectorError;
use crate::log_safe::IdHash;
use crate::platform::fs::{Create, Files, Fs};
use std::cell::Cell;
use std::fs::{self, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

pub const LOG_MAX: u64 = 5 * 1024 * 1024;
pub const LOG_KEPT: u32 = 2;

/// The targets a bare level in `SIDEVOICE_LOG` applies to; every other target stays at `warn` unless named. Native
/// tracing from dependencies (tokio, hyper, rustls) must not reach a file because someone asked for `debug`.
const OUR_TARGETS: &[&str] = &[
    "sidevoice_connector",
    "sidevoice_platform",
    PANIC_TARGET,
    BACKTRACE_TARGET,
];

/// The panic record: location, thread, version and, for a literal message, the message (guideline section 9).
const PANIC_TARGET: &str = "panic";
/// The backtrace of a panic, a second event for the app file only: never stderr, never a system log. The two targets
/// share no prefix, so one filter directive never reaches the other.
const BACKTRACE_TARGET: &str = "crash_backtrace";
/// What a panic's message is replaced by when the payload is not a literal: a formatted message carries values.
const FORMATTED_MARKER: &str = "<formatted panic message omitted>";
const NON_TEXT_MARKER: &str = "<non-text panic payload omitted>";

struct Sink {
    file: PathBuf,
    service: bool,
    /// The home as every text spelling of it a line may hold, longest first, each shown as `~`.
    home: Vec<String>,
}

static SINK: OnceLock<Sink> = OnceLock::new();
/// The home as the environment names it, from [`install`]: what hides it before [`init`] has the resolved one.
static EARLY_HOME: OnceLock<Vec<String>> = OnceLock::new();
/// Serialises rotate + append, so concurrent lines never race the copy-and-truncate.
static WRITE: Mutex<()> = Mutex::new(());

thread_local! {
    /// Set while the panic hook logs: a panic raised while this thread holds [`WRITE`] must not wait for it.
    static IN_PANIC_HOOK: Cell<bool> = const { Cell::new(false) };
}

/// From now on this process logs to `file` (and to stderr, on a terminal, unless it runs as a service). `home` is the
/// one this process resolved: it is shown as `~` in every line.
pub fn init(file: PathBuf, service: bool, home: &Path) {
    let _ = SINK.set(Sink {
        file,
        service,
        home: spellings(home),
    });
}

/// Every text spelling of `home` a line may hold, longest first: as is, with forward slashes (Windows), escaped as
/// `Debug` prints it, and as a harness's project directories name it (every separator and dot as `-`).
fn spellings(home: &Path) -> Vec<String> {
    let plain = home.to_string_lossy().into_owned();
    let mut spellings = vec![
        plain.replace('\\', "/"),
        plain.replace('\\', "\\\\"),
        plain.clone(),
        plain.replace(|c: char| !c.is_ascii_alphanumeric(), "-"),
    ];
    spellings.retain(|text| text.len() > 1);
    spellings.sort_by_key(|text| std::cmp::Reverse(text.len()));
    spellings.dedup();
    spellings
}

/// `SIDEVOICE_LOG`'s filter: `spec` (else `default`) with a bare level confined to [`OUR_TARGETS`]. The flag says
/// `spec` was not valid and `default` is used instead.
fn filter(spec: Option<&str>, default: &str) -> (EnvFilter, bool) {
    let parsed = spec.map(|spec| EnvFilter::try_new(scoped(spec, default)));
    match parsed {
        Some(Ok(filter)) => (filter, false),
        other => (
            EnvFilter::new(scoped(default, default)),
            matches!(other, Some(Err(_))),
        ),
    }
}

/// A target-less `info`, `debug` or `trace` becomes `warn` plus that level for our targets; directives that name a
/// target are kept as written. A spec with no bare level at all (only target directives) keeps `default`'s rule for
/// everything else, so the panic hook and every warning stay on.
fn scoped(spec: &str, default: &str) -> String {
    let is_level = |directive: &str| {
        matches!(
            directive.trim().to_ascii_lowercase().as_str(),
            "trace" | "debug" | "info" | "warn" | "error" | "off"
        )
    };
    let base = if spec.split(',').any(is_level) || spec == default {
        String::new()
    } else {
        format!("{},", scoped(default, default))
    };
    base + &spec
        .split(',')
        .map(str::trim)
        .map(|directive| match directive.to_ascii_lowercase().as_str() {
            level @ ("info" | "debug" | "trace") => std::iter::once("warn".to_owned())
                .chain(OUR_TARGETS.iter().map(|target| format!("{target}={level}")))
                .collect::<Vec<_>>()
                .join(","),
            _ => directive.to_owned(),
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Installs the process's subscriber and panic hook; `default` is the level when `SIDEVOICE_LOG` is not set (`info` for
/// the daemon, `warn` for anything whose stderr a person or a protocol reads). Call once, first thing.
pub fn install(default: &str) {
    let _ = EARLY_HOME.set(
        dirs::home_dir()
            .map(|home| spellings(&home))
            .unwrap_or_default(),
    );
    let (filter, invalid) = filter(std::env::var("SIDEVOICE_LOG").ok().as_deref(), default);
    let layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(true)
        .log_internal_errors(false)
        .with_writer(LogWriter);
    if tracing_subscriber::registry()
        .with(filter)
        .with(layer)
        .try_init()
        .is_err()
    {
        return;
    }
    // The default hook is never chained: it would write the panic a second time, raw, to a stderr that is this file when
    // a service runs and a terminal or capture otherwise. The two events of `record_panic` are the one record.
    std::panic::set_hook(Box::new(|info| {
        IN_PANIC_HOOK.with(|flag| flag.set(true));
        record_panic(
            info.payload(),
            info.location().map(|at| (at.file(), at.line())),
            std::thread::current().name(),
        );
        IN_PANIC_HOOK.with(|flag| flag.set(false));
    }));
    if invalid {
        tracing::warn!(
            error.type = ConnectorError::InvalidLogFilter.code(),
            "SIDEVOICE_LOG is not a valid filter; the default filter is used instead"
        );
    }
}

/// The panic's two events. First `panic`: where, on which thread, which version, and the message only when the payload
/// is a literal (`&'static str`); a formatted `String` is replaced by a marker and its length, never its text, a prefix
/// or a hash, because `unwrap()` and `assert_eq!` put values in it. Then `crash_backtrace`: the frames, in one line,
/// for the app file only (see [`Line`]). Frames carry symbols and lines, no values.
fn record_panic(
    payload: &(dyn std::any::Any + Send),
    location: Option<(&str, u32)>,
    thread: Option<&str>,
) {
    let (message, len) = match (
        payload.downcast_ref::<&'static str>(),
        payload.downcast_ref::<String>(),
    ) {
        (Some(literal), _) => (*literal, None),
        (None, Some(formatted)) => (FORMATTED_MARKER, Some(formatted.len())),
        (None, None) => (NON_TEXT_MARKER, None),
    };
    let (file, line) = location.unzip();
    tracing::event!(
        target: "panic",
        tracing::Level::ERROR,
        exception.type = "panic",
        exception.message = message,
        len,
        code.file.path = file,
        code.line.number = line,
        thread.name = thread.unwrap_or("unnamed"),
        service.version = crate::identity::VERSION,
        "the program panicked; the work of that thread was abandoned"
    );
    // No file sink (the CLI, the MCP server): the backtrace has nowhere to go, so it is not even captured.
    if SINK.get().is_none() {
        return;
    }
    let backtrace = std::backtrace::Backtrace::force_capture();
    tracing::event!(
        target: "crash_backtrace",
        tracing::Level::ERROR,
        exception.type = "panic",
        exception.stacktrace = ?backtrace,
        "the backtrace of the panic recorded just before"
    );
}

/// What this process is, as OpenTelemetry's resource conventions name it: a closed list set here, logged once in the
/// start event and not repeated on each line. No detectors: they add host names, command lines and owners.
pub struct Resource {
    pub service_name: &'static str,
    pub service_version: &'static str,
    pub os_type: &'static str,
    pub os_name: &'static str,
    pub host_arch: &'static str,
}

impl Resource {
    pub fn current() -> Resource {
        Resource::of(std::env::consts::OS, std::env::consts::ARCH)
    }

    /// `os` and `arch` as Rust names them, in OpenTelemetry's values; one Rust does not name here travels as is.
    fn of(os: &'static str, arch: &'static str) -> Resource {
        Resource {
            service_name: "sidevoice-connector",
            service_version: crate::identity::VERSION,
            os_type: match os {
                "macos" => "darwin",
                other => other,
            },
            os_name: match os {
                "macos" => "macOS",
                "linux" => "Linux",
                "windows" => "Windows",
                other => other,
            },
            host_arch: match arch {
                "x86_64" => "amd64",
                "aarch64" => "arm64",
                other => other,
            },
        }
    }
}

/// The subscriber's writer: each event is one buffer, redacted and written whole when it is dropped.
#[derive(Clone, Copy)]
pub struct LogWriter;

pub struct Line {
    bytes: Vec<u8>,
    /// A panic's backtrace: the app file only, never stderr (guideline sections 8 and 9).
    file_only: bool,
}

impl Write for Line {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        let mut text = redact(&String::from_utf8_lossy(&self.bytes));
        let sink = SINK.get();
        let home = sink.map(|sink| &sink.home).or_else(|| EARLY_HOME.get());
        for spelling in home.into_iter().flatten() {
            text = text.replace(spelling.as_str(), "~");
        }
        let Some(sink) = sink else {
            if !self.file_only {
                let _ = std::io::stderr().write_all(text.as_bytes());
            }
            return;
        };
        // A panic always reaches stderr (a service's stderr is this file, so not then); other lines only on a terminal.
        if !self.file_only
            && !sink.service
            && (IN_PANIC_HOOK.with(Cell::get) || std::io::stderr().is_terminal())
        {
            let _ = std::io::stderr().write_all(text.as_bytes());
        }
        // Inside the panic hook the lock is only tried: the panicking thread may be the one holding it. Without it the
        // line is appended but never rotated, so a rotation is never run unlocked.
        if IN_PANIC_HOOK.with(Cell::get) {
            match WRITE.try_lock() {
                Ok(_held) => append_line(&sink.file, &text),
                Err(std::sync::TryLockError::Poisoned(held)) => {
                    let _held = held.into_inner();
                    append_line(&sink.file, &text);
                }
                Err(std::sync::TryLockError::WouldBlock) => append_unrotated(&sink.file, &text),
            }
        } else {
            let _held = WRITE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            append_line(&sink.file, &text);
        }
    }
}

impl<'a> MakeWriter<'a> for LogWriter {
    type Writer = Line;

    fn make_writer(&'a self) -> Line {
        Line {
            bytes: Vec::new(),
            file_only: false,
        }
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Line {
        Line {
            bytes: Vec::new(),
            file_only: is_backtrace_target(meta.target()),
        }
    }
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
        let mut target = Files::open_private(&numbered(1), Create::ReadWrite)?;
        target.set_len(0)?;
        std::io::copy(&mut source, &mut target)?;
        OpenOptions::new().write(true).open(file)?.set_len(0)
    })();
    copied.is_ok()
}

/// Whether `target` is the backtrace event's: the same prefix rule as the filter directive `crash_backtrace=…`.
fn is_backtrace_target(target: &str) -> bool {
    target == BACKTRACE_TARGET
        || target
            .strip_prefix(BACKTRACE_TARGET)
            .is_some_and(|rest| rest.starts_with("::"))
}

/// One line appended to a rotated log, created 0600.
pub fn append_line(file: &Path, line: &str) {
    rotate(file, LOG_MAX);
    append_unrotated(file, line);
}

/// One line appended as is, in a single write, created 0600. For a caller that does not hold [`WRITE`].
fn append_unrotated(file: &Path, line: &str) {
    let opened = Files::open_private(file, Create::Append);
    if let Ok(mut opened) = opened {
        let mut text = line.to_owned();
        if !text.ends_with('\n') {
            text.push('\n');
        }
        let _ = opened.write_all(text.as_bytes());
    }
}

/// Conversation-id prefixes whose ids let a chat speak as it, set once at start from the harnesses.
static PRIVATE_IDS: OnceLock<Vec<&'static str>> = OnceLock::new();

/// Which conversation ids the log names only by a hash: those after one of `prefixes` (a harness declares them).
pub fn private_ids(prefixes: Vec<&'static str>) {
    let _ = PRIVATE_IDS.set(prefixes);
}

/// A line with every private conversation id ([`private_ids`]) named by a hash.
pub fn redact(line: &str) -> String {
    PRIVATE_IDS
        .get()
        .into_iter()
        .flatten()
        .fold(line.to_owned(), |line, prefix| redact_after(&line, prefix))
}

fn redact_after(line: &str, prefix: &str) -> String {
    const ID: usize = 36;
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find(prefix) {
        out.push_str(&rest[..at + prefix.len()]);
        let tail = &rest[at + prefix.len()..];
        let candidate = tail.get(..ID).filter(|id| {
            id.bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase() || byte == b'-')
        });
        match candidate {
            Some(id) => {
                out.push_str(&IdHash(&format!("{prefix}{id}")).to_string());
                rest = &tail[ID..];
            }
            None => rest = tail,
        }
    }
    out.push_str(rest);
    out
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

    #[test]
    fn a_log_past_its_limit_is_copied_aside_and_truncated_in_place() {
        let scratch = crate::test_support::temp_dir("log");
        let file = scratch.path().join("connector.log");
        append_line(&file, "first");
        Files::check_private(&Files::open_read(&file).unwrap()).unwrap();
        // A writer holding the file open, as a service manager does.
        let mut held = OpenOptions::new().append(true).open(&file).unwrap();
        let identity = || Files::identity(&Files::open_read(&file).unwrap()).unwrap();
        let inode = identity();
        assert!(!rotate(&file, 1024), "under the limit");
        fs::write(&file, vec![b'a'; 64]).unwrap();
        assert!(rotate(&file, 32));
        assert_eq!(
            fs::read(scratch.path().join("connector.log.1"))
                .unwrap()
                .len(),
            64
        );
        assert_eq!(fs::metadata(&file).unwrap().len(), 0);
        assert!(identity() == inode, "never renamed");
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
            fs::read(scratch.path().join("connector.log.1")).unwrap()[0],
            b'c'
        );
        assert_eq!(
            fs::read(scratch.path().join("connector.log.2")).unwrap()[0],
            b'b'
        );
        assert!(
            !scratch.path().join("connector.log.3").exists(),
            "two previous logs are kept"
        );
    }

    #[test]
    fn private_conversation_ids_are_logged_by_hash() {
        let id = "chat-x-0123abcd-0123-4567-89ab-0123456789ab";
        assert_eq!(
            redact_after(id, "chat-x-"),
            format!("chat-x-{}", IdHash(id))
        );
        let line = redact_after(&format!("joined {id} and chat-x-short"), "chat-x-");
        assert!(!line.contains("0123abcd"), "{line}");
        assert!(line.starts_with("joined chat-x-h:"), "{line}");
        assert!(line.ends_with(" and chat-x-short"), "{line}");
        assert_eq!(
            line,
            redact_after(&format!("joined {id} and chat-x-short"), "chat-x-")
        );
    }

    #[test]
    fn a_bare_level_applies_to_our_targets_only() {
        assert_eq!(
            scoped("info", "info"),
            "warn,sidevoice_connector=info,sidevoice_platform=info,panic=info,crash_backtrace=info"
        );
        assert_eq!(
            scoped("debug,reqwest=debug,sidevoice_connector::adapters=trace", "info"),
            "warn,sidevoice_connector=debug,sidevoice_platform=debug,panic=debug,crash_backtrace=debug,reqwest=debug,sidevoice_connector::adapters=trace"
        );
        assert_eq!(scoped("warn", "info"), "warn");
        assert_eq!(
            scoped("reqwest=debug", "info"),
            "warn,sidevoice_connector=info,sidevoice_platform=info,panic=info,crash_backtrace=info,reqwest=debug"
        );
        let (_, invalid) = filter(Some("info,=[="), "info");
        assert!(invalid);
        let (_, invalid) = filter(Some("debug"), "info");
        assert!(!invalid);
    }

    #[test]
    fn the_home_is_shown_as_a_tilde_in_every_spelling() {
        let mut text = "/Users/ana/x and -Users-ana-proj and C:\\\\Users\\\\ana".to_owned();
        for spelling in spellings(Path::new("/Users/ana"))
            .iter()
            .chain(spellings(Path::new("C:\\Users\\ana")).iter())
        {
            text = text.replace(spelling.as_str(), "~");
        }
        assert_eq!(text, "~/x and ~-proj and ~");
    }

    #[test]
    fn the_resource_is_a_closed_list_in_opentelemetry_values() {
        let mac = Resource::of("macos", "aarch64");
        assert_eq!(
            (mac.os_type, mac.os_name, mac.host_arch),
            ("darwin", "macOS", "arm64")
        );
        let linux = Resource::of("linux", "x86_64");
        assert_eq!(
            (linux.os_type, linux.os_name, linux.host_arch),
            ("linux", "Linux", "amd64")
        );
        let windows = Resource::of("windows", "x86_64");
        assert_eq!((windows.os_type, windows.os_name), ("windows", "Windows"));
        assert_eq!(mac.service_name, "sidevoice-connector");
        assert!(!mac.service_version.is_empty());
    }

    #[test]
    fn the_backtrace_target_shares_no_prefix_with_another_of_ours() {
        for (index, one) in OUR_TARGETS.iter().enumerate() {
            for other in &OUR_TARGETS[index + 1..] {
                assert!(
                    !one.starts_with(other) && !other.starts_with(one),
                    "{one} {other}"
                );
            }
        }
    }

    #[test]
    fn timestamps_are_utc_rfc3339() {
        assert_eq!(format_utc(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_utc(951_782_400, 5), "2000-02-29T00:00:00.005Z");
        assert_eq!(format_utc(1_791_307_845, 123), "2026-10-06T17:30:45.123Z");
        assert_eq!(timestamp().len(), 24);
    }
}
