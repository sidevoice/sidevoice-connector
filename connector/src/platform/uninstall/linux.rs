//! Linux: the mount is the device and the mount id from `/proc/self/fdinfo`.

use std::os::fd::RawFd;

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
