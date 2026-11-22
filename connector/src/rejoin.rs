//! The record that lets a conversation rejoin voice by itself when its agent's MCP server starts again (the agent
//! restarts, or its session is reopened).
//!
//! One small owner-only file per joined conversation in `D/rejoin/`, named after the harness and the harness's own id
//! of the conversation (the id its agent gives the conversation) and holding the title and the core's binding id. It is
//! written when the conversation joins and removed when it leaves, when the rejoin is refused because the person or the
//! agent closed it, and when it is older than [`MAX_AGE`]. Whether a conversation may rejoin is not this record's to
//! say: the rejoin is a resume of the core's binding (its id is sent), so the daemon refuses a conversation it knows
//! closed, left or blocked (`conversation-state.json`) and the core refuses one the person closed, removed or that
//! expired while no daemon was listening (`room.binding_closed`). Only the conversation whose own identity the starting
//! MCP server reads can match a record, and nothing is ever launched.

use crate::platform::fs::{Files, Fs};
use crate::profile::Profile;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const DIR: &str = "rejoin";
const RECORD_LIMIT: u64 = 4096;
/// A record not renewed for this long is removed: the core closes a conversation no device has shown for 30 days.
pub const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub harness: String,
    pub id: String,
    pub title: String,
    /// The core's id of the conversation's binding, as last known. A rejoin sends it so the core treats it as a resume
    /// and refuses it when the conversation was closed meanwhile; without one the conversation does not rejoin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_id: Option<String>,
}

fn path(profile: &Profile, harness: &str, id: &str) -> PathBuf {
    let digest = Sha256::digest(format!("{harness}\n{id}"));
    profile
        .data
        .join(DIR)
        .join(format!("{}.json", hex::encode(&digest[..16])))
}

/// A binding id the core gave, not the daemon's placeholder while the core is not reached.
fn core_binding(binding_id: Option<&str>) -> Option<String> {
    binding_id
        .filter(|id| !id.is_empty() && !id.starts_with("local-"))
        .map(str::to_owned)
}

/// Remembers that `harness`'s conversation `id` joined under `title`, bound in the core as `binding_id`. A failure is
/// logged and never fails the join.
pub fn save(profile: &Profile, harness: &str, id: &str, title: &str, binding_id: Option<&str>) {
    write(
        profile,
        &Record {
            harness: harness.to_owned(),
            id: id.to_owned(),
            title: title.to_owned(),
            binding_id: core_binding(binding_id),
        },
    );
}

fn write(profile: &Profile, record: &Record) {
    let result = Files::ensure_private_dir(&profile.data.join(DIR))
        .and_then(|()| Files::write_json(&path(profile, &record.harness, &record.id), record));
    if let Err(error) = result {
        tracing::warn!(error.type = crate::error::ConnectorError::StateWrite.code(), kind = ?error.kind(), os_error = error.raw_os_error(), "the rejoin record could not be saved; this conversation will not rejoin by itself");
    }
}

/// Keeps the core's binding id of a recorded conversation current (the core gives a new one after it restarts). A
/// conversation with no record (it left, or it is not one that rejoins) stays without one.
pub fn update_binding(profile: &Profile, harness: &str, id: &str, binding_id: &str) {
    let Some(binding_id) = core_binding(Some(binding_id)) else {
        return;
    };
    if let Some(mut record) = load(profile, harness, id) {
        if record.binding_id.as_deref() != Some(binding_id.as_str()) {
            record.binding_id = Some(binding_id);
            write(profile, &record);
        }
    }
}

/// Forgets the conversation: it will not rejoin by itself.
pub fn forget(profile: &Profile, harness: &str, id: &str) {
    remove(&path(profile, harness, id));
}

fn remove(path: &Path) {
    if let Err(error) = std::fs::remove_file(path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(error.type = crate::error::ConnectorError::StateWrite.code(), kind = ?error.kind(), os_error = error.raw_os_error(), "the rejoin record could not be removed");
        }
    }
}

/// This conversation's record, when one exists. An unreadable or foreign record is no record.
pub fn load(profile: &Profile, harness: &str, id: &str) -> Option<Record> {
    let bytes = Files::read_private(&path(profile, harness, id), RECORD_LIMIT)
        .ok()
        .flatten()?;
    let record: Record = serde_json::from_slice(&bytes).ok()?;
    (record.harness == harness && record.id == id).then_some(record)
}

/// Removes the records last written more than `max_age` before `now`: sessions that never came back.
pub fn sweep(profile: &Profile, now: SystemTime, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(profile.data.join(DIR)) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > max_age);
        if stale {
            remove(&entry.path());
        }
    }
}

/// The parameters of the register call that rejoins a conversation: a resume (`resume`) of its core binding
/// (`binding_id`), which the daemon refuses for a conversation it knows closed, left or blocked and the core for one
/// closed or removed while nobody listened. `None` when there is nothing to do: no record, already joined, or no core
/// binding is known (a plain register would be the agent connecting anew and would bring back a closed conversation).
pub fn rejoin_params(
    record: Option<Record>,
    already_joined: bool,
    mut register: Value,
) -> Option<Value> {
    let record = record.filter(|_| !already_joined)?;
    register["binding_id"] = Value::String(record.binding_id?);
    register["title"] = Value::String(record.title);
    register["resume"] = Value::Bool(true);
    Some(register)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn profile(dir: &std::path::Path) -> Profile {
        Profile::for_test(dir, &[])
    }

    #[test]
    fn a_record_lives_from_join_to_leave_and_belongs_to_its_own_conversation() {
        let home = crate::test_support::temp_dir("rejoin");
        let profile = profile(home.path());
        assert_eq!(load(&profile, "a-harness", "s1"), None);
        save(&profile, "a-harness", "s1", "my chat", Some("b-1"));
        let record = load(&profile, "a-harness", "s1").unwrap();
        assert_eq!(record.title, "my chat");
        assert_eq!(record.binding_id.as_deref(), Some("b-1"));
        assert_eq!(load(&profile, "a-harness", "s2"), None);
        assert_eq!(load(&profile, "another-harness", "s1"), None);
        save(&profile, "a-harness", "s1", "renamed", Some("b-1"));
        assert_eq!(load(&profile, "a-harness", "s1").unwrap().title, "renamed");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path(&profile, "a-harness", "s1"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0);
        }
        forget(&profile, "a-harness", "s1");
        assert_eq!(load(&profile, "a-harness", "s1"), None);
        forget(&profile, "a-harness", "s1");
    }

    #[test]
    fn a_record_keeps_the_cores_binding_id_current_and_never_a_placeholder() {
        let home = crate::test_support::temp_dir("rejoin");
        let profile = profile(home.path());
        save(&profile, "h", "s1", "chat", Some("local-123"));
        assert_eq!(load(&profile, "h", "s1").unwrap().binding_id, None);
        update_binding(&profile, "h", "s1", "local-456");
        assert_eq!(load(&profile, "h", "s1").unwrap().binding_id, None);
        update_binding(&profile, "h", "s1", "b-1");
        assert_eq!(
            load(&profile, "h", "s1").unwrap().binding_id.as_deref(),
            Some("b-1")
        );
        update_binding(&profile, "h", "s1", "b-2");
        assert_eq!(
            load(&profile, "h", "s1").unwrap().binding_id.as_deref(),
            Some("b-2")
        );
        // A conversation without a record (left, or one that does not rejoin) does not get one.
        update_binding(&profile, "h", "s2", "b-3");
        assert_eq!(load(&profile, "h", "s2"), None);
    }

    #[test]
    fn a_record_that_names_another_conversation_is_ignored() {
        let home = crate::test_support::temp_dir("rejoin");
        let profile = profile(home.path());
        save(&profile, "a-harness", "s1", "chat", Some("b-1"));
        std::fs::copy(
            path(&profile, "a-harness", "s1"),
            path(&profile, "a-harness", "other"),
        )
        .unwrap();
        assert_eq!(load(&profile, "a-harness", "other"), None);
    }

    #[test]
    fn records_of_sessions_that_never_came_back_are_swept() {
        let home = crate::test_support::temp_dir("rejoin");
        let profile = profile(home.path());
        save(&profile, "h", "old", "chat", Some("b-1"));
        save(&profile, "h", "new", "chat", Some("b-2"));
        let now = SystemTime::now();
        let old = std::fs::File::options()
            .write(true)
            .open(path(&profile, "h", "old"))
            .unwrap();
        old.set_modified(now - MAX_AGE - Duration::from_secs(60))
            .unwrap();
        drop(old);
        sweep(&profile, now, MAX_AGE);
        assert_eq!(load(&profile, "h", "old"), None);
        assert!(load(&profile, "h", "new").is_some());
        // No directory yet: nothing to do.
        let empty = crate::test_support::temp_dir("rejoin");
        sweep(&self::profile(empty.path()), now, MAX_AGE);
    }

    #[test]
    fn rejoin_resumes_the_core_binding_only_for_a_recorded_conversation_not_yet_joined() {
        let register = json!({"client_ref":"s1","harness":"a-harness","thread":"s1","title":"","detachable":false});
        let record = |binding_id: Option<&str>| Record {
            harness: "a-harness".into(),
            id: "s1".into(),
            title: "my chat".into(),
            binding_id: binding_id.map(str::to_owned),
        };
        let params = rejoin_params(Some(record(Some("b-1"))), false, register.clone()).unwrap();
        assert_eq!(params["resume"], true);
        assert_eq!(params["binding_id"], "b-1");
        assert_eq!(params["title"], "my chat");
        assert_eq!(params["thread"], "s1");
        assert_eq!(rejoin_params(None, false, register.clone()), None);
        assert_eq!(
            rejoin_params(Some(record(Some("b-1"))), true, register.clone()),
            None
        );
        // No core binding known: a plain register would bring a closed conversation back, so it does not rejoin.
        assert_eq!(rejoin_params(Some(record(None)), false, register), None);
    }
}
