//! Unix: a running program can be unlinked, so no process holds an installed file in a way that stops its removal.
//! The processes running from a release are still ended on uninstall (found by their program's path, through
//! `sysinfo`), so that no server of ours outlives the files it runs from.
//! Who listens on a port is `lsof`'s answer.

use super::{lsof_listeners, Holder, Holding};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use sysinfo::{
    Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, Signal, System, UpdateKind,
};

/// Where `lsof` is: macOS's own, then a Linux package's, then whatever the `PATH` finds.
const LSOF: [&str; 3] = ["/usr/sbin/lsof", "/usr/bin/lsof", "lsof"];
/// How long `lsof` may take before it is given up on (and killed).
const LSOF_LIMIT: Duration = Duration::from_secs(5);

pub struct Os;

/// How long a process asked to leave (`SIGTERM`) has before it is killed.
const GRACE: Duration = Duration::from_secs(3);

/// Whether the process found as `(pid, start)` is still that process (a pid reused since is another start time).
fn alive(system: &mut System, (pid, start): (Pid, u64)) -> bool {
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    system.process(pid).is_some_and(|process| {
        process.start_time() == start && process.status() != ProcessStatus::Zombie
    })
}

/// The processes whose program is a file inside `program_root`, other than this one, asked to leave (`SIGTERM`) and
/// killed when they are still there after [`GRACE`]. A process is only ever acted on while its start time is still
/// the one seen when it was found, so a pid reused meanwhile is left alone, and the OS refuses a signal to a process
/// that is not this user's: it is then not reported. Returns the pids that are gone.
fn stop_programs_inside(program_root: &Path) -> Vec<u32> {
    let own = std::process::id();
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
    );
    let mut found: Vec<(Pid, u64)> = Vec::new();
    for (pid, process) in system.processes() {
        let inside = process
            .exe()
            .is_some_and(|exe| super::runs_from(exe, program_root));
        if inside && pid.as_u32() != own && process.status() != ProcessStatus::Zombie {
            process.kill_with(Signal::Term);
            found.push((*pid, process.start_time()));
        }
    }
    let deadline = Instant::now() + GRACE;
    while Instant::now() < deadline && found.iter().any(|&target| alive(&mut system, target)) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut ended = Vec::new();
    for target in found {
        if alive(&mut system, target) {
            if let Some(process) = system.process(target.0) {
                process.kill();
            }
            std::thread::sleep(Duration::from_millis(100));
            if alive(&mut system, target) {
                continue;
            }
        }
        ended.push(target.0.as_u32());
    }
    ended
}

impl Holding for Os {
    fn of(_: &Path) -> Vec<Holder> {
        Vec::new()
    }

    fn stop_running_from(_: &[&Path], program_root: &Path) -> Vec<u32> {
        stop_programs_inside(program_root)
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
