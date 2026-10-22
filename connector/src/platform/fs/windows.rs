//! Private storage on Windows: the owner SID and the DACL, reparse points refused, the handle judged as opened.
//!
//! "Private" is a protected DACL that grants only this user, SYSTEM and Administrators, set explicitly when a file
//! or directory is created (never inherited), with this user as its owner. It is verified on the handle that was
//! opened (`GetSecurityInfo`): the owner is this user and no allow entry names anybody else. A path is never
//! followed through a reparse point (`FILE_FLAG_OPEN_REPARSE_POINT`, then `FILE_ATTRIBUTE_REPARSE_POINT` refused),
//! and only a disk file (`GetFileType`) is ever read, so a pipe or a device put in a record's place is not waited on.
//!
//! Own `unsafe` over `windows-sys`: no maintained crate covers "create with this DACL" and "judge this handle's
//! DACL" together, and the surface is a few calls.

use anyhow::{bail, Context, Result};
use std::ffi::c_void;
use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    CreateWellKnownSid, EqualSid, GetAce, WinBuiltinAdministratorsSid, WinLocalSystemSid, ACL,
    DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, CreateFileW, GetFileInformationByHandle, GetFileType,
    BY_HANDLE_FILE_INFORMATION, CREATE_NEW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_DISK,
    OPEN_ALWAYS, OPEN_EXISTING, READ_CONTROL,
};

use super::super::user::{self, UserId};

/// The access a file or directory is left with. On Windows there is only one kind: the protected private DACL
/// every file and directory made here is created with; what a file had before is not copied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode(());

impl Mode {
    pub const PRIVATE: Mode = Mode(());
    pub const PRIVATE_EXECUTABLE: Mode = Mode(());
}

pub fn mode_of(_metadata: &Metadata) -> Mode {
    Mode::PRIVATE
}

/// Nothing to do: files are created private, and a DACL is not a mode that can be copied from one file to another.
pub fn set_mode(_path: &Path, _mode: Mode) -> io::Result<()> {
    Ok(())
}

// What the ACEs of an access mask can do. Values of `winnt.h`, written out because they are the contract being
// checked and not something to be renamed under us.
const FILE_WRITE_DATA: u32 = 0x0002; // FILE_ADD_FILE on a directory
const FILE_APPEND_DATA: u32 = 0x0004; // FILE_ADD_SUBDIRECTORY on a directory
const FILE_WRITE_EA: u32 = 0x0010;
const FILE_DELETE_CHILD: u32 = 0x0040;
const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
const DELETE: u32 = 0x0001_0000;
const WRITE_DAC: u32 = 0x0004_0000;
const WRITE_OWNER: u32 = 0x0008_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const GENERIC_ALL: u32 = 0x1000_0000;

/// What lets somebody change a file's contents or who may.
const CHANGES_FILE: u32 = FILE_WRITE_DATA
    | FILE_APPEND_DATA
    | FILE_WRITE_EA
    | FILE_WRITE_ATTRIBUTES
    | DELETE
    | WRITE_DAC
    | WRITE_OWNER
    | GENERIC_WRITE
    | GENERIC_ALL;
/// What lets somebody replace what is in a directory, or take it over. Adding entries (`FILE_ADD_FILE`,
/// `FILE_ADD_SUBDIRECTORY`) is not here: there is no sticky bit, and `C:\` itself lets every user add folders; what
/// matters is that nobody else can delete or rename ours, and a directory of ours is created by us, with an owner
/// and DACL that are checked.
const REPLACES_ENTRIES: u32 = FILE_DELETE_CHILD | DELETE | WRITE_DAC | WRITE_OWNER | GENERIC_ALL;
/// Any access at all.
const ANY_ACCESS: u32 = u32::MAX;

// ACE types of a DACL (`winnt.h`).
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_DENIED_ACE_TYPE: u8 = 1;
const ACCESS_ALLOWED_OBJECT_ACE_TYPE: u8 = 5;
const ACCESS_DENIED_OBJECT_ACE_TYPE: u8 = 6;
const ACCESS_ALLOWED_CALLBACK_ACE_TYPE: u8 = 9;
const ACCESS_DENIED_CALLBACK_ACE_TYPE: u8 = 10;
const ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE: u8 = 11;
const ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE: u8 = 12;
/// The entry grants nothing to the object itself, only to what is created in it.
const INHERIT_ONLY_ACE: u8 = 0x08;

const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
const GENERIC_READ_ACCESS: u32 = 0x8000_0000;
const GENERIC_WRITE_ACCESS: u32 = 0x4000_0000;
/// `FILE_GENERIC_WRITE` without `FILE_WRITE_DATA`: what appending needs and no more.
const APPEND_ACCESS: u32 = 0x0012_0116 & !FILE_WRITE_DATA;

/// The user of this process, read once.
fn me() -> &'static UserId {
    static ME: OnceLock<UserId> = OnceLock::new();
    ME.get_or_init(user::current)
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// The path is a link; what [`is_link_error`] recognises.
#[derive(Debug)]
struct LinkError;

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a reparse point (a link)")
    }
}

impl std::error::Error for LinkError {}

fn link_error() -> io::Error {
    io::Error::other(LinkError)
}

pub fn is_link_error(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.downcast_ref::<LinkError>().is_some())
}

/// SIDs of the well-known principals allowed in a private DACL besides this user.
struct Principals {
    system: Vec<u32>,
    administrators: Vec<u32>,
}

fn well_known(kind: i32) -> Vec<u32> {
    // SECURITY_MAX_SID_SIZE is 68 bytes.
    let mut words = vec![0u32; 17];
    let mut size = 68u32;
    // SAFETY: the buffer is SECURITY_MAX_SID_SIZE bytes, DWORD-aligned, and `size` says so.
    let ok = unsafe { CreateWellKnownSid(kind, null_mut(), words.as_mut_ptr() as PSID, &mut size) };
    assert!(ok != 0, "a well-known SID can be built");
    words
}

fn principals() -> &'static Principals {
    static P: OnceLock<Principals> = OnceLock::new();
    P.get_or_init(|| Principals {
        system: well_known(WinLocalSystemSid),
        administrators: well_known(WinBuiltinAdministratorsSid),
    })
}

fn same(a: PSID, b: PSID) -> bool {
    // SAFETY: both point at valid SIDs (null is refused first).
    !a.is_null() && !b.is_null() && unsafe { EqualSid(a, b) } != 0
}

/// Whether `sid` is this user, SYSTEM or Administrators: who may hold anything in what is ours.
fn is_trusted_principal(sid: PSID) -> bool {
    let p = principals();
    me().is(sid)
        || same(sid, p.system.as_ptr() as PSID)
        || same(sid, p.administrators.as_ptr() as PSID)
}

/// The security descriptor of an open handle: owner and DACL, released on drop.
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

fn judge(file: &File) -> io::Result<Judged> {
    let mut owner: PSID = null_mut();
    let mut dacl: *mut ACL = null_mut();
    let mut descriptor: *mut c_void = null_mut();
    // SAFETY: the handle is open for the call; on success the pointers are into `descriptor`.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    Ok(Judged {
        owner,
        dacl,
        descriptor,
    })
}

impl Judged {
    /// Whether the owner is this user (or, for what an administrator's token creates, the token's default owner).
    fn owned_by_us(&self) -> bool {
        me().is(self.owner)
    }

    /// Whether the owner is this user, SYSTEM or Administrators.
    fn owned_by_trusted(&self) -> bool {
        is_trusted_principal(self.owner)
    }

    /// Whether somebody other than this user, SYSTEM and Administrators is granted anything in `mask`. No DACL at
    /// all grants everything to everybody; an entry type this does not read is taken to grant it.
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
            // SAFETY: every ACE starts with its header (type, flags, size) then, for the ones read here, the access
            // mask and the SID.
            let (kind, flags, access, sid) = unsafe {
                let header = *(ace as *const [u8; 4]);
                let access = *((ace as *const u8).add(4) as *const u32);
                (
                    header[0],
                    header[1],
                    access,
                    (ace as *const u8).add(8) as PSID,
                )
            };
            if flags & INHERIT_ONLY_ACE != 0 {
                continue;
            }
            match kind {
                ACCESS_DENIED_ACE_TYPE
                | ACCESS_DENIED_OBJECT_ACE_TYPE
                | ACCESS_DENIED_CALLBACK_ACE_TYPE
                | ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE => {}
                ACCESS_ALLOWED_ACE_TYPE => {
                    if access & mask != 0 && !is_trusted_principal(sid) {
                        return true;
                    }
                }
                // Another layout, or a condition on the grant: not read, so not trusted.
                ACCESS_ALLOWED_OBJECT_ACE_TYPE
                | ACCESS_ALLOWED_CALLBACK_ACE_TYPE
                | ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE
                    if access & mask != 0 =>
                {
                    return true;
                }
                _ => {}
            }
        }
        false
    }
}

/// The SDDL of the private descriptor: this user owns it, and it is protected (inherits nothing) and grants full
/// control to this user, SYSTEM (`SY`) and Administrators (`BA`) only.
fn private_sddl() -> io::Result<&'static Vec<u16>> {
    static SDDL: OnceLock<Vec<u16>> = OnceLock::new();
    if let Some(sddl) = SDDL.get() {
        return Ok(sddl);
    }
    let mut text: *mut u16 = null_mut();
    // SAFETY: the user's SID is valid for the process; `text` is allocated by the call and released below.
    if unsafe { ConvertSidToStringSidW(me().sid(), &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut length = 0;
    // SAFETY: `text` is a NUL-terminated wide string.
    while unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    let sid = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) });
    unsafe { LocalFree(text as *mut c_void) };
    let sddl = format!("O:{sid}D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)");
    Ok(SDDL.get_or_init(|| sddl.encode_utf16().chain(std::iter::once(0)).collect()))
}

/// The security attributes of a new private file or directory, and the descriptor they point at.
struct PrivateAttributes {
    attributes: SECURITY_ATTRIBUTES,
}

impl PrivateAttributes {
    fn new() -> io::Result<Self> {
        let mut descriptor: *mut c_void = null_mut();
        // SAFETY: the SDDL is NUL-terminated; the descriptor is allocated by the call and released on drop.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                private_sddl()?.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            attributes: SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            },
        })
    }

    fn as_ptr(&self) -> *const SECURITY_ATTRIBUTES {
        &self.attributes
    }
}

impl Drop for PrivateAttributes {
    fn drop(&mut self) {
        // SAFETY: allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW` with `LocalAlloc`.
        unsafe { LocalFree(self.attributes.lpSecurityDescriptor) };
    }
}

fn open(
    path: &Path,
    access: u32,
    attributes: *const SECURITY_ATTRIBUTES,
    disposition: u32,
    flags: u32,
) -> io::Result<File> {
    let wide = wide(path);
    // SAFETY: `wide` is NUL-terminated and outlives the call; `attributes` is null or valid.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            SHARE_ALL,
            attributes,
            disposition,
            flags,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a handle just returned by `CreateFileW`, owned by nobody else.
    Ok(unsafe { File::from_raw_handle(handle as RawHandle) })
}

fn information(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    // SAFETY: zeroed is a valid value of the plain struct the call fills.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is open for the call.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
}

fn is_reparse(info: &BY_HANDLE_FILE_INFORMATION) -> bool {
    info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn is_directory(info: &BY_HANDLE_FILE_INFORMATION) -> bool {
    info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0
}

fn is_disk(file: &File) -> bool {
    // SAFETY: the handle is open for the call.
    unsafe { GetFileType(file.as_raw_handle() as HANDLE) == FILE_TYPE_DISK }
}

/// Opens what `path` names itself (a link is not followed) for judging its owner, DACL and kind, and says what it
/// is: `None` when it is a link, which is never what is wanted.
fn open_judged(path: &Path) -> io::Result<Option<(File, BY_HANDLE_FILE_INFORMATION)>> {
    let file = open(
        path,
        READ_CONTROL | FILE_READ_ATTRIBUTES,
        null(),
        OPEN_EXISTING,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
    )?;
    let info = information(&file)?;
    if is_reparse(&info) {
        return Ok(None);
    }
    Ok(Some((file, info)))
}

/// Whether `path` is a directory (`want_directory`) or a regular file that passes `accept` on its owner and DACL.
fn judged_is(
    path: &Path,
    want_directory: bool,
    accept: impl FnOnce(&Judged) -> bool,
) -> io::Result<bool> {
    let Some((file, info)) = open_judged(path)? else {
        return Ok(false);
    };
    if is_directory(&info) != want_directory {
        return Ok(false);
    }
    Ok(accept(&judge(&file)?))
}

fn private(judged: &Judged) -> bool {
    judged.owned_by_us() && !judged.grants_others(ANY_ACCESS)
}

fn unwritable_by_others(judged: &Judged) -> bool {
    judged.owned_by_us() && !judged.grants_others(CHANGES_FILE)
}

pub fn check_private_dir(path: &Path) -> Result<()> {
    let found = judged_is(path, true, private)
        .with_context(|| format!("missing private directory {}", path.display()))?;
    if !found {
        bail!("unsafe private directory {}", path.display());
    }
    Ok(())
}

pub fn check_private_file(path: &Path) -> Result<()> {
    if !judged_is(path, false, private)? {
        bail!("unsafe private file {}", path.display());
    }
    Ok(())
}

pub fn is_user_directory(path: &Path) -> bool {
    judged_is(path, true, unwritable_by_others).unwrap_or(false)
}

pub fn is_user_file(path: &Path) -> bool {
    judged_is(path, false, unwritable_by_others).unwrap_or(false)
}

pub fn is_owned_file(path: &Path) -> bool {
    judged_is(path, false, Judged::owned_by_us).unwrap_or(false)
}

pub fn is_private_unshared_handle(file: &File) -> io::Result<bool> {
    let info = information(file)?;
    Ok(!is_directory(&info) && info.nNumberOfLinks == 1 && is_disk(file) && private(&judge(file)?))
}

pub fn has_single_link(file: &File) -> io::Result<bool> {
    Ok(information(file)?.nNumberOfLinks == 1)
}

pub fn owner_can_write(metadata: &Metadata) -> bool {
    !metadata.permissions().readonly()
}

pub fn is_trusted_handle(file: &File) -> io::Result<bool> {
    let info = information(file)?;
    Ok(!is_directory(&info)
        && !is_reparse(&info)
        && is_disk(file)
        && unwritable_by_others(&judge(file)?))
}

/// Every ancestor of `path`: this user's, SYSTEM's or Administrators', and nobody else able to delete or rename
/// what is in it, or to take it over (change its DACL or owner). A reparse point on the way (a redirected known
/// folder, a mounted volume) is followed: what is judged is the directory it leads to.
pub fn verify_ancestors(path: &Path) -> Result<()> {
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir.as_os_str().is_empty() {
            break;
        }
        let file = open(
            dir,
            READ_CONTROL | FILE_READ_ATTRIBUTES,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
        )
        .with_context(|| format!("missing {}", dir.display()))?;
        let judged = judge(&file)?;
        let owner = judged.owned_by_trusted();
        if !owner || judged.grants_others(REPLACES_ENTRIES) {
            bail!(
                "unsafe directory {}: {}",
                dir.display(),
                if owner {
                    "others can replace what is in it"
                } else {
                    "owned by another user"
                }
            );
        }
        current = dir.parent();
    }
    Ok(())
}

pub fn create_private_dir(path: &Path) -> io::Result<()> {
    let attributes = PrivateAttributes::new()?;
    let wide = wide(path);
    // SAFETY: `wide` is NUL-terminated; the attributes are valid for the call.
    if unsafe { CreateDirectoryW(wide.as_ptr(), attributes.as_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn create_private_dir_all(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    match create_private_dir(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                create_private_dir_all(parent)?;
            }
            match create_private_dir(path) {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => {
                    Ok(())
                }
                other => other,
            }
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        other => other,
    }
}

/// Refuses a handle that turned out to be a link (the open did not follow it).
fn not_a_link(file: File) -> io::Result<File> {
    if is_reparse(&information(&file)?) {
        return Err(link_error());
    }
    Ok(file)
}

pub fn open_no_follow(path: &Path) -> io::Result<File> {
    not_a_link(open(
        path,
        GENERIC_READ_ACCESS,
        null(),
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
    )?)
}

fn create_new(path: &Path) -> io::Result<File> {
    let attributes = PrivateAttributes::new()?;
    open(
        path,
        GENERIC_WRITE_ACCESS,
        attributes.as_ptr(),
        CREATE_NEW,
        FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
    )
}

pub fn create_private_new(path: &Path) -> io::Result<File> {
    create_new(path)
}

/// There is no executable bit on Windows: what runs is decided by the extension.
pub fn create_private_executable_new(path: &Path) -> io::Result<File> {
    create_new(path)
}

pub fn open_private_append(path: &Path) -> io::Result<File> {
    let attributes = PrivateAttributes::new()?;
    not_a_link(open(
        path,
        APPEND_ACCESS,
        attributes.as_ptr(),
        OPEN_ALWAYS,
        FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
    )?)
}

pub fn open_private_truncate(path: &Path) -> io::Result<File> {
    let attributes = PrivateAttributes::new()?;
    let file = not_a_link(open(
        path,
        GENERIC_WRITE_ACCESS,
        attributes.as_ptr(),
        OPEN_ALWAYS,
        FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
    )?)?;
    file.set_len(0)?;
    Ok(file)
}

pub fn open_private_lock(path: &Path) -> io::Result<File> {
    let attributes = PrivateAttributes::new()?;
    not_a_link(open(
        path,
        GENERIC_READ_ACCESS | GENERIC_WRITE_ACCESS,
        attributes.as_ptr(),
        OPEN_ALWAYS,
        FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
    )?)
}

pub fn read_user_text(path: &Path, limit: u64) -> Result<String> {
    // A link is followed (a dotfiles manager's, say) and the file it leads to is what is judged, on the handle.
    let file = open(
        path,
        GENERIC_READ_ACCESS,
        null(),
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL,
    )
    .context("open profile file")?;
    if !is_trusted_handle(&file)? || file.metadata()?.len() > limit {
        bail!("unsafe profile file");
    }
    let mut text = String::new();
    file.take(limit)
        .read_to_string(&mut text)
        .context("read profile file")?;
    Ok(text)
}

/// Windows has no way to sync a directory; a rename is as durable as the file system makes it.
pub fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Whether `path` names a program by its extension (`PATHEXT`).
fn has_executable_extension(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let list = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned());
    list.split(';')
        .filter_map(|entry| entry.strip_prefix('.'))
        .any(|entry| entry.eq_ignore_ascii_case(extension))
}

pub fn trusted_executable(path: &Path) -> Result<Metadata> {
    let Some((file, info)) = open_judged(path)? else {
        bail!("unsafe executable {}", path.display());
    };
    if is_directory(&info)
        || !is_disk(&file)
        || !has_executable_extension(path)
        || !unwritable_by_others(&judge(&file)?)
    {
        bail!("unsafe executable {}", path.display());
    }
    Ok(file.metadata()?)
}

pub fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file()) && has_executable_extension(path)
}

#[cfg(test)]
mod tests {
    use super::super::tests::Scratch;
    use super::super::*;
    use super::is_link_error;
    use std::process::Command;

    fn icacls(path: &Path, args: &[&str]) {
        let status = Command::new("icacls")
            .arg(path)
            .args(args)
            .output()
            .unwrap()
            .status;
        assert!(status.success(), "icacls {args:?}");
    }

    #[test]
    fn what_is_created_is_private_and_refused_once_somebody_else_may_enter() {
        let scratch = Scratch::new("win-private");
        let dir = scratch.0.join("data");
        ensure_private_dir(&dir).unwrap();
        check_private_dir(&dir).unwrap();
        let file = dir.join("record");
        write_private(&file, b"x").unwrap();
        check_private_file(&file).unwrap();
        // Everyone may read: no longer private, still not writable by others.
        icacls(&file, &["/grant", "*S-1-1-0:R"]);
        assert!(check_private_file(&file).is_err());
        assert!(is_user_file(&file));
        // Everyone may write.
        icacls(&file, &["/grant", "*S-1-1-0:W"]);
        assert!(!is_user_file(&file));
        assert!(read_trusted(&file, 16).is_err());
        icacls(&dir, &["/grant", "*S-1-1-0:(OI)(CI)R"]);
        assert!(check_private_dir(&dir).is_err());
        assert!(ensure_private_dir(&dir).is_err(), "refused, not repaired");
    }

    #[test]
    fn a_directory_others_can_delete_from_is_refused_as_an_ancestor() {
        let scratch = Scratch::new("win-ancestor");
        let open = scratch.0.join("open");
        std::fs::create_dir(&open).unwrap();
        icacls(&open, &["/grant", "*S-1-1-0:(OI)(CI)(DC)"]);
        assert!(verify_ancestors(&open.join("data")).is_err());
        icacls(&open, &["/remove:g", "*S-1-1-0"]);
        verify_ancestors(&open.join("data")).unwrap();
    }

    #[test]
    fn a_reparse_point_is_not_followed() {
        let scratch = Scratch::new("win-reparse");
        let target = scratch.0.join("target");
        ensure_private_dir(&target).unwrap();
        let junction = scratch.0.join("junction");
        let made = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .output()
            .unwrap()
            .status
            .success();
        assert!(made, "a junction can be made without privilege");
        assert!(check_private_dir(&junction).is_err());
        assert!(!is_user_directory(&junction));
        assert!(open_no_follow(&junction)
            .err()
            .is_some_and(|error| is_link_error(&error)));
    }

    #[test]
    fn a_second_name_is_seen_on_the_handle() {
        let scratch = Scratch::new("win-links");
        let file = scratch.0.join("lock");
        let handle = open_private_lock(&file).unwrap();
        assert!(is_private_unshared_handle(&handle).unwrap());
        std::fs::hard_link(&file, scratch.0.join("other")).unwrap();
        assert!(!has_single_link(&handle).unwrap());
        assert!(!is_private_unshared_handle(&handle).unwrap());
    }

    #[test]
    fn an_executable_needs_a_program_extension_and_nobody_else_writing() {
        let scratch = Scratch::new("win-exe");
        let program = scratch.0.join("run.exe");
        std::fs::write(&program, b"MZ").unwrap();
        trusted_executable(&program).unwrap();
        assert!(is_executable_file(&program));
        let text = scratch.0.join("notes.txt");
        std::fs::write(&text, b"x").unwrap();
        assert!(trusted_executable(&text).is_err());
        assert!(!is_executable_file(&text));
        icacls(&program, &["/grant", "*S-1-1-0:W"]);
        assert!(trusted_executable(&program).is_err());
    }
}
