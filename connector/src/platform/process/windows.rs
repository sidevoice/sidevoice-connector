//! Windows: no console and a group of its own for what is detached, a Job Object for a tree (`process-wrap`).

use process_wrap::tokio::{CommandWrap, CreationFlags, JobObject};
use std::io;
use std::path::PathBuf;
use tokio::process::Command;
// `windows`, not `windows-sys`: `CreationFlags` takes its flags type.
use windows::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS,
};

/// This OS's processes.
pub struct Os;

impl super::Process for Os {
    fn asks_to_leave() -> bool {
        false
    }

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
}
