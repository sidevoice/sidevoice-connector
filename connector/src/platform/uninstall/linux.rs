//! Linux: the mount is the device and the mount id from `/proc/self/fdinfo`; any entry can be opened with `O_PATH`.

use std::ffi::CStr;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Mount {
    pub(super) device: libc::dev_t,
    id: u64,
}

pub(super) fn mount(fd: RawFd) -> Result<Mount, &'static str> {
    let stat = super::imp::stat_fd(fd)?;
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))
        .map_err(|_| "cannot inspect uninstall mount")?;
    let id = info
        .lines()
        .find_map(|line| line.strip_prefix("mnt_id:\t"))
        .and_then(|id| id.trim().parse().ok())
        .ok_or("cannot inspect uninstall mount")?;
    Ok(Mount {
        device: stat.st_dev,
        id,
    })
}

/// `name` in `fd`, opened to be checked against `before`: any kind of entry, by `O_PATH`.
pub(super) fn open_entry(
    fd: RawFd,
    name: &CStr,
    _before: &libc::stat,
) -> Result<Option<OwnedFd>, &'static str> {
    let child = unsafe {
        libc::openat(
            fd,
            name.as_ptr(),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if child < 0 {
        return Err("uninstall entry replaced");
    }
    Ok(Some(unsafe { OwnedFd::from_raw_fd(child) }))
}
