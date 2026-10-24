//! Only parsed versions are cached. Resolution and file stats happen outside the cache lock on every delivery.
use super::{run, steer};
use crate::platform::file_stamp::Stamp;
use crate::profile::Profile;
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

type Version = (u64, u64, u64);
pub(super) static CACHE: Cache = Cache::new();

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Key {
    binary: Stamp,
    state_path: PathBuf,
    state: Option<Stamp>,
}

impl Key {
    pub fn read(program: &Path, profile: &Profile) -> Option<Self> {
        let state_path = profile.data.join("agents.json");
        Some(Self {
            binary: Stamp::read(program).ok()?,
            state: Stamp::read(&state_path).ok(),
            state_path,
        })
    }
}

pub(super) struct Cache(Mutex<Option<(Key, Version)>>);

impl Cache {
    pub const fn new() -> Self {
        Self(Mutex::new(None))
    }

    pub fn invalidate(&self, key: &Option<Key>) {
        let mut entry = self.0.lock().unwrap();
        if entry
            .as_ref()
            .is_some_and(|(cached, _)| Some(cached) == key.as_ref())
        {
            *entry = None;
        }
    }

    pub fn steer_failed(&self, key: &Option<Key>, error: &anyhow::Error) {
        if matches!(
            error.downcast_ref::<steer::Failure>(),
            Some(
                steer::Failure::VersionTooOld
                    | steer::Failure::VersionUnknown
                    | steer::Failure::Rpc { .. }
                    | steer::Failure::InvalidReply
            )
        ) || matches!(
            error.downcast_ref::<tokio_tungstenite::tungstenite::Error>(),
            Some(tokio_tungstenite::tungstenite::Error::Protocol(_))
        ) {
            self.invalidate(key);
        }
    }

    pub async fn version(
        &self,
        key: &Option<Key>,
        mut command: tokio::process::Command,
    ) -> Result<Version> {
        if let Some(key) = key {
            let entry = self.0.lock().unwrap();
            if let Some((_, version)) = entry.as_ref().filter(|(cached, _)| cached == key) {
                return Ok(*version);
            }
        }
        // No mutex guard or filesystem I/O in the critical section; duplicate concurrent probes are fine.
        command.arg("--version");
        let (status, stdout) = run(command, true, Duration::from_secs(10)).await?;
        if !status.success() {
            bail!(steer::Failure::VersionUnknown);
        }
        let version = steer::parse_version(&String::from_utf8_lossy(&stdout))
            .ok_or(steer::Failure::VersionUnknown)?;
        if let Some(key) = key {
            *self.0.lock().unwrap() = Some((key.clone(), version));
        }
        Ok(version)
    }
}

#[cfg(test)]
#[path = "cache/tests.rs"]
mod tests;
