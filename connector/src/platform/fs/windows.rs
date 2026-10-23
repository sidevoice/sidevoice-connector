//! Windows: a protected DACL set when a file or directory is created (a record too: staged under that DACL, then
//! renamed over the record), read back from the handle; reparse points (links, junctions, mount points) refused, at
//! the path and on the way to it.
//!
//! "Private" is a protected DACL (it inherits nothing) granting full control to the owner (`OWNER RIGHTS`), SYSTEM
//! and Administrators only, inherited by what is created inside. What others may do is read from the DACL of the
//! handle that was opened (`GetSecurityInfo`): an allow entry for anybody but the file's owner, this user, SYSTEM,
//! Administrators or `OWNER RIGHTS` counts; and, for whether it is private, so do a DACL that is not protected (it
//! takes whatever its parent passes down) and an entry for others that only what is created inside inherits. Who the
//! owner is, is `platform::user`'s. The rule is `sidevoice-platform`'s (its `fs/windows.rs`, sidevoice-core#170), so
//! the two can be made one.
//!
//! Own `unsafe` over `windows-sys`: no maintained crate covers "create with this DACL" and "read what this handle's
//! DACL grants" (`windows-acl` is unmaintained; `windows-permissions` is a thin binding with the same calls left to
//! us), and the surface is a few calls. This user's SID is read from the process token here because
//! `sidevoice-platform`'s `user` does not offer comparing a SID with this user. The link count (and the identity of a
//! file system with no 128-bit ids) comes from `winapi-util`; a file's identity is `FileIdInfo`, one more call. A
//! record's staging file (a fresh name beside it, removed on failure, renamed over it) is `tempfile`'s.

use super::{Create, FileId, Others};
use crate::platform::user::{User, UserId};
use anyhow::{bail, Context, Result};
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::path::Path;
use std::ptr::null_mut;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{
    CloseHandle, LocalFree, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED,
    ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    ConvertStringSidToSidW, GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    EqualSid, GetAce, GetSecurityDescriptorControl, GetTokenInformation, TokenUser, ACL,
    DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES,
    SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, CreateFileW, FileIdInfo, GetFileInformationByHandleEx, CREATE_NEW,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_128, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_ALWAYS, READ_CONTROL,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

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

    // `Change`: an entry for others that applies to the object itself. Anything else short of private is `Read`: an
    // entry for others of any kind, including one only what is created inside inherits (inherit-only too), and a DACL
    // that is not protected, which takes whatever its parent passes down, now or later.
    fn others_of(file: &File) -> io::Result<Others> {
        let judged = Judged::of(file)?;
        Ok(if judged.grants_others(CHANGES, false) {
            Others::Change
        } else if !judged.protected()? || judged.grants_others(u32::MAX, true) {
            Others::Read
        } else {
            Others::Nothing
        })
    }

    // The 128-bit file id (`FileIdInfo`): a 64-bit index is not unique on ReFS. A file system that has no such id
    // (the call is refused as unsupported) is told by `GetFileInformationByHandle`'s 64-bit index, through
    // `winapi-util`.
    fn identity(file: &File) -> io::Result<FileId> {
        let mut info = FILE_ID_INFO {
            VolumeSerialNumber: 0,
            FileId: FILE_ID_128 {
                Identifier: [0; 16],
            },
        };
        // SAFETY: the handle is open for the call; `info` is a `FILE_ID_INFO` of the size passed.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle() as HANDLE,
                FileIdInfo,
                &mut info as *mut FILE_ID_INFO as *mut c_void,
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if ok != 0 {
            return Ok(FileId {
                volume: info.VolumeSerialNumber,
                index: u128::from_le_bytes(info.FileId.Identifier),
            });
        }
        let error = io::Error::last_os_error();
        if !matches!(
            error.raw_os_error().map(|code| code as u32),
            Some(ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED | ERROR_INVALID_FUNCTION)
        ) {
            return Err(error);
        }
        let information = winapi_util::file::information(file)?;
        Ok(FileId {
            volume: information.volume_serial_number(),
            index: u128::from(information.file_index()),
        })
    }
}

/// How many names the file behind `file` has (`winapi-util`).
pub(super) fn link_count(file: &File) -> io::Result<u64> {
    Ok(winapi_util::file::information(file)?.number_of_links())
}

/// A directory on the way to something private (`sidevoice-platform`'s rule): this user's or the system's, nobody
/// but this user and the system able to delete or rename what is in it, or to take it over. Entries only what is
/// created inside inherits are not judged here (`C:\` passes Modify down to Authenticated Users): what is ours is
/// created with its own DACL. A reparse point on the way (junction, symlink, mount point, a redirected folder) is
/// refused, not followed: only the path as written is judged, and one leading into a directory others can change would
/// hide it. A default profile has none on the way to its folders (`C:\Users\<name>\AppData\Local`): the legacy
/// junctions (`Application Data`, `Local Settings`, `C:\Documents and Settings`) sit beside that path, not on it.
/// Adding entries is not judged: there is no sticky bit, and `C:\` lets every user add folders.
pub(super) fn check_ancestor(dir: &Path) -> Result<()> {
    let file = OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(dir)
        .with_context(|| format!("missing {}", dir.display()))?;
    if is_link(&file)? {
        bail!("unsafe directory {}: a link on the way", dir.display());
    }
    let judged = Judged::of(&file)?;
    if !UserId::current().owns_file(&file)? && !judged.owner_is_system() {
        bail!("unsafe directory {}: owned by another user", dir.display());
    }
    if judged.grants_others(REPLACES_ENTRIES, false) {
        bail!(
            "unsafe directory {}: others can replace what is in it",
            dir.display()
        );
    }
    Ok(())
}

/// Writes `bytes` as `path` whole or not at all: staged in a new file beside it, created private
/// ([`Fs::open_private`](super::Fs::open_private): its own protected DACL, nothing taken from the directory) before
/// any byte is written, synced, then renamed over `path`. An existing file's DACL is not carried over to its
/// replacement, which is private (`keep_access` is Unix's).
pub(super) fn write_whole(path: &Path, bytes: &[u8], _keep_access: bool) -> io::Result<()> {
    use super::Fs as _;
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut staged =
        tempfile::Builder::new().make_in(dir, |temp| Os::open_private(temp, Create::New))?;
    staged.write_all(bytes)?;
    staged.as_file().sync_all()?;
    staged.persist(path).map_err(|error| error.error)?;
    Ok(())
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

/// Refuses a handle that turned out to be a link of any kind (the open did not follow it).
fn not_a_link(file: File) -> io::Result<File> {
    if is_link(&file)? {
        return Err(io::Error::other("a link (reparse point)"));
    }
    Ok(file)
}

/// Whether a handle opened with `OPEN_REPARSE_POINT` is a reparse point of any kind: a symlink, a junction, a mount
/// point.
fn is_link(file: &File) -> io::Result<bool> {
    let metadata = file.metadata()?;
    Ok(metadata.is_symlink() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

/// An owned SID, allocated by `ConvertStringSidToSidW`.
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

    /// This process's user: the token's `TokenUser`, copied through its string form.
    fn user() -> io::Result<Sid> {
        let mut token: HANDLE = null_mut();
        // SAFETY: the pseudo-handle of the current process needs no closing; the token is closed below.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let sid = Self::token_user(token);
        // SAFETY: the token opened above, closed once.
        unsafe { CloseHandle(token) };
        sid
    }

    fn token_user(token: HANDLE) -> io::Result<Sid> {
        let mut needed = 0u32;
        // SAFETY: a null buffer of length 0 asks only for the size (it fails with INSUFFICIENT_BUFFER).
        unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut needed) };
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }
        // `u64` words: `TOKEN_USER` holds a pointer, so the buffer must be pointer-aligned.
        let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: the buffer is at least `needed` bytes.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr() as *mut c_void,
                needed,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: on success the buffer starts with a `TOKEN_USER` whose SID points inside the buffer.
        let user = unsafe { (*(buffer.as_ptr() as *const TOKEN_USER)).User.Sid };
        let mut text: *mut u16 = null_mut();
        // SAFETY: `user` is a valid SID while `buffer` lives; on success `text` is allocated with `LocalAlloc`.
        if unsafe { ConvertSidToStringSidW(user, &mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut sid: PSID = null_mut();
        // SAFETY: `text` is the NUL-terminated string just made; on success `sid` is allocated with `LocalAlloc`.
        let ok = unsafe { ConvertStringSidToSidW(text, &mut sid) };
        // SAFETY: allocated by `ConvertSidToStringSidW` with `LocalAlloc`, freed once.
        unsafe { LocalFree(text as *mut c_void) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Sid(sid))
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

/// This user, SYSTEM, Administrators and `OWNER RIGHTS`: who may hold anything in what is ours besides its owner; and
/// TrustedInstaller, which owns `C:\` and the system's folders.
struct Principals {
    user: Sid,
    system: Sid,
    administrators: Sid,
    owner_rights: Sid,
    trusted_installer: Sid,
}

fn principals() -> &'static Principals {
    static PRINCIPALS: OnceLock<Principals> = OnceLock::new();
    PRINCIPALS.get_or_init(|| Principals {
        user: Sid::user().expect("the process token is readable"),
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

    /// Whether the DACL is protected: it takes nothing its parent passes down.
    fn protected(&self) -> io::Result<bool> {
        let (mut control, mut revision) = (0u16, 0u32);
        // SAFETY: `descriptor` is the valid descriptor `GetSecurityInfo` returned, alive as long as `self`.
        if unsafe { GetSecurityDescriptorControl(self.descriptor, &mut control, &mut revision) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(control & SE_DACL_PROTECTED != 0)
    }

    /// Whether the owner is the system: SYSTEM, Administrators or TrustedInstaller.
    fn owner_is_system(&self) -> bool {
        let p = principals();
        p.system.is(self.owner)
            || p.administrators.is(self.owner)
            || p.trusted_installer.is(self.owner)
    }

    /// Who may hold anything in what is ours: its owner; this user, whoever the owner is (a profile folder owned by
    /// Administrators or SYSTEM that grants its user full control is a default layout); SYSTEM, Administrators and
    /// `OWNER RIGHTS`.
    fn trusted(&self, sid: PSID) -> bool {
        let p = principals();
        // SAFETY: both point at valid SIDs (null is refused first).
        (!self.owner.is_null() && !sid.is_null() && unsafe { EqualSid(self.owner, sid) } != 0)
            || p.user.is(sid)
            || p.system.is(sid)
            || p.administrators.is(sid)
            || p.owner_rights.is(sid)
    }

    /// Whether somebody untrusted is granted anything in `mask`, counting the entries only what is created inside
    /// inherits when `inherit_only` says so. No DACL at all grants everything to everybody; an allow entry of a layout
    /// not read here is taken to grant it.
    fn grants_others(&self, mask: u32, inherit_only: bool) -> bool {
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
            if (flags & INHERIT_ONLY_ACE != 0 && !inherit_only) || access & mask == 0 {
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

#[cfg(any(test, feature = "test-support"))]
impl super::TestFiles for Os {
    fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) {
        let (target, link) = (target.as_ref(), link.as_ref());
        if target.is_dir() {
            std::os::windows::fs::symlink_dir(target, link).unwrap();
        } else {
            std::os::windows::fs::symlink_file(target, link).unwrap();
        }
    }

    // The extension decides what runs: nothing to mark.
    fn make_executable(_: &Path) {}

    fn let_others_read(path: &Path) {
        icacls(path, &["/grant", "*S-1-1-0:(RX)"]);
    }

    fn let_others_change(path: &Path) {
        icacls(path, &["/grant", "*S-1-1-0:(F)"]);
    }

    // Inheritance off first (what was inherited is kept as the file's own entries and the DACL protected), so a file a
    // test wrote with `std` in a scratch directory ends up as private as one `open_private` makes.
    fn shut_others_out(path: &Path) {
        icacls(path, &["/inheritance:d"]);
        icacls(path, &["/remove:g", "*S-1-1-0"]);
    }
}

/// `icacls <path> <args>` (tests), which must succeed. Everyone is `S-1-1-0`.
#[cfg(any(test, feature = "test-support"))]
pub(super) fn icacls(path: &Path, args: &[&str]) {
    let status = std::process::Command::new("icacls")
        .arg(path)
        .args(args)
        .output()
        .unwrap()
        .status;
    assert!(status.success(), "icacls {args:?}");
}

/// Makes `link` lead to the directory `target`: a junction, which needs no privilege (tests).
#[cfg(test)]
pub(super) fn link_dir(target: &Path, link: &Path) {
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap()
        .status;
    assert!(status.success(), "mklink /J");
}
