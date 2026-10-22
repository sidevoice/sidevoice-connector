//! Endpoints another program owns and names: the inbox an agent's session binds, an agent daemon's control socket,
//! an editor's bridge. They are not ours (decision "Exceptions may exist for integrations with
//! third-party sockets/IPC", 2026-10-21): their name, their place and their protocol are the agent's, so unlike
//! [`super::ipc`] nothing here makes, names or secures one. What is checked is who serves it: a connection is handed
//! out only once the process at the other end is this user's, because what callers write first is the agent's token
//! and a message, and a token proves the client to the server, never the server to the client. Another user squatting
//! the name (a pipe made first, a socket planted where the location allows) gets a refused connection and nothing
//! written. The transport is the local channel's (`interprocess`: a Unix socket on macOS and Linux, a named pipe on
//! Windows), so adapters stay free of both.
//!
//! The interface is the [`Foreign`] trait; [`Foreigners`] is the one OS implementation compiled in. A route is the
//! text the agent published for its endpoint (an environment variable, a record in its own home), kept as the OS
//! gives it. An OS reaches only the routes it can name: an absolute path on Unix, a pipe on this computer
//! (`\\.\pipe\<name>`) on Windows. Anything else is refused, never reinterpreted: how an agent names its endpoint on an
//! OS is the agent's to document. Callers import both: `use crate::platform::foreign::{Foreign, Foreigners};` then
//! `Foreigners::connect(route).await`.

#[cfg(unix)]
#[path = "foreign/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "foreign/windows.rs"]
mod imp;

pub use imp::Os as Foreigners;

use interprocess::local_socket::traits::tokio::Stream as _;
use interprocess::local_socket::{GenericFilePath, ToFsName};
use std::ffi::OsStr;
use std::future::Future;
use std::io;
use std::path::Path;

/// One connection to a third party's endpoint, served by this user. `AsyncRead + AsyncWrite + Unpin`.
pub type ForeignStream = interprocess::local_socket::tokio::Stream;

/// What every OS implements ([`Foreigners`], in `foreign/unix.rs` and `foreign/windows.rs`): which routes it can
/// reach, and who serves a connected one. Connecting is one body for every OS.
pub trait Foreign {
    /// Whether this OS can reach `route`: `InvalidInput` for a route that is no endpoint name at all, `Unsupported`
    /// for one that names an endpoint kind this OS has no way to open. Windows judges the name with the same
    /// conversion [`Foreign::connect`] uses (`interprocess`'s), so what passes here is what it can open.
    fn check(route: &OsStr) -> io::Result<()>;

    /// Whether the endpoint `route` names, connected as `stream`, is served by this user. Unix: the file at `route`,
    /// judged itself (no link followed), is a socket this user owns, and the connection's peer credentials
    /// (`SO_PEERCRED` / `LOCAL_PEERCRED`) are this user's effective uid. Windows: the pipe's server process
    /// (`GetNamedPipeServerProcessId`) is a live process of this user's (its token's SID).
    fn served_by_this_user(route: &OsStr, stream: &ForeignStream) -> io::Result<bool>;

    /// Connects to the endpoint `route` names and hands the connection out only once it is served by this user
    /// ([`Foreign::served_by_this_user`]); otherwise it is dropped unwritten and the error is `PermissionDenied`.
    /// No timeout: callers bound it.
    fn connect(route: impl AsRef<OsStr>) -> impl Future<Output = io::Result<ForeignStream>> + Send {
        let route = route.as_ref().to_os_string();
        let name = Self::check(&route).and_then(|()| route.clone().to_fs_name::<GenericFilePath>());
        async move {
            let stream = ForeignStream::connect(name?).await?;
            trusted(&route, Self::served_by_this_user(&route, &stream))?;
            Ok(stream)
        }
    }
}

/// The decision over what [`Foreign::served_by_this_user`] found: only a server known to be this user's is talked
/// to; one that is not, or whose owner cannot be told, is refused.
fn trusted(route: &OsStr, served_by_this_user: io::Result<bool>) -> io::Result<()> {
    let route = Path::new(route).display();
    match served_by_this_user {
        Ok(true) => Ok(()),
        Ok(false) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("the endpoint {route} is not served by this user: nothing was sent to it"),
        )),
        Err(error) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "who serves the endpoint {route} cannot be told ({error}): nothing was sent to it"
            ),
        )),
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
    async fn a_route_this_user_serves_is_connected_to_and_bytes_flow_both_ways() {
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
        assert!(Foreigners::check(OsStr::new(&route)).is_ok());
        let mut client = Foreigners::connect(&route).await.unwrap();
        assert!(Foreigners::served_by_this_user(OsStr::new(&route), &client).unwrap());
        client.write_all(b"hello").await.unwrap();
        let mut echoed = [0u8; 5];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"hello");
        server.await.unwrap();
    }

    /// Another user's server cannot be made without a second account, so the decision is checked on its own.
    #[test]
    fn only_a_server_known_to_be_this_users_is_talked_to() {
        let route = OsStr::new("theirs");
        assert!(trusted(route, Ok(true)).is_ok());
        let not_ours = trusted(route, Ok(false)).unwrap_err();
        assert_eq!(not_ours.kind(), io::ErrorKind::PermissionDenied);
        assert!(not_ours.to_string().contains("not served by this user"));
        let unknown = trusted(route, Err(io::Error::other("no credentials"))).unwrap_err();
        assert_eq!(unknown.kind(), io::ErrorKind::PermissionDenied);
        assert!(unknown.to_string().contains("no credentials"));
    }

    #[tokio::test]
    async fn a_route_nobody_listens_at_is_an_error_not_a_hang() {
        let dir = scratch();
        let route = Endpoint::new(dir.path(), "nobody").to_string();
        assert!(Foreigners::connect(&route).await.is_err());
    }

    #[tokio::test]
    async fn what_is_no_route_this_os_can_name_is_refused_before_anything_is_opened() {
        // A pipe's shape is refused on Unix (no absolute path), and on Windows in any form `interprocess` cannot
        // open (`PIPE` in capitals) or on another computer.
        for route in [
            "",
            "relative/theirs",
            "theirs",
            "uds:/tmp/theirs",
            r"\\.\PIPE\theirs",
            r"\\server\pipe\theirs",
        ] {
            let error = Foreigners::check(OsStr::new(route)).unwrap_err();
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
