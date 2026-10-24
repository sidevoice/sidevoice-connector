//! Package-manager launchers → native agent programs. No shell or JavaScript is executed.
//!
//! A shim names the actual package entry, rather than an assumed global prefix. Canonicalizing that entry's
//! package directory is essential for pnpm: Node resolves optional dependencies beside the real `.pnpm` package.
//! Only Node's package-directory lookup is needed: exports/main do not affect locating that directory.
//! Native installs and shims belonging to other packages are left alone.

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
    /// Older releases also carried this layout inside the main package (a legacy `vendor` tree).
    pub bundled: bool,
}

/// Resolve a Windows `.cmd` launcher. `None` means it is a native program or an unrelated launcher.
/// A recognized launcher with missing native dependencies, or Yarn PnP, is an actionable error rather than a
/// fallback to cmd.exe (which cannot carry arbitrary message arguments).
pub fn resolve(launcher: &Path, package: &NativePackage) -> io::Result<Option<PathBuf>> {
    if !is_cmd(launcher) {
        return Ok(None);
    }
    // Only .cmd launchers need replacing. Keep platform selection here, and test their layouts on every host.
    let (suffix, target) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => ("win32-x64", "x86_64-pc-windows-msvc"),
        ("windows", "aarch64") => ("win32-arm64", "aarch64-pc-windows-msvc"),
        _ => return Ok(None),
    };
    resolve_for(launcher, package, suffix, target)
}

fn is_cmd(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd"))
}

fn resolve_for(
    launcher: &Path,
    package: &NativePackage,
    suffix: &str,
    target: &str,
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
    let relative = package
        .program
        .replace("{target}", target)
        .replace("{exe}", ".exe");
    let dependency = format!("{}{suffix}", package.platform_prefix);
    if let Some(resolved) = node_package(&root, &dependency) {
        let program = resolved.join(&relative);
        if program.is_file() {
            return Ok(Some(program));
        }
    }
    if package.bundled {
        let program = root.join(relative);
        if program.is_file() {
            return Ok(Some(program));
        }
    }
    // Older pure-JavaScript releases do not declare any native package.
    // Their CLI remains usable through the original launcher.
    if !package.bundled {
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
        format!("{} native program is missing ({dependency}); reinstall the agent with optional dependencies enabled", package.package),
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

fn has_pnp(dir: &Path) -> bool {
    dir.ancestors()
        .any(|parent| parent.join(".pnp.cjs").is_file() || parent.join(".pnp.js").is_file())
}

fn pnp_error() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported,
        "Yarn Plug'n'Play (PnP) agent installs are not supported; use Yarn nodeLinker: node-modules or install the agent with npm/pnpm")
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT: NativePackage = NativePackage {
        package: "@example/agent",
        platform_prefix: "@example/agent-",
        program: "vendor/{target}/bin/agent{exe}",
        bundled: true,
    };
    const OTHER: NativePackage = NativePackage {
        package: "@example/other",
        platform_prefix: "@example/other-",
        program: "other{exe}",
        bundled: false,
    };
    const TARGET: &str = "x86_64-pc-windows-msvc";

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn package(root: &Path, name: &str, entry: &str) {
        write(
            &root.join("package.json"),
            &format!(
                r#"{{"name":"{name}","exports":{{".":"./hidden.js"}},"optionalDependencies":{{"{name}-win32-x64":"1.0.0"}}}}"#
            ),
        );
        write(&root.join(entry), "");
    }

    fn native(root: &Path, spec: &NativePackage) -> PathBuf {
        let path = root.join(
            spec.program
                .replace("{target}", TARGET)
                .replace("{exe}", ".exe"),
        );
        write(&path, "native");
        path
    }

    fn resolved(shim: &Path, spec: &NativePackage) -> io::Result<Option<PathBuf>> {
        resolve_for(shim, spec, "win32-x64", TARGET)
    }

    #[test]
    fn npm_global_nested_and_local_hoisted_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        for nested in [true, false] {
            let prefix = dir.path().join(if nested { "global" } else { "local" });
            let modules = prefix.join("node_modules");
            let root = modules.join(AGENT.package);
            package(&root, AGENT.package, "bin/agent.js");
            let dep = if nested {
                root.join("node_modules")
            } else {
                modules.clone()
            };
            let expected = native(&dep.join("@example/agent-win32-x64"), &AGENT);
            // An installed package for another OS/architecture must never win.
            native(&dep.join("@example/agent-darwin-x64"), &AGENT);
            let shim = if nested {
                prefix.join("agent.cmd")
            } else {
                modules.join(".bin/agent.cmd")
            };
            let text = include_str!("npm/tests/npm.cmd");
            let text = if nested {
                text.to_owned()
            } else {
                text.replace(r"\node_modules\", r"\..\")
            };
            write(&shim, &text);
            assert_eq!(
                resolved(&shim, &AGENT).unwrap(),
                Some(dunce::canonicalize(expected).unwrap())
            );
        }
    }

    #[test]
    fn yarn_classic_global_shim_uses_its_entry_not_a_guessed_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let modules = dir.path().join("Data/global/node_modules");
        package(&modules.join(AGENT.package), AGENT.package, "bin/agent.js");
        let expected = native(&modules.join("@example/agent-win32-x64"), &AGENT);
        let shim = dir.path().join("bin/agent.cmd");
        write(&shim, include_str!("npm/tests/yarn.cmd"));
        assert_eq!(
            resolved(&shim, &AGENT).unwrap(),
            Some(dunce::canonicalize(expected).unwrap())
        );
    }

    // Symlinks need Developer Mode/privilege on Windows. Unix exercises pnpm's real linked store; the remaining
    // fixture trees and Windows batch syntax are tested on every host.
    #[cfg(unix)]
    #[test]
    fn pnpm_follows_package_symlink_and_resolves_optional_dependency_in_store() {
        let dir = tempfile::tempdir().unwrap();
        let modules = dir.path().join("node_modules");
        let store = modules.join(".pnpm/@example+agent@1.0.0/node_modules");
        let root = store.join(AGENT.package);
        package(&root, AGENT.package, "bin/agent.js");
        let native_root = modules
            .join(".pnpm/@example+agent-win32-x64@1.0.0/node_modules/@example/agent-win32-x64");
        let expected = native(&native_root, &AGENT);
        fs::create_dir_all(modules.join("@example")).unwrap();
        std::os::unix::fs::symlink(&root, modules.join(AGENT.package)).unwrap();
        std::os::unix::fs::symlink(&native_root, store.join("@example/agent-win32-x64")).unwrap();
        // A stale hoisted copy must not hide the package's own dependency.
        native(&modules.join("@example/agent-win32-x64"), &AGENT);
        let shim = modules.join(".bin/agent.cmd");
        write(&shim, include_str!("npm/tests/pnpm.cmd"));
        assert_eq!(
            resolved(&shim, &AGENT).unwrap(),
            Some(dunce::canonicalize(expected).unwrap())
        );
    }

    #[test]
    fn other_uses_its_native_platform_package_too() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("node_modules/@example/other");
        package(&root, OTHER.package, "bin/other.exe");
        let expected = native(&root.join("node_modules/@example/other-win32-x64"), &OTHER);
        let shim = dir.path().join("other.cmd");
        write(
            &shim,
            r#"@"%dp0%\node_modules\@example\other\bin\other.exe" %*"#,
        );
        assert_eq!(
            resolved(&shim, &OTHER).unwrap(),
            Some(dunce::canonicalize(expected).unwrap())
        );
    }

    #[test]
    fn older_pure_javascript_other_remains_a_launcher() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("node_modules/@example/other");
        package(&root, OTHER.package, "cli.js");
        write(&root.join("package.json"), r#"{"name":"@example/other"}"#);
        let shim = dir.path().join("other.cmd");
        write(&shim, r#"@"%dp0%\node_modules\@example\other\cli.js" %*"#);
        assert!(resolved(&shim, &OTHER).unwrap().is_none());
    }

    #[test]
    fn bundled_agent_and_missing_or_wrong_platform_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("node_modules/@example/agent");
        package(&root, AGENT.package, "bin/agent.js");
        let shim = dir.path().join("agent.cmd");
        write(
            &shim,
            r#"@"%dp0%\node_modules\@example\agent\bin\agent.js" %*"#,
        );
        native(
            &root.join("node_modules/@example/agent-win32-arm64"),
            &AGENT,
        );
        let error = resolved(&shim, &AGENT).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("optional dependencies"));
        let expected = native(&root, &AGENT);
        assert_eq!(
            resolved(&shim, &AGENT).unwrap(),
            Some(dunce::canonicalize(expected).unwrap())
        );
    }

    #[test]
    fn native_programs_unrelated_shims_and_pnp() {
        let dir = tempfile::tempdir().unwrap();
        // Bun installs executable launchers: leave their argument handling to the executable.
        let executable = dir.path().join("agent.exe");
        write(&executable, "MZ");
        assert!(resolve(&executable, &AGENT).unwrap().is_none());
        let shim = dir.path().join("agent.cmd");
        let other = dir.path().join("node_modules/other");
        package(&other, "other", "cli.js");
        write(&shim, r#"@"%dp0%\node_modules\other\cli.js" %*"#);
        assert!(resolved(&shim, &AGENT).unwrap().is_none());
        write(&dir.path().join(".pnp.cjs"), "");
        let error = resolved(&shim, &AGENT).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("nodeLinker: node-modules"));
    }
}
