//! `cargo xtask verify ARCHIVE`: unpack it elsewhere and run the connector from there.

use std::path::Path;
use std::process::Command;

use serde_json::json;

use crate::archive::unpack_checked;
use crate::util::*;
use crate::{Result, ENTRYPOINT, PACKAGE};

/// The libraries a Linux binary may load: the C library and its own parts, present on every distribution.
const LINUX_SYSTEM: [&str; 9] = [
    "linux-vdso.so.1",
    "libc.so.6",
    "libm.so.6",
    "libgcc_s.so.1",
    "libpthread.so.0",
    "libdl.so.2",
    "librt.so.1",
    "ld-linux-x86-64.so.2",
    "ld-linux-aarch64.so.1",
];

/// Unpacks the archive somewhere else (a path with a space), checks the binary loads only what every machine of
/// its target has, and runs it from there: `--version` and `runtime-identity --json` must name the inventory's
/// version, target and source commit.
pub(crate) fn verify(archive: &Path) -> Result<()> {
    let work = TempDir::new_in(Path::new("/tmp"), "sidevoice connector relocated")?;
    let (root, inventory) = unpack_checked(archive, &work.0)?;
    let binary = root.join(ENTRYPOINT);
    let binary_str = path_str(&binary)?;
    let target = inventory["target"].as_str().unwrap_or("");
    if target != host_target()? {
        return Err(format!(
            "the archive is for {target}, this machine is {}",
            host_target()?
        ));
    }

    let libraries = linked_libraries(binary_str, target)?;
    let foreign: Vec<&String> = libraries
        .iter()
        .filter(|library| !is_system_library(library, target))
        .collect();
    if !foreign.is_empty() {
        return Err(format!(
            "the binary loads libraries a machine may not have: {foreign:?}"
        ));
    }

    // A clean environment: nothing the build or the runner set may stand in for what the binary carries.
    let run = |args: &[&str]| -> Result<String> {
        let result = Command::new(&binary)
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .output()
            .map_err(|error| format!("{binary_str} {}: {error}", args.join(" ")))?;
        if !result.status.success() {
            return Err(format!(
                "{} {}: {}",
                ENTRYPOINT,
                args.join(" "),
                String::from_utf8_lossy(&result.stderr)
            ));
        }
        String::from_utf8(result.stdout).map_err(|_| "output is not UTF-8".into())
    };
    let version = run(&["--version"])?;
    let expected = format!("{PACKAGE} {}", inventory["version"].as_str().unwrap_or(""));
    if version.trim() != expected {
        return Err(format!(
            "--version says {:?}, the archive is {expected:?}",
            version.trim()
        ));
    }
    let identity = parse_json(
        run(&["runtime-identity", "--json"])?.as_bytes(),
        "runtime-identity",
    )?;
    for (field, wanted) in [
        ("version", &inventory["version"]),
        ("target", &inventory["target"]),
        ("source_sha", &inventory["source_sha"]),
    ] {
        if identity[field] != *wanted {
            return Err(format!(
                "runtime identity {field} is {}, the archive says {wanted}",
                identity[field]
            ));
        }
    }
    println!(
        "{}",
        json!({"target": target, "version": inventory["version"], "source_sha": inventory["source_sha"],
               "identity": identity, "libraries": libraries, "relocation": true,
               "files": inventory["files"].as_array().map(Vec::len)})
    );
    Ok(())
}

fn linked_libraries(binary: &str, target: &str) -> Result<Vec<String>> {
    if target.starts_with("linux-") {
        let listing = output("ldd", &[binary], None)?;
        Ok(listing
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .map(|name| name.rsplit('/').next().unwrap_or(name).to_string())
            .collect())
    } else {
        let listing = output("otool", &["-L", binary], None)?;
        Ok(listing
            .lines()
            .skip(1)
            .filter_map(|line| line.split_whitespace().next())
            .map(str::to_string)
            .collect())
    }
}

fn is_system_library(library: &str, target: &str) -> bool {
    if target.starts_with("linux-") {
        LINUX_SYSTEM.contains(&library)
    } else {
        library.starts_with("/usr/lib/") || library.starts_with("/System/Library/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_platform_libraries_count_as_present_everywhere() {
        assert!(is_system_library("libc.so.6", "linux-x86_64"));
        assert!(!is_system_library("libssl.so.3", "linux-x86_64"));
        assert!(is_system_library(
            "/usr/lib/libSystem.B.dylib",
            "macos-aarch64"
        ));
        assert!(is_system_library(
            "/System/Library/Frameworks/Security.framework/Versions/A/Security",
            "macos-aarch64"
        ));
        assert!(!is_system_library(
            "/opt/homebrew/lib/libzstd.1.dylib",
            "macos-aarch64"
        ));
    }
}
