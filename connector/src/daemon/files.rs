//! The daemon's own files: the conversation state, its guard, and the outbox.

use crate::platform::fs::{Create, Files, Fs};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

pub(super) const CONVERSATION_STATE_MAX_BYTES: usize = 1 << 20;
const CONVERSATION_GUARD_MAGIC: &[u8; 8] = b"SVGUARD1";
pub(super) const GUARD_CLEAR: u8 = 0;
pub(super) const GUARD_BLOCK_RESUME: u8 = 1;

pub(super) fn prepare_conversation_guard(path: &Path) -> Result<(u8, fs::File)> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            // Judged and used on one handle, opened without following a link.
            let mut file = Files::open_private(path, Create::ReadWrite)?;
            Files::check_private(&file)
                .with_context(|| format!("unsafe private file {}", path.display()))?;
            let metadata = file.metadata()?;
            if !metadata.is_file() || metadata.len() != 16 || metadata.permissions().readonly() {
                bail!("invalid conversation state guard");
            }
            let mut bytes = [0; 16];
            file.read_exact(&mut bytes)?;
            if &bytes[..8] != CONVERSATION_GUARD_MAGIC || bytes[9..].iter().any(|byte| *byte != 0) {
                bail!("invalid conversation state guard");
            }
            if !matches!(bytes[8], GUARD_CLEAR | GUARD_BLOCK_RESUME) {
                bail!("invalid conversation state guard");
            }
            Ok((bytes[8], file))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut file = Files::open_private(path, Create::New)?;
            let mut bytes = [0; 16];
            bytes[..8].copy_from_slice(CONVERSATION_GUARD_MAGIC);
            file.write_all(&bytes)?;
            file.sync_all()?;
            Files::sync_dir(path.parent().context("conversation guard parent")?)?;
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

/// The speech waiting for the core, as this connector writes it: every row a speech of a conversation (its event,
/// binding, conversation reference, utterance and text). A row of any other shape makes the outbox invalid.
pub(super) fn read_outbox(path: &Path) -> Result<Vec<Value>> {
    let Some(bytes) = Files::read_private(path, 8 << 20)? else {
        return Ok(Vec::new());
    };
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

/// The conversation state as this connector writes it: never above its size bound, which a larger file makes invalid.
pub(super) fn read_conversation_state(path: &Path) -> Result<Option<Vec<u8>>> {
    Files::read_private(path, CONVERSATION_STATE_MAX_BYTES as u64)
}

#[cfg(test)]
mod tests {
    use crate::platform::fs::{Files, TestFiles};

    #[test]
    fn an_outbox_or_a_state_of_another_shape_is_invalid() {
        let dir =
            std::env::temp_dir().join(format!("sidevoice-daemon-state-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            Files::shut_others_out(&path);
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

        let state = write("conversation-state.json", br#"{"closed_by_client":{}}"#);
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
