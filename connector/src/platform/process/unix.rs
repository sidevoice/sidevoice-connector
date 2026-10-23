//! Unix: a process group per tree (`process-wrap`) and the Linux and BSD ways of listing open files.

use process_wrap::tokio::{CommandWrap, ProcessGroup};
use std::io;
use std::path::PathBuf;
use tokio::process::Command;

/// This OS's processes.
pub struct Os;

impl super::Process for Os {
    // A group of its own is also what detaches it: a ^C or a hangup aimed at the caller's group misses it.
    fn detached(command: Command) -> CommandWrap {
        Self::tree(command)
    }

    fn tree(command: Command) -> CommandWrap {
        let mut wrap = CommandWrap::from(command);
        wrap.wrap(ProcessGroup::leader());
        wrap
    }

    // `/proc/<pid>/fd` where there is one, else `lsof`. Both name a file by where it really is.
    fn open_files(pid: u32) -> io::Result<Vec<PathBuf>> {
        if let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
            return Ok(entries
                .flatten()
                .filter_map(|entry| std::fs::read_link(entry.path()).ok())
                .collect());
        }
        let output = std::process::Command::new("lsof")
            .args(["-n", "-P", "-Fn", "-p", &pid.to_string()])
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "lsof could not list what {pid} has open"
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.strip_prefix('n').map(PathBuf::from))
            .collect())
    }
}
