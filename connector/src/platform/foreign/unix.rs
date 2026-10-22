//! Unix: a third party's endpoint is a Unix socket, named by its absolute path.

use std::io;
use std::path::Path;

/// This OS's third-party endpoints.
pub struct Os;

impl super::Foreign for Os {
    fn check(route: &str) -> io::Result<()> {
        if Path::new(route).is_absolute() {
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not an absolute socket path: {route:?}"),
        ))
    }
}
