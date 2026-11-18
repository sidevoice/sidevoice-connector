//! The last warnings and errors of our own targets, kept in memory for the record of a crash.
//!
//! A layer of the process's subscriber keeps each such event as it is: its time, level and target, the spans it was in,
//! its message and its fields, each field apart. Nothing is read back from a log file (a process that logs to stderr only has
//! the same tail), and nothing is formatted until a record is made: then the ids are re-keyed by the name of the field that
//! carries them (`device`, `session`, any `*_id`), so one id is the same token inside a record (`d1`, `d2`) and none across
//! records.
//!
//! The ring is bounded (`TAIL_LINES` events, fields and texts cut), written with a lock that is only tried while the panic
//! hook runs (a panic may come from inside a write), and read with a lock that is only ever tried. The `panic` and
//! `crash_backtrace` targets are not in it: the record itself says what the panic was.

use std::collections::VecDeque;
use std::fmt::Debug;
use std::sync::{Mutex, PoisonError, TryLockError};
use std::time::SystemTime;

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use super::{OWN_TARGETS, TAIL_LINES};

const MAX_FIELDS: usize = 16;
const MAX_SPANS: usize = 8;
const MAX_TEXT: usize = 200;

/// An event of ours at `warn` or `error`, as it was logged.
struct Entry {
    at: SystemTime,
    level: Level,
    target: String,
    spans: Vec<Span>,
    message: String,
    fields: Vec<(String, String)>,
}

struct Span {
    name: &'static str,
    fields: Vec<(String, String)>,
}

/// The fields of a span, kept in its extensions.
struct SpanFields(Vec<(String, String)>);

static RING: Mutex<VecDeque<Entry>> = Mutex::new(VecDeque::new());

/// Collects an event's or span's fields, each value as text and cut.
#[derive(Default)]
struct Fields {
    message: String,
    fields: Vec<(String, String)>,
}

impl Fields {
    fn put(&mut self, name: &str, value: String) {
        let value: String = value.chars().take(MAX_TEXT).collect();
        if name == "message" {
            self.message = value;
        } else if self.fields.len() < MAX_FIELDS {
            match self.fields.iter_mut().find(|(known, _)| known == name) {
                Some(known) => known.1 = value,
                None => self.fields.push((name.to_owned(), value)),
            }
        }
    }
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field.name(), format!("{value:?}"));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.put(field.name(), format!("{value:?}"));
    }
}

/// The layer, with its filter: our own targets, spans of any level (an event's span carries ids) and events at `warn` or
/// `error`. `in_hook` says whether this thread is inside the panic hook.
pub fn layer<S>(in_hook: fn() -> bool) -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    Tail { in_hook }.with_filter(filter_fn(ours as fn(&Metadata<'_>) -> bool))
}

fn ours(meta: &Metadata<'_>) -> bool {
    let target = meta.target();
    OWN_TARGETS.iter().any(|own| {
        target == *own
            || target
                .strip_prefix(own)
                .is_some_and(|rest| rest.starts_with("::"))
    }) && (meta.is_span() || *meta.level() <= Level::WARN)
}

struct Tail {
    in_hook: fn() -> bool,
}

impl<S> Layer<S> for Tail
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: Context<'_, S>) {
        let mut visitor = Fields::default();
        attributes.record(&mut visitor);
        if let Some(span) = context.span(id) {
            span.extensions_mut().insert(SpanFields(visitor.fields));
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, context: Context<'_, S>) {
        let mut visitor = Fields::default();
        values.record(&mut visitor);
        if let Some(span) = context.span(id) {
            if let Some(SpanFields(fields)) = span.extensions_mut().get_mut::<SpanFields>() {
                for (name, value) in visitor.fields {
                    if let Some(known) = fields.iter_mut().find(|(known, _)| *known == name) {
                        known.1 = value;
                    } else if fields.len() < MAX_FIELDS {
                        fields.push((name, value));
                    }
                }
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        let mut visitor = Fields::default();
        event.record(&mut visitor);
        let spans = context
            .event_scope(event)
            .map(|scope| {
                scope
                    .from_root()
                    .take(MAX_SPANS)
                    .map(|span| Span {
                        name: span.name(),
                        fields: span
                            .extensions()
                            .get::<SpanFields>()
                            .map(|fields| fields.0.clone())
                            .unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let meta = event.metadata();
        let entry = Entry {
            at: SystemTime::now(),
            level: *meta.level(),
            target: meta.target().to_owned(),
            spans,
            message: visitor.message,
            fields: visitor.fields,
        };
        // Inside the panic hook a lock somebody holds is never waited for: this thread may be the one that panicked in it.
        let mut ring = if (self.in_hook)() {
            match RING.try_lock() {
                Ok(ring) => ring,
                Err(TryLockError::Poisoned(ring)) => ring.into_inner(),
                Err(TryLockError::WouldBlock) => return,
            }
        } else {
            RING.lock().unwrap_or_else(PoisonError::into_inner)
        };
        if ring.len() >= TAIL_LINES {
            ring.pop_front();
        }
        ring.push_back(entry);
    }
}

/// The tail as lines for a record whose crash is at `crashed`: empty when the ring is busy (a record without a tail is still
/// a record).
pub(super) fn snapshot(crashed: SystemTime) -> Vec<String> {
    let ring = match RING.try_lock() {
        Ok(ring) => ring,
        Err(TryLockError::Poisoned(ring)) => ring.into_inner(),
        Err(TryLockError::WouldBlock) => return Vec::new(),
    };
    render(ring.iter(), crashed)
}

/// The ids the logs name by a field, and the letter of the tokens that stand for their values (`d1`, `d2`, ...).
const KNOWN_IDS: [(&str, char); 13] = [
    ("device", 'd'),
    ("client", 'c'),
    ("session", 's'),
    ("turn", 't'),
    ("utterance", 'u'),
    ("message", 'm'),
    ("conversation", 'v'),
    ("call", 'k'),
    ("connector", 'n'),
    ("binding", 'b'),
    ("launch", 'l'),
    ("marker", 'r'),
    ("chat", 'h'),
];

/// The token letter of the id a log field named `name` carries, if it carries one: a known id (`device`, `device_id`,
/// `device.id`, ...), or generically any field that is `id` or ends in `_id` or `.id` (token letter `x`).
fn id_kind(name: &str) -> Option<char> {
    let stem = name
        .strip_suffix("_id")
        .or_else(|| name.strip_suffix(".id"))
        .unwrap_or(name);
    KNOWN_IDS
        .iter()
        .find(|(known, _)| *known == stem)
        .map(|(_, kind)| *kind)
        .or_else(|| (name == "id" || stem != name).then_some('x'))
}

/// Whether `value` (a field's text, quoted or not) is shaped like an id: a UUID, a long hex string with a letter in it, or the
/// connector's own name for a conversation's id in a log (`IdHash`: `h:` and 12 hex digits).
fn looks_like_id(value: &str) -> bool {
    let bare = value.trim_matches('"');
    let uuid = bare.len() == 36
        && bare.char_indices().all(|(at, c)| {
            if [8, 13, 18, 23].contains(&at) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        });
    let hex = bare.len() >= 16
        && bare.bytes().all(|b| b.is_ascii_hexdigit())
        && bare.bytes().any(|b| b.is_ascii_alphabetic());
    let hashed = bare
        .strip_prefix("h:")
        .is_some_and(|rest| rest.len() == 12 && rest.bytes().all(|b| b.is_ascii_hexdigit()));
    uuid || hex || hashed
}

/// The tokens of one record: the same value of one kind of id is the same token (`d1` for the first device named, `d2` for
/// the next).
#[derive(Default)]
struct Tokens(Vec<(char, String)>);

impl Tokens {
    fn of(&mut self, kind: char, value: &str) -> String {
        let index = self
            .0
            .iter()
            .filter(|(known, _)| *known == kind)
            .position(|(_, seen)| seen == value)
            .unwrap_or_else(|| {
                self.0.push((kind, value.to_owned()));
                self.0.iter().filter(|(known, _)| *known == kind).count() - 1
            });
        format!("{kind}{}", index + 1)
    }

    /// `name=value`, the value a token when the field carries an id, by its name or, whatever the name, by its shape (a UUID, 16 or
    /// more hex digits with a letter among them, or `h:` and 12 hex digits); an empty value stays empty.
    fn field(&mut self, name: &str, value: &str) -> String {
        match id_kind(name).or_else(|| looks_like_id(value).then_some('x')) {
            Some(_) if value.is_empty() => format!("{name}="),
            Some(kind) => format!("{name}={}", self.of(kind, value)),
            None => format!("{name}={value}"),
        }
    }
}

/// `<offset> <LEVEL> [<span>{fields}: ]<target>: <message> <fields>`, the grammar `record::is_ours` accepts, the offset
/// from `crashed` (`-12.345s`, milliseconds, `+` for an event after the crash).
fn render<'a>(entries: impl Iterator<Item = &'a Entry>, crashed: SystemTime) -> Vec<String> {
    let mut tokens = Tokens::default();
    entries
        .map(|entry| {
            let millis = match entry.at.duration_since(crashed) {
                Ok(after) => after.as_millis() as i128,
                Err(before) => -(before.duration().as_millis() as i128),
            };
            let mut line = format!(
                "{}{}.{:03}s {}",
                if millis < 0 { '-' } else { '+' },
                millis.unsigned_abs() / 1000,
                millis.unsigned_abs() % 1000,
                entry.level.as_str(),
            );
            for span in &entry.spans {
                line.push(' ');
                line.push_str(span.name);
                if !span.fields.is_empty() {
                    let fields: Vec<String> = span
                        .fields
                        .iter()
                        .map(|(name, value)| tokens.field(name, value))
                        .collect();
                    line.push_str(&format!("{{{}}}", fields.join(" ")));
                }
                line.push(':');
            }
            line.push_str(&format!(" {}: {}", entry.target, entry.message));
            for (name, value) in &entry.fields {
                line.push(' ');
                line.push_str(&tokens.field(name, value));
            }
            line
        })
        .collect()
}

#[cfg(test)]
pub(super) mod testing {
    use super::*;

    /// The ring is the process's: the tests that fill or empty it take this, one at a time.
    pub(crate) static SERIAL: Mutex<()> = Mutex::new(());

    pub(crate) fn clear() {
        RING.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn entry(at: SystemTime, level: Level, spans: Vec<Span>, fields: &[(&str, &str)]) -> Entry {
        Entry {
            at,
            level,
            target: "sidevoice_connector::server::call".to_owned(),
            spans,
            message: "the call stopped".to_owned(),
            fields: fields
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
        }
    }

    #[test]
    fn ids_are_re_keyed_by_the_name_of_their_field_and_only_those() {
        let crashed = SystemTime::now();
        let entries = [
            entry(
                crashed - Duration::from_millis(12_345),
                Level::ERROR,
                vec![Span {
                    name: "call",
                    fields: vec![("session".into(), "\"abc\"".into())],
                }],
                &[
                    ("device", "\"d-77\""),
                    ("note", "device=\"secret\""),
                    ("session_id", "\"abc\""),
                ],
            ),
            entry(
                crashed + Duration::from_millis(250),
                Level::WARN,
                vec![],
                &[
                    ("device", "\"d-88\""),
                    ("device_id", "\"d-77\""),
                    ("turn", ""),
                ],
            ),
        ];
        let lines = render(entries.iter(), crashed);
        assert_eq!(
            lines[0],
            "-12.345s ERROR call{session=s1}: sidevoice_connector::server::call: the call stopped device=d1 note=device=\"secret\" session_id=s1"
        );
        assert_eq!(
            lines[1],
            "+0.250s WARN sidevoice_connector::server::call: the call stopped device=d2 device_id=d1 turn="
        );
        for line in &lines {
            assert!(super::super::record::is_ours(line), "{line}");
        }
    }

    #[test]
    fn a_value_shaped_like_an_id_is_re_keyed_under_any_field_name() {
        let crashed = SystemTime::now();
        let lines = render(
            [entry(
                crashed,
                Level::WARN,
                vec![Span {
                    name: "call",
                    fields: vec![(
                        "trace".into(),
                        "\"550e8400-e29b-41d4-a716-446655440000\"".into(),
                    )],
                }],
                &[
                    ("trace", "\"550e8400-e29b-41d4-a716-446655440000\""),
                    ("blob", "9f86d081884c7d65"),
                    ("other", "\"550E8400-E29B-41D4-A716-446655440001\""),
                    ("size", "1234567890123456"),
                    ("name", "\"deadbeef\""),
                    ("note", "\"plain text\""),
                    ("who", "\"h:0123456789ab\""),
                    ("again", "h:0123456789AB"),
                    ("short", "\"h:0123456789\""),
                    ("long", "\"h:0123456789abc\""),
                ],
            )]
            .iter(),
            crashed,
        );
        assert_eq!(
            lines[0],
            "+0.000s WARN call{trace=x1}: sidevoice_connector::server::call: the call stopped trace=x1 blob=x2 other=x3 size=1234567890123456 name=\"deadbeef\" note=\"plain text\" who=x4 again=x5 short=\"h:0123456789\" long=\"h:0123456789abc\""
        );
        assert!(!looks_like_id("1234567890123456"));
        assert!(!looks_like_id("beef"));
        assert!(looks_like_id("h:0123456789ab"));
    }

    #[test]
    fn the_layer_keeps_the_last_events_of_ours_with_their_spans_and_nothing_else() {
        use tracing_subscriber::layer::SubscriberExt;
        let _serial = testing::SERIAL
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        testing::clear();
        let subscriber = tracing_subscriber::registry().with(layer(|| false));
        tracing::subscriber::with_default(subscriber, || {
            let span =
                tracing::info_span!(target: "sidevoice_connector::x", "call", device = "d-1");
            let _entered = span.enter();
            tracing::info!(target: "sidevoice_connector::x", "too quiet");
            tracing::warn!(target: "sidevoice_connector::x", client = "c-9", "kept");
            tracing::error!(target: "reqwest::x", "a dependency");
            tracing::error!(target: "panic", "the panic itself");
            tracing::error!(target: "crash_backtrace", "a backtrace");
            for n in 0..30 {
                tracing::error!(target: "sidevoice_platform::y", n, "fill");
            }
        });
        let lines = snapshot(SystemTime::now());
        assert_eq!(lines.len(), TAIL_LINES);
        assert!(
            lines
                .iter()
                .all(|line| line.contains("sidevoice_platform::y")),
            "{lines:?}"
        );
        assert!(lines.last().unwrap().ends_with("fill n=29"), "{lines:?}");
        testing::clear();
        let subscriber = tracing_subscriber::registry().with(layer(|| false));
        tracing::subscriber::with_default(subscriber, || {
            let span =
                tracing::info_span!(target: "sidevoice_connector::x", "call", device = "d-1");
            let _entered = span.enter();
            tracing::info!(target: "sidevoice_connector::x", "too quiet");
            tracing::warn!(target: "sidevoice_connector::x", client = "c-9", "kept");
            tracing::error!(target: "panic", "the panic itself");
        });
        let lines = snapshot(SystemTime::now());
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("call{device=d1}: sidevoice_connector::x: kept client=c1"),
            "{}",
            lines[0]
        );
        testing::clear();
    }
}
