//! macOS: an ACL's entries come on top of the mode bits, so they are removed: an empty extended ACL set on the open
//! descriptor (`acl_set_fd_np`), the directory judged and nothing put at its name since. Own `unsafe` over
//! libSystem's `<sys/acl.h>`: `exacl` sets an ACL only by path, and `sidevoice-platform` reads one by descriptor the
//! same way.

use std::ffi::{c_int, c_uint, c_void};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

/// `ACL_TYPE_EXTENDED`, `<sys/acl.h>`.
const ACL_TYPE_EXTENDED: c_uint = 0x0000_0100;

extern "C" {
    fn acl_init(count: c_int) -> *mut c_void;
    fn acl_set_fd_np(fd: c_int, acl: *mut c_void, kind: c_uint) -> c_int;
    fn acl_free(object: *mut c_void) -> c_int;
}

pub(super) fn remove_extended(dir: &File) -> io::Result<()> {
    // SAFETY: `acl_init` returns a new ACL (or null, with errno set) to be released with `acl_free`.
    let empty = unsafe { acl_init(0) };
    if empty.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor is open for the call; `empty` is a valid ACL.
    let set = unsafe { acl_set_fd_np(dir.as_raw_fd(), empty, ACL_TYPE_EXTENDED) };
    let error = io::Error::last_os_error();
    // SAFETY: `empty` came from `acl_init` and is freed only here.
    unsafe { acl_free(empty) };
    if set != 0 {
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{OwnerOnly, Restrictor};
    use crate::platform::fs::{Files, Fs, Others};
    use exacl::{AclEntry, Perm};
    use std::ffi::OsStr;
    use std::path::Path;

    fn give_everyone(dir: &Path) {
        let entries = [AclEntry::allow_group(
            "everyone",
            Perm::READ | Perm::WRITE,
            None,
        )];
        exacl::setfacl(&[dir], &entries, None).unwrap();
    }

    // An ACL entry is not in the mode bits: `0700` alone leaves it, so it is removed too.
    #[test]
    fn an_acl_letting_others_in_is_removed() {
        let scratch = crate::test_support::temp_dir("owner-only-acl");
        let dir = scratch.path().join("data");
        Files::create_private_dir(&dir).unwrap();
        give_everyone(&dir);
        let open = Restrictor::open(scratch.path(), OsStr::new("data")).unwrap();
        assert_ne!(Files::others_of(&open).unwrap(), Others::Nothing);
        Restrictor::restrict(&open).unwrap();
        Files::check_private(&open).unwrap();
        assert!(exacl::getfacl(&dir, None).unwrap().is_empty());
    }

    // Removed by the descriptor: a directory with an ACL moved into the name after the open keeps its ACL.
    #[test]
    fn the_acl_of_a_directory_put_at_its_name_after_the_open_is_kept() {
        let scratch = crate::test_support::temp_dir("owner-only-acl-swap");
        let dir = scratch.path().join("data");
        Files::create_private_dir(&dir).unwrap();
        give_everyone(&dir);
        let open = Restrictor::open(scratch.path(), OsStr::new("data")).unwrap();
        let aside = scratch.path().join("aside");
        std::fs::rename(&dir, &aside).unwrap();
        Files::create_private_dir(&dir).unwrap();
        give_everyone(&dir);
        Restrictor::restrict(&open).unwrap();
        Files::check_private(&open).unwrap();
        assert!(exacl::getfacl(&aside, None).unwrap().is_empty());
        assert!(!exacl::getfacl(&dir, None).unwrap().is_empty());
    }
}
