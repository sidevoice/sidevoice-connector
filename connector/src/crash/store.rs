//! The bounded store: one private file per record, coalesced by signature, evicted by the caps, tolerant of anything
//! it finds that is not a record.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use super::record::CrashRecord;
use super::{MAX_AGE_DAYS, MAX_BYTES, MAX_RECORDS, MAX_RECORD_BYTES};
use crate::platform::fs::{Files, Fs};

/// The records' directory, in the connector's data directory (`D`).
pub(super) const STORE: &str = "connector-crashes";

/// What the core's answers have told this process so far.
const UNKNOWN: u8 = 0;
const ON: u8 = 1;
const OFF: u8 = 2;

/// The one lock of the store in this process. Only the daemon writes records; another process (`crash-reports off` on
/// the command line) only deletes: a file that is gone under a read is one less record.
static LOCK: Mutex<()> = Mutex::new(());

pub struct CrashStore {
    dir: PathBuf,
    /// Every spelling of the home directory, hidden again in what is read back.
    homes: Vec<String>,
    /// What this process learned of the setting, from the core's answers (memory only, never written): [`UNKNOWN`] until the
    /// first answer, then [`ON`] or [`OFF`]. Nothing is kept once it is off; the handled errors are queued only once it is on.
    learned: AtomicU8,
}

/// A record on disk and the bytes it takes.
struct Kept {
    record: CrashRecord,
    bytes: u64,
}

impl CrashStore {
    /// The store of the connector whose data directory is `data`.
    pub fn new(data: &Path, homes: Vec<String>) -> Self {
        Self {
            dir: data.join(STORE),
            homes,
            learned: AtomicU8::new(UNKNOWN),
        }
    }

    fn file(&self, id: &str) -> PathBuf {
        self.dir.join(format!("crash-{id}.json"))
    }

    /// Keeps `record`: merged into the one with its signature if there is one, then the caps are applied. Nothing is kept once
    /// this process learned that sending is off: that is asked again under the lock, so a record that raced the erasing of an
    /// `off` is not left behind (the erasing takes the same lock).
    pub fn record(&self, record: CrashRecord) -> io::Result<()> {
        let _held = lock();
        if self.is_off() {
            return Ok(());
        }
        self.add(record)
    }

    /// As [`CrashStore::record`] for the panic hook's capture: a lock somebody else holds means this record is dropped,
    /// never waited for. Nothing is returned: there is nobody to tell.
    pub fn record_without_waiting(&self, record: CrashRecord) {
        let _held = match LOCK.try_lock() {
            Ok(held) => held,
            Err(TryLockError::Poisoned(held)) => held.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        if !self.is_off() {
            let _ = self.add(record);
        }
    }

    /// The records waiting to be sent, newest first; what is expired or unreadable is deleted.
    pub fn pending(&self) -> io::Result<Vec<CrashRecord>> {
        let _held = lock();
        let mut kept = self.load()?;
        self.enforce(&mut kept);
        kept.sort_by(|a, b| b.record.last_at.cmp(&a.record.last_at));
        Ok(kept.into_iter().map(|kept| kept.record).collect())
    }

    /// Deletes the record `id`, sent or not wanted. Only a UUID names a file: nothing else reaches the file system.
    pub fn remove(&self, id: &str) -> io::Result<()> {
        if Uuid::parse_str(id).map(|uuid| uuid.to_string()).as_deref() != Ok(id) {
            return Ok(());
        }
        let _held = lock();
        match fs::remove_file(self.file(id)) {
            Ok(()) => Files::sync_dir(&self.dir),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// What this process learned of the setting: on (records are kept) or off (none are).
    pub fn learned(&self, on: bool) {
        self.learned
            .store(if on { ON } else { OFF }, Ordering::SeqCst);
    }

    /// Whether this process learned that sending is off.
    pub fn is_off(&self) -> bool {
        self.learned.load(Ordering::SeqCst) == OFF
    }

    /// Whether this process learned that sending is on: the latest answer of the core was "on". Not knowing is not on.
    pub fn is_on(&self) -> bool {
        self.learned.load(Ordering::SeqCst) == ON
    }

    /// Whether the record `id` is still on disk, asked under the store's lock (an erasing takes it too): right before a send, so
    /// a record that an `off` deleted meanwhile is not posted. Only a UUID names a file.
    pub fn holds(&self, id: &str) -> bool {
        if Uuid::parse_str(id).map(|uuid| uuid.to_string()).as_deref() != Ok(id) {
            return false;
        }
        let _held = lock();
        fs::symlink_metadata(self.file(id)).is_ok_and(|meta| meta.is_file())
    }

    /// Deletes every record. A directory that is written into while it is removed (a capture that had the lock before this one)
    /// is removed again; one that still is after a few tries is an error (`DirectoryNotEmpty`), which the caller reports: sending
    /// is already off, nothing new is kept, and the next start erases again.
    pub fn erase(&self) -> io::Result<()> {
        let _held = lock();
        let mut last: io::Error = io::ErrorKind::DirectoryNotEmpty.into();
        for _ in 0..5 {
            match fs::remove_dir_all(&self.dir) {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => last = error,
                Err(error) => return Err(error),
            }
        }
        Err(last)
    }

    fn add(&self, mut record: CrashRecord) -> io::Result<()> {
        Files::ensure_private_dir(&self.dir)?;
        let mut kept = self.load()?;
        let same = kept
            .iter()
            .position(|kept| kept.record.signature == record.signature);
        match same {
            Some(index) => {
                let found = &mut kept[index];
                found.record.count = found.record.count.saturating_add(record.count);
                if record.last_at > found.record.last_at {
                    found.record.last_at = record.last_at;
                }
                if record.first_at < found.record.first_at {
                    found.record.first_at = record.first_at;
                }
                let bytes = serde_json::to_vec(&found.record)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                Files::write_private(&self.file(&found.record.id), &bytes)?;
                found.bytes = bytes.len() as u64;
            }
            None => {
                let bytes = serde_json::to_vec(&record)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                if bytes.len() as u64 > MAX_RECORD_BYTES {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "too large"));
                }
                // A new record never replaces a file: an id that exists is not made again.
                record.id = record.id.to_lowercase();
                Files::create_private(&self.file(&record.id), &bytes)?;
                kept.push(Kept {
                    record,
                    bytes: bytes.len() as u64,
                });
            }
        }
        self.enforce(&mut kept);
        Ok(())
    }

    /// Every valid record on disk. What is a record's name but is not one (too big, not JSON, not the closed shape, not
    /// named by its own id) is deleted; what is not named like a record is left alone.
    fn load(&self) -> io::Result<Vec<Kept>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut kept = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(id) = name
                .strip_prefix("crash-")
                .and_then(|rest| rest.strip_suffix(".json"))
            else {
                continue;
            };
            match read(&entry.path(), &self.homes).filter(|(record, _)| record.id == id) {
                Some((record, bytes)) => kept.push(Kept { record, bytes }),
                None => {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        Ok(kept)
    }

    /// Deletes what is past the age, then the oldest until the count and the size fit.
    fn enforce(&self, kept: &mut Vec<Kept>) {
        let limit = Utc::now() - Duration::days(MAX_AGE_DAYS);
        let expired = |kept: &Kept| {
            DateTime::parse_from_rfc3339(&kept.record.last_at).map_or(true, |at| at < limit)
        };
        let remove = |kept: &Kept| {
            let _ = fs::remove_file(self.file(&kept.record.id));
        };
        kept.retain(|item| {
            let old = expired(item);
            if old {
                remove(item);
            }
            !old
        });
        kept.sort_by(|a, b| a.record.last_at.cmp(&b.record.last_at));
        while kept.len() > MAX_RECORDS || kept.iter().map(|k| k.bytes).sum::<u64>() > MAX_BYTES {
            let oldest = kept.remove(0);
            remove(&oldest);
        }
        let _ = Files::sync_dir(&self.dir);
    }
}

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The record in `path` and its size, `None` when it is not one: not a private regular file of ours (as the log is
/// judged), over the size, not JSON, or not the closed shape.
fn read(path: &Path, homes: &[String]) -> Option<(CrashRecord, u64)> {
    let file = Files::open_created_file(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return None;
    }
    let value = serde_json::from_slice(&bytes).ok()?;
    CrashRecord::from_untrusted(&value, homes).map(|record| (record, bytes.len() as u64))
}
