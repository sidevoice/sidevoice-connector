//! `cargo xtask verify ARCHIVE`: unpack it elsewhere and run the connector from there, and the core it carries.
//! `cargo xtask verify-floor ARCHIVE`: the same, running the connector on the oldest Linux it supports.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;

use crate::archive::unpack_checked;
use crate::glibc;
use crate::util::*;
use crate::{entrypoint, Result, PACKAGE};

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

/// Where the unpacked archive is mounted in the container `verify-floor` runs.
const CONTAINER_ROOT: &str = "/opt/sidevoice-connector";

/// Unpacks the archive somewhere else (a path with a space), checks the binary loads only what every machine of
/// its target has and, on Linux, needs no glibc newer than the floor its inventory records, and runs it from
/// there: `--version` and `--version --json` must name the inventory's version, target and source commit, and
/// `stage-core` must stage the core the inventory names into a release, checked against the core's own inventory.
///
/// With `image` (a container image of this machine's architecture), the binary runs in that container instead,
/// read-only, without network; the container's own glibc must be the inventory's floor. The core is not staged
/// there: the floor is the connector binary's, and the pinned core release has its own (newer, until the core's
/// floor change is released), so its checks stay with the plain `verify` on the build machine.
pub(crate) fn verify(archive: &Path, image: Option<&str>) -> Result<()> {
    let work = TempDir::new_in(&scratch_base(), "sidevoice connector relocated")?;
    let (root, inventory) = unpack_checked(archive, &work.0)?;
    let target = inventory["target"].as_str().unwrap_or("");
    let entrypoint = entrypoint(target);
    let binary = root.join(&entrypoint);
    let binary_str = path_str(&binary)?;
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

    let floor = inventory["glibc"].as_str();
    let needed = match floor {
        Some(floor) => {
            let needed = glibc::needed(binary_str)?;
            glibc::check(&needed, floor)?;
            Some(needed)
        }
        None => None,
    };

    if let Some(image) = image {
        let floor = floor.ok_or("the archive records no glibc floor to run it on")?;
        let result = in_container(image, &root, &["ldd", "--version"])?
            .output()
            .map_err(|error| format!("docker: {error}"))?;
        let listing = String::from_utf8_lossy(&result.stdout);
        let present = listing
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().last())
            .unwrap_or("");
        if !result.status.success() || present != floor {
            return Err(format!(
                "{image} has glibc {present:?}, not the floor {floor}: {}",
                String::from_utf8_lossy(&result.stderr)
            ));
        }
    }

    // A clean environment: nothing the build or the runner set may stand in for what the binary carries.
    let run = |args: &[&str]| -> Result<String> {
        let result = match image {
            Some(image) => {
                let inside = format!("{CONTAINER_ROOT}/{entrypoint}");
                let mut full = vec![inside.as_str()];
                full.extend(args);
                in_container(image, &root, &full)?.output()
            }
            None => clean_environment(Command::new(&binary).args(args)).output(),
        };
        let result = result.map_err(|error| format!("{binary_str} {}: {error}", args.join(" ")))?;
        if !result.status.success() {
            return Err(format!(
                "{} {}: {}{}",
                entrypoint,
                args.join(" "),
                String::from_utf8_lossy(&result.stderr),
                String::from_utf8_lossy(&result.stdout)
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
        run(&["--version", "--json"])?.as_bytes(),
        "--version --json",
    )?;
    for (field, wanted) in [
        ("version", &inventory["version"]),
        ("target", &inventory["target"]),
        ("source_sha", &inventory["source_sha"]),
    ] {
        if identity[field] != *wanted {
            return Err(format!(
                "--version --json says {field} is {}, the archive says {wanted}",
                identity[field]
            ));
        }
    }
    // The core inside: staged into a release by the connector itself, as an install does it. Not in
    // the floor's container (read-only, and the core's own glibc floor is not the connector's yet).
    let staged = match image {
        Some(_) => None,
        None => {
            let release = work.0.join("release");
            mkdir(&release)?;
            chmod(&release, 0o700)?;
            let staged = parse_json(
                run(&["stage-core", path_str(&release)?, "--json"])?.as_bytes(),
                "stage-core --json",
            )?;
            for field in ["version", "sha256", "size", "source_sha"] {
                if staged["core"][field] != inventory["core"][field] {
                    return Err(format!(
                        "stage-core staged a core whose {field} is {}, the archive says {}",
                        staged["core"][field], inventory["core"][field]
                    ));
                }
            }
            Some(staged)
        }
    };
    println!(
        "{}",
        json!({"target": target, "version": inventory["version"], "source_sha": inventory["source_sha"],
               "identity": identity, "libraries": libraries, "relocation": true,
               "glibc": {"floor": floor, "needed": needed}, "ran_in": image,
               "files": inventory["files"].as_array().map(Vec::len),
               "core": staged.as_ref().map(|staged| &staged["core"])})
    );
    Ok(())
}

/// `args` run in a throwaway container of `image`, with the unpacked archive `root` mounted read-only at
/// [`CONTAINER_ROOT`] and no network.
fn in_container(image: &str, root: &Path, args: &[&str]) -> Result<Command> {
    let mut command = Command::new("docker");
    command
        .args(["run", "--rm", "--network", "none", "--mount"])
        .arg(format!(
            "type=bind,source={},target={CONTAINER_ROOT},readonly",
            path_str(root)?
        ))
        .arg(image)
        .args(args);
    Ok(command)
}

/// Where `verify` unpacks: a path with a space, away from the checkout (`/tmp`; Windows has no such place, so the
/// user's temporary directory).
fn scratch_base() -> PathBuf {
    if cfg!(windows) {
        std::env::temp_dir()
    } else {
        PathBuf::from("/tmp")
    }
}

/// A clean environment: nothing the build or the runner set may stand in for what the binary carries. Windows
/// programs need `SystemRoot` to start at all (Winsock, the loader), and look their DLLs up in System32.
fn clean_environment(command: &mut Command) -> &mut Command {
    command.env_clear();
    if cfg!(windows) {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        command
            .env("PATH", format!(r"{root}\System32"))
            .env("SystemRoot", root)
    } else {
        command.env("PATH", "/usr/bin:/bin")
    }
}

/// The DLLs a PE file imports (the import table; delay-loaded ones included).
fn imported_dlls(binary: &str) -> Result<Vec<String>> {
    let data = read(Path::new(binary))?;
    let file = object::read::pe::PeFile64::parse(data.as_slice())
        .map_err(|error| format!("{binary}: {error}"))?;
    let mut names = Vec::new();
    for import in object::Object::imports(&file).map_err(|error| format!("{binary}: {error}"))? {
        let import = import.map_err(|error| format!("{binary}: {error}"))?;
        names.push(String::from_utf8_lossy(import.library()).to_ascii_lowercase());
    }
    names.sort();
    names.dedup();
    Ok(names)
}

fn linked_libraries(binary: &str, target: &str) -> Result<Vec<String>> {
    if target.starts_with("windows-") {
        imported_dlls(binary)
    } else if target.starts_with("linux-") {
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

/// Whether a Windows DLL is on every machine: an API set (resolved by the loader) or a file in System32, and not
/// the Visual C++ runtime, which only some machines have (the binary links it statically, `+crt-static`).
fn is_windows_system_dll(library: &str, system32: &Path) -> bool {
    let library = library.to_ascii_lowercase();
    const RUNTIME: [&str; 5] = ["vcruntime", "msvcp", "concrt", "vccorlib", "vcomp"];
    if RUNTIME.iter().any(|prefix| library.starts_with(prefix)) {
        return false;
    }
    library.starts_with("api-ms-win-")
        || library.starts_with("ext-ms-win-")
        || system32.join(&library).is_file()
}

fn is_system_library(library: &str, target: &str) -> bool {
    if target.starts_with("windows-") {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        is_windows_system_dll(library, &Path::new(&root).join("System32"))
    } else if target.starts_with("linux-") {
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

    #[test]
    fn a_windows_binary_may_load_api_sets_and_system_dlls_but_not_the_visual_c_runtime() {
        let system32 = TempDir::new("xtask-system32").unwrap();
        write(&system32.0.join("kernel32.dll"), b"").unwrap();
        write(&system32.0.join("vcruntime140.dll"), b"").unwrap();
        assert!(is_windows_system_dll("KERNEL32.dll", &system32.0));
        assert!(is_windows_system_dll(
            "api-ms-win-core-synch-l1-2-0.dll",
            &system32.0
        ));
        assert!(!is_windows_system_dll("vcruntime140.dll", &system32.0));
        assert!(!is_windows_system_dll("MSVCP140.dll", &system32.0));
        assert!(!is_windows_system_dll("libzstd.dll", &system32.0));
    }
}
