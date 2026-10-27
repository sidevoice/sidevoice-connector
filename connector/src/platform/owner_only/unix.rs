//! Unix: the entry opened relative to its directory without following it (`openat`, `O_NOFOLLOW`), and its mode set
//! on that descriptor (`fchmod`, `rustix`); then the extended ACL, per OS (`super::acl`), by the same descriptor.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::Path;

use crate::platform::fs::{Files, Fs};
use rustix::fs::{fchmod, openat, Mode, OFlags};

pub struct Os;

impl super::OwnerOnly for Os {
    fn open(parent: &Path, name: &OsStr) -> io::Result<File> {
        let parent = Files::open_read(parent)?;
        let flags = OFlags::RDONLY
            | OFlags::DIRECTORY
            | OFlags::NOFOLLOW
            | OFlags::NONBLOCK
            | OFlags::NOCTTY
            | OFlags::CLOEXEC;
        Ok(File::from(openat(&parent, name, flags, Mode::empty())?))
    }

    fn restrict(dir: &File) -> io::Result<()> {
        fchmod(dir, Mode::RWXU)?;
        super::acl::remove_extended(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{OwnerOnly, Restrictor};
    use crate::platform::fs::{Files, Fs, Others, TestFiles};
    use std::ffi::OsStr;

    #[test]
    fn a_directory_others_can_enter_is_left_to_its_owner() {
        let scratch = crate::test_support::temp_dir("owner-only");
        let dir = scratch.path().join("data");
        Files::create_private_dir(&dir).unwrap();
        Files::let_others_change(&dir);
        let open = Restrictor::open(scratch.path(), OsStr::new("data")).unwrap();
        Restrictor::restrict(&open).unwrap();
        Files::check_private(&open).unwrap();
    }

    #[test]
    fn a_link_is_refused_not_followed() {
        let scratch = crate::test_support::temp_dir("owner-only-link");
        let dir = scratch.path().join("data");
        Files::create_private_dir(&dir).unwrap();
        Files::symlink(&dir, scratch.path().join("link"));
        assert!(Restrictor::open(scratch.path(), OsStr::new("link")).is_err());
        let parent = scratch.path().join("parent");
        Files::symlink(scratch.path(), &parent);
        assert!(Restrictor::open(&parent, OsStr::new("data")).is_err());
    }

    // The change goes through the handle: a directory moved into the name after it was opened is untouched.
    #[test]
    fn a_directory_put_at_its_name_after_the_open_is_untouched() {
        let scratch = crate::test_support::temp_dir("owner-only-swap");
        let dir = scratch.path().join("data");
        Files::create_private_dir(&dir).unwrap();
        Files::let_others_read(&dir);
        let open = Restrictor::open(scratch.path(), OsStr::new("data")).unwrap();
        std::fs::rename(&dir, scratch.path().join("aside")).unwrap();
        Files::create_private_dir(&dir).unwrap();
        Files::let_others_read(&dir);
        Restrictor::restrict(&open).unwrap();
        Files::check_private(&open).unwrap();
        let replacement = Files::open_read(&dir).unwrap();
        assert_eq!(Files::others_of(&replacement).unwrap(), Others::Read);
    }
}
