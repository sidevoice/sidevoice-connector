//! Linux: nothing beyond the mode. With an extended POSIX ACL the group bits are its mask, so `0700` leaves every
//! named user and group without access; `sidevoice-platform` judges it that way too.

use std::fs::File;
use std::io;

pub(super) fn remove_extended(_dir: &File) -> io::Result<()> {
    Ok(())
}
