//! Package-manager launchers → native agent programs. No shell or JavaScript is executed.
//!
//! A shim names the actual package entry, rather than an assumed global prefix. Canonicalizing that entry's
//! package directory is essential for pnpm: Node resolves optional dependencies beside the real `.pnpm` package.
//! Only Node's package-directory lookup is needed: exports/main do not affect locating that directory.
//! Native installs and shims belonging to other packages are left alone.

use super::architecture::{Architecture, Cpu, Native};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// A harness's published npm package and native layout. Platform names use npm's spelling.
pub struct NativePackage {
    pub package: &'static str,
    /// Optional dependency name prefix, followed by e.g. `win32-x64`.
    pub platform_prefix: &'static str,
    /// Path inside that dependency; `{target}` is the Rust target, `{exe}` the executable suffix.
    pub program: &'static str,
}

/// Resolve a Windows `.cmd` launcher. `None` means it is a native program or an unrelated launcher.
/// Missing dependencies and Yarn PnP return diagnostics; callers keep the original launcher on resolution failure.
pub fn resolve(launcher: &Path, package: &NativePackage) -> io::Result<Option<PathBuf>> {
    if !is_cmd(launcher) {
        return Ok(None);
    }
    if std::env::consts::OS != "windows" {
        return Ok(None);
    }
    resolve_for(launcher, package, windows_targets(Native::native()?))
}

/// Prefer the machine's native package; x64 is also runnable through Windows ARM64 emulation.
fn windows_targets(cpu: Cpu) -> &'static [(&'static str, &'static str)] {
    match cpu {
        Cpu::X64 => &[("win32-x64", "x86_64-pc-windows-msvc")],
        Cpu::Arm64 => &[
            ("win32-arm64", "aarch64-pc-windows-msvc"),
            ("win32-x64", "x86_64-pc-windows-msvc"),
        ],
    }
}

fn is_cmd(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd"))
}

fn resolve_for(
    launcher: &Path,
    package: &NativePackage,
    targets: &[(&str, &str)],
) -> io::Result<Option<PathBuf>> {
    let text = fs::read_to_string(launcher)?;
    let dir = launcher
        .parent()
        .ok_or_else(|| io::Error::other("launcher has no directory"))?;
    let Some(root) = package_root(&text, dir, package.package) else {
        if text.contains(".pnp.cjs") || text.contains(".pnp.js") || has_pnp(dir) {
            return Err(pnp_error());
        }
        return Ok(None);
    };
    for (suffix, target) in targets {
        let relative = package
            .program
            .replace("{target}", target)
            .replace("{exe}", ".exe");
        let dependency = format!("{}{suffix}", package.platform_prefix);
        if let Some(resolved) = node_package(&root, &dependency) {
            let program = resolved.join(relative);
            if program.is_file() {
                return Ok(Some(program));
            }
        }
    }
    // Older pure-JavaScript releases do not declare any native package.
    // Their CLI remains usable through the original launcher.
    {
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("package.json"))?)?;
        let native_declared = manifest["optionalDependencies"]
            .as_object()
            .is_some_and(|deps| {
                deps.keys()
                    .any(|name| name.starts_with(package.platform_prefix))
            });
        if !native_declared {
            return Ok(None);
        }
    }
    if has_pnp(&root) || has_pnp(dir) {
        return Err(pnp_error());
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "{} native program is missing (tried {})",
            package.package,
            targets
                .iter()
                .map(|(suffix, _)| format!("{}{suffix}", package.platform_prefix))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ))
}

/// Node's LOAD_NODE_MODULES directory search, from the real package path. A node_modules ancestor is itself a
/// search root (never node_modules/node_modules). Resolve each candidate before returning so linked stores and
/// Windows junctions behave as Node's default realpath lookup does. No main/exports or JavaScript evaluation.
fn node_package(from: &Path, name: &str) -> Option<PathBuf> {
    from.ancestors()
        .filter(|parent| parent.file_name().is_none_or(|name| name != "node_modules"))
        .map(|parent| parent.join("node_modules").join(name))
        .find_map(|candidate| {
            dunce::canonicalize(candidate)
                .ok()
                .filter(|path| path.is_dir())
        })
}

/// npm/yarn use `%~dp0` or `%dp0%`; pnpm also emits `%~dp0\..\<package>`. Only quoted, existing entry paths
/// belonging to the declared package are accepted. No environment expansion or batch evaluation takes place.
fn package_root(text: &str, dir: &Path, name: &str) -> Option<PathBuf> {
    for quoted in text.split('"').skip(1).step_by(2) {
        let lower = quoted.to_ascii_lowercase();
        let relative = if lower.starts_with("%~dp0") || lower.starts_with("%dp0%") {
            &quoted[5..]
        } else {
            continue;
        };
        let relative = relative.replace('\\', "/");
        let entry = dir.join(relative.trim_start_matches('/'));
        // Canonicalize before walking ancestors so a symlinked package sees its own node_modules.
        let Ok(entry) = dunce::canonicalize(entry) else {
            continue;
        };
        if !entry.is_file() {
            continue;
        }
        for root in entry.ancestors().skip(1) {
            if let Ok(bytes) = fs::read(root.join("package.json")) {
                if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    if json["name"].as_str() == Some(name) {
                        return Some(root.to_path_buf());
                    }
                }
                // The nearest package.json owns this entry; do not claim an unrelated dependency's launcher.
                break;
            }
        }
    }
    None
}

/// Check only the nearest project boundary. An unrelated PnP file above that project must not classify its agent.
fn has_pnp(dir: &Path) -> bool {
    let project = dir
        .ancestors()
        .find(|parent| parent.join("package.json").is_file());
    project.is_some_and(|root| root.join(".pnp.cjs").is_file() || root.join(".pnp.js").is_file())
}

fn pnp_error() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported,
        "Yarn Plug'n'Play (PnP) native agent resolution is not supported; use Yarn nodeLinker: node-modules or install the agent with npm/pnpm")
}

#[cfg(test)]
#[path = "npm/tests.rs"]
mod tests;
