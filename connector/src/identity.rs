//! What this machine says about itself, in the one place that says it: at every handshake with the core, which
//! keeps the latest so that a person looking at the list of paired machines reads a machine and not a UUID. Every
//! field is read from what this machine actually is: nothing is asked of a harness or of a client, and nothing is
//! guessed.

use crate::profile::Profile;
use serde_json::{json, Value};

/// This build's version: the release version stamped at build time (`build.rs`).
pub const VERSION: &str = env!("SIDEVOICE_CONNECTOR_VERSION");

/// `--version --json` advertises this only for the guarded, recoverable uninstall contract.
pub const SAFE_UNINSTALL: u8 = 1;

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
    let name = crate::platform::user::host_name();
    if name.is_empty() {
        "unknown".into()
    } else {
        name
    }
}

/// The platform, said the way a person reading the app's list of machines calls it: `macOS arm64`, `Linux x64`. A system or
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

/// Which harnesses live on this machine, by the configuration home each keeps. Nothing is run to find out.
pub fn harnesses(profile: &Profile) -> Vec<&'static str> {
    crate::adapters::Registry::standard()
        .agents()
        .filter(|(_, registration)| registration.config_root(profile).exists())
        .map(|(id, _)| id)
        .collect()
}

/// This machine, as a client will list it.
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
        let profile = Profile::for_test(&scratch.0, &crate::adapters::Registry::standard().homes());
        let registry = crate::adapters::Registry::standard();
        let mut agents: Vec<_> = registry.agents().collect();
        let (gone, registration) = agents.pop().unwrap();
        std::fs::remove_dir(registration.config_root(&profile)).unwrap();
        let machine = machine(&profile);
        let present: Vec<_> = agents.iter().map(|(id, _)| *id).collect();
        assert_eq!(machine["harnesses"], json!(present));
        assert!(!present.contains(&gone));
        assert_eq!(machine["version"], VERSION);
        assert_eq!(machine["platform"], platform());
        let host = machine["host"].as_str().unwrap();
        assert!(!host.is_empty() && host != "rust-proof", "{host}");
    }
}
