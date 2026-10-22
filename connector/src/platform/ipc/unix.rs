//! Unix: a socket file in a private directory. The peer is checked by the kernel's credentials of the connection
//! (`SO_PEERCRED` / `LOCAL_PEERCRED`, through `peer_creds()`); a socket is trusted before it is touched only when it
//! is a socket, this user's, and open to nobody else.

// The tests below open sockets themselves to speak as a peer.
#![allow(clippy::disallowed_types)]

use anyhow::{bail, Context, Result};
use interprocess::local_socket::tokio::{Listener, Stream};
use interprocess::local_socket::traits::StreamCommon as _;
use interprocess::local_socket::{GenericFilePath, ListenerOptions, Name, ToFsName};
use interprocess::os::unix::local_socket::ListenerOptionsExt;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Unix side of [`super::Channel`].
pub(super) struct Os;

impl super::Channel for Os {
    type Addr = PathBuf;

    fn in_dir(dir: &Path, name: &str) -> Self::Addr {
        dir.join(format!("{name}.sock"))
    }

    fn from_record(record: &str) -> io::Result<Self::Addr> {
        let path = PathBuf::from(record);
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not an absolute socket path: {record:?}"),
            ));
        }
        Ok(path)
    }

    #[cfg(test)]
    fn record(endpoint: &Self::Addr) -> String {
        endpoint.to_string_lossy().into_owned()
    }

    fn name(endpoint: &Self::Addr) -> io::Result<Name<'static>> {
        endpoint.clone().to_fs_name::<GenericFilePath>()
    }

    fn listen(endpoint: &Self::Addr) -> io::Result<Listener> {
        // A socket left by a dead process is replaced, once it is known to be ours and private.
        if endpoint.exists() {
            verify_socket(endpoint).map_err(|error| io::Error::other(format!("{error:#}")))?;
            std::fs::remove_file(endpoint)?;
        }
        let listener = ListenerOptions::new()
            .name(Self::name(endpoint)?)
            .mode(0o600)
            .create_tokio()?;
        std::fs::set_permissions(endpoint, std::fs::Permissions::from_mode(0o600))?;
        Ok(listener)
    }

    /// A socket before anything talks to it: in a private directory, a socket, this user's, open to nobody else.
    fn verify_before_connect(endpoint: &Self::Addr) -> io::Result<()> {
        verify_socket(endpoint).map_err(|error| io::Error::other(format!("{error:#}")))
    }

    fn peer_is_this_user(stream: &Stream) -> io::Result<bool> {
        let me = rustix::process::geteuid().as_raw();
        Ok(stream.peer_creds()?.euid() == Some(me))
    }
}

fn verify_socket(path: &Path) -> Result<()> {
    crate::secure_fs::private_dir(path.parent().context("socket parent")?)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket()
        || !crate::platform::user::current().owns_path(path)?
        || metadata.mode() & 0o077 != 0
    {
        bail!("unsafe socket {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::ipc;
    use crate::platform::ipc::Channel as _;
    use crate::secure_fs::tests::Scratch;
    use std::os::unix::net::UnixListener;

    fn chmod(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn a_socket_is_trusted_only_in_a_private_directory_and_open_to_nobody_else() {
        let scratch = Scratch::new("sock");
        let path = scratch.0.join("s.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        chmod(&path, 0o600);
        verify_socket(&path).unwrap();
        chmod(&path, 0o666);
        assert!(verify_socket(&path).is_err());
        chmod(&path, 0o600);
        chmod(&scratch.0, 0o755);
        assert!(verify_socket(&path).is_err());
        chmod(&scratch.0, 0o700);
        let file = scratch.0.join("plain");
        crate::secure_fs::write_private(&file, b"").unwrap();
        assert!(verify_socket(&file).is_err());
    }

    #[tokio::test]
    async fn a_stale_socket_is_replaced_but_a_file_that_is_not_ours_is_not() {
        let scratch = Scratch::new("stale");
        let endpoint = Os::in_dir(&scratch.0, "t");
        // Left behind by a process that died: the file stays, nothing listens.
        drop(UnixListener::bind(&endpoint).unwrap());
        chmod(&endpoint, 0o600);
        assert!(endpoint.exists());
        let listener = Os::listen(&endpoint).unwrap();
        assert!(ipc::connect(&ipc::Endpoint(endpoint.clone())).await.is_ok());
        drop(listener);
        assert!(!endpoint.exists(), "the socket file goes with its listener");
        // Something that is not a socket is never unlinked.
        crate::secure_fs::write_private(&endpoint, b"mine").unwrap();
        assert!(Os::listen(&endpoint).is_err());
        assert_eq!(std::fs::read(&endpoint).unwrap(), b"mine");
    }

    #[tokio::test]
    async fn a_peer_is_this_user_when_its_effective_uid_is_ours() {
        let scratch = Scratch::new("peer");
        let endpoint = ipc::Endpoint::in_dir(&scratch.0, "t");
        let listener = ipc::listen(&endpoint).unwrap();
        let stream = ipc::connect(&endpoint).await.unwrap();
        assert!(Os::peer_is_this_user(&stream).unwrap());
        drop(listener);
    }

    #[tokio::test]
    async fn a_socket_open_to_others_is_refused_before_connecting() {
        let scratch = Scratch::new("open");
        let endpoint = ipc::Endpoint::in_dir(&scratch.0, "t");
        let _listener = ipc::listen(&endpoint).unwrap();
        chmod(&scratch.0.join("t.sock"), 0o666);
        assert!(ipc::connect(&endpoint).await.is_err());
    }
}
