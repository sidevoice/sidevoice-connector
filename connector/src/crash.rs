//! Crash reports: what a panic of the connector daemon leaves on the machine, and its sending to Sentry at the next start.
//!
//! Decided with the operator (sidevoice/sidevoice-desktop#523): each piece reports **its own** panics, on by default and
//! never asked, and nothing relays through another piece. The connector sends its own and keeps no record for anyone
//! else to read.
//!
//! **One switch, one owner.** The machine's "send crash reports" setting is the core's (`D/core/crash-reports.json`;
//! `sidevoice-core crash-reports on|off|status`, and the core's local-only `/api/local/crash-reports`, which the desktop app
//! sets). The connector keeps no setting of its own: before it sends a record, and to decide whether to keep or discard
//! them, it asks the core over the local channel (`Profile::crash_reports`). A core that cannot be asked (not running, no
//! answer, one that has no such setting yet) is not "off": the records are kept, nothing is sent, and the next start asks
//! again. An answer "off" deletes the records and the connector's id, and none is made in this process afterwards.
//! `sidevoice-connector crash-reports on|off|status` is that same setting, forwarded to the core. What is the connector's
//! own is its random id (`settings`), made on the first send and replaced after a year.
//!
//! - A panic of the daemon is captured ([`capture`]) by the panic hook: the frames of the dying thread (addresses only,
//!   `trace`), then a thread of its own resolves them, matches the loaded modules that hold a frame, takes the tail of our
//!   own warnings and errors (`tail`, an in-memory ring) and writes one record (`record`, `store`: private files in
//!   `D/connector-crashes`, at most 20, 1 MiB and 30 days, the same signature merged with a count). The hook cannot ask
//!   the core, so a record is made unless this process already learned "off"; it is then judged at the next start. The hook
//!   stays safe (`docs/logging.md`, section 9): one capture at a time, locks only tried, a panic in the capture thread is
//!   contained there, the wait is bounded, and the default hook is never chained.
//! - At the next start, a thread of its own ([`start`], `report`) waits for the core, asks it, sends each record to Sentry
//!   as an event built from the record's closed field list and deletes it once Sentry took it. No DSN in the build (a local
//!   one, a pull request) means nothing is asked or sent, silently.
//!
//! A record holds a closed list of fields ([`CrashRecord`]) and nothing else; text in it is bounded and free of control
//! characters. The panic message is only ever a `&'static str` literal or the fixed marker (with the formatted message's
//! length); the formatted message of `docs/logging.md` section 9 is never stored. Only the daemon records: the CLI and the
//! MCP server have no file sink and no record.

mod record;
mod report;
mod settings;
mod store;
mod tail;
#[cfg(test)]
mod tests;
mod trace;

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, OnceLock};
use std::time::{Duration, SystemTime};

use crate::error::ConnectorError;
use crate::messages::Keyed;
use crate::profile::Profile;

pub use record::{CrashRecord, Exception, Frame, Image, Location, Panic};
pub use report::{event, EXTRAS, SERVICE, TAGS};
pub use store::CrashStore;
pub use tail::layer as tail_layer;

/// The connector's own log targets: what a record's tail keeps. Not `panic` and `crash_backtrace` (the hook's own, which the
/// record itself replaces).
pub const OWN_TARGETS: [&str; 2] = ["sidevoice_connector", "sidevoice_platform"];

/// The most records kept.
pub const MAX_RECORDS: usize = 20;
/// The most bytes the records take together.
pub const MAX_BYTES: u64 = 1024 * 1024;
/// The longest a record is kept, from its last occurrence.
pub const MAX_AGE_DAYS: i64 = 30;
/// The most bytes one record file may have: a bigger one is not ours and is deleted.
pub const MAX_RECORD_BYTES: u64 = 64 * 1024;
/// The most events of the connector's own log a record carries: its last warnings and errors.
pub const TAIL_LINES: usize = 20;
/// The most frames of the backtrace a record carries.
pub const MAX_FRAMES: usize = 64;
/// The most loaded modules a record carries: the ones that hold a frame.
pub const MAX_IMAGES: usize = 16;
/// How long the panic hook waits for the capture thread in all.
const HOOK_WAIT: Duration = Duration::from_secs(5);

/// What the capture needs and cannot ask for while the process is panicking.
struct Context {
    data: PathBuf,
    store: CrashStore,
    homes: Vec<String>,
    os: &'static str,
    arch: &'static str,
    /// The loaded modules, listed at start.
    images: Vec<Image>,
    /// The log's own backstop for what it must not name (private conversation ids), applied to the tail as to the file.
    redact: fn(&str) -> String,
}

static CONTEXT: OnceLock<Context> = OnceLock::new();
/// One capture at a time, process-wide: a panic that comes while one is under way (another thread's, or the capture's own)
/// is not recorded.
static CAPTURING: AtomicBool = AtomicBool::new(false);
/// One pass at start per process.
static STARTED: AtomicBool = AtomicBool::new(false);
/// Whether the core said "off" to this process. Memory only, never written: the core's answer is the setting, and a process
/// that has not learned it records and lets the next start judge.
static KNOWN_OFF: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Set on the sender's thread: a panic of the reporter itself is logged and contained, never recorded (no loop).
    static SENDING: Cell<bool> = const { Cell::new(false) };
}

/// What this process has learned of the setting.
fn known(on: bool) {
    KNOWN_OFF.store(!on, Ordering::SeqCst);
}

/// From now on a panic of this process leaves a record. `data` is the connector's data directory; `homes` are the spellings
/// of the home directory, each shown as `~` and `redact` is the log's other backstop (the log's own, reused here). Lists the loaded
/// modules now.
pub fn install(
    data: &Path,
    homes: Vec<String>,
    os: &'static str,
    arch: &'static str,
    redact: fn(&str) -> String,
) {
    let _ = CONTEXT.set(Context {
        data: data.to_path_buf(),
        store: CrashStore::new(data, homes.clone()),
        homes,
        os,
        arch,
        images: trace::loaded(),
        redact,
    });
}

/// Sends what a previous run left, in the background (call once the daemon is serving). Never blocks and never fails.
pub fn start(profile: Profile) {
    if let Some(context) = CONTEXT.get() {
        if !STARTED.swap(true, Ordering::SeqCst) {
            report::send_at_start(context, profile);
        }
    }
}

/// Deletes the connector's id and every pending record, as the core's answer "off" does: for the command that turns the
/// setting off, from any process of this machine.
pub fn erase(data: &Path) -> std::io::Result<()> {
    settings::erase(data)
}

/// `crash-reports on|off|status`: the core's setting, forwarded (`set` is the new value, `None` to only ask). An answer "off"
/// also deletes this connector's own id and pending records, as the pass at start does. A core that does not answer is an
/// error, never a guess.
pub async fn forward(profile: &Profile, set: Option<bool>) -> anyhow::Result<bool> {
    let answer = match set {
        Some(send) => profile.set_crash_reports(send).await,
        None => profile.crash_reports().await,
    };
    let send =
        answer.map_err(|_| Keyed::new("crash-reports.core-unavailable", serde_json::json!({})))?;
    if !send {
        if let Err(error) = settings::erase(&profile.data) {
            tracing::warn!(
                error.type = ConnectorError::CrashReportsUnavailable.code(),
                kind = ?error.kind(),
                "the crash reports of a machine set not to send could not be deleted; the next start deletes them"
            );
        }
    }
    Ok(send)
}

struct Release;

impl Drop for Release {
    fn drop(&mut self) {
        CAPTURING.store(false, Ordering::SeqCst);
    }
}

/// Records a panic. Best effort and never panics: it is called by the panic hook, where a panic aborts the process and
/// cannot be caught, so only the frames' addresses (the dying thread's) are taken here; a thread of its own does the rest,
/// and the hook waits for it for [`HOOK_WAIT`] at most.
pub fn capture(literal: Option<&str>, formatted_len: usize, file: &str, line: u32) {
    let Some(context) = CONTEXT.get() else {
        return;
    };
    if KNOWN_OFF.load(Ordering::SeqCst) || SENDING.get() || CAPTURING.swap(true, Ordering::SeqCst) {
        return;
    }
    let trace = backtrace::Backtrace::new_unresolved();
    let (literal, file) = (literal.map(str::to_owned), file.to_owned());
    let (done, finished) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("crash-capture".into())
        .spawn(move || {
            let release = Release;
            write(
                context,
                literal.as_deref(),
                formatted_len,
                &file,
                line,
                trace,
            );
            // Released before the hook is told, so that the next panic, right after this one, finds the capture free. A panic
            // in `write` releases it as it unwinds.
            drop(release);
            let _ = done.send(());
        });
    match spawned {
        Ok(_) => {
            let _ = finished.recv_timeout(HOOK_WAIT);
        }
        Err(_) => CAPTURING.store(false, Ordering::SeqCst),
    }
}

/// The capture thread's work.
fn write(
    context: &Context,
    literal: Option<&str>,
    formatted_len: usize,
    file: &str,
    line: u32,
    mut trace: backtrace::Backtrace,
) {
    trace.resolve();
    let frames = trace::frames(&trace);
    let images = trace::holding(&context.images, &frames);
    let crashed = SystemTime::now();
    let record = CrashRecord::from_panic(
        Panic {
            version: crate::identity::VERSION,
            os: context.os,
            arch: context.arch,
            at: crashed.into(),
            literal,
            formatted_len,
            file,
            line,
            frames,
            images,
            tail: tail::snapshot(crashed)
                .iter()
                .map(|line| (context.redact)(line))
                .collect(),
        },
        &context.homes,
    );
    // A thread that panicked inside the store holds its lock: do not wait for it.
    context.store.record_without_waiting(record);
}
