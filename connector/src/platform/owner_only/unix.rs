//! Unix: the mode set on the open descriptor (`fchmod`, `rustix`), so the directory changed is the one judged; then
//! the extended ACL, per OS (`super::acl`).

use std::fs::File;
use std::io;
use std::path::Path;

use crate::platform::fs::{Files, Fs};
use rustix::fs::{fchmod, Mode};

pub struct Os;

impl super::OwnerOnly for Os {
    fn open(path: &Path) -> io::Result<File> {
        Files::open_read(path)
    }

    fn restrict(path: &Path, dir: &File) -> io::Result<()> {
        fchmod(dir, Mode::RWXU)?;
        super::acl::remove_extended(path, dir)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{OwnerOnly, Restrictor};
    use crate::platform::fs::{Files, Fs, TestFiles};

    #[test]
    fn a_directory_others_can_enter_is_left_to_its_owner() {
        let scratch = crate::test_support::temp_dir("owner-only");
        let dir = scratch.path().join("data");
        Files::create_private_dir(&dir).unwrap();
        Files::let_others_change(&dir);
        let open = Restrictor::open(&dir).unwrap();
        Restrictor::restrict(&dir, &open).unwrap();
        Files::check_private(&open).unwrap();
    }

    #[test]
    fn a_link_is_refused_not_followed() {
        let scratch = crate::test_support::temp_dir("owner-only-link");
        let dir = scratch.path().join("data");
        Files::create_private_dir(&dir).unwrap();
        let link = scratch.path().join("link");
        Files::symlink(&dir, &link);
        assert!(Restrictor::open(&link).is_err());
    }
}
