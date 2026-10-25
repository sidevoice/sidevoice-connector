//! macOS: the mount is the device and where it is mounted (`fstatfs`).

use std::ffi::CStr;
use std::os::fd::RawFd;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Mount {
    pub(super) device: libc::dev_t,
    mounted_at: Vec<u8>,
}

pub(super) fn mount(fd: RawFd) -> Result<Mount, &'static str> {
    let stat = super::imp::stat_fd(fd)?;
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
