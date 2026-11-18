//! Handled errors: the `error` events of our own targets, sent to Sentry under the crash-report switch
//! (`docs/logging.md`, section 10).
//!
//! What is sent is decided here and by construction, not by filtering a bigger event afterwards:
//!
//! - **Which events.** `error` level, a target of ours ([`OWN_TARGETS`]), never the panic hook's own (`PANIC_TARGETS`, written
//!   out and checked first: a target is matched whole or up to a `::`, so a lookalike such as `panic_x` is not ours either).
//!   The connector has no webview target. One that carries no `error.type`, or one whose `error.type` is not a code of the
//!   connector's own list ([`crate::error::codes`]), is not sent: it is counted ([`untyped`]) and nothing else.
//! - **What of an event.** The layer reads exactly one field of it, `error.type`, only as a string, and only to find it in the
//!   list of the connector's codes: what is kept is that list's own `&'static str`, never the event's text, so a value that
//!   merely looks like a code (a host name, an address a call site logged by mistake) cannot go. The message and every other
//!   field value are never looked at. The place is the callsite's own file and line (`code.file.path`, `code.line.number`),
//!   compiled into the binary, as a short relative path or `unknown`. The sent event ([`event`]) is the release, the five tags
//!   of [`TAGS`] and one exception with that type and that one frame: no message, breadcrumb, extra, context, user or host name.
//! - **How often.** One event per `error.type` per day (UTC) per install, the same type again that day is dropped. The day's
//!   list lives in memory and in `connector-handled-errors.json` (private, in the connector's data directory, read at start and
//!   written after a send); a different day, a clock that went back or a file that says another day all mean a new day. At most
//!   [`MAX_TYPES`] types a day, so neither a storm of codes nor a bad file can make the list or the day's sends unbounded.
//! - **Under the switch, which the core owns.** The connector has no setting: the layer queues only while the core's last
//!   answer, which the daemon already keeps (`CrashStore::is_on`), was "on". An answer never learned (the core is not running
//!   yet, has no such setting) and "off" queue nothing. The sender checks that again, asks the core itself before each send, and
//!   asks once more after the id and the day's list exist, as the crash reports do for the id: "off" erases the list with the
//!   id and the records, and what the sender remembered. A core that cannot be asked sends nothing.
//! - **Never breaks the connector.** The layer does no network, no file and no waiting: it checks, reads one atomic and
//!   `try_send`s into a queue of [`QUEUE`] (a full queue drops the event). The POST is on a thread of its own, one at a time,
//!   [`POST_BUDGET`] each, `catch_unwind` around each event, and after a failed send, or a core that did not answer, it waits
//!   [`QUIET`] before trying again (an error that comes with the network down comes in storms). That thread never feeds itself:
//!   what it logs is `warn`, and the layer ignores events of a sending thread.

use std::borrow::Cow;
use std::fmt::Debug;
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use chrono::{NaiveDate, Utc};
use sentry::protocol::{Event, Exception, Frame, Level, Stacktrace, Values};
use sentry::types::{Dsn, Uuid};
use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing::{warn, Event as Logged, Metadata, Subscriber};
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::{Context as Scope, Layer};
use tracing_subscriber::registry::LookupSpan;

use super::record::relative_path;
use super::report::{agent, deliver, discard, dsn, release, tags, Ask, SERVICE};
use super::{settings, Context, CONTEXT, OWN_TARGETS, PANIC_TARGETS, SENDING};
use crate::error::{codes, ConnectorError};
use crate::platform::fs::{Files, Fs};
use crate::profile::Profile;

/// The tags a handled error's event may carry.
pub const TAGS: [&str; 5] = [
    "service.name",
    "os.type",
    "os.name",
    "host.arch",
    "installation.id",
];
/// How many events wait for the sender.
const QUEUE: usize = 16;
/// How many error types one day's list holds.
pub(super) const MAX_TYPES: usize = 64;
/// How long the sender leaves the network and the core alone after a failed send or an unanswered ask.
const QUIET: Duration = Duration::from_secs(10 * 60);
/// The day's list, a private file of the connector's data directory.
const CAP: &str = "connector-handled-errors.json";
/// The most the file may hold when read: `MAX_TYPES` codes of the connector's (none is near 64 characters), with room.
const LIMIT: u64 = 8 * 1024;

/// A handled error, as much of it as may leave: its code (one of the connector's own, from [`codes`]) and the place of the
/// `error!` that logged it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Handled {
    pub kind: &'static str,
    pub file: String,
    pub line: u32,
}

/// The events without a usable `error.type` seen by this process.
static UNTYPED: AtomicU64 = AtomicU64::new(0);

/// How many `error` events of ours had no `error.type` of the connector's own list since this process started. They are not
/// sent; this is all that is kept of them.
pub fn untyped() -> u64 {
    UNTYPED.load(Ordering::Relaxed)
}

/// What the layer queues into, until the sender takes it (`send_from_now_on`).
static PENDING: Mutex<Option<Receiver<Handled>>> = Mutex::new(None);

/// The connector's own code that `text` is, `None` when it is not exactly one: the only way an `error.type` becomes a
/// [`Handled::kind`].
pub(super) fn known(text: &str) -> Option<&'static str> {
    codes().find(|code| *code == text)
}

/// The layer of the process: `None` (nothing is ever queued) when the build has no DSN, silently, as for the crash reports.
/// It is made before the daemon knows its data directory, so it asks the process's crash context at each event: no context
/// (the CLI, the MCP server) or a core that has not said "on" queues nothing.
pub fn layer<S>(in_hook: fn() -> bool) -> Option<impl Layer<S>>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    dsn()?;
    let (queue, waiting) = sync_channel(QUEUE);
    *PENDING.lock().unwrap_or_else(PoisonError::into_inner) = Some(waiting);
    Some(queueing(
        || CONTEXT.get().is_some_and(|context| context.store.is_on()),
        queue,
        &UNTYPED,
        in_hook,
    ))
}

/// The layer, with its filter: events at `error` of our targets, the panic hook's own never. `on` says whether the core's last
/// answer was "on".
pub(super) fn queueing<S>(
    on: impl Fn() -> bool + Send + Sync + 'static,
    queue: SyncSender<Handled>,
    untyped: &'static AtomicU64,
    in_hook: fn() -> bool,
) -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    Queueing {
        on,
        queue,
        untyped,
        in_hook,
    }
    .with_filter(filter_fn(handled as fn(&Metadata<'_>) -> bool))
}

fn handled(meta: &Metadata<'_>) -> bool {
    let target = meta.target();
    meta.is_event()
        && *meta.level() == tracing::Level::ERROR
        && !in_targets(target, &PANIC_TARGETS)
        && in_targets(target, &OWN_TARGETS)
}

/// Whether `target` is one of `names` or below it (`name::module`): never a lookalike that only shares a prefix.
fn in_targets(target: &str, names: &[&str]) -> bool {
    names.iter().any(|name| {
        target == *name
            || target
                .strip_prefix(name)
                .is_some_and(|rest| rest.starts_with("::"))
    })
}

/// Reads one field of an event, `error.type`, and only when it is a string that is one of the connector's own codes.
#[derive(Default)]
struct ErrorType(Option<&'static str>);

impl Visit for ErrorType {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "error.type" {
            self.0 = known(value);
        }
    }

    /// Every other field, and any other way of recording `error.type`, is not looked at.
    fn record_debug(&mut self, _: &Field, _: &dyn Debug) {}
}

struct Queueing<G> {
    on: G,
    queue: SyncSender<Handled>,
    untyped: &'static AtomicU64,
    /// Whether this thread is inside the panic hook: nothing is done there.
    in_hook: fn() -> bool,
}

impl<S: Subscriber, G: Fn() -> bool + 'static> Layer<S> for Queueing<G> {
    fn on_event(&self, event: &Logged<'_>, _: Scope<'_, S>) {
        // The sender's own thread is never a source (no loop), nor is the panic hook's.
        if (self.in_hook)() || SENDING.get() {
            return;
        }
        let mut field = ErrorType::default();
        event.record(&mut field);
        let Some(kind) = field.0 else {
            self.untyped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        // The core has not said "on" (never answered, or off): nothing is queued.
        if !(self.on)() {
            return;
        }
        let meta = event.metadata();
        // A full queue drops the event: the logging thread never waits.
        let _ = self.queue.try_send(Handled {
            kind,
            file: relative_path(meta.file().unwrap_or("")),
            line: meta.line().unwrap_or(0),
        });
    }
}

/// The event of a handled error, from its closed field list: a random Sentry `event_id`, the connector's version as the
/// release, the tags of [`TAGS`] and one exception, the code, with one frame (the place). `server_name` is unset; nothing else is.
pub(super) fn event(context: &Context, error: &Handled, machine_id: &str) -> Event<'static> {
    let frame = Frame {
        filename: Some(error.file.clone()),
        lineno: Some(u64::from(error.line)),
        ..Default::default()
    };
    Event {
        event_id: Uuid::new_v4(),
        level: Level::Error,
        platform: Cow::Borrowed("native"),
        // Never the host name: said, not assumed (no client exists to fill it in).
        server_name: None,
        timestamp: SystemTime::now(),
        release: Some(release(crate::identity::VERSION)),
        tags: tags([
            ("service.name", SERVICE),
            ("os.type", context.os),
            ("os.name", context.os_name),
            ("host.arch", context.arch),
            ("installation.id", machine_id),
        ]),
        exception: Values::from(vec![Exception {
            ty: error.kind.to_owned(),
            value: None,
            stacktrace: Stacktrace::from_frames_reversed(vec![frame]),
            ..Default::default()
        }]),
        ..Default::default()
    }
}

/// The types sent today. The day is the UTC date: any other date than today's, earlier or later, is a new day.
struct Cap {
    day: Option<NaiveDate>,
    sent: Vec<&'static str>,
    /// How many types a day holds ([`MAX_TYPES`]; less in a test).
    max: usize,
}

/// The file's shape when written; read back loosely (a value that is not ours is dropped, not the whole file).
#[derive(Serialize)]
struct Kept {
    day: String,
    sent: Vec<&'static str>,
}

impl Cap {
    fn empty(max: usize) -> Self {
        Self {
            day: None,
            sent: Vec::new(),
            max,
        }
    }

    /// What the file says about `today`: nothing for another day, a file that is not ours or cannot be read, and only the
    /// connector's own codes, at most `max`, otherwise.
    fn load(data: &Path, today: NaiveDate, max: usize) -> Self {
        let Ok(Some(bytes)) = Files::read_private(&data.join(CAP), LIMIT) else {
            return Self::empty(max);
        };
        let Ok(kept) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return Self::empty(max);
        };
        let same_day = kept["day"]
            .as_str()
            .is_some_and(|day| NaiveDate::parse_from_str(day, "%Y-%m-%d") == Ok(today));
        if !same_day {
            return Self::empty(max);
        }
        let mut sent: Vec<&'static str> = Vec::new();
        let listed = kept["sent"].as_array().map_or(&[][..], Vec::as_slice);
        for kind in listed
            .iter()
            .filter_map(|kind| kind.as_str())
            .filter_map(known)
        {
            if sent.len() < max && !sent.contains(&kind) {
                sent.push(kind);
            }
        }
        Self {
            day: Some(today),
            sent,
            max,
        }
    }

    /// Whether `kind` may be sent on `today`. A day that is not the one the list is for starts a new list.
    fn permits(&mut self, kind: &str, today: NaiveDate) -> bool {
        if self.day != Some(today) {
            *self = Self {
                day: Some(today),
                sent: Vec::new(),
                max: self.max,
            };
        }
        self.sent.len() < self.max && !self.sent.contains(&kind)
    }

    /// Records that `kind` was sent (after `permits` said it might be).
    fn mark(&mut self, kind: &'static str) {
        self.sent.push(kind);
    }

    fn save(&self, data: &Path) -> io::Result<()> {
        Files::write_json(
            &data.join(CAP),
            &Kept {
                day: self.day.map(|day| day.to_string()).unwrap_or_default(),
                sent: self.sent.clone(),
            },
        )
    }
}

/// Deletes the day's list.
pub(super) fn remove_cap(data: &Path) -> io::Result<()> {
    match std::fs::remove_file(data.join(CAP)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Why a handled error was or was not sent.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Sent,
    /// The core's last answer is "off" (or the ask just now said so), or it has not said "on" yet.
    Off,
    /// The core could not be asked just now.
    Unknown,
    /// Its type was already sent today, or the day's list is full.
    Capped,
    /// The network failed, or the core did not answer, a moment ago.
    Quiet,
    /// Not sent now: no id, or the POST failed.
    Unsent,
}

/// The sender: one event at a time, under the core's answer, the day's cap and the quiet after a failure.
pub(super) struct Worker<'a> {
    context: &'a Context,
    agent: ureq::Agent,
    dsn: Dsn,
    cap: Cap,
    quiet_until: Option<Instant>,
}

impl<'a> Worker<'a> {
    pub(super) fn new(context: &'a Context, dsn: Dsn) -> Self {
        Self {
            context,
            agent: agent(),
            dsn,
            cap: Cap::load(&context.data, Utc::now().date_naive(), MAX_TYPES),
            quiet_until: None,
        }
    }

    /// The same, with a day's list of `max` types (a test cannot name [`MAX_TYPES`] of the connector's few codes).
    #[cfg(test)]
    pub(super) fn limited(mut self, max: usize) -> Self {
        self.cap = Cap::load(&self.context.data, Utc::now().date_naive(), max);
        self
    }

    /// Sends `error` unless the core's answer is not "on", its type is capped on `today` or the network failed a moment ago.
    /// The type is counted as sent only once Sentry took the event, so an error that came with the network down is reported
    /// when it comes again.
    pub(super) fn handle(&mut self, error: Handled, today: NaiveDate, ask: Ask<'_>) -> Outcome {
        let context = self.context;
        if self.quiet_until.is_some_and(|until| Instant::now() < until) {
            return Outcome::Quiet;
        }
        // What this process learned from the core: off means what this one remembers goes with what the machine kept; not
        // knowing is not on.
        if context.store.is_off() {
            self.cap = Cap::empty(self.cap.max);
            return Outcome::Off;
        }
        if !context.store.is_on() {
            return Outcome::Off;
        }
        if !self.cap.permits(error.kind, today) {
            return Outcome::Capped;
        }
        match ask() {
            Some(true) => {}
            Some(false) => {
                discard(context);
                self.cap = Cap::empty(self.cap.max);
                return Outcome::Off;
            }
            // Cannot be learned: never sent blind, and the core is left alone for a while.
            None => {
                self.quiet_until = Some(Instant::now() + QUIET);
                warn!(
                    error.type = ConnectorError::CrashReportsUnavailable.code(),
                    "the core did not say whether crash reports may be sent, so a handled error was not reported; none is tried again for a while"
                );
                return Outcome::Unknown;
            }
        }
        let id = match settings::machine_id(&context.data) {
            Ok(id) => id,
            Err(failure) => {
                warn!(
                    error.type = ConnectorError::CrashReportsUnavailable.code(),
                    kind = ?failure.kind(),
                    "the install id could not be had, so a handled error was not reported"
                );
                return Outcome::Unsent;
            }
        };
        if !deliver(&self.agent, &self.dsn, event(context, &error, &id)) {
            self.quiet_until = Some(Instant::now() + QUIET);
            warn!(
                error.type = ConnectorError::CrashReportFailed.code(),
                "a handled error could not be reported; none is tried again for a while"
            );
            return Outcome::Unsent;
        }
        self.cap.mark(error.kind);
        self.keep(ask);
        Outcome::Sent
    }

    /// Writes the day's list. A failure is one line and the list holds in memory until the process ends.
    fn keep(&mut self, ask: Ask<'_>) {
        let context = self.context;
        if let Err(failure) = self.cap.save(&context.data) {
            warn!(
                error.type = ConnectorError::CrashReportsUnavailable.code(),
                kind = ?failure.kind(),
                "the day's list of reported errors could not be saved; it holds until the connector stops"
            );
        }
        // "Off" may have come after the answer that let this send go and before the id and this file were made: asked again,
        // now that they exist, so nothing is kept for a machine set to off. An answer that does not come leaves them for the
        // next ask, which erases them the same way.
        if ask() == Some(false) {
            discard(context);
            self.cap = Cap::empty(self.cap.max);
        }
    }
}

/// Starts the sender, once the daemon is serving: it takes what the layer queued until now and every event after it. Nothing
/// when the layer was not made (no DSN).
pub(super) fn send_from_now_on(context: &'static Context, profile: Profile) {
    let Some(waiting) = PENDING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
    else {
        return;
    };
    let Some(dsn) = dsn() else { return };
    let _ = std::thread::Builder::new()
        .name("crash-handled".into())
        .spawn(move || {
            SENDING.set(true);
            // The core is asked over the local channel by this thread's own small runtime.
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            let ask = || runtime.block_on(profile.crash_reports()).ok();
            serve(context, dsn, &waiting, &ask);
        });
}

/// The sender's loop: it ends only if the layer's queue is gone, and an event that makes it panic costs that event.
pub(super) fn serve(context: &Context, dsn: Dsn, waiting: &Receiver<Handled>, ask: Ask<'_>) {
    let mut worker = Worker::new(context, dsn);
    while let Ok(error) = waiting.recv() {
        let today = Utc::now().date_naive();
        if catch_unwind(AssertUnwindSafe(|| worker.handle(error, today, ask))).is_err() {
            warn!(
                error.type = ConnectorError::CrashReportFailed.code(),
                "the reporter of handled errors failed and was contained; the connector goes on"
            );
        }
    }
}
