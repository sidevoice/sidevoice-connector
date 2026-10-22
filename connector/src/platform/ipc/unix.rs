//! Unix: a socket file in a private directory. The peer is checked by the kernel's credentials of the connection
//! (`SO_PEERCRED` / `LOCAL_PEERCRED`, through `peer_creds()`); a socket is trusted before it is touched only when it
//! is a socket, this user's, and open to nobody else.

use super::{Endpoint, Listener, Stream};
use crate::platform::fs::{Files, Fs};
use crate::platform::user::{User, UserId};
use interprocess::local_socket::traits::StreamCommon as _;
use interprocess::local_socket::{ListenerOptions, Name};
use interprocess::os::unix::local_socket::ListenerOptionsExt;
use std::ffi::OsString;
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;

/// Unix side of [`super::Channel`].
pub struct Os;

impl super::Channel for Os {
    // The socket `dir/<name>.sock`.
    fn address(dir: &Path, name: &str) -> OsString {
        dir.join(format!("{name}.sock")).into_os_string()
    }

    // An absolute socket path.
    fn check(record: &str) -> io::Result<()> {
        if !Path::new(record).is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not an absolute socket path: {record:?}"),
            ));
        }
        Ok(())
    }

    fn listen(endpoint: &Endpoint) -> io::Result<Listener> {
        // A socket left by a dead process is replaced, once it is known to be ours and private.
        if socket(endpoint).exists() {
            verify_socket(socket(endpoint))?;
            std::fs::remove_file(socket(endpoint))?;
        }
        let listener = ListenerOptions::new()
            .name(endpoint.name()?)
            .mode(0o600)
            .create_tokio()?;
        std::fs::set_permissions(socket(endpoint), std::fs::Permissions::from_mode(0o600))?;
        Ok(Listener(listener))
    }
}

/// What [`super::Channel::connect`] dials: the socket, once it is this user's, open to nobody else, in a private
/// directory.
pub(super) fn to_connect(endpoint: &Endpoint) -> io::Result<Name<'static>> {
    verify_socket(socket(endpoint))?;
    endpoint.name()
}

/// Whether the process at the other end of `stream` is this user: the connection's own credentials, the peer's
/// effective uid (`SO_PEERCRED` / `LOCAL_PEERCRED`), which the kernel vouches for (macOS gives no peer pid).
pub(super) fn peer_is_this_user(stream: &Stream) -> io::Result<bool> {
    Ok(stream.peer_creds()?.euid() == Some(rustix::process::geteuid().as_raw()))
}

/// The socket file an endpoint names.
fn socket(endpoint: &Endpoint) -> &Path {
    Path::new(&endpoint.0)
}

/// A socket before anything talks to it or replaces it: in a private directory, a socket, this user's, open to
/// nobody else.
fn verify_socket(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| unsafe_socket(path))?;
    Files::check_private(&Files::open_read(parent)?).map_err(|error| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("unsafe private directory {}: {error:#}", parent.display()),
        )
    })?;
    // A socket cannot be opened: judged by its path.
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket()
        || !UserId::current().owns_path(path)?
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(unsafe_socket(path));
    }
    Ok(())
}

fn unsafe_socket(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("unsafe socket {}", path.display()),
    )
}

// Unix only: file modes and the socket file a dead process leaves behind have no counterpart on Windows, where a
// pipe has no file, no mode, and goes with its last handle.
#[cfg(test)]
mod tests {
    use super::super::Channel as _;
    use super::*;
    use std::path::PathBuf;

    /// A private directory for one test, inside a scratch directory that goes when the guard is dropped.
    fn private_dir(label: &str) -> (tempfile::TempDir, PathBuf) {
        let scratch = crate::test_support::temp_dir(label);
        let dir = scratch.path().join("private");
        Files::create_private_dir(&dir).unwrap();
        (scratch, dir)
    }

    fn chmod(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[tokio::test]
    async fn a_socket_is_trusted_only_in_a_private_directory_and_open_to_nobody_else() {
        let (_scratch, dir) = private_dir("sock");
        let endpoint = Endpoint::new(&dir, "s");
        let _listener = Os::listen(&endpoint).unwrap();
        Os::connect(&endpoint).await.unwrap();
        chmod(socket(&endpoint), 0o666);
        assert!(Os::connect(&endpoint).await.is_err());
        chmod(socket(&endpoint), 0o600);
        chmod(&dir, 0o755);
        assert!(Os::connect(&endpoint).await.is_err());
        chmod(&dir, 0o700);
        let file = Endpoint::new(&dir, "plain");
        Files::write_private(socket(&file), b"").unwrap();
        assert!(Os::connect(&file).await.is_err());
    }

    #[tokio::test]
    async fn a_stale_socket_is_replaced_but_a_file_that_is_not_one_is_not() {
        let (_scratch, dir) = private_dir("stale");
        let endpoint = Endpoint::new(&dir, "t");
        // Left behind by a process that died: the file stays, nothing listens. (A listener whose file is not
        // reclaimed on drop leaves exactly that.)
        let mut dead = Os::listen(&endpoint).unwrap();
        interprocess::local_socket::traits::tokio::Listener::do_not_reclaim_name_on_drop(
            &mut dead.0,
        );
        drop(dead);
        assert!(socket(&endpoint).exists());
        let listener = Os::listen(&endpoint).unwrap();
        Os::connect(&endpoint).await.unwrap();
        drop(listener);
        assert!(
            !socket(&endpoint).exists(),
            "the socket file goes with its listener"
        );
        // Something that is not a socket is never unlinked.
        Files::write_private(socket(&endpoint), b"mine").unwrap();
        assert!(Os::listen(&endpoint).is_err());
        assert_eq!(std::fs::read(socket(&endpoint)).unwrap(), b"mine");
    }
}
