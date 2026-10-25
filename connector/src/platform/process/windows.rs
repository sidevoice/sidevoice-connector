//! Windows open-file inspection and retryable executable errors.
use std::{io, path::PathBuf};
use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

impl super::Process for super::Processes {
    fn executable_is_busy(error: &io::Error) -> bool {
        error.raw_os_error() == i32::try_from(ERROR_SHARING_VIOLATION).ok()
    }

    fn open_files(_: u32) -> io::Result<Vec<PathBuf>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows does not list the files another process has open",
        ))
    }
}
