//! A record as a Sentry event, the POST, and the pass that sends what a previous run left, at start.
//!
//! The event is built from the record's closed field list and nothing else ([`event`]; no SDK client, scope, integration or
//! transport exists in the process, only the SDK's event and envelope types). Sending never breaks the connector: the pass
//! runs on a thread of its own, every POST has a timeout and the pass a budget, a panic is contained, and whatever fails is
//! one log line; the record stays for the next start.
//!
//! Whether the machine sends is not decided here: the core owns that setting and the pass asks it (`ask`), once it is
//! running and again just before each POST. A core that cannot be asked (not running, no answer, a core that has no such
//! setting) is not "off": the records stay and nothing is sent, ever blind. An answer "off" discards the records and the id.

use std::borrow::Cow;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime};

use sentry::protocol::{
    Addr, DebugImage, DebugMeta, Event, Exception, Frame, Level, Map, Stacktrace,
    SymbolicDebugImage, Value, Values,
};
use sentry::types::{CodeId, DebugId, Dsn, Uuid};
use sentry::Envelope;
use tracing::{info, warn};

use super::record::CrashRecord;
use super::{known, settings, Context, SENDING};
use crate::error::ConnectorError;
use crate::profile::Profile;

/// The `service.name` and the release's name: `sidevoice-connector@<version>`.
pub const SERVICE: &str = "sidevoice-connector";
/// How long one POST may take.
const POST_BUDGET: Duration = Duration::from_secs(10);
/// How long the sending part of a pass may take; what is left waits for the next start.
const PASS_BUDGET: Duration = Duration::from_secs(60);

/// The tags an event may carry.
pub const TAGS: [&str; 4] = ["service.name", "os.type", "host.arch", "installation.id"];
/// The `extra` entries an event may carry.
pub const EXTRAS: [&str; 5] = ["count", "first_at", "location", "log_tail", "len"];

/// What the core says now about sending, or `None` when it cannot be learned.
pub(super) type Ask<'a> = &'a dyn Fn() -> Option<bool>;

/// How long the pass waits for the core to answer before it leaves the records for the next start: the core is its own
/// job and is often still starting when the connector is.
#[derive(Clone, Copy)]
pub(super) struct Patience {
    /// The first wait, doubled after each failed try.
    pub first: Duration,
    /// The longest single wait.
    pub longest: Duration,
    /// In all.
    pub budget: Duration,
}

pub(super) const PATIENCE: Patience = Patience {
    first: Duration::from_secs(1),
    longest: Duration::from_secs(30),
    budget: Duration::from_secs(180),
};

/// The event of a record, from its closed field list: the Sentry `event_id` is the record's id, the release the
/// connector's version, and `installation.id` the connector's own random id.
pub fn event(record: &CrashRecord, machine_id: &str) -> Event<'static> {
    let time = |text: &str| {
        chrono::DateTime::parse_from_rfc3339(text).map_or(SystemTime::now(), SystemTime::from)
    };
    let tags: Map<String, String> = [
        ("service.name", SERVICE),
        ("os.type", record.os.as_str()),
        ("host.arch", record.arch.as_str()),
        ("installation.id", machine_id),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value.to_owned()))
    .collect();
    let frames: Vec<Frame> = record
        .backtrace
        .iter()
        .map(|frame| Frame {
            function: frame.function.clone(),
            filename: frame.file.clone(),
            lineno: frame.line.map(u64::from),
            instruction_addr: Some(Addr(frame.addr)),
            ..Default::default()
        })
        .collect();
    let images: Vec<DebugImage> = record
        .images
        .iter()
        .filter_map(|image| {
            Some(DebugImage::Symbolic(SymbolicDebugImage {
                name: image.name.clone(),
                arch: None,
                image_addr: Addr(image.addr),
                image_size: image.size,
                image_vmaddr: Addr(image.vmaddr),
                id: DebugId::from_str(&image.id).ok()?,
                code_id: image
                    .code_id
                    .as_deref()
                    .map(|id| CodeId::new(id.to_owned())),
                debug_file: None,
            }))
        })
        .collect();
    let mut extra: Map<String, Value> = Map::new();
    extra.insert("count".to_owned(), Value::from(record.count));
    extra.insert("first_at".to_owned(), Value::from(record.first_at.clone()));
    extra.insert(
        "location".to_owned(),
        Value::from(format!("{}:{}", record.location.file, record.location.line)),
    );
    extra.insert("log_tail".to_owned(), Value::from(record.log_tail.clone()));
    if let Some(len) = record.exception.len {
        extra.insert("len".to_owned(), Value::from(len));
    }
    Event {
        event_id: Uuid::parse_str(&record.id).unwrap_or_else(|_| Uuid::new_v4()),
        level: Level::Fatal,
        platform: Cow::Borrowed("native"),
        // Never the host name: the SDK's client would fill it in, and no client exists here; still said, not assumed.
        server_name: None,
        timestamp: time(&record.last_at),
        release: Some(Cow::Owned(format!("{SERVICE}@{}", record.version))),
        tags,
        extra,
        exception: Values::from(vec![Exception {
            ty: record.exception.kind.clone(),
            value: Some(record.exception.message.clone()),
            stacktrace: Stacktrace::from_frames_reversed(frames),
            ..Default::default()
        }]),
        debug_meta: Cow::Owned(DebugMeta {
            sdk_info: None,
            images,
        }),
        ..Default::default()
    }
}

/// The one door anything leaves through: the envelope posted within [`POST_BUDGET`]. `true` when Sentry took it.
fn deliver(agent: &ureq::Agent, dsn: &Dsn, event: Event<'static>) -> bool {
    let mut body = Vec::new();
    if Envelope::from(event).to_writer(&mut body).is_err() {
        return false;
    }
    agent
        .post(dsn.envelope_api_url().as_str())
        .header(
            "X-Sentry-Auth",
            dsn.to_auth(Some(concat!(
                "sidevoice-connector/",
                env!("CARGO_PKG_VERSION")
            )))
            .to_string(),
        )
        .header("Content-Type", "application/x-sentry-envelope")
        .send(&body[..])
        .is_ok_and(|answer| answer.status().is_success())
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(POST_BUDGET))
        // A refusal is an answer, not an error: `deliver` reads the status.
        .http_status_as_error(false)
        .build()
        .into()
}

/// The records a previous run left, sent one at a time, newest first. A record is deleted once Sentry took it; the first
/// failure ends the pass (the network is likely down, and every record would wait out its timeout). The core is asked
/// again before each one. Returns how many were sent.
pub(super) fn drain(context: &Context, ask: Ask<'_>, dsn: &Dsn, deadline: Instant) -> usize {
    let mut sent = 0;
    let records = match context.store.pending() {
        Ok(records) => records,
        Err(error) => {
            warn!(
                error.type = ConnectorError::CrashReportsUnavailable.code(),
                kind = ?error.kind(),
                "the pending crash reports could not be read"
            );
            return sent;
        }
    };
    let agent = agent();
    for record in records {
        if Instant::now() >= deadline {
            break;
        }
        match ask() {
            Some(true) => {}
            Some(false) => {
                discard(context);
                break;
            }
            // Cannot be learned: never sent blind.
            None => break,
        }
        let Ok(id) = settings::machine_id(&context.data) else {
            break;
        };
        if !deliver(&agent, dsn, event(&record, &id)) {
            warn!(
                error.type = ConnectorError::CrashReportFailed.code(),
                "a crash report could not be sent; it stays for the next start"
            );
            break;
        }
        sent += 1;
        if let Err(error) = context.store.remove(&record.id) {
            warn!(
                error.type = ConnectorError::CrashReportsUnavailable.code(),
                kind = ?error.kind(),
                "a sent crash report could not be deleted; it will be sent again under the same id"
            );
        }
    }
    sent
}

/// What the core says, asked until it answers or `patience` is spent.
fn learn(ask: Ask<'_>, patience: Patience) -> Option<bool> {
    let end = Instant::now() + patience.budget;
    let mut wait = patience.first;
    loop {
        if let Some(on) = ask() {
            return Some(on);
        }
        if Instant::now() + wait >= end {
            return None;
        }
        std::thread::sleep(wait);
        wait = (wait * 2).min(patience.longest);
    }
}

/// The core said off: the id and every record go, and none is made again in this process.
fn discard(context: &Context) {
    known(false);
    if let Err(error) = settings::erase(&context.data) {
        warn!(
            error.type = ConnectorError::CrashReportsUnavailable.code(),
            kind = ?error.kind(),
            "the crash reports of a machine set not to send could not be deleted"
        );
    }
}

/// What the build was given: the DSN, set by the release and nightly workflows from the organisation's variable
/// `SENTRY_DSN`. None (a local build, a pull request), or one that does not parse, means nothing is ever sent, silently.
fn dsn() -> Option<Dsn> {
    option_env!("SIDEVOICE_SENTRY_DSN")
        .filter(|dsn| !dsn.is_empty())
        .and_then(|dsn| Dsn::from_str(dsn).ok())
}

/// The pass at start, on a thread of its own so that nothing here can hold the connector up or take it down. The core is
/// asked over the local channel by this thread's own small runtime.
pub(super) fn send_at_start(context: &'static Context, profile: Profile) {
    // A build with no DSN (a local one, a pull request) can never send: nothing is asked and nothing is started.
    let Some(dsn) = dsn() else { return };
    let _ = std::thread::Builder::new()
        .name("crash-send".into())
        .spawn(move || {
            SENDING.set(true);
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                let ask = || runtime.block_on(profile.crash_reports()).ok();
                pass(context, &ask, &dsn, PATIENCE);
            }));
            if outcome.is_err() {
                warn!(
                    error.type = ConnectorError::CrashReportFailed.code(),
                    "the crash reporter failed and was contained; the connector goes on"
                );
            }
        });
}

pub(super) fn pass(context: &Context, ask: Ask<'_>, dsn: &Dsn, patience: Patience) {
    match learn(ask, patience) {
        Some(true) => known(true),
        // Records made while off, or left from before it, are discarded.
        Some(false) => {
            discard(context);
            return;
        }
        None => {
            warn!(
                error.type = ConnectorError::CrashReportsUnavailable.code(),
                "the core did not say whether crash reports may be sent; the records stay for the next start"
            );
            return;
        }
    }
    let sent = drain(context, ask, dsn, Instant::now() + PASS_BUDGET);
    if sent > 0 {
        info!(sent, "crash reports from a previous run were sent");
    }
}
