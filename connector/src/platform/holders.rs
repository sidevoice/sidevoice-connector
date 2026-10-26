//! Who has the installed files open: the processes that may stop them being deleted or replaced.
//!
//! Windows cannot delete or replace a program that is running, and an agent app or editor keeps a
//! `sidevoice-connector mcp` server running from the release for as long as a conversation is open. When removing
//! the installed files fails, the caller asks this module which processes hold them so that the error can name them.
//! This module only observes: what to say, and whether to say it, is the caller's.
//!
//! Windows asks the Restart Manager (`RmGetList`), the OS's own account of which applications use a file; it refuses
//! directories, so a lock on a directory alone is not seen. Unix can
//! unlink a running program, so nothing there holds an installed file in a way that matters: it reports no holders.

use std::path::Path;

#[cfg(unix)]
#[path = "holders/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "holders/windows.rs"]
mod imp;
pub use imp::Os as Holders;

/// A process that has a file open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    pub pid: u32,
    /// What the OS calls the process: its application name, else its program.
    pub name: String,
}

/// The processes holding installed files.
pub trait Holding {
    /// The processes that have a file open that is `root` or under it, in no particular order and without repeats. Only
    /// files are asked about: Windows cannot say who holds a directory, so a lock on a directory alone is not seen.
    /// Empty when there are none, when `root` is not there, and when the OS cannot tell: an observation that fails
    /// is no holder.
    fn of(root: &Path) -> Vec<Holder>;
}
