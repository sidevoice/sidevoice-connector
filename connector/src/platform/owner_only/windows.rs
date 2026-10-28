//! Windows: the entry opened as itself (`FILE_FLAG_OPEN_REPARSE_POINT`, a reparse point refused) below a directory that
//! is not one either, and the protected DACL `sidevoice-platform` gives what it creates private set on that handle
//! with the low-level call (`SetKernelObjectSecurity`), which passes nothing down to what is already inside, unlike
//! `SetSecurityInfo`: only what is created in it later inherits. Own `unsafe` over `windows-sys`, the few calls
//! `sidevoice-platform`'s own Windows files make for the same descriptor.

use std::ffi::{c_void, OsStr};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr::null_mut;

use crate::platform::fs::{Files, Fs};
use windows_sys::Win32::Foundation::{LocalFree, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{SetKernelObjectSecurity, DACL_SECURITY_INFORMATION};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
    WRITE_DAC,
};

/// Protected (`P`: it takes nothing from its parent), full control to the owner (`OWNER RIGHTS`), SYSTEM and
/// Administrators, inherited by what is created inside: `sidevoice-platform`'s private descriptor.
const PRIVATE_SDDL: &str = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

pub struct Os;

impl super::OwnerOnly for Os {
    fn open(parent: &Path, name: &OsStr) -> io::Result<File> {
        // `open_read` refuses a reparse point at the path itself.
        drop(Files::open_read(parent)?);
        let file = OpenOptions::new()
            .access_mode(READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(parent.join(name))?;
        if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "a link (reparse point)",
            ));
        }
        Ok(file)
    }

    fn restrict(dir: &File) -> io::Result<()> {
        let descriptor = Descriptor::private()?;
        // SAFETY: the handle is open with `WRITE_DAC` for the call; the descriptor is valid until dropped. Its
        // control carries `SE_DACL_PROTECTED` (the SDDL's `P`).
        let ok = unsafe {
            SetKernelObjectSecurity(
                dir.as_raw_handle() as HANDLE,
                DACL_SECURITY_INFORMATION,
                descriptor.0,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// A security descriptor made from [`PRIVATE_SDDL`], released on drop.
struct Descriptor(*mut c_void);

impl Descriptor {
    fn private() -> io::Result<Self> {
        let sddl: Vec<u16> = PRIVATE_SDDL
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut descriptor: *mut c_void = null_mut();
        // SAFETY: the SDDL is NUL-terminated; the descriptor is allocated by the call and released on drop.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(descriptor))
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW` with `LocalAlloc`.
        unsafe { LocalFree(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::super::{OwnerOnly, Restrictor};
    use crate::platform::fs::{Files, Fs, Others, TestFiles};
    use std::ffi::OsStr;
    use std::path::Path;

    /// What `icacls` shows of `path`'s DACL, its own name left out.
    fn dacl(path: &Path) -> String {
        let output = std::process::Command::new("icacls")
            .arg(path)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).replace(&*path.to_string_lossy(), "")
    }

    #[test]
    fn a_directory_others_can_enter_gets_the_private_dacl() {
        let scratch = crate::test_support::temp_dir("owner-only");
        let dir = scratch.path().join("data");
        std::fs::create_dir(&dir).unwrap(); // Inherits the scratch directory's DACL: not protected, so not private.
        Files::let_others_change(&dir);
        let open = Restrictor::open(scratch.path(), OsStr::new("data")).unwrap();
        assert!(Files::check_private(&open).is_err());
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

    // What is already inside keeps its DACL, what it inherited included: a file, one with a second name outside,
    // and a directory.
    #[test]
    fn what_is_inside_keeps_its_access() {
        let scratch = crate::test_support::temp_dir("owner-only-children");
        let dir = scratch.path().join("data");
        std::fs::create_dir(&dir).unwrap();
        Files::let_others_change(&dir);
        let file = dir.join("file");
        std::fs::write(&file, b"inherits").unwrap();
        let linked = dir.join("linked");
        std::fs::write(&linked, b"two names").unwrap();
        let outside = scratch.path().join("outside");
        std::fs::hard_link(&linked, &outside).unwrap();
        let child = dir.join("child");
        std::fs::create_dir(&child).unwrap();
        let before: Vec<String> = [&file, &linked, &outside, &child]
            .iter()
            .map(|path| dacl(path))
            .collect();
        let open = Restrictor::open(scratch.path(), OsStr::new("data")).unwrap();
        Restrictor::restrict(&open).unwrap();
        Files::check_private(&open).unwrap();
        let after: Vec<String> = [&file, &linked, &outside, &child]
            .iter()
            .map(|path| dacl(path))
            .collect();
        assert_eq!(before, after);
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
        assert_ne!(Files::others_of(&replacement).unwrap(), Others::Nothing);
    }
}
