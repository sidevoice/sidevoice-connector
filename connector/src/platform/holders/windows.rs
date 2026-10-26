//! Windows: the Restart Manager's account of which processes use the programs under a directory.

use super::{Holder, Holding};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
use windows_sys::Win32::System::RestartManager::{
    RmEndSession, RmGetList, RmRegisterResources, RmStartSession, CCH_RM_SESSION_KEY,
    RM_PROCESS_INFO,
};

/// How many programs are registered with the Restart Manager at most: a release holds a handful.
const PROGRAM_LIMIT: usize = 1000;

pub struct Os;

/// The programs (`.exe`, `.dll`) under `root`; links are listed as themselves, never entered.
fn programs(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file()
                && path.extension().is_some_and(|extension| {
                    extension.eq_ignore_ascii_case("exe") || extension.eq_ignore_ascii_case("dll")
                })
                && found.len() < PROGRAM_LIMIT
            {
                found.push(path);
            }
        }
    }
    found
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

fn holding(files: &[PathBuf]) -> Option<Vec<Holder>> {
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
                        .map(|info| Holder {
                            pid: info.Process.dwProcessId,
                            name: text(&info.strAppName),
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
        let files = programs(root);
        if files.is_empty() {
            return Vec::new();
        }
        let mut holders = holding(&files).unwrap_or_default();
        holders.sort_by_key(|holder| holder.pid);
        holders.dedup_by_key(|holder| holder.pid);
        holders
    }
}
