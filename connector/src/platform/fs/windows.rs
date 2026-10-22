//! Windows: a protected DACL set when a file or directory is created, read back from the handle; reparse points
//! (links, junctions) refused.
//!
//! "Private" is a protected DACL (it inherits nothing) granting full control to the owner (`OWNER RIGHTS`), SYSTEM
//! and Administrators only, inherited by what is created inside. What others may do is read from the DACL of the
//! handle that was opened (`GetSecurityInfo`): an allow entry for anybody but the file's owner, SYSTEM,
//! Administrators or `OWNER RIGHTS` counts; who the owner is, is `platform::user`'s.
//!
//! Own `unsafe` over `windows-sys`: no maintained crate covers "create with this DACL" and "read what this handle's
//! DACL grants" (`windows-acl` is unmaintained; `windows-permissions` is a thin binding with the same calls left to
//! us), and the surface is a few calls. The link count and a file's identity come from `winapi-util`.

use super::{Create, FileId, Others};
use crate::platform::user::{User, UserId};
use anyhow::{bail, Context, Result};
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::path::Path;
use std::ptr::null_mut;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    EqualSid, GetAce, ACL, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSID,
    SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_ALWAYS, READ_CONTROL,
};

/// This OS's files.
pub struct Os;

impl super::Fs for Os {
    fn create_private_dir(path: &Path) -> io::Result<()> {
        let attributes = PrivateAttributes::new()?;
        let wide = wide(path);
        // SAFETY: `wide` is NUL-terminated; the attributes are valid for the call.
        if unsafe { CreateDirectoryW(wide.as_ptr(), attributes.as_ptr()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    // `CreateFileW` because `std` cannot pass the security attributes of what it creates.
    fn open_private(path: &Path, how: Create) -> io::Result<File> {
        let (access, disposition) = match how {
            Create::New | Create::NewProgram => (GENERIC_WRITE, CREATE_NEW),
            Create::Append => (APPEND, OPEN_ALWAYS),
            Create::ReadWrite => (GENERIC_READ | GENERIC_WRITE, OPEN_ALWAYS),
        };
        let attributes = PrivateAttributes::new()?;
        let wide = wide(path);
        // SAFETY: `wide` is NUL-terminated and the attributes valid for the call.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                attributes.as_ptr(),
                disposition,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a handle just returned by `CreateFileW`, owned by nobody else.
        not_a_link(unsafe { File::from_raw_handle(handle as RawHandle) })
    }

    // `BACKUP_SEMANTICS` lets a directory be opened too.
    fn open_read(path: &Path) -> io::Result<File> {
        not_a_link(
            OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(path)?,
        )
    }

    fn others_of(file: &File) -> io::Result<Others> {
        let judged = Judged::of(file)?;
        Ok(if judged.grants_others(CHANGES) {
            Others::Change
        } else if judged.grants_others(u32::MAX) {
            Others::Read
        } else {
            Others::Nothing
        })
    }

    // `GetFileInformationByHandle` through `winapi-util`.
    fn identity(file: &File) -> io::Result<FileId> {
        let information = winapi_util::file::information(file)?;
        Ok(FileId {
            volume: information.volume_serial_number(),
            index: information.file_index(),
        })
    }
}

/// How many names the file behind `file` has (`winapi-util`).
pub(super) fn link_count(file: &File) -> io::Result<u64> {
    Ok(winapi_util::file::information(file)?.number_of_links())
}

/// A directory on the way to something private: this user's or the system's, nobody else able to delete or rename
/// what is in it, or to take it over. A reparse point on the way (a redirected known folder, a mounted volume) is
/// followed: what is judged is the directory it leads to. Adding entries is not judged: there is no sticky bit, and
/// `C:\` lets every user add folders.
pub(super) fn check_ancestor(dir: &Path) -> Result<()> {
    let file = open_for_security(dir).with_context(|| format!("missing {}", dir.display()))?;
    let judged = Judged::of(&file)?;
    if !UserId::current().owns_file(&file)? && !judged.owner_is_system() {
        bail!("unsafe directory {}: owned by another user", dir.display());
    }
    if judged.grants_others(REPLACES_ENTRIES) {
        bail!(
            "unsafe directory {}: others can replace what is in it",
            dir.display()
        );
    }
    Ok(())
}

/// How a record is written whole. Nothing to set: what is created takes its directory's inheritable entries,
/// private in a private directory; an existing file's DACL is not carried over to its replacement.
pub(super) fn atomic(_keep_access: bool) -> atomic_write_file::OpenOptions {
    atomic_write_file::OpenOptions::new()
}

// Access rights (`winnt.h`), written out because they are the contract being checked.
const FILE_WRITE_DATA: u32 = 0x0002; // FILE_ADD_FILE on a directory
const FILE_APPEND_DATA: u32 = 0x0004; // FILE_ADD_SUBDIRECTORY on a directory
const FILE_WRITE_EA: u32 = 0x0010;
const FILE_DELETE_CHILD: u32 = 0x0040;
const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
const DELETE: u32 = 0x0001_0000;
const WRITE_DAC: u32 = 0x0004_0000;
const WRITE_OWNER: u32 = 0x0008_0000;
const GENERIC_ALL: u32 = 0x1000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const GENERIC_READ: u32 = 0x8000_0000;
/// `FILE_GENERIC_WRITE` without `FILE_WRITE_DATA`: what appending needs and no more.
const APPEND: u32 = 0x0012_0116 & !FILE_WRITE_DATA;

/// What lets somebody change a file or what is in a directory, or who may.
const CHANGES: u32 = FILE_WRITE_DATA
    | FILE_APPEND_DATA
    | FILE_WRITE_EA
    | FILE_DELETE_CHILD
    | FILE_WRITE_ATTRIBUTES
    | DELETE
    | WRITE_DAC
    | WRITE_OWNER
    | GENERIC_WRITE
    | GENERIC_ALL;
/// What lets somebody replace what is in a directory, or take it over.
const REPLACES_ENTRIES: u32 = FILE_DELETE_CHILD | DELETE | WRITE_DAC | WRITE_OWNER | GENERIC_ALL;

// ACE types and flags of a DACL (`winnt.h`).
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_ALLOWED_OBJECT_ACE_TYPE: u8 = 5;
const ACCESS_ALLOWED_CALLBACK_ACE_TYPE: u8 = 9;
const ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE: u8 = 11;
/// The entry grants nothing to the object itself, only to what is created in it.
const INHERIT_ONLY_ACE: u8 = 0x08;

/// The private descriptor: protected, full control to the owner, SYSTEM and Administrators, inherited by what is
/// created inside. The owner is left to the system: the token's default owner, this user.
const PRIVATE_SDDL: &str = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Opens the directory `path` leads to, for reading its security descriptor.
fn open_for_security(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

/// Refuses a handle that turned out to be a link (the open did not follow it).
fn not_a_link(file: File) -> io::Result<File> {
    if file.metadata()?.is_symlink() {
        return Err(io::Error::other("a link (reparse point)"));
    }
    Ok(file)
}

/// An owned SID, parsed from its string form.
struct Sid(PSID);

// SAFETY: the SID is immutable once made and freed only on drop.
unsafe impl Send for Sid {}
unsafe impl Sync for Sid {}

impl Sid {
    fn parse(text: &str) -> Sid {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sid: PSID = null_mut();
        // SAFETY: `wide` is NUL-terminated; on success `sid` is allocated with `LocalAlloc`.
        let ok = unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut sid) };
        assert!(ok != 0, "a well-known SID parses");
        Sid(sid)
    }

    fn is(&self, other: PSID) -> bool {
        // SAFETY: both point at valid SIDs (null is refused first).
        !other.is_null() && unsafe { EqualSid(self.0, other) } != 0
    }
}

impl Drop for Sid {
    fn drop(&mut self) {
        // SAFETY: allocated by `ConvertStringSidToSidW` with `LocalAlloc`.
        unsafe { LocalFree(self.0) };
    }
}

/// SYSTEM, Administrators, `OWNER RIGHTS` and TrustedInstaller (which owns `C:\` and the system's folders).
struct Principals {
    system: Sid,
    administrators: Sid,
    owner_rights: Sid,
    trusted_installer: Sid,
}

fn principals() -> &'static Principals {
    static PRINCIPALS: OnceLock<Principals> = OnceLock::new();
    PRINCIPALS.get_or_init(|| Principals {
        system: Sid::parse("SY"),
        administrators: Sid::parse("BA"),
        owner_rights: Sid::parse("OW"),
        trusted_installer: Sid::parse(
            "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
        ),
    })
}

/// The owner and DACL of an open handle, released on drop.
struct Judged {
    owner: PSID,
    dacl: *mut ACL,
    descriptor: *mut c_void,
}

impl Drop for Judged {
    fn drop(&mut self) {
        // SAFETY: allocated by `GetSecurityInfo` with `LocalAlloc`; `owner` and `dacl` point inside it.
        unsafe { LocalFree(self.descriptor) };
    }
}

impl Judged {
    fn of(file: &File) -> io::Result<Judged> {
        let mut judged = Judged {
            owner: null_mut(),
            dacl: null_mut(),
            descriptor: null_mut(),
        };
        // SAFETY: the handle is open for the call; on success the pointers are into `descriptor`.
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle() as HANDLE,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut judged.owner,
                null_mut(),
                &mut judged.dacl,
                null_mut(),
                &mut judged.descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(judged)
    }

    fn owner_is_system(&self) -> bool {
        let p = principals();
        p.system.is(self.owner)
            || p.administrators.is(self.owner)
            || p.trusted_installer.is(self.owner)
    }

    /// Who may hold anything in what is ours: its owner, SYSTEM, Administrators and `OWNER RIGHTS`.
    fn trusted(&self, sid: PSID) -> bool {
        let p = principals();
        // SAFETY: both point at valid SIDs (null is refused first).
        (!self.owner.is_null() && !sid.is_null() && unsafe { EqualSid(self.owner, sid) } != 0)
            || p.system.is(sid)
            || p.administrators.is(sid)
            || p.owner_rights.is(sid)
    }

    /// Whether somebody untrusted is granted anything in `mask`. No DACL at all grants everything to everybody; an
    /// allow entry of a layout not read here is taken to grant it.
    fn grants_others(&self, mask: u32) -> bool {
        if self.dacl.is_null() {
            return true;
        }
        // SAFETY: `dacl` points at a valid ACL inside the descriptor, which lives as long as `self`.
        let count = unsafe { (*self.dacl).AceCount };
        for index in 0..u32::from(count) {
            let mut ace: *mut c_void = null_mut();
            // SAFETY: `index` is below the ACL's entry count.
            if unsafe { GetAce(self.dacl, index, &mut ace) } == 0 || ace.is_null() {
                return true;
            }
            // SAFETY: every ACE starts with its header (type, flags, size), then the access mask; for the plain
            // allow entry the SID follows.
            let (kind, flags, access) = unsafe {
                let header = *(ace as *const [u8; 4]);
                (
                    header[0],
                    header[1],
                    *((ace as *const u8).add(4) as *const u32),
                )
            };
            if flags & INHERIT_ONLY_ACE != 0 || access & mask == 0 {
                continue;
            }
            match kind {
                ACCESS_ALLOWED_ACE_TYPE => {
                    // SAFETY: as above.
                    let sid = unsafe { (ace as *const u8).add(8) as PSID };
                    if !self.trusted(sid) {
                        return true;
                    }
                }
                ACCESS_ALLOWED_OBJECT_ACE_TYPE
                | ACCESS_ALLOWED_CALLBACK_ACE_TYPE
                | ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE => return true,
                // Deny entries and the rest grant nothing.
                _ => {}
            }
        }
        false
    }
}

/// The security attributes of a new private file or directory, and the descriptor they point at.
struct PrivateAttributes(SECURITY_ATTRIBUTES);

impl PrivateAttributes {
    fn new() -> io::Result<Self> {
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
        Ok(Self(SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        }))
    }

    fn as_ptr(&self) -> *const SECURITY_ATTRIBUTES {
        &self.0
    }
}

impl Drop for PrivateAttributes {
    fn drop(&mut self) {
        // SAFETY: allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW` with `LocalAlloc`.
        unsafe { LocalFree(self.0.lpSecurityDescriptor) };
    }
}

/// Lets everybody change `path` (tests).
#[cfg(test)]
pub(super) fn let_others_change(path: &Path) {
    let status = std::process::Command::new("icacls")
        .arg(path)
        .args(["/grant", "*S-1-1-0:(F)"])
        .output()
        .unwrap()
        .status;
    assert!(status.success(), "icacls");
}
