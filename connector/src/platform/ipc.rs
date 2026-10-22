//! The local channel between this user's processes: the connector's endpoint for its clients, and the core's.
//!
//! One endpoint per channel, whatever the OS offers: a Unix socket on macOS and Linux, a named pipe on Windows,
//! both through `interprocess`. What this module adds is the user check, on every OS and on both ends: a listener
//! only ever hands out streams whose peer is this user, and a client only talks to a server that is this user.
//! Streams are plain `AsyncRead + AsyncWrite + Unpin`, so the line-JSON protocol, the WebSocket handshake and the
//! HTTP health probe run over them unchanged.
//!
//! The interface is declared here once; `imp` implements, per OS, only what differs: the name, how a listener is
//! made private, and who is at the other end.

#[cfg(unix)]
#[path = "ipc/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "ipc/windows.rs"]
mod imp;

use interprocess::local_socket::tokio::{Listener as RawListener, Stream as RawStream};
use interprocess::local_socket::traits::tokio::{Listener as _, Stream as _};
use std::io;
use std::path::Path;

/// One connection. `AsyncRead + AsyncWrite + Unpin`; its peer has been checked to be this user.
pub type Stream = RawStream;

/// Where a channel is. Opaque: ours is made by [`Endpoint::in_dir`], the core's is read from its ready file as a
/// record ([`Endpoint::from_record`]); nothing outside derives one from the other or looks inside.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint(imp::Addr);

impl Endpoint {
    /// Our own endpoint called `name` for the data directory `dir`. Unix: the socket `dir/<name>.sock`. Windows: a
    /// pipe named after the user and `dir`, so two profiles or two users never meet on one name.
    pub fn in_dir(dir: &Path, name: &str) -> Endpoint {
        Endpoint(imp::in_dir(dir, name))
    }

    /// An endpoint another process published, as [`Endpoint::record`] writes it (the core's ready file keeps one).
    /// Unix: an absolute socket path. Windows: a full pipe name, `\\.\pipe\...`. Anything else is refused.
    pub fn from_record(record: &str) -> io::Result<Endpoint> {
        imp::from_record(record).map(Endpoint)
    }

    /// The text [`Endpoint::from_record`] reads back (what a process publishes for the others; only the core does,
    /// so here it serves the round trip the tests check).
    #[cfg(test)]
    pub fn record(&self) -> String {
        imp::record(&self.0)
    }
}

/// A listening endpoint.
pub struct Listener(RawListener);

/// Starts listening at `endpoint`, reachable by this user alone. Unix: a socket left by a dead process is
/// replaced, but only once it is checked to be this user's and private; the socket file goes away when the
/// listener is dropped. Windows: the first instance of the pipe, with a security descriptor for this user (plus
/// SYSTEM and Administrators), refusing remote clients; a pipe that already exists is an error.
pub fn listen(endpoint: &Endpoint) -> io::Result<Listener> {
    imp::listen(&endpoint.0).map(Listener)
}

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

/// Connects to `endpoint`, refusing a server that is not this user (Unix: also a socket that is not private to
/// this user). No timeout: callers bound it.
pub async fn connect(endpoint: &Endpoint) -> io::Result<Stream> {
    imp::verify_before_connect(&endpoint.0)?;
    let stream = RawStream::connect(imp::name(&endpoint.0)?).await?;
    if !imp::peer_is_this_user(&stream)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the server at the endpoint is not this user",
        ));
    }
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn scratch(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sv-ipc-{label}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        dir
    }

    #[test]
    fn an_endpoint_survives_its_record() {
        let dir = scratch("record");
        let endpoint = Endpoint::in_dir(&dir, "test");
        assert_eq!(Endpoint::from_record(&endpoint.record()).unwrap(), endpoint);
        assert!(Endpoint::from_record("").is_err());
        assert!(Endpoint::from_record("relative/local.sock").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_directory_and_a_name_make_one_endpoint_and_nothing_else_does() {
        let (a, b) = (scratch("a"), scratch("b"));
        let endpoint = Endpoint::in_dir(&a, "connector");
        assert_eq!(endpoint, Endpoint::in_dir(&a, "connector"));
        assert_ne!(endpoint, Endpoint::in_dir(&b, "connector"));
        assert_ne!(endpoint, Endpoint::in_dir(&a, "core"));
        std::fs::remove_dir_all(a).unwrap();
        std::fs::remove_dir_all(b).unwrap();
    }

    #[tokio::test]
    async fn this_user_listens_and_connects_and_bytes_flow_both_ways() {
        let dir = scratch("echo");
        let endpoint = Endpoint::in_dir(&dir, "test");
        let listener = listen(&endpoint).unwrap();
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            let mut line = [0u8; 5];
            stream.read_exact(&mut line).await.unwrap();
            stream.write_all(&line).await.unwrap();
        });
        let mut client = connect(&endpoint).await.unwrap();
        client.write_all(b"hello").await.unwrap();
        let mut echoed = [0u8; 5];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"hello");
        server.await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn an_endpoint_nobody_listens_at_is_an_error_not_a_hang() {
        let dir = scratch("none");
        assert!(connect(&Endpoint::in_dir(&dir, "test")).await.is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn the_first_instance_of_a_pipe_is_exclusive() {
        let dir = scratch("second");
        let endpoint = Endpoint::in_dir(&dir, "test");
        let _first = listen(&endpoint).unwrap();
        assert!(listen(&endpoint).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
