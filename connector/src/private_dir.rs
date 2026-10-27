//! The directories the connector keeps private (the data directory `D`, the core's in it, the releases) when one was
//! only left open: made by an older build with `0755`, or given an ACL (sidevoice/sidevoice-connector#310). Such a
//! directory is made private again, with one line in the log, instead of refused; whoever checks it next finds it
//! private. It is made private again only when it is a real directory (not a link, junction or mount point), this
//! user's, under a directory that nobody else can change, nor anything above it. Anything else is left exactly as it
//! is for the check that follows to refuse, as before: another owner, a link, an ancestor others can change. A repair
//! that fails, or that leaves it still open, is an error naming the directory.
//!
//! `sidevoice-platform` judges and never changes access on what exists; the change itself is
//! [`crate::platform::owner_only`]'s.

use crate::platform::fs::{Access, Directory, Files, Fs, HeldDirectory, Others};
use crate::platform::owner_only::{OwnerOnly, Restrictor};
use crate::platform::user::{User, UserId};
use std::io;
use std::path::Path;

/// Makes the directory at `path` private again when it is one of ours that was only left open (see the module).
/// Returns whether it did. Nothing there, or something that is not ours to repair, is `Ok(false)`, untouched.
pub fn restore(path: &Path) -> io::Result<bool> {
    // A link there is refused by the open, not followed; missing or unreadable is the next check's to say.
    let Ok(dir) = Restrictor::open(path) else {
        return Ok(false);
    };
    if !dir.metadata()?.is_dir()
        || Files::others_of(&dir)? == Others::Nothing
        || !UserId::current().owns_file(&dir)?
    {
        return Ok(false);
    }
    // Every directory above it is somebody trusted's and nobody else can change it (`HeldDirectory::open` checks
    // the whole way); otherwise who else could have put it there is not ours to settle.
    let Some(parent) = path.parent() else {
        return Ok(false);
    };
    if HeldDirectory::open(parent, Access::NotWritableByOthers).is_err() {
        return Ok(false);
    }
    Restrictor::restrict(path, &dir)
        .and_then(|()| Files::check_private(&dir))
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "{} is open to others and could not be made private: {error}",
                    path.display()
                ),
            )
        })?;
    crate::logfile::log(&format!(
        "{} was open to others; made it private again",
        path.display()
    ));
    Ok(true)
}

/// [`Fs::ensure_private_dir`], once a directory of ours that was only left open is made private again ([`restore`]).
pub fn ensure(path: &Path) -> io::Result<()> {
    restore(path)?;
    Files::ensure_private_dir(path)
}

/// [`Fs::check_private`] of the existing directory at `path`, once a directory of ours that was only left open is
/// made private again ([`restore`]).
pub fn check_existing(path: &Path) -> io::Result<()> {
    restore(path)?;
    Files::check_private(&Files::open_read(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::fs::TestFiles;
    use crate::test_support::temp_dir;

    fn private(path: &Path) -> bool {
        Files::open_read(path)
            .and_then(|dir| Files::check_private(&dir))
            .is_ok()
    }

    fn others(path: &Path) -> Others {
        Files::others_of(&Files::open_read(path).unwrap()).unwrap()
    }

    #[test]
    fn a_directory_of_ours_others_can_read_is_made_private_again() {
        let scratch = temp_dir("restore");
        let dir = scratch.path().join("data");
        Files::create_private_dir(&dir).unwrap();
        Files::let_others_read(&dir);
        assert!(!private(&dir));
        assert!(restore(&dir).unwrap());
        assert!(private(&dir));
        assert!(!restore(&dir).unwrap(), "a private one is left as it is");
        ensure(&dir).unwrap();
        check_existing(&dir).unwrap();
    }

    #[test]
    fn a_directory_of_ours_others_can_change_is_made_private_again() {
        let scratch = temp_dir("restore-change");
        let dir = scratch.path().join("data");
        Files::create_private_dir(&dir).unwrap();
        Files::let_others_change(&dir);
        ensure(&dir).unwrap();
        assert!(private(&dir));
    }

    #[test]
    fn nothing_there_is_left_for_the_check_and_ensure_makes_it() {
        let scratch = temp_dir("restore-missing");
        let dir = scratch.path().join("data");
        assert!(!restore(&dir).unwrap());
        assert!(check_existing(&dir).is_err());
        ensure(&dir).unwrap();
        assert!(private(&dir));
    }

    #[test]
    fn a_link_to_an_open_directory_is_refused_not_repaired() {
        let scratch = temp_dir("restore-link");
        let open = scratch.path().join("open");
        Files::create_private_dir(&open).unwrap();
        Files::let_others_read(&open);
        let link = scratch.path().join("data");
        Files::symlink(&open, &link);
        assert!(!restore(&link).unwrap());
        assert!(check_existing(&link).is_err());
        assert!(ensure(&link).is_err());
        assert_eq!(others(&open), Others::Read, "what it leads to is untouched");
    }

    #[test]
    fn a_directory_under_one_others_can_change_is_refused_not_repaired() {
        let scratch = temp_dir("restore-ancestor");
        let parent = scratch.path().join("shared");
        Files::create_private_dir(&parent).unwrap();
        let dir = parent.join("data");
        Files::create_private_dir(&dir).unwrap();
        Files::let_others_read(&dir);
        Files::let_others_change(&parent);
        assert!(!restore(&dir).unwrap());
        assert!(check_existing(&dir).is_err());
        assert!(ensure(&dir).is_err());
        assert_eq!(others(&dir), Others::Read, "not repaired");
        Files::shut_others_out(&parent);
    }

    // Somebody else's directory others can read: the one the home is in (`/home`, `/Users`, `C:\Users`), the system's.
    #[test]
    fn somebody_elses_directory_is_refused_not_repaired() {
        let home = dirs::home_dir().unwrap();
        let theirs = home.parent().unwrap();
        let before = Files::open_read(theirs).unwrap();
        assert!(
            !UserId::current().owns_file(&before).unwrap(),
            "{} is this user's",
            theirs.display()
        );
        let access = Files::others_of(&before).unwrap();
        assert_ne!(access, Others::Nothing);
        assert!(!restore(theirs).unwrap());
        assert!(check_existing(theirs).is_err());
        assert_eq!(others(theirs), access, "untouched");
    }

    #[test]
    fn a_file_is_not_a_directory_to_repair() {
        let scratch = temp_dir("restore-file");
        let file = scratch.path().join("data");
        std::fs::write(&file, b"not a directory").unwrap();
        Files::let_others_read(&file);
        assert!(!restore(&file).unwrap());
        assert!(ensure(&file).is_err());
        assert_eq!(others(&file), Others::Read);
    }
}
