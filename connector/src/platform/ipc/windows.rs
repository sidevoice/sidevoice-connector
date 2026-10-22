//! Windows: a named pipe, `\\.\pipe\sidevoice-<hash of (user name, data dir)>-<name>`. The pipe is created as the
//! first instance (a name already taken is an error) and refusing remote clients, both `interprocess`'s defaults,
//! with a protected descriptor that lets in only its owner (`OW`: the user whose process made it), SYSTEM and
//! Administrators. The peer check is the trait's one body: the pipe's other process belongs to this user.

use super::Listener;
use crate::platform::user::{User, UserId};
use interprocess::local_socket::{GenericFilePath, ListenerOptions, Name, ToFsName};
use interprocess::os::windows::local_socket::ListenerOptionsExt;
use interprocess::os::windows::security_descriptor::SecurityDescriptor;
use sha2::{Digest, Sha256};
use std::fmt;
use std::io;
use std::path::Path;

const PREFIX: &str = r"\\.\pipe\";

/// Full access for the pipe's owner, SYSTEM and Administrators; nobody else, nothing inherited.
const OWNER_ONLY: &str = "D:P(A;;GA;;;OW)(A;;GA;;;SY)(A;;GA;;;BA)";

/// An endpoint as its full pipe name.
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoint(String);

impl super::Channel for Endpoint {
    fn in_dir(dir: &Path, name: &str) -> Endpoint {
        let mut hash = Sha256::new();
        hash.update(UserId::name().unwrap_or_default().to_lowercase().as_bytes());
        hash.update([0]);
        hash.update(dir.to_string_lossy().to_lowercase().as_bytes());
        Endpoint(format!(
            "{PREFIX}sidevoice-{}-{name}",
            hex::encode(&hash.finalize()[..8])
        ))
    }

    fn from_record(record: &str) -> io::Result<Endpoint> {
        if record.len() <= PREFIX.len() || !record.starts_with(PREFIX) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not a local pipe name: {record:?}"),
            ));
        }
        Ok(Endpoint(record.to_owned()))
    }

    fn listen(&self) -> io::Result<Listener> {
        let descriptor = widestring::U16CString::from_str(OWNER_ONLY).map_err(io::Error::other)?;
        ListenerOptions::new()
            .name(self.to_connect()?)
            .security_descriptor(SecurityDescriptor::deserialize(&descriptor)?)
            .create_tokio()
            .map(Listener)
    }

    fn to_connect(&self) -> io::Result<Name<'static>> {
        self.0.clone().to_fs_name::<GenericFilePath>()
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
