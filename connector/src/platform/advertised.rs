//! An endpoint of ours that another process advertised: the connector's, which it publishes in `D/connector.json`
//! once it listens, for its clients to reach it there instead of working its place out from their own environment.
//! The connector and a client may see different environments (macOS: launchd's `$TMPDIR` and an agent app's), so on
//! Unix the place a client computes ([`super::ipc::Address::new`]) can differ from where the connector listens.
//!
//! The record is a file of ours, but what it names is judged before anything connects to it, fail-closed: only the
//! connector's own socket name, in the directory [`super::ipc`] gives its data directory, private to this user. The
//! peer check on connect ([`super::ipc::Channel::connect`]) stays on top. Callers import both:
//! `use crate::platform::advertised::{Advertised, AdvertisedEndpoint};` then `Advertised::trust(&endpoint, data, name)`.

#[cfg(unix)]
#[path = "advertised/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "advertised/windows.rs"]
mod imp;

pub use imp::Os as Advertised;

use super::ipc::Endpoint;
use std::io;
use std::path::Path;

/// What every OS implements ([`Advertised`], in `advertised/unix.rs` and `advertised/windows.rs`).
pub trait AdvertisedEndpoint {
    /// Whether `endpoint`, advertised as the endpoint called `name` of the data directory `dir`, may be connected to;
    /// `PermissionDenied`, saying why, when not.
    ///
    /// Unix: the socket `<name>.sock`, its path absolute with no `..`, in either `dir` itself or the directory
    /// `sidevoice-<hash of dir>` (the name [`super::ipc::Address::new`] gives it) right below a temporary or runtime
    /// directory of this user's (`$TMPDIR`, `/tmp`, `$XDG_RUNTIME_DIR`, `/run/user/<uid>` on Linux, the per-user
    /// temporary directory and `/private/tmp` on macOS); that directory a real one (not a link), this user's, open to
    /// nobody else. Windows: a pipe's name does not depend on the environment (it hashes the user and the data
    /// directory), so the only one trusted is the one this process computes.
    fn trust(endpoint: &Endpoint, dir: &Path, name: &str) -> io::Result<()>;
}

/// The refusal of the advertised `endpoint`, saying `why`.
fn untrusted(endpoint: impl std::fmt::Display, why: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("the advertised endpoint {endpoint} is not trusted: {why}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::fs::{Files, Fs};
    use crate::platform::ipc::Address;

    #[test]
    fn the_endpoint_this_process_computes_is_trusted() {
        let scratch = crate::test_support::temp_dir("advertised");
        let dir = scratch.path().join("data");
        Files::ensure_private_dir(&dir).unwrap();
        let endpoint = Endpoint::new(&dir, "connector");
        // Where there is a runtime directory, the endpoint's own directory is made by whoever listens.
        #[cfg(unix)]
        Files::ensure_private_dir(
            std::path::Path::new(&endpoint.to_string())
                .parent()
                .unwrap(),
        )
        .unwrap();
        let trusted = Advertised::trust(&endpoint, &dir, "connector");
        #[cfg(unix)]
        {
            let parent = std::path::Path::new(&endpoint.to_string())
                .parent()
                .unwrap()
                .to_owned();
            if parent != dir {
                std::fs::remove_dir(&parent).unwrap();
            }
        }
        trusted.unwrap();
        let other = Endpoint::new(&scratch.path().join("other"), "connector");
        assert_eq!(
            Advertised::trust(&other, &dir, "connector")
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
