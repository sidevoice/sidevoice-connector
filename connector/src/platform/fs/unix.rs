//! Unix: owner and mode bits, `O_NOFOLLOW`, the descriptor judged as opened. No `unsafe` of ours.

// The raw calls the rest of the crate is not allowed to make (`clippy.toml`) are made here.
#![allow(clippy::disallowed_methods)]

use super::{Create, Others};
use crate::platform::user::{User, UserId};
use anyhow::{bail, Result};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

/// This OS's files.
pub struct Os;

impl super::Fs for Os {
    fn create_private_dir(path: &Path) -> io::Result<()> {
        DirBuilder::new().mode(0o700).create(path)
    }

    fn open_private(path: &Path, how: Create) -> io::Result<File> {
        let mut options = OpenOptions::new();
        match how {
            Create::New => options.write(true).create_new(true).mode(0o600),
            Create::NewProgram => options.write(true).create_new(true).mode(0o700),
            Create::Append => options.append(true).create(true).mode(0o600),
            Create::ReadWrite => options
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600),
        };
        options.custom_flags(libc::O_NOFOLLOW).open(path)
    }

    fn open_read(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(path)
    }

    fn others_of(file: &File) -> io::Result<Others> {
        Ok(others(file.metadata()?.mode()))
    }
}

/// How many names the file behind `file` has.
pub(super) fn link_count(file: &File) -> io::Result<u64> {
    Ok(file.metadata()?.nlink())
}

/// A directory on the way to something private: the entry itself (a link is judged as a link); this user's or
/// root's; others writing is harmless only under the sticky bit (`/tmp`), where nobody can remove or rename what is
/// not theirs.
pub(super) fn check_ancestor(dir: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(dir)?;
    if !UserId::current().owns_path(dir)? && metadata.uid() != 0 {
        bail!("unsafe directory {}: owned by another user", dir.display());
    }
    if metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0 {
        bail!(
            "unsafe directory {}: others can replace what is in it",
            dir.display()
        );
    }
    Ok(())
}

/// How a record is written whole: a new file `0600`, an existing one's mode kept when `keep_access`.
pub(super) fn atomic(keep_access: bool) -> atomic_write_file::OpenOptions {
    use atomic_write_file::unix::OpenOptionsExt as _;
    let mut options = atomic_write_file::OpenOptions::new();
    options
        .mode(0o600)
        .preserve_mode(keep_access)
        .preserve_owner(false);
    options
}

fn others(mode: u32) -> Others {
    if mode & 0o022 != 0 {
        Others::Change
    } else if mode & 0o044 != 0 || mode & 0o011 != 0 {
        Others::Read
    } else {
        Others::Nothing
    }
}

/// Lets group and others change `path` (tests).
#[cfg(test)]
pub(super) fn let_others_change(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path).unwrap().mode() | 0o077;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}
