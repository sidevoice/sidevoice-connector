//! Windows: no console and a group of its own for what is detached, a Job Object for a tree (`process-wrap`).

use process_wrap::tokio::{CommandWrap, CreationFlags, JobObject};
use std::io;
use std::path::PathBuf;
use tokio::process::Command;
use windows_sys::Win32::Foundation::{
    SetHandleInformation, ERROR_SHARING_VIOLATION, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
// `windows`, not `windows-sys`: `CreationFlags` takes its flags type.
use windows::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS,
};

/// Rust starts every child with `bInheritHandles`, so a process detached from here would also inherit this one's own
/// standard handles: when this process was started with a pipe for its output (a desktop app, a test capturing it),
/// the reader would not see the pipe close until the detached process ended (the core's idle exit, minutes later).
/// They are made non-inheritable first; `Stdio::inherit` duplicates what it passes on, so no later spawn is
/// affected. Nothing maintained covers it (`std`'s handle list is unstable): own `unsafe`, three calls.
fn keep_own_stdio_out() {
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: `GetStdHandle` returns this process's handle or none (skipped: nothing to close); only the inherit flag
        // of that handle changes.
        let failed = unsafe {
            let handle = GetStdHandle(which);
            !handle.is_null()
                && handle != INVALID_HANDLE_VALUE
                && SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) == 0
        };
        // The spawn goes on (the process must start), but the pipe may then be held open: said in the log, not hidden.
        if failed {
            crate::logfile::log(&format!(
                "could not make a standard handle non-inheritable; a detached process may hold it open: {}",
                io::Error::last_os_error()
            ));
        }
    }
}

/// This OS's processes.
pub struct Os;

impl super::Process for Os {
    fn executable_is_busy(error: &io::Error) -> bool {
        error.raw_os_error() == i32::try_from(ERROR_SHARING_VIOLATION).ok()
    }

    // Not `CREATE_BREAKAWAY_FROM_JOB`: it is refused (access denied) when the caller's job does not allow breakaway.
    fn detached(command: Command) -> CommandWrap {
        keep_own_stdio_out();
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
}
