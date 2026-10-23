//! What differs by the machine xtask runs on, as opposed to the target it packages. xtask deals with other targets
//! than its host (`manifest` and `npm` read every target's archive from any machine), so its target tables are data
//! (`TARGETS`, keyed by name) and never `cfg`. The few places where the *host* decides are here, and only here:
//! file modes, the scratch directory, the clean environment `verify` runs a binary in.
//!
//! Not the connector's `platform` concern (a trait, one file per OS): xtask is a build tool of a handful of such
//! functions, none of them shipped, so a trait with two implementations would be more code than it replaces.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Result;

/// Sets the Unix permission bits. Windows has none (a program is one by its `.exe` name): nothing to do.
#[cfg(unix)]
pub(crate) fn chmod(path: &Path, mode: u32) -> Result<()> {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(not(unix))]
pub(crate) fn chmod(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

/// Makes the directory `path` private to the user running xtask, as the connector requires of the directory it stages
/// into: mode `0700` on Unix; on Windows the inherited access is removed and only this user is granted (`icacls`).
pub(crate) fn make_private_dir(path: &Path) -> Result<()> {
    if cfg!(windows) {
        let user = std::env::var("USERNAME").map_err(|error| format!("USERNAME: {error}"))?;
        let output = Command::new("icacls")
            .arg(path)
            .args(["/inheritance:r", "/grant:r"])
            .arg(format!("{user}:(OI)(CI)F"))
            .output()
            .map_err(|error| format!("icacls: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "icacls {}: {}{}",
                path.display(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(())
    } else {
        chmod(path, 0o700)
    }
}

/// Where `verify` unpacks: a path with a space, away from the checkout (`/tmp`; Windows has no such place, so the
/// user's temporary directory).
pub(crate) fn scratch_base() -> PathBuf {
    if cfg!(windows) {
        std::env::temp_dir()
    } else {
        PathBuf::from("/tmp")
    }
}

/// A clean environment: nothing the build or the runner set may stand in for what the binary carries. Windows
/// programs need `SystemRoot` to start at all (Winsock, the loader), and look their DLLs up in System32.
pub(crate) fn clean_environment(command: &mut Command) -> &mut Command {
    command.env_clear();
    if cfg!(windows) {
        let root = system_root();
        command
            .env("PATH", format!(r"{root}\System32"))
            .env("SystemRoot", root)
    } else {
        command.env("PATH", "/usr/bin:/bin")
    }
}

/// The Windows directory of this machine.
fn system_root() -> String {
    std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::{write, TempDir};
    use std::fs;

    #[test]
    fn a_file_mode_is_set_where_the_host_has_modes_and_ignored_where_it_has_none() {
        let dir = TempDir::new("xtask-host-chmod").unwrap();
        let file = dir.0.join("program");
        write(&file, b"").unwrap();
        chmod(&file, 0o755).unwrap();
        assert!(fs::metadata(&file).unwrap().is_file());
    }

    #[test]
    fn the_scratch_base_exists() {
        assert!(scratch_base().is_dir());
    }
}
