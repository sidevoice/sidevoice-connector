//! Private storage on Unix: owner and mode bits, `O_NOFOLLOW`, the descriptor judged as opened.

// The raw calls the rest of the crate is not allowed to make (`clippy.toml`) are made here.
#![allow(clippy::disallowed_methods)]

use anyhow::{bail, Context, Result};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// The access a file or directory is left with, in the host's own terms. Opaque: callers name what they want
/// (`PRIVATE`) or copy what a file had ([`mode_of`], [`set_mode`]) and never see what it is made of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode(u32);

impl Mode {
    /// A file of ours: this user's, open to nobody else.
    pub const PRIVATE: Mode = Mode(0o600);
    /// A program of ours: this user's, open to nobody else, executable.
    pub const PRIVATE_EXECUTABLE: Mode = Mode(0o700);
}

pub fn mode_of(metadata: &Metadata) -> Mode {
    Mode(metadata.permissions().mode() & 0o777)
}

pub fn set_mode(path: &Path, mode: Mode) -> std::io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode.0))
}

pub fn check_private_dir(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)
        .with_context(|| format!("missing private directory {}", path.display()))?;
    if !m.is_dir()
        || m.file_type().is_symlink()
        || !super::super::user::current().owns(&m)
        || m.mode() & 0o077 != 0
    {
        bail!("unsafe private directory {}", path.display());
    }
    Ok(())
}

pub fn check_private_file(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_file()
        || m.file_type().is_symlink()
        || !super::super::user::current().owns(&m)
        || m.mode() & 0o077 != 0
    {
        bail!("unsafe private file {}", path.display());
    }
    Ok(())
}

pub fn is_user_directory(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| {
        m.is_dir()
            && !m.file_type().is_symlink()
            && super::super::user::current().owns(&m)
            && m.mode() & 0o022 == 0
    })
}

pub fn is_user_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| is_user_file_metadata(&m))
}

fn is_user_file_metadata(m: &Metadata) -> bool {
    m.is_file()
        && !m.file_type().is_symlink()
        && super::super::user::current().owns(m)
        && m.mode() & 0o022 == 0
}

pub fn is_private_unshared_handle(file: &File) -> std::io::Result<bool> {
    let m = file.metadata()?;
    Ok(m.is_file()
        && super::super::user::current().owns(&m)
        && m.mode() & 0o077 == 0
        && m.nlink() == 1)
}

pub fn has_single_link(file: &File) -> std::io::Result<bool> {
    Ok(file.metadata()?.nlink() == 1)
}

pub fn owner_can_write(metadata: &Metadata) -> bool {
    metadata.mode() & 0o200 != 0
}

pub fn is_trusted_handle(file: &File) -> std::io::Result<bool> {
    let m = file.metadata()?;
    Ok(m.is_file() && super::super::user::current().owns(&m) && m.mode() & 0o022 == 0)
}

/// A socket before anything talks to it: in a private directory, a socket, this user's, open to nobody else.
/// Moves to the endpoint module with the sockets.
pub fn verify_socket(path: &Path) -> Result<()> {
    check_private_dir(path.parent().context("socket parent")?)?;
    let m = fs::symlink_metadata(path)?;
    if !m.file_type().is_socket()
        || !super::super::user::current().owns(&m)
        || m.mode() & 0o077 != 0
    {
        bail!("unsafe socket {}", path.display());
    }
    Ok(())
}

pub fn verify_ancestors(path: &Path) -> Result<()> {
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir.as_os_str().is_empty() {
            break;
        }
        let m = fs::symlink_metadata(dir).with_context(|| format!("missing {}", dir.display()))?;
        let owner = super::super::user::current().owns(&m) || m.uid() == 0;
        let replaceable = m.mode() & 0o022 != 0 && m.mode() & 0o1000 == 0;
        if !owner || replaceable {
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

pub fn create_private_dir(path: &Path) -> std::io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(path)
}

pub fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

pub fn open_no_follow(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Whether `error` (from [`open_no_follow`]) is that the path is a link.
pub fn is_link_error(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::ELOOP)
}

fn create_new(path: &Path, mode: Mode) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode.0)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

pub fn create_private_new(path: &Path) -> std::io::Result<File> {
    create_new(path, Mode::PRIVATE)
}

pub fn create_private_executable_new(path: &Path) -> std::io::Result<File> {
    create_new(path, Mode::PRIVATE_EXECUTABLE)
}

pub fn open_private_append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

pub fn open_private_truncate(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

pub fn open_private_lock(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

pub fn read_user_text(path: &Path, limit: u64) -> Result<String> {
    let regular = |metadata: &Metadata| {
        metadata.is_file()
            && super::super::user::current().owns(metadata)
            && metadata.mode() & 0o022 == 0
            && metadata.len() <= limit
    };
    if !regular(&fs::metadata(path)?) {
        bail!("unsafe profile file");
    }
    // Opened without waiting (a FIFO put there since would wait for a writer), then judged again as opened.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .context("open profile file")?;
    if !regular(&file.metadata()?) {
        bail!("unsafe profile file");
    }
    let mut text = String::new();
    file.take(limit)
        .read_to_string(&mut text)
        .context("read profile file")?;
    Ok(text)
}

/// Makes what was renamed into `dir` durable.
pub fn sync_dir(dir: &Path) -> std::io::Result<()> {
    File::open(dir)?.sync_all()
}

pub fn trusted_executable(path: &Path) -> Result<Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    if !is_user_file_metadata(&metadata) || metadata.mode() & 0o111 == 0 {
        bail!("unsafe executable {}", path.display());
    }
    Ok(metadata)
}

pub fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0)
}

pub fn is_owned_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && super::super::user::current().owns(&m))
}
