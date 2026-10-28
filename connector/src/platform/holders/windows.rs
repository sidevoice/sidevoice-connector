//! Windows: the Restart Manager's account of which processes use the files under a path, registered in batches.

use super::{Holder, Holding};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_MORE_DATA, ERROR_SUCCESS, FILETIME, HANDLE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows_sys::Win32::System::RestartManager::{
    RmEndSession, RmGetList, RmRegisterResources, RmStartSession, CCH_RM_SESSION_KEY,
    RM_PROCESS_INFO,
};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
    WaitForSingleObject, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};

/// How many files are registered with the Restart Manager in one session.
const BATCH: usize = 256;

pub struct Os;

/// The regular files under `root` (`root` itself when it is one); links are never followed and directories are not
/// listed: the Restart Manager refuses them.
fn files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if std::fs::symlink_metadata(root).is_ok_and(|meta| meta.is_file()) {
        found.push(root.to_path_buf());
    }
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => pending.push(entry.path()),
                Ok(kind) if kind.is_file() => found.push(entry.path()),
                _ => {}
            }
        }
    }
    found
}

/// Whether `path` is a link, junction or other reparse point; `None` when it cannot be read.
fn is_link(path: &Path) -> Option<bool> {
    use std::os::windows::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path).ok()?;
    Some(meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

/// A Restart Manager session, ended when dropped.
struct Session(u32);

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: the handle was returned by `RmStartSession` and is ended once.
        unsafe { RmEndSession(self.0) };
    }
}

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}

fn text(buffer: &[u16]) -> String {
    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

/// A FILETIME as one number.
fn filetime(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

/// Who holds `files`, each with the time its process started (as the Restart Manager saw it).
fn holding(files: &[PathBuf]) -> Option<Vec<(Holder, u64)>> {
    let mut handle = 0;
    let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
    // SAFETY: `handle` and `key` are writable and `key` has the room the API documents.
    if unsafe { RmStartSession(&mut handle, 0, key.as_mut_ptr()) } != ERROR_SUCCESS {
        return None;
    }
    let session = Session(handle);
    let names: Vec<Vec<u16>> = files.iter().map(|file| wide(file.as_os_str())).collect();
    let pointers: Vec<*const u16> = names.iter().map(|name| name.as_ptr()).collect();
    // SAFETY: `pointers` holds `files.len()` NUL-terminated strings that outlive the call; no applications or
    // services are registered.
    let registered = unsafe {
        RmRegisterResources(
            session.0,
            u32::try_from(pointers.len()).ok()?,
            pointers.as_ptr(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
        )
    };
    if registered != ERROR_SUCCESS {
        return None;
    }
    let mut capacity = 0u32;
    let mut reasons = 0u32;
    // The list can grow between the size question and the answer: ask again while the room is too small.
    for _ in 0..5 {
        let mut needed = 0u32;
        // SAFETY: `list` has `capacity` entries (none when 0, which asks for the size).
        let mut list: Vec<RM_PROCESS_INFO> = Vec::new();
        list.resize_with(capacity as usize, || unsafe { std::mem::zeroed() });
        let mut count = capacity;
        let status = unsafe {
            RmGetList(
                session.0,
                &mut needed,
                &mut count,
                if capacity == 0 {
                    std::ptr::null_mut()
                } else {
                    list.as_mut_ptr()
                },
                &mut reasons,
            )
        };
        match status {
            ERROR_SUCCESS => {
                list.truncate(count as usize);
                return Some(
                    list.iter()
                        .map(|info| {
                            (
                                Holder {
                                    pid: info.Process.dwProcessId,
                                    name: text(&info.strAppName),
                                },
                                filetime(info.Process.ProcessStartTime),
                            )
                        })
                        .collect(),
                );
            }
            ERROR_MORE_DATA => capacity = needed,
            _ => return None,
        }
    }
    None
}

impl Holding for Os {
    fn of(root: &Path) -> Vec<Holder> {
        if !super::walkable(root, is_link) {
            return Vec::new();
        }
        let mut holders: Vec<Holder> = files(root)
            .chunks(BATCH)
            .flat_map(|batch| holding(batch).unwrap_or_default())
            .map(|(holder, _)| holder)
            .collect();
        holders.sort_by_key(|holder| holder.pid);
        holders.dedup_by_key(|holder| holder.pid);
        holders
    }

    fn stop_running_from(roots: &[&Path], program_root: &Path) -> Vec<u32> {
        Self::stop_running_from_impl(roots, program_root)
    }
}

/// An open process handle, closed when dropped.
struct Process(HANDLE);

impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: the handle was returned by `OpenProcess` and is closed once.
        unsafe { CloseHandle(self.0) };
    }
}

impl Process {
    /// `pid` opened for what is needed to check it and end it; `None` when it is gone or not this user's.
    fn open(pid: u32) -> Option<Self> {
        // SAFETY: plain call; a null handle is a failure.
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        (!handle.is_null()).then_some(Self(handle))
    }

    /// When the process started.
    fn started(&self) -> Option<u64> {
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            },
            FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            },
            FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            },
            FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            },
        );
        // SAFETY: the four times are writable.
        let read =
            unsafe { GetProcessTimes(self.0, &mut created, &mut exited, &mut kernel, &mut user) };
        (read != 0).then(|| filetime(created))
    }

    /// The full path of the process's program.
    fn image(&self) -> Option<PathBuf> {
        let mut buffer = vec![0u16; 32_768];
        let mut size = u32::try_from(buffer.len()).ok()?;
        // SAFETY: `buffer` has `size` units; the call writes at most that many and sets `size` to the length.
        let read = unsafe {
            QueryFullProcessImageNameW(self.0, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut size)
        };
        (read != 0).then(|| PathBuf::from(String::from_utf16_lossy(&buffer[..size as usize])))
    }

    /// Ends the process and waits a little for it to be gone.
    fn end(&self) -> bool {
        // SAFETY: the handle has the terminate right.
        let ended = unsafe { TerminateProcess(self.0, 1) } != 0;
        // SAFETY: the handle has the synchronize right.
        ended && unsafe { WaitForSingleObject(self.0, 5_000) } == WAIT_OBJECT_0
    }
}

impl Os {
    fn stop_running_from_impl(roots: &[&Path], program_root: &Path) -> Vec<u32> {
        let own = std::process::id();
        let mut found: Vec<(u32, u64)> = Vec::new();
        for root in roots {
            if !super::walkable(root, is_link) {
                continue;
            }
            for (holder, start) in files(root)
                .chunks(BATCH)
                .flat_map(|batch| holding(batch).unwrap_or_default())
            {
                if holder.pid != own && !found.iter().any(|(pid, _)| *pid == holder.pid) {
                    found.push((holder.pid, start));
                }
            }
        }
        let mut ended = Vec::new();
        for (pid, start) in found {
            // One handle for the checks and the ending: a pid reused since the Restart Manager looked is another
            // start time, and is left alone.
            let Some(process) = Process::open(pid) else {
                continue;
            };
            let ours = process.started() == Some(start)
                && process
                    .image()
                    .is_some_and(|image| super::runs_from(&image, program_root));
            if ours && process.end() {
                ended.push(pid);
            }
        }
        ended
    }
}
