//! macOS: the mount is the device and where it is mounted (`fstatfs`); only regular files are opened to be checked.

use std::ffi::CStr;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Mount {
    pub(super) device: libc::dev_t,
    mounted_at: Vec<u8>,
}

pub(super) fn mount(fd: RawFd) -> Result<Mount, &'static str> {
    let stat = super::stat_fd(fd)?;
    let mut fs_stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(fd, &mut fs_stat) } != 0 {
        return Err("cannot inspect uninstall mount");
    }
    let mounted_at = unsafe { CStr::from_ptr(fs_stat.f_mntonname.as_ptr()) }
        .to_bytes()
        .to_vec();
    Ok(Mount {
        device: stat.st_dev,
        mounted_at,
    })
}

/// `name` in `fd`, opened to be checked against `before`: regular files only (`None` for the rest).
pub(super) fn open_entry(
    fd: RawFd,
    name: &CStr,
    before: &libc::stat,
) -> Result<Option<OwnedFd>, &'static str> {
    if before.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Ok(None);
    }
    let child = unsafe {
        libc::openat(
            fd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if child < 0 {
        return Err("uninstall entry replaced");
    }
    Ok(Some(unsafe { OwnedFd::from_raw_fd(child) }))
}
