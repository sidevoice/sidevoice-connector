//! Windows: a third party's endpoint is a named pipe on this computer, `\\.\pipe\<name>`, judged by the conversion
//! `interprocess` opens it with (which takes `pipe` in lowercase only). A route in any other shape is refused: no agent
//! documents another one here, and a name is never derived from it. Its server is this user's when the pipe's server
//! process is a live process of this user's.

use super::ForeignStream;
use crate::platform::process::{Process, Processes};
use interprocess::local_socket::traits::StreamCommon as _;
use interprocess::local_socket::{GenericFilePath, ToFsName};
use std::ffi::OsStr;
use std::io;

/// This computer's pipes; `interprocess` also opens `\\<host>\pipe\...`, which is refused here.
const LOCAL: &str = r"\\.\";

/// This OS's third-party endpoints.
pub struct Os;

impl super::Foreign for Os {
    fn check(route: &OsStr) -> io::Result<()> {
        if route.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "an empty endpoint name",
            ));
        }
        let refused = || {
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!("not a named pipe on this computer (\\\\.\\pipe\\<name>): {route:?}"),
            )
        };
        if !route.to_str().is_some_and(|text| text.starts_with(LOCAL)) {
            return Err(refused());
        }
        route
            .to_os_string()
            .to_fs_name::<GenericFilePath>()
            .map(drop)
            .map_err(|_| refused())
    }

    // `interprocess` asks a client's pipe for its server's pid (`GetNamedPipeServerProcessId`); `Processes` compares
    // that process's owner with this one's (the token SIDs, through `sysinfo`), and refuses a process that is gone.
    fn served_by_this_user(_: &OsStr, stream: &ForeignStream) -> io::Result<bool> {
        let pid = stream
            .peer_creds()?
            .pid()
            .ok_or_else(|| io::Error::other("the pipe's server process is unknown"))?;
        Ok(Processes::is_process(pid, |_| true))
    }
}
