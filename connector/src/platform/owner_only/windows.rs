//! Windows: the protected DACL `sidevoice-platform` gives what it creates private, set on the handle that was opened
//! (`SetSecurityInfo`), which also passes its inheritable entries down to what is already inside. Own `unsafe` over
//! `windows-sys`, the few calls `sidevoice-platform`'s own Windows files make for the same descriptor.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SetSecurityInfo, SDDL_REVISION_1,
    SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    GetSecurityDescriptorDacl, ACL, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
    WRITE_DAC,
};

/// Protected, full control to the owner (`OWNER RIGHTS`), SYSTEM and Administrators, inherited by what is created
/// inside: `sidevoice-platform`'s private descriptor.
const PRIVATE_SDDL: &str = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

pub struct Os;

impl super::OwnerOnly for Os {
    fn open(path: &Path) -> io::Result<File> {
        let file = OpenOptions::new()
            .access_mode(READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "a link (reparse point)",
            ));
        }
        Ok(file)
    }

    fn restrict(_path: &Path, dir: &File) -> io::Result<()> {
        let descriptor = Descriptor::private()?;
        let dacl = descriptor.dacl()?;
        // SAFETY: the handle is open with `WRITE_DAC` for the call; `dacl` points inside `descriptor`, alive until
        // the end of this function.
        let status = unsafe {
            SetSecurityInfo(
                dir.as_raw_handle() as HANDLE,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                dacl,
                null_mut(),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
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

    fn dacl(&self) -> io::Result<*const ACL> {
        let (mut present, mut defaulted) = (0, 0);
        let mut dacl: *mut ACL = null_mut();
        // SAFETY: `self.0` is the valid descriptor made in `private`; `dacl` points inside it.
        if unsafe { GetSecurityDescriptorDacl(self.0, &mut present, &mut dacl, &mut defaulted) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        if present == 0 || dacl.is_null() {
            return Err(io::Error::other("the private descriptor has no DACL"));
        }
        Ok(dacl)
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
    use crate::platform::fs::{Files, Fs, TestFiles};

    #[test]
    fn a_directory_others_can_enter_gets_the_private_dacl() {
        let scratch = crate::test_support::temp_dir("owner-only");
        let dir = scratch.path().join("data");
        std::fs::create_dir(&dir).unwrap(); // Inherits the scratch directory's DACL: not protected, so not private.
        Files::let_others_change(&dir);
        let open = Restrictor::open(&dir).unwrap();
        assert!(Files::check_private(&open).is_err());
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
