//! Windows: no console and a group of its own for what is detached, a Job Object for a tree (`process-wrap`).
//! Nothing in the standard API lists another process's open handles.

use process_wrap::tokio::{CommandWrap, CreationFlags, JobObject};
use std::path::PathBuf;
use tokio::process::Command;
use windows::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS,
};

pub(super) struct Os;

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

    fn open_files(_: u32) -> Option<Vec<PathBuf>> {
        None
    }
}
