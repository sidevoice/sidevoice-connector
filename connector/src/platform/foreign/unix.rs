//! Unix: a third party's endpoint is a Unix socket, named by its absolute path. Its server is this user's when the
//! socket file is this user's and the kernel's credentials of the connection say the peer runs as this user.

use super::ForeignStream;
use crate::platform::user::{User, UserId};
use interprocess::local_socket::traits::StreamCommon as _;
use std::ffi::OsStr;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;

/// This OS's third-party endpoints.
pub struct Os;

impl super::Foreign for Os {
    fn check(route: &OsStr) -> io::Result<()> {
        if Path::new(route).is_absolute() {
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not an absolute socket path: {route:?}"),
        ))
    }

    // Both: the peer's credentials are what the kernel vouches for at this connection (a file can be swapped after
    // it is judged); the file is what the route names, refused when another user owns it whoever serves it.
    fn served_by_this_user(route: &OsStr, stream: &ForeignStream) -> io::Result<bool> {
        let path = Path::new(route);
        let socket = std::fs::symlink_metadata(path)?.file_type().is_socket();
        Ok(socket
            && UserId::current().owns_path(path)?
            && stream.peer_creds()?.euid() == Some(rustix::process::geteuid().as_raw()))
    }
}
