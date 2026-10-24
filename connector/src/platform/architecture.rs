//! The native CPU of the running machine, independently of the architecture of this executable.

use std::io;

#[cfg(windows)]
#[path = "architecture/windows.rs"]
mod imp;
#[cfg(windows)]
pub use imp::Os as Native;

/// CPU families whose native agent packages this connector can run.
pub enum Cpu {
    X64,
    Arm64,
}

/// The machine's native CPU, including when the connector itself is running under emulation.
pub trait Architecture {
    /// Returns the native CPU. `sysinfo` uses the kernel's machine name on Unix; Windows overrides this because
    /// `sysinfo` uses GetSystemInfo there, which can report the emulated process architecture.
    fn native() -> io::Result<Cpu> {
        let name = sysinfo::System::cpu_arch();
        match name.as_str() {
            "x86_64" => Ok(Cpu::X64),
            "aarch64" | "arm64" => Ok(Cpu::Arm64),
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported native CPU: {name}"),
            )),
        }
    }
}

// Unix needs only the default implementation; no empty per-OS file.
#[cfg(unix)]
pub struct Native;
#[cfg(unix)]
impl Architecture for Native {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_native_cpu_is_detected_on_the_supported_machines() {
        assert!(matches!(Native::native().unwrap(), Cpu::X64 | Cpu::Arm64));
    }
}
