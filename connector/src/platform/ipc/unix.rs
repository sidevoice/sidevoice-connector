//! Unix: a socket file in a private directory. The peer is checked by the kernel's credentials of the connection
//! (`SO_PEERCRED` / `LOCAL_PEERCRED`, through `peer_creds()`); a socket left behind is replaced only when it
//! is a socket, this user's, open to nobody else, in a private directory.

use super::{Listener, Stream};
use crate::platform::fs::{Files, Fs};
use crate::platform::user::{User, UserId};
use interprocess::local_socket::traits::StreamCommon as _;
use interprocess::local_socket::{GenericFilePath, ListenerOptions, Name, ToFsName};
use std::fmt;
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Unix side of [`super::Address`]: an endpoint is its socket path.
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoint(PathBuf);

impl super::Address for Endpoint {
    fn new(dir: &Path, name: &str) -> Endpoint {
        Endpoint(dir.join(format!("{name}.sock")))
    }
}

impl FromStr for Endpoint {
    type Err = io::Error;

    fn from_str(record: &str) -> io::Result<Endpoint> {
        let path = PathBuf::from(record);
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not an absolute socket path: {record:?}"),
            ));
        }
        Ok(Endpoint(path))
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display().fmt(f)
    }
}

impl Endpoint {
    /// The endpoint as `interprocess` names it.
    pub(super) fn name(&self) -> io::Result<Name<'static>> {
        self.0.clone().to_fs_name::<GenericFilePath>()
    }
}

/// Unix side of [`super::Channel`].
pub struct Os;

impl super::Channel for Os {
    fn listen(endpoint: &Endpoint) -> io::Result<Listener> {
        // Only in a private directory: nobody else can reach the socket between its creation and its `chmod` below.
        // (Choosing the mode before `bind`, `interprocess`'s `mode`, is `Unsupported` on macOS, whose `fchmod`
        // refuses sockets.)
        verify_dir(socket(endpoint))?;
        // A socket left by a dead process is replaced, once it is known to be ours and private.
        if socket(endpoint).exists() {
            verify_socket(socket(endpoint))?;
            std::fs::remove_file(socket(endpoint))?;
        }
        let listener = ListenerOptions::new()
            .name(endpoint.name()?)
            .create_tokio()?;
        std::fs::set_permissions(socket(endpoint), std::fs::Permissions::from_mode(0o600))?;
        Ok(Listener(listener))
    }
}

/// Whether the process at the other end of `stream` is this user: the connection's own credentials, the peer's
/// effective uid (`SO_PEERCRED` / `LOCAL_PEERCRED`), which the kernel vouches for (macOS gives no peer pid).
pub(super) fn peer_is_this_user(stream: &Stream) -> io::Result<bool> {
    Ok(stream.peer_creds()?.euid() == Some(rustix::process::geteuid().as_raw()))
}

/// The socket file an endpoint names.
fn socket(endpoint: &Endpoint) -> &Path {
    &endpoint.0
}

/// A socket before anything talks to it or replaces it: in a private directory ([`verify_dir`]), a socket, this
/// user's, open to nobody else.
fn verify_socket(path: &Path) -> io::Result<()> {
    verify_dir(path)?;
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

/// The directory the socket `path` is in: private, this user's and open to nobody else.
fn verify_dir(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| unsafe_socket(path))?;
    Files::check_private(&Files::open_read(parent)?).map_err(|error| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("unsafe private directory {}: {error:#}", parent.display()),
        )
    })
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
    use super::super::{Address as _, Channel as _};
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
    async fn a_stale_socket_is_replaced_only_while_it_is_private() {
        let (_scratch, dir) = private_dir("sock");
        let endpoint = Endpoint::new(&dir, "s");
        // Left behind by a process that died, then opened to others: not ours to trust, so not replaced.
        let mut dead = Os::listen(&endpoint).unwrap();
        interprocess::local_socket::traits::tokio::Listener::do_not_reclaim_name_on_drop(
            &mut dead.0,
        );
        drop(dead);
        chmod(socket(&endpoint), 0o666);
        assert!(Os::listen(&endpoint).is_err());
        chmod(socket(&endpoint), 0o600);
        let _listener = Os::listen(&endpoint).unwrap();
        Os::connect(&endpoint).await.unwrap();
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

    #[tokio::test]
    async fn a_listener_is_made_only_in_a_private_directory_and_its_socket_is_open_to_nobody_else()
    {
        let (_scratch, dir) = private_dir("listen");
        let endpoint = Endpoint::new(&dir, "l");
        let listener = Os::listen(&endpoint).unwrap();
        let mode = std::fs::symlink_metadata(socket(&endpoint))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        drop(listener);
        chmod(&dir, 0o755);
        assert!(Os::listen(&endpoint).is_err());
        assert!(!socket(&endpoint).exists(), "nothing is created there");
        chmod(&dir, 0o700);
    }
}
