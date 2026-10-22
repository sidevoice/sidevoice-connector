//! Windows: a third party's endpoint is a named pipe on this computer, `\\.\pipe\<name>`. A route in any other shape
//! is refused: no agent documents another one here, and a name is never derived from it.

use std::io;

const PREFIX: &str = r"\\.\pipe\";

/// This OS's third-party endpoints.
pub struct Os;

impl super::Foreign for Os {
    fn check(route: &str) -> io::Result<()> {
        let local_pipe = route
            .get(..PREFIX.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(PREFIX))
            && route.len() > PREFIX.len();
        if local_pipe {
            return Ok(());
        }
        Err(io::Error::new(
            if route.is_empty() {
                io::ErrorKind::InvalidInput
            } else {
                io::ErrorKind::Unsupported
            },
            format!("not a local named pipe (\\\\.\\pipe\\<name>): {route:?}"),
        ))
    }
}
