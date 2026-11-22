//! The record that lets a conversation rejoin voice by itself when its agent's MCP server starts again (the agent
//! restarts, or its session is reopened).
//!
//! One small owner-only file per joined conversation in `D/rejoin/`, named after the harness and the harness's own id
//! of the conversation (the id its agent gives the conversation) and holding the title. It is written
//! when the conversation joins and removed when it leaves or the daemon says the person or the agent closed it. Whether
//! a conversation may rejoin is the daemon's alone to say: it refuses a resume of a closed, left or blocked
//! conversation (`conversation-state.json`), and this record is then removed. Only the conversation whose own
//! identity the starting MCP server reads can match a record, and nothing is ever launched.

use crate::platform::fs::{Files, Fs};
use crate::profile::Profile;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

const DIR: &str = "rejoin";
const RECORD_LIMIT: u64 = 4096;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub harness: String,
    pub id: String,
    pub title: String,
}

fn path(profile: &Profile, harness: &str, id: &str) -> PathBuf {
    let digest = Sha256::digest(format!("{harness}\n{id}"));
    profile
        .data
        .join(DIR)
        .join(format!("{}.json", hex::encode(&digest[..16])))
}

/// Remembers that `harness`'s conversation `id` joined under `title`. A failure is logged and never fails the join.
pub fn save(profile: &Profile, harness: &str, id: &str, title: &str) {
    let record = Record {
        harness: harness.to_owned(),
        id: id.to_owned(),
        title: title.to_owned(),
    };
    let result = Files::ensure_private_dir(&profile.data.join(DIR))
        .and_then(|()| Files::write_json(&path(profile, harness, id), &record));
    if let Err(error) = result {
        tracing::warn!(error.type = crate::error::ConnectorError::StateWrite.code(), kind = ?error.kind(), os_error = error.raw_os_error(), "the rejoin record could not be saved; this conversation will not rejoin by itself");
    }
}

/// Forgets the conversation: it will not rejoin by itself.
pub fn forget(profile: &Profile, harness: &str, id: &str) {
    if let Err(error) = std::fs::remove_file(path(profile, harness, id)) {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(error.type = crate::error::ConnectorError::StateWrite.code(), kind = ?error.kind(), os_error = error.raw_os_error(), "the rejoin record could not be removed");
        }
    }
}

/// The title this conversation joined under, when a record of it exists. An unreadable or foreign record is no record.
pub fn load(profile: &Profile, harness: &str, id: &str) -> Option<String> {
    let bytes = Files::read_private(&path(profile, harness, id), RECORD_LIMIT)
        .ok()
        .flatten()?;
    let record: Record = serde_json::from_slice(&bytes).ok()?;
    (record.harness == harness && record.id == id).then_some(record.title)
}

/// The parameters of the register call that rejoins a conversation: its replay (`resume`), which the daemon refuses for
/// a conversation closed, left or blocked. `None` when there is nothing to do: no record, or already joined.
pub fn rejoin_params(
    title: Option<String>,
    already_joined: bool,
    mut register: Value,
) -> Option<Value> {
    let title = title.filter(|_| !already_joined)?;
    register["title"] = Value::String(title);
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
        save(&profile, "a-harness", "s1", "my chat");
        assert_eq!(load(&profile, "a-harness", "s1"), Some("my chat".into()));
        assert_eq!(load(&profile, "a-harness", "s2"), None);
        assert_eq!(load(&profile, "another-harness", "s1"), None);
        save(&profile, "a-harness", "s1", "renamed");
        assert_eq!(load(&profile, "a-harness", "s1"), Some("renamed".into()));
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
    fn a_record_that_names_another_conversation_is_ignored() {
        let home = crate::test_support::temp_dir("rejoin");
        let profile = profile(home.path());
        save(&profile, "a-harness", "s1", "chat");
        std::fs::copy(
            path(&profile, "a-harness", "s1"),
            path(&profile, "a-harness", "other"),
        )
        .unwrap();
        assert_eq!(load(&profile, "a-harness", "other"), None);
    }

    #[test]
    fn rejoin_replays_the_registration_only_for_a_recorded_conversation_not_yet_joined() {
        let register = json!({"client_ref":"s1","harness":"a-harness","thread":"s1","title":"","detachable":false});
        let params = rejoin_params(Some("my chat".into()), false, register.clone()).unwrap();
        assert_eq!(params["resume"], true);
        assert_eq!(params["title"], "my chat");
        assert_eq!(params["thread"], "s1");
        assert_eq!(rejoin_params(None, false, register.clone()), None);
        assert_eq!(rejoin_params(Some("my chat".into()), true, register), None);
    }
}
