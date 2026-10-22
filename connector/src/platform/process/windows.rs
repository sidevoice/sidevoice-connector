//! Windows: no console and a group of its own for what is detached, a Job Object for a tree (`process-wrap`); which
//! of some files a process has open, from the Restart Manager.
//!
//! Own `unsafe` over `windows-sys`, for the Restart Manager only: the one crate on crates.io that wraps it (version
//! 0.1.0, one release, a whole session/shutdown/restart API) is far more than the one read-only question asked here,
//! and nothing else maintained covers it. The calls are the documented sequence `RmStartSession`,
//! `RmRegisterResources`, `RmGetList`, `RmEndSession`.

use process_wrap::tokio::{CommandWrap, CreationFlags, JobObject};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use tokio::process::Command;
// `windows`, not `windows-sys`: `CreationFlags` takes its flags type.
use windows::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS,
};
use windows_sys::Win32::Foundation::ERROR_MORE_DATA;
use windows_sys::Win32::System::RestartManager::{
    RmEndSession, RmGetList, RmRegisterResources, RmStartSession, CCH_RM_SESSION_KEY,
    RM_PROCESS_INFO,
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

    // The Restart Manager answers "which processes use these files", not "which files does this process use": one
    // question about all the candidates says whether `pid` holds any, and only then is each asked about alone.
    fn open_among(pid: u32, candidates: &[PathBuf]) -> Option<Vec<PathBuf>> {
        if !holders(candidates)?.contains(&pid) {
            return Some(Vec::new());
        }
        let mut open = Vec::new();
        for candidate in candidates {
            if holders(std::slice::from_ref(candidate))?.contains(&pid) {
                open.push(candidate.clone());
            }
        }
        Some(open)
    }
}

/// The most files registered with one session.
const FILES_PER_SESSION: usize = 256;

/// The pids of the processes that have any of `files` open; `None` when the Restart Manager cannot be asked.
fn holders(files: &[PathBuf]) -> Option<Vec<u32>> {
    let mut pids = Vec::new();
    for chunk in files.chunks(FILES_PER_SESSION) {
        let session = Session::start()?;
        pids.extend(session.holders(chunk)?);
    }
    Some(pids)
}

/// A Restart Manager session, ended when dropped.
struct Session(u32);

impl Session {
    fn start() -> Option<Session> {
        let mut handle = 0u32;
        let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
        // SAFETY: `handle` and `key` are live for the call, `key` has the `CCH_RM_SESSION_KEY + 1` units it needs.
        let status = unsafe { RmStartSession(&mut handle, 0, key.as_mut_ptr()) };
        (status == 0).then_some(Session(handle))
    }

    fn holders(&self, files: &[PathBuf]) -> Option<Vec<u32>> {
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
        if status != 0 {
            return None;
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
                0 => {
                    return Some(
                        list.iter()
                            .take(have as usize)
                            .map(|info| info.Process.dwProcessId)
                            .collect(),
                    )
                }
                ERROR_MORE_DATA => capacity = needed,
                _ => return None,
            }
        }
        None
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: the session was started and is ended once.
        unsafe { RmEndSession(self.0) };
    }
}
