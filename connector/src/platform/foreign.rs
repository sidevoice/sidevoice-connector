//! Endpoints another program owns and names: the inbox an agent's session binds, an agent daemon's control socket,
//! an editor's bridge. They are not ours (decision "Exceptions may exist for integrations with
//! third-party sockets/IPC", 2026-10-21): the peer is the agent, whoever made it, and what authorises a message is
//! the token the agent gave, so unlike [`super::ipc`] nothing is checked about who is at the other end. The
//! transport is the same one (`interprocess`: a Unix socket on macOS and Linux, a named pipe on Windows), so adapters
//! stay free of both.
//!
//! The interface is the [`Foreign`] trait; [`Foreigners`] is the one OS implementation compiled in. A route is the
//! text the agent published for its endpoint (an environment variable, a record in its own home). An OS reaches only
//! the routes it can name: a path on Unix, a local pipe name (`\\.\pipe\...`) on Windows. Anything else is refused
//! as `Unsupported`, never reinterpreted: how an agent names its endpoint on an OS is the agent's to document.
//! Callers import both: `use crate::platform::foreign::{Foreign, Foreigners};` then `Foreigners::connect(route).await`.

#[cfg(unix)]
#[path = "foreign/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "foreign/windows.rs"]
mod imp;

pub use imp::Os as Foreigners;

use interprocess::local_socket::traits::tokio::Stream as _;
use interprocess::local_socket::{GenericFilePath, ToFsName};
use std::future::Future;
use std::io;

/// One connection to a third party's endpoint. `AsyncRead + AsyncWrite + Unpin`.
pub type ForeignStream = interprocess::local_socket::tokio::Stream;

/// What every OS implements ([`Foreigners`], in `foreign/unix.rs` and `foreign/windows.rs`): which routes it can
/// reach. Connecting is one body for every OS.
pub trait Foreign {
    /// Whether this OS can reach `route`: `InvalidInput` for a route that is no endpoint name at all, `Unsupported`
    /// for one that names an endpoint kind this OS has no way to open.
    fn check(route: &str) -> io::Result<()>;

    /// Connects to the endpoint `route` names. No timeout: callers bound it.
    fn connect(route: &str) -> impl Future<Output = io::Result<ForeignStream>> + Send {
        let name =
            Self::check(route).and_then(|()| route.to_owned().to_fs_name::<GenericFilePath>());
        async move { ForeignStream::connect(name?).await }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::ipc::{Address, Channel, Channels, Endpoint};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A scratch directory open to this user alone (a Unix socket is only made in one).
    fn scratch() -> tempfile::TempDir {
        let dir = tempfile::Builder::new().prefix("sv-for").tempdir().unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        dir
    }

    #[tokio::test]
    async fn a_route_an_other_program_published_is_connected_to_and_bytes_flow_both_ways() {
        let dir = scratch();
        // The route as this OS publishes an endpoint: what the other program's variable or record would hold.
        let route = Endpoint::new(dir.path(), "theirs").to_string();
        let listener = Channels::listen(&route.parse().unwrap()).unwrap();
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            let mut line = [0u8; 5];
            stream.read_exact(&mut line).await.unwrap();
            stream.write_all(&line).await.unwrap();
        });
        assert!(Foreigners::check(&route).is_ok());
        let mut client = Foreigners::connect(&route).await.unwrap();
        client.write_all(b"hello").await.unwrap();
        let mut echoed = [0u8; 5];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"hello");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_route_nobody_listens_at_is_an_error_not_a_hang() {
        let dir = scratch();
        let route = Endpoint::new(dir.path(), "nobody").to_string();
        assert!(Foreigners::connect(&route).await.is_err());
    }

    #[tokio::test]
    async fn what_is_no_route_this_os_can_name_is_refused_before_anything_is_opened() {
        for route in ["", "relative/theirs", "theirs", "uds:/tmp/theirs"] {
            let error = Foreigners::check(route).unwrap_err();
            assert!(
                matches!(
                    error.kind(),
                    io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported
                ),
                "{route:?}: {error}"
            );
            assert!(Foreigners::connect(route).await.is_err(), "{route:?}");
        }
    }
}
