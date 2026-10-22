//! Windows: no console and a group of its own for what is detached, a Job Object for a tree (`process-wrap`); which
//! of some files a process has open, from the Restart Manager.
//!
//! Own `unsafe` over `windows-sys`, for the Restart Manager only: the one crate on crates.io that wraps it
//! (`restart-manager` 0.1.0) is assessed in the PR that brought this; the calls are the documented sequence
//! `RmStartSession`, `RmRegisterResources`, `RmGetList`, `RmEndSession`.

use process_wrap::tokio::{CommandWrap, CreationFlags, JobObject};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use sysinfo::{Pid, ProcessesToUpdate};
use tokio::process::Command;
// `windows`, not `windows-sys`: `CreationFlags` takes its flags type.
use windows::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS,
};
use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS, FILETIME};
use windows_sys::Win32::System::RestartManager::{
    RmEndSession, RmGetList, RmRegisterResources, RmStartSession, CCH_RM_SESSION_KEY,
    RM_PROCESS_INFO, RM_UNIQUE_PROCESS,
};

/// This OS's processes.
pub struct Os;

impl super::Process for Os {
    // Not `CREATE_BREAKAWAY_FROM_JOB`: it is refused (access denied) when the caller's job does not allow breakaway.
    fn detached(command: Command) -> CommandWrap {
        let mut wrap = CommandWrap::from(command);
        wrap.wrap(CreationFlags(
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW,
        ));
        wrap
    }

    // The Job Object ends everything in it when the `Child` is dropped (`KillOnDrop`) or `kill_tree` terminates it.
    fn tree(command: Command) -> CommandWrap {
        let mut wrap = CommandWrap::from(command);
        wrap.wrap(CreationFlags(CREATE_NO_WINDOW)).wrap(JobObject);
        wrap
    }

    fn open_files(_: u32) -> io::Result<Vec<PathBuf>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows does not list the files another process has open",
        ))
    }

    // The Restart Manager answers "which processes use these files", not "which files does this process use": one
    // question per batch says whether `pid` holds any of it, and only a batch it holds something in is split in
    // halves and each half asked again, down to single files.
    fn open_among(pid: u32, candidates: &[PathBuf]) -> io::Result<Vec<PathBuf>> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let target = Target::of(pid)?;
        let mut questions = QUESTIONS;
        let mut open = Vec::new();
        for batch in candidates.chunks(FILES_PER_SESSION) {
            narrow(&target, batch, &mut questions, &mut open)?;
        }
        Ok(open)
    }
}

/// The most files registered with one session.
const FILES_PER_SESSION: usize = 256;

/// The most Restart Manager sessions one [`super::Process::open_among`] starts. A batch `pid` holds nothing in costs
/// one; each file it holds costs about two per halving (8 for 256 files). Enough for a few held files among hundreds
/// of candidates; beyond it the question is given up rather than asked hundreds of times.
const QUESTIONS: u32 = 64;

/// The process asked about, as the Restart Manager tells one from another: its pid and when it started.
struct Target {
    pid: u32,
    /// Seconds since the Unix epoch, as `sysinfo` reads it (from the same creation time the Restart Manager gives).
    start: u64,
}

impl Target {
    fn of(pid: u32) -> io::Result<Target> {
        let id = Pid::from_u32(pid);
        let start = super::read(ProcessesToUpdate::Some(&[id]))
            .process(id)
            .map(sysinfo::Process::start_time)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no process {pid}")))?;
        Ok(Target { pid, start })
    }

    /// Whether `holder` is this process, and not another one that has its pid now.
    fn is(&self, holder: &RM_UNIQUE_PROCESS) -> bool {
        holder.dwProcessId == self.pid && unix_seconds(holder.ProcessStartTime) == self.start
    }
}

/// A `FILETIME` (100 ns ticks since 1601) as whole seconds since the Unix epoch.
fn unix_seconds(time: FILETIME) -> u64 {
    let ticks = (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    (ticks / 10_000_000).saturating_sub(11_644_473_600)
}

/// Adds to `open`, in their order, the `files` that `target` holds, spending one of `questions` per session.
fn narrow(
    target: &Target,
    files: &[PathBuf],
    questions: &mut u32,
    open: &mut Vec<PathBuf>,
) -> io::Result<()> {
    let Some(left) = questions.checked_sub(1) else {
        return Err(io::Error::other(
            "too many of the files are held to tell which within the Restart Manager questions allowed",
        ));
    };
    *questions = left;
    if !Session::start()?
        .holders(files)?
        .iter()
        .any(|holder| target.is(holder))
    {
        return Ok(());
    }
    if let [file] = files {
        open.push(file.clone());
        return Ok(());
    }
    let (first, second) = files.split_at(files.len() / 2);
    narrow(target, first, questions, open)?;
    narrow(target, second, questions, open)
}

/// A Restart Manager session, ended when dropped.
struct Session(u32);

impl Session {
    fn start() -> io::Result<Session> {
        let mut handle = 0u32;
        let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
        // SAFETY: `handle` and `key` are live for the call, `key` has the `CCH_RM_SESSION_KEY + 1` units it needs.
        let status = unsafe { RmStartSession(&mut handle, 0, key.as_mut_ptr()) };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        // Started: only now is there a session for `Drop` to end.
        Ok(Session(handle))
    }

    /// The processes that have any of `files` open.
    fn holders(&self, files: &[PathBuf]) -> io::Result<Vec<RM_UNIQUE_PROCESS>> {
        let names: Vec<Vec<u16>> = files
            .iter()
            .map(|file| file.as_os_str().encode_wide().chain([0]).collect())
            .collect();
        let pointers: Vec<*const u16> = names.iter().map(|name| name.as_ptr()).collect();
        // SAFETY: `pointers` holds `files.len()` pointers to NUL-terminated names that outlive the call; no
        // applications or services are registered (null, 0).
        let status = unsafe {
            RmRegisterResources(
                self.0,
                pointers.len() as u32,
                pointers.as_ptr(),
                0,
                null(),
                0,
                null(),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        // The list can grow between the call that sizes it and the one that fills it: ask again, a few times.
        let mut capacity = 0u32;
        for _ in 0..5 {
            // SAFETY: all-zero is a valid `RM_PROCESS_INFO` (integers, arrays and a flag).
            let mut list =
                vec![unsafe { std::mem::zeroed::<RM_PROCESS_INFO>() }; capacity as usize];
            let (mut needed, mut have, mut reasons) = (0u32, capacity, 0u32);
            // SAFETY: `list` has room for `have` entries and the other arguments are live for the call.
            let status = unsafe {
                RmGetList(
                    self.0,
                    &mut needed,
                    &mut have,
                    if capacity == 0 {
                        null_mut()
                    } else {
                        list.as_mut_ptr()
                    },
                    &mut reasons,
                )
            };
            match status {
                ERROR_SUCCESS => {
                    return Ok(list
                        .iter()
                        .take(have as usize)
                        .map(|info| info.Process)
                        .collect())
                }
                ERROR_MORE_DATA => capacity = needed,
                _ => return Err(io::Error::from_raw_os_error(status as i32)),
            }
        }
        Err(io::Error::other(
            "the Restart Manager's list kept growing while it was read",
        ))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: the session was started (`start` builds one only on success) and is ended once.
        unsafe { RmEndSession(self.0) };
    }
}
