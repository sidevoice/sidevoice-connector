//! Windows: an endpoint is a named pipe whose name hashes the user and the data directory, never a temporary
//! directory, so every process of this user's computes the one the connector listens on. An advertised pipe is
//! trusted only when it is that one; the pipe's own descriptor and the peer check do the rest.

use crate::platform::ipc::{Address, Endpoint};
use std::io;
use std::path::Path;

/// This OS's advertised endpoints.
pub struct Os;

impl super::AdvertisedEndpoint for Os {
    fn trust(endpoint: &Endpoint, dir: &Path, name: &str) -> io::Result<()> {
        let computed = Endpoint::new(dir, name);
        if *endpoint == computed {
            return Ok(());
        }
        Err(super::untrusted(
            endpoint,
            format!("not this data directory's pipe {computed}"),
        ))
    }
}
