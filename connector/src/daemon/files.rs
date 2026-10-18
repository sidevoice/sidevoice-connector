//! The daemon's own files: the conversation state, its guard, and the outbox.

use crate::secure_fs::{private_file, read_trusted};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

pub(super) const CONVERSATION_STATE_MAX_BYTES: usize = 1 << 20;
pub(super) const SPEECH_IDENTITIES_MAX_BYTES: usize = 2 << 20;
const CONVERSATION_GUARD_MAGIC: &[u8; 8] = b"SVGUARD1";
const SPEECH_IDENTITIES_GUARD_MAGIC: &[u8; 9] = b"SVSPEECH1";
pub(super) const GUARD_CLEAR: u8 = 0;
pub(super) const GUARD_BLOCK_RESUME: u8 = 1;
pub(super) const GUARD_REVOKED: u8 = 2;

pub(super) fn prepare_conversation_guard(path: &Path) -> Result<(u8, fs::File)> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            private_file(path)?;
            let metadata = fs::metadata(path)?;
            if metadata.len() != 16 || metadata.mode() & 0o200 == 0 {
                bail!("invalid conversation state guard");
            }
            let mut file = fs::OpenOptions::new().read(true).write(true).open(path)?;
            let mut bytes = [0; 16];
            file.read_exact(&mut bytes)?;
            if &bytes[..8] != CONVERSATION_GUARD_MAGIC || bytes[9..].iter().any(|byte| *byte != 0) {
                bail!("invalid conversation state guard");
            }
            if !matches!(bytes[8], GUARD_CLEAR | GUARD_BLOCK_RESUME | GUARD_REVOKED) {
                bail!("invalid conversation state guard");
            }
            Ok((bytes[8], file))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)?;
            let mut bytes = [0; 16];
            bytes[..8].copy_from_slice(CONVERSATION_GUARD_MAGIC);
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::File::open(path.parent().context("conversation guard parent")?)?.sync_all()?;
            file.seek(SeekFrom::Start(0))?;
            Ok((GUARD_CLEAR, file))
        }
        Err(error) => Err(error.into()),
    }
}

pub(super) fn write_conversation_guard(file: &mut fs::File, state: u8) -> Result<()> {
    file.seek(SeekFrom::Start(8))?;
    file.write_all(&[state])?;
    file.sync_all()?;
    Ok(())
}

/// The speech waiting for the room, as this connector writes it: every row a speech of a conversation (its event,
/// binding, conversation reference, utterance and text). A row of any other shape makes the outbox invalid.
pub(super) fn read_outbox(path: &Path) -> Result<Vec<Value>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    private_file(path)?;
    let bytes = fs::read(path)?;
    if bytes.len() > 8 << 20 {
        bail!("outbox too large");
    }
    let rows = serde_json::from_slice::<Vec<Value>>(&bytes).context("invalid existing outbox")?;
    if !rows.iter().all(|row| {
        [
            "event_id",
            "binding_id",
            "client_ref",
            "utterance_id",
            "text",
        ]
        .iter()
        .all(|key| row.get(key).is_some_and(Value::is_string))
    }) {
        bail!("invalid existing outbox: a row is not a speech of a conversation");
    }
    Ok(rows)
}

/// The durable v1 ID fence contains hashes and IDs, never original reply text.
pub(super) fn read_speech_identities(path: &Path) -> Result<Vec<Value>> {
    let bytes = read_trusted(path, SPEECH_IDENTITIES_MAX_BYTES as u64)?
        .context("speech identity ledger missing after initialization")?;
    let rows =
        serde_json::from_slice::<Vec<Value>>(&bytes).context("invalid speech identity ledger")?;
    let mut calls = std::collections::HashSet::new();
    let mut originals = std::collections::HashSet::new();
    if rows.len() > 8192
        || !rows.iter().all(|row| {
            let string = |key: &str| {
                row.get(key)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
            };
            let Some(kind @ ("say" | "replay")) = string("kind") else {
                return false;
            };
            let (
                Some(client_ref),
                Some(call_id),
                Some(utterance_id),
                Some(instance),
                Some(fingerprint),
            ) = (
                string("client_ref"),
                string("call_id"),
                string("utterance_id"),
                string("voice_contract_instance"),
                string("fingerprint"),
            )
            else {
                return false;
            };
            if fingerprint.len() != 64
                || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
                || !calls.insert((client_ref, kind, call_id))
                || row.get("retired").is_some_and(|value| !value.is_boolean())
            {
                return false;
            }
            if kind == "say" {
                if string("binding_id").is_none() || !originals.insert((client_ref, utterance_id)) {
                    return false;
                }
            } else {
                let Some(request) = row.get("request").filter(|value| value.is_object()) else {
                    return false;
                };
                let request_string = |key: &str| {
                    request
                        .get(key)
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                };
                if request.get("voice_contract") != Some(&serde_json::json!(1))
                    || request_string("binding_id").is_none()
                    || string("binding_id")
                        .is_some_and(|binding| request_string("binding_id") != Some(binding))
                    || request_string("voice_contract_instance") != Some(instance)
                    || request_string("utterance_id") != Some(utterance_id)
                    || request_string("replay_id") != Some(call_id)
                    || !matches!(request_string("mode"), Some("resume" | "restart"))
                    || request_string("session_id").is_none()
                    || request.get("revision").and_then(Value::as_u64).is_none()
                    || request.get("epoch").and_then(Value::as_u64).is_none()
                    || request_string("context_message_id").is_none()
                {
                    return false;
                }
            }
            if let Some(result) = row.get("result") {
                if !result.is_object()
                    || result.get("voice_contract") != Some(&serde_json::json!(1))
                    || result
                        .get("status")
                        .and_then(Value::as_str)
                        .is_none_or(str::is_empty)
                    || result.get("utterance_id").and_then(Value::as_str) != Some(utterance_id)
                    || result
                        .get(if kind == "say" {
                            "event_id"
                        } else {
                            "replay_id"
                        })
                        .and_then(Value::as_str)
                        != Some(call_id)
                    || ["attempt_id", "playback", "reason"]
                        .iter()
                        .any(|key| result.get(*key).is_some_and(|value| !value.is_string()))
                {
                    return false;
                }
            }
            true
        })
    {
        bail!("invalid speech identity ledger");
    }
    Ok(rows)
}

/// A second durable file distinguishes a pristine installation from isolated loss of the
/// ID ledger. Neither accepted speech nor replay can start before both files are synced.
pub(super) fn prepare_speech_identities(path: &Path) -> Result<Vec<Value>> {
    let guard = path.with_extension("guard");
    let marker = read_trusted(&guard, SPEECH_IDENTITIES_GUARD_MAGIC.len() as u64)?;
    if marker
        .as_deref()
        .is_some_and(|bytes| bytes != SPEECH_IDENTITIES_GUARD_MAGIC)
    {
        bail!("invalid speech identity guard");
    }
    let ledger_exists = match fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    if marker.is_some() && !ledger_exists {
        bail!("speech identity ledger missing after initialization");
    }
    if !ledger_exists {
        create_private_once(path, b"[]")?;
    }
    let rows = read_speech_identities(path)?;
    if marker.is_none() {
        create_private_once(&guard, SPEECH_IDENTITIES_GUARD_MAGIC)?;
        if read_trusted(&guard, SPEECH_IDENTITIES_GUARD_MAGIC.len() as u64)?.as_deref()
            != Some(SPEECH_IDENTITIES_GUARD_MAGIC.as_slice())
        {
            bail!("invalid speech identity guard");
        }
    }
    Ok(rows)
}

fn create_private_once(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::File::open(path.parent().context("speech identity parent")?)?.sync_all()?;
    Ok(())
}

/// The conversation state as this connector writes it: never above its size bound, which a larger file makes invalid.
pub(super) fn read_conversation_state(path: &Path) -> Result<Option<Vec<u8>>> {
    if !path.exists() {
        return Ok(None);
    }
    private_file(path)?;
    if fs::metadata(path)?.len() > CONVERSATION_STATE_MAX_BYTES as u64 {
        bail!("conversation state too large");
    }
    Ok(Some(fs::read(path)?))
}

#[cfg(test)]
mod tests {
    #[test]
    fn speech_identity_guard_distinguishes_first_start_from_lost_ledger() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("sidevoice-ledger-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let ledger = dir.join("speech-identities.json");
        let guard = dir.join("speech-identities.guard");
        assert!(super::prepare_speech_identities(&ledger)
            .unwrap()
            .is_empty());
        assert_eq!(std::fs::read(&ledger).unwrap(), b"[]");
        assert_eq!(
            std::fs::read(&guard).unwrap(),
            super::SPEECH_IDENTITIES_GUARD_MAGIC
        );
        std::fs::remove_file(&ledger).unwrap();
        assert!(super::prepare_speech_identities(&ledger)
            .unwrap_err()
            .to_string()
            .contains("missing after initialization"));
        assert!(!ledger.exists(), "a lost ledger must never be recreated");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn speech_identity_ledger_refuses_unknown_or_incomplete_rows() {
        use serde_json::json;
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("sidevoice-identity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("speech-identities.json");
        let write = |rows: &[serde_json::Value]| {
            std::fs::write(&path, serde_json::to_vec(rows).unwrap()).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        };
        let say = json!({"kind":"say","client_ref":"client","call_id":"event",
            "utterance_id":"original","voice_contract_instance":"instance",
            "fingerprint":"a".repeat(64),"binding_id":"original-binding",
            "result":{"voice_contract":1,"status":"accepted","event_id":"event",
                "utterance_id":"original","playback":"queued","attempt_id":"attempt"},
            "future_field":{"ignored":true}});
        let replay = json!({"kind":"replay","client_ref":"client","call_id":"repeat",
            "utterance_id":"original","voice_contract_instance":"instance",
            "fingerprint":"b".repeat(64),
            "request":{"voice_contract":1,"binding_id":"original-binding",
                "voice_contract_instance":"instance","utterance_id":"original",
                "replay_id":"repeat","mode":"resume","session_id":"session",
                "revision":1,"epoch":0,"context_message_id":"message"}});
        write(&[say.clone(), replay.clone()]);
        assert_eq!(super::read_speech_identities(&path).unwrap().len(), 2);

        let mut unknown = say.clone();
        unknown["kind"] = json!("future-kind");
        let mut no_binding = say.clone();
        no_binding.as_object_mut().unwrap().remove("binding_id");
        let mut no_request = replay.clone();
        no_request.as_object_mut().unwrap().remove("request");
        let mut mismatched_request = replay.clone();
        mismatched_request["request"]["replay_id"] = json!("other");
        let mut corrupt_result = say.clone();
        corrupt_result["result"]["event_id"] = json!("other");
        for rows in [
            vec![unknown],
            vec![no_binding],
            vec![no_request],
            vec![mismatched_request],
            vec![corrupt_result],
            vec![say.clone(), say.clone()],
        ] {
            write(&rows);
            let original = std::fs::read(&path).unwrap();
            assert!(super::read_speech_identities(&path)
                .unwrap_err()
                .to_string()
                .contains("invalid speech identity ledger"));
            assert_eq!(std::fs::read(&path).unwrap(), original);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_outbox_or_a_state_of_another_shape_is_invalid() {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("sidevoice-daemon-state-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            path
        };
        let speech = serde_json::json!({"event_id":"e","binding_id":"b","client_ref":"c","session_id":"s",
            "revision":1,"utterance_id":"u","text":"hola","language":null});
        let outbox = write(
            "outbox.json",
            serde_json::to_vec(std::slice::from_ref(&speech))
                .unwrap()
                .as_slice(),
        );
        assert_eq!(super::read_outbox(&outbox).unwrap(), vec![speech.clone()]);
        let mut copied = speech;
        copied.as_object_mut().unwrap().remove("client_ref");
        let outbox = write(
            "outbox.json",
            serde_json::to_vec(&[copied]).unwrap().as_slice(),
        );
        let error = super::read_outbox(&outbox).unwrap_err();
        assert!(
            error.to_string().contains("not a speech of a conversation"),
            "{error}"
        );
        assert!(super::read_outbox(&dir.join("absent.json"))
            .unwrap()
            .is_empty());

        let state = write(
            "conversation-state.json",
            br#"{"refused":null,"closed_by_room":{}}"#,
        );
        assert!(super::read_conversation_state(&state).unwrap().is_some());
        let oversized = write(
            "conversation-state.json",
            &vec![b' '; super::CONVERSATION_STATE_MAX_BYTES + 1],
        );
        assert!(super::read_conversation_state(&oversized)
            .unwrap_err()
            .to_string()
            .contains("too large"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
