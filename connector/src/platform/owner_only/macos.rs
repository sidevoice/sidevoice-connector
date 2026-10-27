//! macOS: an ACL's entries come on top of the mode bits, so they are removed (`exacl`), from the directory itself, not
//! following a link. By its path: `exacl` has no call by descriptor; the directories above it are ones nobody else
//! can change (the caller checked), and the caller judges the open descriptor again afterwards.

use std::fs::File;
use std::io;
use std::path::Path;

use exacl::{setfacl, AclEntry, AclOption};

pub(super) fn remove_extended(path: &Path, _dir: &File) -> io::Result<()> {
    setfacl(&[path], &[] as &[AclEntry], AclOption::SYMLINK_ACL)
}

#[cfg(test)]
mod tests {
    use super::super::{OwnerOnly, Restrictor};
    use crate::platform::fs::{Files, Fs, Others};
    use exacl::{AclEntry, Perm};

    // An ACL entry is not in the mode bits: `0700` alone leaves it, so it is removed too.
    #[test]
    fn an_acl_letting_others_in_is_removed() {
        let scratch = crate::test_support::temp_dir("owner-only-acl");
        let dir = scratch.path().join("data");
        Files::create_private_dir(&dir).unwrap();
        let entries = [AclEntry::allow_group(
            "everyone",
            Perm::READ | Perm::WRITE,
            None,
        )];
        exacl::setfacl(&[&dir], &entries, None).unwrap();
        let open = Restrictor::open(&dir).unwrap();
        assert_ne!(Files::others_of(&open).unwrap(), Others::Nothing);
        Restrictor::restrict(&dir, &open).unwrap();
        Files::check_private(&open).unwrap();
        assert!(exacl::getfacl(&dir, None).unwrap().is_empty());
    }
}
