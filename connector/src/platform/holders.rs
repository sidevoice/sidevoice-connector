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

    /// Kills every process that has a file open under one of `roots` and whose program is a file inside `program_root`
    /// (its full path, links resolved: the name a process gives itself proves nothing). The process is opened once;
    /// its start time (as the OS reported it holding the file) and its program are checked on that same handle, and it
    /// is ended through it, so a pid reused meanwhile is never touched. Never this process. Returns the pids ended.
    /// Unix reports no holders, so ends nothing.
    fn stop_running_from(roots: &[&Path], program_root: &Path) -> Vec<u32>;
}

/// Whether the program at `image` is a file inside `root`, both as the file system resolves them.
#[cfg(any(windows, test))]
fn runs_from(image: &Path, root: &Path) -> bool {
    match (std::fs::canonicalize(image), std::fs::canonicalize(root)) {
        (Ok(image), Ok(root)) => dunce::simplified(&image).starts_with(dunce::simplified(&root)),
        _ => false,
    }
}

/// Whether `root` may be walked: neither it nor any directory above it is a link or junction, as `is_link` says. A
/// path the remover refused for that reason must never be enumerated, or its target would be searched for holders.
/// Not being able to tell is a link.
#[cfg(any(windows, test))]
fn walkable(root: &Path, is_link: impl Fn(&Path) -> Option<bool>) -> bool {
    root.ancestors().all(|path| is_link(path) == Some(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn a_program_belongs_to_an_installation_by_its_path_not_its_name() {
        let scratch = crate::test_support::temp_dir("holders-runs-from");
        let root = scratch.path().join("install");
        let other = scratch.path().join("install2");
        for dir in [&root, &other] {
            std::fs::create_dir_all(dir.join("bin")).unwrap();
            std::fs::write(dir.join("bin/sidevoice-connector"), "").unwrap();
        }
        assert!(runs_from(&root.join("bin/sidevoice-connector"), &root));
        // The same name from another installation, or from a folder that only shares the prefix, is not ours.
        assert!(!runs_from(&other.join("bin/sidevoice-connector"), &root));
        assert!(!runs_from(&root.join("missing"), &root));
    }

    #[test]
    fn a_linked_root_or_a_linked_ancestor_is_never_walked() {
        let linked = PathBuf::from("/data/link");
        let looks = |path: &Path| Some(path == linked);
        assert!(walkable(Path::new("/data/real/install"), looks));
        assert!(!walkable(&linked, looks));
        assert!(!walkable(&linked.join("deeper/install"), looks));
        assert!(!walkable(Path::new("/data/unknown"), |_| None));
    }
}
