//! What is left of the connector's own file checks once private files moved behind `platform::fs`: the check of a
//! Unix socket before anything talks to it. It belongs to the local channel and goes with it into `platform::ipc`
//! (connector#222).

use crate::platform::fs::{Files, Fs, Others};
use crate::platform::user::{User, UserId};
use anyhow::{bail, Context, Result};
use std::os::unix::fs::FileTypeExt;
use std::path::Path;

/// A socket before anything talks to it: in a private directory, a socket, this user's, open to nobody else.
pub fn verify_socket(path: &Path) -> Result<()> {
    Files::check_private_dir(path.parent().context("socket parent")?)?;
    if !std::fs::symlink_metadata(path)?.file_type().is_socket()
        || !UserId::current().owns_path(path)?
        || Files::others(path)? != Others::Nothing
    {
        bail!("unsafe socket {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::fs::tests::Scratch;
    use std::os::unix::fs::PermissionsExt;

    fn chmod(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn a_socket_is_trusted_only_in_a_private_directory_and_open_to_nobody_else() {
        let scratch = Scratch::new("sock");
        let path = scratch.0.join("s.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        chmod(&path, 0o600);
        verify_socket(&path).unwrap();
        chmod(&path, 0o666);
        assert!(verify_socket(&path).is_err());
        chmod(&path, 0o600);
        chmod(&scratch.0, 0o755);
        assert!(verify_socket(&path).is_err());
        chmod(&scratch.0, 0o700);
        let file = scratch.0.join("plain");
        Files::write_private(&file, b"").unwrap();
        assert!(verify_socket(&file).is_err());
    }
}
