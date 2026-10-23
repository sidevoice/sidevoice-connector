//! What this build is as a release target, and how programs are named on it.
//!
//! The interface is the [`Target`] trait; [`Host`] is the one implementation, for the machine the binary was compiled
//! for. Both operations have one body for every OS, built on `std::env::consts` (the compiler's own statement of the
//! target), so they are the trait's default methods and there is no per-OS file: an OS that needs another answer
//! overrides the method. Callers import both: `use crate::platform::target::{Host, Target};` then
//! `Host::name()`, `Host::exe_name("sidevoice-core-rust")`.

/// What a build knows about the target it is for.
pub trait Target {
    /// The release target this binary was compiled for (`linux-x86_64`, `linux-aarch64`, `macos-aarch64`,
    /// `windows-x86_64`), or `None` on a system no release is made for. The names are those of the release archives
    /// and, with npm's `os` and `cpu`, of the npm platform packages (`xtask/src/main.rs`, `TARGETS`).
    fn name() -> Option<&'static str> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "aarch64") => Some("macos-aarch64"),
            ("linux", "x86_64") => Some("linux-x86_64"),
            ("linux", "aarch64") => Some("linux-aarch64"),
            ("windows", "x86_64") => Some("windows-x86_64"),
            _ => None,
        }
    }

    /// The file name of a program on this system: `stem` and, on Windows, `.exe`. Every program the connector names
    /// by file (its own binary, the core's, the copy that finishes an uninstall) goes through here.
    fn exe_name(stem: &str) -> String {
        format!("{stem}{}", std::env::consts::EXE_SUFFIX)
    }
}

/// The machine this binary was compiled for.
pub struct Host;

impl Target for Host {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_is_named_the_way_this_system_names_programs() {
        assert_eq!(
            Host::exe_name("sidevoice-connector"),
            format!("sidevoice-connector{}", std::env::consts::EXE_SUFFIX)
        );
        assert!(Host::exe_name("x").starts_with('x'));
        assert_eq!(Host::exe_name("x").ends_with(".exe"), cfg!(windows));
    }

    #[test]
    fn this_build_names_its_release_target_by_its_system_and_architecture() {
        let name = Host::name();
        let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
        match name {
            Some(name) => {
                let (name_os, name_arch) = name.split_once('-').unwrap();
                assert_eq!(name_os, os);
                assert_eq!(name_arch, arch);
            }
            None => assert!(![
                "macos-aarch64",
                "linux-x86_64",
                "linux-aarch64",
                "windows-x86_64"
            ]
            .contains(&format!("{os}-{arch}").as_str())),
        }
    }
}
