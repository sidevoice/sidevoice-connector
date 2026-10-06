//! What this machine says about itself, in the one place that says it: at every handshake with the core, which
//! keeps the latest so that a person looking at the list of paired machines reads a machine and not a UUID. Every
//! field is read from what this machine actually is: nothing is asked of a harness or of the room, and nothing is
//! guessed.

use crate::profile::Profile;
use serde_json::{json, Value};

/// This build's version: the release version stamped at build time (`build.rs`).
pub const VERSION: &str = env!("SIDEVOICE_CONNECTOR_VERSION");

/// The release target this binary was compiled for (`linux-x86_64`, `linux-aarch64`, `macos-aarch64`), or `None`
/// on a platform no release is made for.
pub fn target() -> Option<&'static str> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return Some("macos-aarch64");
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        return Some("linux-x86_64");
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        return Some("linux-aarch64");
    }
    #[allow(unreachable_code)]
    None
}

/// The host name, as the operating system reports it. `SIDEVOICE_HOST_ID` overrides it (tests, several profiles on
/// one machine).
pub fn host() -> String {
    if let Some(id) = std::env::var("SIDEVOICE_HOST_ID")
        .ok()
        .filter(|id| !id.trim().is_empty())
    {
        return id;
    }
    let mut buffer = [0u8; 256];
    let read = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    let name = if read == 0 {
        let end = buffer
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(buffer.len());
        String::from_utf8_lossy(&buffer[..end]).trim().to_owned()
    } else {
        String::new()
    };
    if name.is_empty() {
        "unknown".into()
    } else {
        name
    }
}

/// The platform, said the way a person reading the room's list calls it: `macOS arm64`, `Linux x64`. A system or
/// architecture not named here travels as Rust names it.
pub fn platform() -> String {
    platform_of(std::env::consts::OS, std::env::consts::ARCH)
}

fn platform_of(os: &str, arch: &str) -> String {
    let os = match os {
        "macos" => "macOS",
        "linux" => "Linux",
        "windows" => "Windows",
        other => other,
    };
    let arch = match arch {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    };
    format!("{os} {arch}")
}

/// Which harnesses live on this machine, by the home each of them keeps. Nothing is run to find out.
pub fn harnesses(profile: &Profile) -> Vec<&'static str> {
    [
        ("claude", &profile.claude),
        ("codex", &profile.codex),
        // Cursor, the CLI or the editor: both keep ~/.cursor, and both read the MCP servers in it.
        ("cursor", &profile.cursor),
    ]
    .into_iter()
    .filter(|(_, home)| home.exists())
    .map(|(name, _)| name)
    .collect()
}

/// This machine, as the room will list it.
pub fn machine(profile: &Profile) -> Value {
    json!({"host":host(),"platform":platform(),"version":profile.connector_version(),
        "harnesses":harnesses(profile)})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure_fs::tests::Scratch;

    #[test]
    fn platforms_are_named_for_people() {
        assert_eq!(platform_of("macos", "aarch64"), "macOS arm64");
        assert_eq!(platform_of("linux", "x86_64"), "Linux x64");
        assert_eq!(platform_of("freebsd", "riscv64"), "freebsd riscv64");
    }

    #[test]
    fn the_machine_names_its_real_host_version_and_the_harnesses_it_has() {
        let scratch = Scratch::new("identity");
        let profile = Profile::for_test(&scratch.0);
        std::fs::remove_dir(&profile.cursor).unwrap();
        let machine = machine(&profile);
        assert_eq!(machine["harnesses"], json!(["claude", "codex"]));
        assert_eq!(machine["version"], VERSION);
        assert_eq!(machine["platform"], platform());
        let host = machine["host"].as_str().unwrap();
        assert!(!host.is_empty() && host != "rust-proof", "{host}");
    }
}
