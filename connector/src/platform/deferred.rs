//! Deleting what a running program still holds, later: the last resort when an uninstall cannot remove a folder
//! because a program we did not start (an antivirus scan, an Explorer window, an app) has a file in it open.
//!
//! The interface is the [`Deferred`] trait; [`Deferring`] is the one OS implementation compiled in. Only Windows has
//! the problem: Unix unlinks an open file, so nothing there is ever left to defer and its implementation refuses.
//!
//! Windows does it with a per-user `RunOnce` entry that deletes the folder at the next sign-in, before anything has
//! had the chance to open it again. `MoveFileEx` with `MOVEFILE_DELAY_UNTIL_REBOOT` would run earlier, but it writes
//! to `HKLM` and is refused to a program that is not elevated, which Sidevoice never is.

use std::io;
use std::path::PathBuf;

#[cfg(unix)]
#[path = "deferred/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "deferred/windows.rs"]
mod imp;

pub use imp::Os as Deferring;

/// What every OS implements ([`Deferring`], in `deferred/unix.rs` and `deferred/windows.rs`).
pub trait Deferred {
    /// Arranges for each of `folders`, with everything in it, to be deleted the next time this user signs in (once;
    /// the entry removes itself). A folder that is gone by then is fine. An error, with nothing arranged for the
    /// folders that were not reached, when the OS refuses or a path cannot be said safely; `Unsupported` where there
    /// is nothing to defer.
    fn at_next_sign_in(folders: &[PathBuf]) -> io::Result<()>;
}

/// The longest command a `RunOnce` entry may hold.
#[cfg(any(windows, test))]
const RUN_ONCE_LIMIT: usize = 260;

/// The command line that deletes `folder` and what is in it, without following a link or junction out of it. `None`
/// for a path that cannot be put safely between quotes in a command line (it has a quote or a character `cmd` or the
/// registry would expand), and for one that is not absolute: the entry runs from wherever Windows starts it.
#[cfg(any(windows, test))]
fn removal_command(folder: &std::path::Path) -> Option<String> {
    let text = folder.to_str()?;
    let unsafe_char =
        |ch: char| ch.is_control() || matches!(ch, '"' | '%' | '^' | '!' | '&' | '|' | '<' | '>');
    let command = format!("cmd.exe /d /c rmdir /s /q \"{text}\"");
    // A `RunOnce` command is at most 260 characters (Microsoft's documentation): a longer one is refused, not cut.
    (folder.is_absolute()
        && !text.chars().any(unsafe_char)
        && command.chars().count() <= RUN_ONCE_LIMIT)
        .then_some(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn the_removal_command_quotes_one_absolute_folder() {
        let folder = if cfg!(windows) {
            Path::new(r"C:\Users\Ana\AppData\Local\sidevoice\.uninstall-claim\release-1")
        } else {
            Path::new("/home/ana/sidevoice/.uninstall-claim/release-1")
        };
        let command = removal_command(folder).unwrap();
        assert_eq!(
            command,
            format!("cmd.exe /d /c rmdir /s /q \"{}\"", folder.display())
        );
    }

    #[test]
    fn a_command_longer_than_run_once_allows_is_refused() {
        let base = if cfg!(windows) { r"C:\" } else { "/" };
        let long = format!("{base}{}", "a".repeat(260));
        assert_eq!(removal_command(Path::new(&long)), None);
        let fits = format!("{base}{}", "a".repeat(200));
        assert!(removal_command(Path::new(&fits)).is_some());
    }

    #[test]
    fn a_path_that_could_say_more_than_a_folder_is_refused() {
        for text in [
            "/a/\"b",
            "/a/%TEMP%",
            "/a/b&calc",
            "/a/b^c",
            "/a/b|c",
            "relative/dir",
        ] {
            assert_eq!(removal_command(Path::new(text)), None, "{text}");
        }
    }
}
