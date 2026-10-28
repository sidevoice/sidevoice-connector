//! Shutting everybody else out of a directory of ours that was left open (sidevoice/sidevoice-connector#310): an older
//! build made the data directory `0755`, or somebody gave it an ACL. `sidevoice-platform` judges and refuses, and never
//! changes access on something that exists; this is the one change it does not make. When to make it is decided by the
//! caller (`crate::private_dir`): only on a real directory of this user's under directories nobody else can change.
//!
//! The interface is the [`OwnerOnly`] trait; [`Restrictor`] is the one OS implementation compiled in, chosen below.
//! What it leaves is what `Fs::create_private_dir` makes: Unix mode `0700` and, on macOS, no extended ACL; Windows the
//! protected DACL granting full control to the owner, SYSTEM and Administrators only. Every change is made through
//! the handle that was opened and judged, never through a name, so it reaches that directory and nothing else: not
//! one put at its name meanwhile, and not what is inside it.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::Path;

#[cfg(unix)]
#[path = "owner_only/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "owner_only/windows.rs"]
mod imp;
#[cfg(target_os = "linux")]
#[path = "owner_only/linux.rs"]
mod linux;
#[cfg(target_os = "macos")]
#[path = "owner_only/macos.rs"]
mod macos;
#[cfg(target_os = "linux")]
use linux as acl;
#[cfg(target_os = "macos")]
use macos as acl;

pub use imp::Os as Restrictor;

/// What every OS implements.
pub trait OwnerOnly {
    /// Opens the entry `name` (one ordinary component) of the directory at `parent`, able to change its access: the
    /// entry itself, a link there (Windows: any reparse point) refused, never followed; `parent` itself not a link.
    fn open(parent: &Path, name: &OsStr) -> io::Result<File>;

    /// Leaves the directory behind `dir`, opened by [`OwnerOnly::open`], open to its owner only, through that handle.
    /// Changes its own access and nothing else: not the owner, not what is inside it (no inherited access is passed
    /// down to it), not the directories above.
    fn restrict(dir: &File) -> io::Result<()>;
}
