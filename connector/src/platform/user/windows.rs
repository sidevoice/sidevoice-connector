//! Windows: the user's SID, read from the process token, compared with `EqualSid` against the owner of a file's
//! security descriptor.
//!
//! Own `unsafe` over `windows-sys` rather than a wrapper crate: the maintained crates around (`whoami` yields
//! names, not the SID; `windows-acl` and `sid` are unmaintained or bring the whole ACL model) do not cover "this
//! process's SID" plus "owner of this file" together, and the surface needed is a handful of calls.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, ERROR_SUCCESS, HANDLE};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    EqualSid, GetLengthSid, GetTokenInformation, TokenOwner, TokenUser, OWNER_SECURITY_INFORMATION,
    PSID, TOKEN_INFORMATION_CLASS, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, READ_CONTROL,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// An owned SID. A SID is a DWORD-aligned structure: the words keep that alignment.
#[derive(Clone, Debug)]
struct Sid {
    words: Vec<u32>,
}

impl Sid {
    /// Copies the SID `sid` points at.
    ///
    /// # Safety
    /// `sid` must point at a valid SID.
    unsafe fn copy_of(sid: PSID) -> Sid {
        let len = GetLengthSid(sid) as usize;
        let mut words = vec![0u32; len.div_ceil(4)];
        std::ptr::copy_nonoverlapping(sid as *const u8, words.as_mut_ptr() as *mut u8, len);
        Sid { words }
    }

    fn as_psid(&self) -> PSID {
        self.words.as_ptr() as PSID
    }

    fn is(&self, other: PSID) -> bool {
        // SAFETY: `self` holds a copy of a valid SID and the caller passes a valid one.
        !other.is_null() && unsafe { EqualSid(self.as_psid(), other) } != 0
    }
}

impl PartialEq for Sid {
    fn eq(&self, other: &Self) -> bool {
        self.is(other.as_psid())
    }
}

impl Eq for Sid {}

/// Windows side of [`super::UserId`]: the user as owned SIDs.
///
/// Two SIDs, because Windows gives a file two candidate owners: the user, and the token's default owner, which is
/// what files this process creates get. For a normal token they are the same; for an elevated administrator the
/// default owner is the Administrators group, and what it creates is its own. Identity (`==`) is the user alone.
#[derive(Clone, Debug)]
pub(super) struct Id {
    user: Sid,
    default_owner: Sid,
}

impl Id {
    // A symlink or junction is judged itself (`OPEN_REPARSE_POINT`).
    pub(super) fn owns_path(&self, path: &Path) -> io::Result<bool> {
        // `BACKUP_SEMANTICS` lets a directory be opened; `READ_CONTROL` is all the owner query needs.
        let file = OpenOptions::new()
            .access_mode(READ_CONTROL)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        self.owns_file(&file)
    }

    pub(super) fn owns_file(&self, file: &File) -> io::Result<bool> {
        let mut owner: PSID = null_mut();
        let mut descriptor = null_mut();
        // SAFETY: the handle is open for the call's duration; on success `owner` points inside `descriptor`, which
        // is released below after the comparison.
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle() as HANDLE,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                null_mut(),
                null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let mine = self.user.is(owner) || self.default_owner.is(owner);
        unsafe { LocalFree(descriptor) };
        Ok(mine)
    }
}

impl PartialEq for Id {
    fn eq(&self, other: &Self) -> bool {
        self.user == other.user
    }
}

impl Eq for Id {}

/// The user of this process: the SIDs in its token.
pub(super) fn current() -> Id {
    read_token().expect("the process token is readable")
}

fn read_token() -> io::Result<Id> {
    let mut token: HANDLE = null_mut();
    // SAFETY: the pseudo-handle of the current process needs no closing; the token is closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let sids =
        token_sid(token, TokenUser).and_then(|user| Ok((user, token_sid(token, TokenOwner)?)));
    unsafe { CloseHandle(token) };
    let (user, default_owner) = sids?;
    Ok(Id {
        user,
        default_owner,
    })
}

/// The SID of a `TokenUser` or `TokenOwner` query: both structs are a single SID pointer.
fn token_sid(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<Sid> {
    let mut needed = 0u32;
    // SAFETY: a null buffer of length 0 asks only for the size (it fails with INSUFFICIENT_BUFFER).
    unsafe { GetTokenInformation(token, class, null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    // `u64` words: the struct holds a pointer, so the buffer must be pointer-aligned.
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: the buffer is at least `needed` bytes.
    let ok = unsafe {
        GetTokenInformation(
            token,
            class,
            buffer.as_mut_ptr() as *mut _,
            needed,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: success filled the buffer with a TOKEN_USER / TOKEN_OWNER (same layout) whose SID points inside it.
    Ok(unsafe { Sid::copy_of((*(buffer.as_ptr() as *const TOKEN_USER)).User.Sid) })
}

/// Windows names the user to the system by SID; there is no login name any caller needs.
pub(super) fn name() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_is_a_valid_sid() {
        let me = current();
        // SAFETY: the SID is one this module copied from a valid one.
        let len = unsafe { GetLengthSid(me.user.as_psid()) };
        assert!(len >= 8, "a SID is at least 8 bytes, got {len}");
        assert_eq!(me, me.clone());
    }

    #[test]
    fn does_not_own_what_another_sid_owns() {
        // Local System, S-1-5-18: revision 1, one sub-authority, NT authority, 18. Neither a user nor an
        // administrator's default owner, so nothing a test creates is its.
        let system = Sid {
            words: vec![0x0000_0101, 0x0500_0000, 0x0000_0012],
        };
        let stranger = Id {
            user: system.clone(),
            default_owner: system,
        };
        assert_ne!(current(), stranger);
        let dir =
            std::env::temp_dir().join(format!("sv-platform-user-win-other-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!stranger.owns_path(&dir).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
