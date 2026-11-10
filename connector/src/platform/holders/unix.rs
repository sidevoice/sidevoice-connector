//! Unix: a running program can be unlinked, so no process holds an installed file in a way that stops its removal.
//! Who listens on a port is `lsof`'s answer.

use super::{lsof_listeners, Holder, Holding};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Where `lsof` is: macOS's own, then a Linux package's, then whatever the `PATH` finds.
const LSOF: [&str; 3] = ["/usr/sbin/lsof", "/usr/bin/lsof", "lsof"];
/// How long `lsof` may take before it is given up on (and killed).
const LSOF_LIMIT: Duration = Duration::from_secs(5);

pub struct Os;

impl Holding for Os {
    fn of(_: &Path) -> Vec<Holder> {
        Vec::new()
    }

    fn stop_running_from(_: &[&Path], _: &Path) -> Vec<u32> {
        Vec::new()
    }

    // `lsof -nP -iTCP:<port> -sTCP:LISTEN -Fpc`: no name or port lookups, listening sockets only, fields for a program
    // to read. It exits 1 when it finds none.
    fn listening(port: u16) -> Vec<Holder> {
        let program = LSOF
            .iter()
            .find(|path| !path.starts_with('/') || Path::new(path).is_file())
            .copied()
            .unwrap_or("lsof");
        let Ok(mut child) = Command::new(program)
            .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fpc"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        else {
            return Vec::new();
        };
        // Its answer is a few lines, well within a pipe's buffer: waiting before reading cannot block it.
        let deadline = Instant::now() + LSOF_LIMIT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Vec::new();
                }
            }
        }
        let mut output = String::new();
        if let Some(stdout) = child.stdout.take() {
            let _ = stdout.take(64 * 1024).read_to_string(&mut output);
        }
        lsof_listeners(&output)
    }
}
