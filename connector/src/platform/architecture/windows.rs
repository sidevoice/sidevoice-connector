//! IsWow64Process2 reports the native machine, even for an x64 connector emulated on ARM64 Windows.

use super::Cpu;
use std::io;
use windows_sys::Win32::System::SystemInformation::{
    IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_ARM64,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, IsWow64Process2};

pub struct Os;

impl super::Architecture for Os {
    fn native() -> io::Result<Cpu> {
        let mut process = 0;
        let mut native = 0;
        // SAFETY: the pseudo-handle names the current process (never closed), and both output pointers refer to
        // initialized, writable IMAGE_FILE_MACHINE values that live through the call.
        if unsafe { IsWow64Process2(GetCurrentProcess(), &mut process, &mut native) } == 0 {
            return Err(io::Error::last_os_error());
        }
        match native {
            IMAGE_FILE_MACHINE_AMD64 => Ok(Cpu::X64),
            IMAGE_FILE_MACHINE_ARM64 => Ok(Cpu::Arm64),
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported native machine: {native:#x}"),
            )),
        }
    }
}
