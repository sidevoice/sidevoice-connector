//! Windows: a named pipe, `\\.\pipe\sidevoice-<hash of (user SID, data dir)>-<name>`. The pipe is created with a
//! security descriptor for this user, SYSTEM and Administrators only, as the first instance (a name already taken
//! is an error) and refusing remote clients (all three are `interprocess`'s defaults or the descriptor given
//! here). The peer is the process at the other end of the pipe, whose token's user must be this user.

use interprocess::local_socket::tokio::{Listener, Stream};
use interprocess::local_socket::traits::StreamCommon as _;
use interprocess::local_socket::{GenericFilePath, ListenerOptions, Name, ToFsName};
use interprocess::os::windows::local_socket::ListenerOptionsExt;
use interprocess::os::windows::security_descriptor::SecurityDescriptor;
use sha2::{Digest, Sha256};
use std::io;
use std::path::Path;

const PREFIX: &str = r"\\.\pipe\";

/// Windows side of [`super::Channel`].
pub(super) struct Os;

impl super::Channel for Os {
    type Addr = String;

    fn in_dir(dir: &Path, name: &str) -> Self::Addr {
        let user = crate::platform::user::current().sid_string();
        let mut hash = Sha256::new();
        hash.update(user.as_bytes());
        hash.update([0]);
        hash.update(dir.to_string_lossy().to_lowercase().as_bytes());
        format!(
            "{PREFIX}sidevoice-{}-{name}",
            hex::encode(&hash.finalize()[..8])
        )
    }

    fn from_record(record: &str) -> io::Result<Self::Addr> {
        if record.len() <= PREFIX.len() || !record.starts_with(PREFIX) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not a local pipe name: {record:?}"),
            ));
        }
        Ok(record.to_owned())
    }

    #[cfg(test)]
    fn record(endpoint: &Self::Addr) -> String {
        endpoint.clone()
    }

    fn name(endpoint: &Self::Addr) -> io::Result<Name<'static>> {
        endpoint.clone().to_fs_name::<GenericFilePath>()
    }

    fn listen(endpoint: &Self::Addr) -> io::Result<Listener> {
        // Protected DACL: full access for this user, SYSTEM (SY) and Administrators (BA), nobody else.
        let sddl = format!(
            "D:P(A;;GA;;;{})(A;;GA;;;SY)(A;;GA;;;BA)",
            crate::platform::user::current().sid_string()
        );
        let wide = widestring::U16CString::from_str(sddl).map_err(io::Error::other)?;
        ListenerOptions::new()
            .name(Self::name(endpoint)?)
            .security_descriptor(SecurityDescriptor::deserialize(&wide)?)
            .create_tokio()
    }

    /// Nothing to look at before connecting: the server is checked once connected.
    fn verify_before_connect(_endpoint: &Self::Addr) -> io::Result<()> {
        Ok(())
    }

    fn peer_is_this_user(stream: &Stream) -> io::Result<bool> {
        let pid = stream
            .peer_creds()?
            .pid()
            .ok_or_else(|| io::Error::other("the pipe's peer process is unknown"))?;
        Ok(crate::platform::user::of_process(pid)? == crate::platform::user::current())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::ipc;
    use crate::platform::ipc::Channel as _;

    #[test]
    fn a_pipe_is_named_after_the_user_and_the_directory() {
        let a = Os::in_dir(Path::new(r"C:\Users\a\.sidevoice"), "connector");
        assert!(a.starts_with(r"\\.\pipe\sidevoice-") && a.ends_with("-connector"));
        assert_eq!(
            a,
            Os::in_dir(Path::new(r"c:\users\a\.sidevoice"), "connector")
        );
        assert_ne!(
            a,
            Os::in_dir(Path::new(r"C:\Users\b\.sidevoice"), "connector")
        );
        assert_eq!(Os::from_record(&a).unwrap(), a);
        assert!(Os::from_record(r"C:\somewhere").is_err());
    }

    #[tokio::test]
    async fn the_pipes_peer_is_this_user() {
        let endpoint = ipc::Endpoint::in_dir(Path::new(r"C:\sv-ipc-peer"), "t");
        let listener = ipc::listen(&endpoint).unwrap();
        let client = ipc::connect(&endpoint).await.unwrap();
        let served = listener.accept().await.unwrap();
        assert!(Os::peer_is_this_user(&client).unwrap());
        assert!(Os::peer_is_this_user(&served).unwrap());
    }
}
