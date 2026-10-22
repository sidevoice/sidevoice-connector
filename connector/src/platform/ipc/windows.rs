//! Windows: a named pipe, `\\.\pipe\sidevoice-<hash of (user name, data dir)>-<name>`. The pipe is created as the
//! first instance (a name already taken is an error) and refusing remote clients, both `interprocess`'s defaults,
//! with a protected descriptor that lets in only its owner (`OW`: the user whose process made it), SYSTEM and
//! Administrators. The peer check: the process at the other end of the pipe belongs to this user.

use super::{Endpoint, Listener, Stream};
use crate::platform::process::{Process, Processes};
use crate::platform::user::{User, UserId};
use interprocess::local_socket::traits::StreamCommon as _;
use interprocess::local_socket::{ListenerOptions, Name};
use interprocess::os::windows::local_socket::ListenerOptionsExt;
use interprocess::os::windows::security_descriptor::SecurityDescriptor;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::io;
use std::path::Path;

const PREFIX: &str = r"\\.\pipe\";

/// Full access for the pipe's owner, SYSTEM and Administrators; nobody else, nothing inherited.
const OWNER_ONLY: &str = "D:P(A;;GA;;;OW)(A;;GA;;;SY)(A;;GA;;;BA)";

/// Windows side of [`super::Channel`].
pub struct Os;

impl super::Channel for Os {
    // The pipe `\\.\pipe\sidevoice-<hash of (user, dir)>-<name>`: two profiles or two users never meet on one name.
    fn address(dir: &Path, name: &str) -> OsString {
        let mut hash = Sha256::new();
        hash.update(UserId::name().unwrap_or_default().to_lowercase().as_bytes());
        hash.update([0]);
        hash.update(dir.to_string_lossy().to_lowercase().as_bytes());
        format!(
            "{PREFIX}sidevoice-{}-{name}",
            hex::encode(&hash.finalize()[..8])
        )
        .into()
    }

    // A full local pipe name, `\\.\pipe\...`.
    fn check(record: &str) -> io::Result<()> {
        if record.len() <= PREFIX.len() || !record.starts_with(PREFIX) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not a local pipe name: {record:?}"),
            ));
        }
        Ok(())
    }

    fn listen(endpoint: &Endpoint) -> io::Result<Listener> {
        let descriptor = widestring::U16CString::from_str(OWNER_ONLY).map_err(io::Error::other)?;
        ListenerOptions::new()
            .name(endpoint.name()?)
            .security_descriptor(SecurityDescriptor::deserialize(&descriptor)?)
            .create_tokio()
            .map(Listener)
    }
}

/// What [`super::Channel::connect`] dials: the pipe by its name; the server is checked once connected.
pub(super) fn to_connect(endpoint: &Endpoint) -> io::Result<Name<'static>> {
    endpoint.name()
}

/// Whether the process at the other end of `stream` is this user's: the pipe's client or server pid, as
/// `interprocess` reports it, is a live process of this user's (`platform::process`, through `sysinfo`).
pub(super) fn peer_is_this_user(stream: &Stream) -> io::Result<bool> {
    let pid = stream
        .peer_creds()?
        .pid()
        .ok_or_else(|| io::Error::other("the peer process is unknown"))?;
    Ok(Processes::is_process(pid, |_| true))
}
