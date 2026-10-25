//! Unix open-file inspection and retryable executable errors.

use std::io;
use std::path::PathBuf;

impl super::Process for super::Processes {
    fn executable_is_busy(error: &io::Error) -> bool {
        error.raw_os_error() == Some(libc::ETXTBSY)
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
