//! The random id a report carries, and what turning sending off erases.
//!
//! The switch itself is not here: the core owns the machine's "send crash reports" setting and the connector asks it
//! (`core_ready`, `Profile::crash_reports`; `crash::report`), never keeping a copy. What the connector keeps is its own:
//!
//! - `connector-crash-id.json`, `{"id", "made"}`: 128 random bits from the OS as 32 hex digits, made on the first send
//!   and again when a year old. It is the only identifier a report carries, nothing derives it from the machine, and the
//!   core's answer "off" deletes it. It is the connector's own, not the core's: each piece on a machine has its own id.

use std::io;
use std::path::Path;

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use super::store::CrashStore;
use crate::platform::fs::{Files, Fs};

const IDENTITY: &str = "connector-crash-id.json";
/// The most the file may hold when read.
const LIMIT: u64 = 4 * 1024;
const ID_LIFETIME_DAYS: i64 = 365;

#[derive(Serialize, Deserialize)]
struct Identity {
    id: String,
    /// RFC 3339.
    made: String,
}

/// Deletes the id and every pending record. A name that is not there is already gone.
pub(super) fn erase(data: &Path) -> io::Result<()> {
    let id = match std::fs::remove_file(data.join(IDENTITY)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    };
    let records = CrashStore::new(data, Vec::new()).erase();
    id.and(records)
}

/// The id: the one kept, or a new one when there is none, it is not ours or it is a year old.
pub(super) fn machine_id(data: &Path) -> io::Result<String> {
    let path = data.join(IDENTITY);
    let now = Utc::now();
    if let Some(bytes) = Files::read_private(&path, LIMIT)? {
        if let Some(identity) = serde_json::from_slice::<Identity>(&bytes)
            .ok()
            .filter(|identity| fresh(identity, now))
        {
            return Ok(identity.id);
        }
    }
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    let id: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let identity = Identity {
        id,
        made: now.to_rfc3339_opts(SecondsFormat::Secs, true),
    };
    Files::write_json(&path, &identity)?;
    Ok(identity.id)
}

fn fresh(identity: &Identity, now: DateTime<Utc>) -> bool {
    let made = DateTime::parse_from_rfc3339(&identity.made).map(|at| at.with_timezone(&Utc));
    identity.id.len() == 32
        && identity.id.bytes().all(|b| b.is_ascii_hexdigit())
        && made.is_ok_and(|made| {
            // A clock that went back by more than a day does not keep an id for years.
            made <= now + Duration::days(1) && now - made < Duration::days(ID_LIFETIME_DAYS)
        })
}

#[cfg(test)]
pub(super) fn identity_made(data: &Path, made: &str) {
    let identity = Identity {
        id: "0".repeat(32),
        made: made.to_owned(),
    };
    Files::write_json(&data.join(IDENTITY), &identity).unwrap();
}
