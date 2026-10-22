//! The local channel between this user's processes: the connector's endpoint for its clients, and the core's.
//!
//! The interface is two traits, each implemented once per OS and chosen below: [`Address`], what an endpoint is
//! ([`Endpoint`]: a socket path on macOS and Linux, a pipe name on Windows), and [`Channel`], listening and
//! connecting at one ([`Channels`]), both through `interprocess`. What this module adds is the user check, on every
//! OS and on both ends: a listener only ever hands out streams whose peer is this user, and a client only talks to a
//! server that is this user. Streams are plain `AsyncRead + AsyncWrite + Unpin`, so the line-JSON protocol, the
//! WebSocket handshake and the HTTP health probe run over them unchanged. Callers, std-style (`Path::new`,
//! `str::parse`, `TcpListener::bind(addr)`): `use crate::platform::ipc::{Address, Channel, Channels, Endpoint};`
//! then `Endpoint::new(&data, "connector")`, `record.parse::<Endpoint>()`, `Channels::listen(&endpoint)`,
//! `Channels::connect(&endpoint).await`.

#[cfg(unix)]
#[path = "ipc/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "ipc/windows.rs"]
mod imp;

pub use imp::{Endpoint, Os as Channels};

use interprocess::local_socket::tokio::Listener as RawListener;
use interprocess::local_socket::traits::tokio::{Listener as _, Stream as _};
use std::fmt;
use std::future::Future;
use std::io;
use std::path::Path;
use std::str::FromStr;

/// One connection. `AsyncRead + AsyncWrite + Unpin`; its peer has been checked to be this user.
pub type Stream = interprocess::local_socket::tokio::Stream;

/// What an endpoint is on this OS ([`Endpoint`], in `ipc/unix.rs` and `ipc/windows.rs`): where a channel is, as
/// one opaque value. Nothing outside derives one from another or looks inside.
///
/// - [`Address::new`] makes our own.
/// - `FromStr` (`record.parse()`) reads one another process published (the core's ready file keeps one), refusing
///   anything that is not an endpoint on this OS: Unix, an absolute socket path; Windows, a full local pipe name,
///   `\\.\pipe\...`.
/// - `Display` writes that record.
pub trait Address:
    Sized + Clone + fmt::Debug + fmt::Display + PartialEq + FromStr<Err = io::Error>
{
    /// Our own endpoint called `name` for the data directory `dir`. Unix: the socket `dir/<name>.sock`. Windows: the
    /// pipe `\\.\pipe\sidevoice-<hash of (user, dir)>-<name>`, so two profiles or two users never meet on one name.
    fn new(dir: &Path, name: &str) -> Self;
}

/// What every OS implements ([`Channels`], in `ipc/unix.rs` and `ipc/windows.rs`): how a listener is made private to
/// this user. Connecting is one body for every OS; the peer check is internal to each OS file (`peer_is_this_user`).
pub trait Channel {
    /// Starts listening at `endpoint`, reachable by this user alone. Unix: a socket left by a dead process is
    /// replaced, but only once it is checked to be this user's and private; the socket file is `0600` and goes away
    /// when the listener is dropped. Windows: the first instance of the pipe (a name already taken is an error),
    /// with a descriptor that lets in its owner, SYSTEM and Administrators only, refusing remote clients.
    fn listen(endpoint: &Endpoint) -> io::Result<Listener>;

    /// Connects to `endpoint`, refusing a server that is not this user (checked on the connection, once made). No
    /// timeout: callers bound it.
    fn connect(endpoint: &Endpoint) -> impl Future<Output = io::Result<Stream>> + Send {
        let name = endpoint.name();
        let at = endpoint.to_string();
        async move {
            let stream = Stream::connect(name?).await?;
            if !imp::peer_is_this_user(&stream)? {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("the server at {at} is not this user"),
                ));
            }
            Ok(stream)
        }
    }
}

/// A listening endpoint ([`Channel::listen`]).
pub struct Listener(RawListener);

impl Listener {
    /// The next connection whose peer is this user. Any other is closed without a word and not reported.
    pub async fn accept(&self) -> io::Result<Stream> {
        loop {
            let stream = self.0.accept().await?;
            if imp::peer_is_this_user(&stream).unwrap_or(false) {
                return Ok(stream);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::fs::{Files, Fs};
    use std::path::PathBuf;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A private directory for one test, inside a scratch directory that goes when the guard is dropped.
    fn private_dir(label: &str) -> (tempfile::TempDir, PathBuf) {
        let scratch = crate::test_support::temp_dir(label);
        let dir = scratch.path().join("private");
        Files::create_private_dir(&dir).unwrap();
        (scratch, dir)
    }

    #[test]
    fn an_endpoint_survives_its_record_and_nothing_else_is_one() {
        let (_scratch, dir) = private_dir("record");
        let endpoint = Endpoint::new(&dir, "test");
        assert_eq!(endpoint.to_string().parse::<Endpoint>().unwrap(), endpoint);
        assert!("".parse::<Endpoint>().is_err());
        assert!("relative/local.sock".parse::<Endpoint>().is_err());
    }

    #[test]
    fn a_directory_and_a_name_make_one_endpoint_and_nothing_else_does() {
        let ((_a, a), (_b, b)) = (private_dir("a"), private_dir("b"));
        let endpoint = Endpoint::new(&a, "connector");
        assert_eq!(endpoint, Endpoint::new(&a, "connector"));
        assert_ne!(endpoint, Endpoint::new(&b, "connector"));
        assert_ne!(endpoint, Endpoint::new(&a, "core"));
        assert!(endpoint.to_string().contains("connector"));
    }

    #[tokio::test]
    async fn this_user_listens_and_connects_and_bytes_flow_both_ways() {
        let (_scratch, dir) = private_dir("echo");
        let endpoint = Endpoint::new(&dir, "test");
        let listener = Channels::listen(&endpoint).unwrap();
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            assert!(imp::peer_is_this_user(&stream).unwrap());
            let mut line = [0u8; 5];
            stream.read_exact(&mut line).await.unwrap();
            stream.write_all(&line).await.unwrap();
        });
        let mut client = Channels::connect(&endpoint).await.unwrap();
        assert!(imp::peer_is_this_user(&client).unwrap());
        client.write_all(b"hello").await.unwrap();
        let mut echoed = [0u8; 5];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"hello");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn an_endpoint_nobody_listens_at_is_an_error_not_a_hang() {
        let (_scratch, dir) = private_dir("none");
        assert!(Channels::connect(&Endpoint::new(&dir, "test"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn an_endpoint_is_free_again_once_its_listener_is_dropped() {
        let (_scratch, dir) = private_dir("again");
        let endpoint = Endpoint::new(&dir, "test");
        drop(Channels::listen(&endpoint).unwrap());
        let _listener = Channels::listen(&endpoint).unwrap();
        Channels::connect(&endpoint).await.unwrap();
    }
}
