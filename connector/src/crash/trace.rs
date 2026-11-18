//! The frames of a panic with their addresses, and the loaded modules they can be matched to.
//!
//! What Sentry needs to name a frame after `strip = "debuginfo"` is its address and the module (debug id, load address) it is
//! in, to match against the debug files the release uploads. The modules are listed once, at start (`loaded`), with `findshlibs`,
//! as bare names; a panic keeps only the ones that hold one of its frames (`holding`).

use backtrace::Backtrace;
use findshlibs::{SharedLibrary, SharedLibraryId, TargetSharedLibrary, TARGET_SUPPORTED};
use sentry::types::{DebugId, Uuid};

use super::record::{bare, Frame, Image};

/// The frames of `trace` (resolved), innermost first, from the function that panicked outwards: the capture's own frames and
/// the panic machinery's (`std::panicking`, `core::panicking`, `rust_begin_unwind`) are dropped.
pub(super) fn frames(trace: &Backtrace) -> Vec<Frame> {
    let mut frames = Vec::new();
    for frame in trace.frames() {
        let addr = frame.ip() as usize as u64;
        if frame.symbols().is_empty() {
            frames.push(Frame {
                function: None,
                file: None,
                line: None,
                addr,
            });
        }
        for symbol in frame.symbols() {
            frames.push(Frame {
                // `{:#}` leaves the hash out.
                function: symbol.name().map(|name| format!("{name:#}")),
                file: symbol
                    .filename()
                    .and_then(|path| path.to_str())
                    .map(str::to_owned),
                line: symbol.lineno(),
                addr,
            });
        }
    }
    trim(frames)
}

/// `frames` from the first frame of the panic machinery on: the run of machinery frames and what is inside it (the capture,
/// the hook) are dropped; what follows is the code that panicked. Without machinery, as it was.
pub(super) fn trim(mut frames: Vec<Frame>) -> Vec<Frame> {
    let machinery = |frame: &Frame| {
        frame.function.as_deref().is_some_and(|function| {
            function.starts_with("std::panicking::")
                || function.starts_with("core::panicking::")
                || function.starts_with("std::sys::backtrace::")
                || function.contains("rust_begin_unwind")
        })
    };
    match frames.iter().position(machinery) {
        Some(first) => {
            let run = frames[first..].iter().take_while(|f| machinery(f)).count();
            frames.split_off(first + run)
        }
        None => frames,
    }
}

/// Every loaded module that has a debug id, as a bare name and its place in memory. A module with no name is the program.
pub(super) fn loaded() -> Vec<Image> {
    let mut images = Vec::new();
    if !TARGET_SUPPORTED {
        return images;
    }
    TargetSharedLibrary::each(|library| {
        let id = match library.debug_id() {
            Some(SharedLibraryId::Uuid(bytes)) => Some(DebugId::from_uuid(Uuid::from_bytes(bytes))),
            Some(SharedLibraryId::GnuBuildId(id)) => debug_id_from_build_id(&id),
            Some(SharedLibraryId::PdbSignature(guid, age)) => {
                DebugId::from_guid_age(&guid, age).ok()
            }
            _ => None,
        };
        let name = library.name().to_string_lossy();
        let name = if name.is_empty() {
            std::env::current_exe()
                .ok()
                .and_then(|path| path.to_str().and_then(bare))
        } else {
            bare(&name)
        };
        let (Some(id), Some(name)) = (id, name) else {
            return;
        };
        // On Windows the bias is the module's base, the address Sentry wants; elsewhere the segments give it.
        let (addr, vmaddr) = if cfg!(windows) {
            (library.virtual_memory_bias().0 as u64, 0)
        } else {
            (
                library.actual_load_addr().0 as u64,
                library.stated_load_addr().0 as u64,
            )
        };
        images.push(Image {
            name,
            addr,
            size: library.len() as u64,
            vmaddr,
            id: id.to_string(),
            code_id: library.id().map(|id| id.to_string()),
        });
    });
    images
}

/// An ELF build id as a debug id: cut or padded to 16 bytes, the first three fields flipped to big endian on a little
/// endian machine, as Breakpad and Sentry expect.
fn debug_id_from_build_id(build_id: &[u8]) -> Option<DebugId> {
    let mut data = [0u8; 16];
    let len = build_id.len().min(16);
    data[..len].copy_from_slice(&build_id[..len]);
    #[cfg(target_endian = "little")]
    {
        data[0..4].reverse();
        data[4..6].reverse();
        data[6..8].reverse();
    }
    Uuid::from_slice(&data).map(DebugId::from_uuid).ok()
}

/// The modules of `all` that hold an address of `frames`: a module no frame is in is never sent (the list of what else is
/// loaded would say what is installed on the machine).
pub(super) fn holding(all: &[Image], frames: &[Frame]) -> Vec<Image> {
    all.iter()
        .filter(|image| {
            frames.iter().any(|frame| {
                frame
                    .addr
                    .checked_sub(image.addr)
                    .is_some_and(|at| at < image.size)
            })
        })
        .cloned()
        .collect()
}
