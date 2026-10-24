//! Package-manager launchers → native agent programs. No shell or JavaScript is executed.
//!
//! A shim names the actual package entry, rather than an assumed global prefix. Canonicalizing that entry's
//! package directory is essential for pnpm: Node resolves optional dependencies beside the real `.pnpm` package.
//! `oxc_resolver` owns Node's ancestor `node_modules` lookup and symlinks; exports/main are irrelevant when locating
//! a package directory. Native installs and shims belonging to other packages are left alone.

use oxc_resolver::{ResolveOptions, Resolver};
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
    /// Older releases also carried this layout inside the main package (Codex's `vendor`).
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
    let resolver = Resolver::new(ResolveOptions {
        resolve_to_context: true,
        exports_fields: Vec::new(),
        ..ResolveOptions::default()
    });
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
    if let Ok(resolved) = resolver.resolve(&root, &dependency) {
        let program = resolved.path().join(&relative);
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
    // Older pure-JavaScript releases (notably Claude Code) do not declare any native package.
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

    use crate::adapters::registration::Registration;
    use std::sync::LazyLock;

    static CODEX: LazyLock<NativePackage> =
        LazyLock::new(|| crate::adapters::codex::Codex.npm_package().unwrap());
    static CLAUDE: LazyLock<NativePackage> =
        LazyLock::new(|| crate::adapters::claude::Claude.npm_package().unwrap());
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
            let root = modules.join(CODEX.package);
            package(&root, CODEX.package, "bin/codex.js");
            let dep = if nested {
                root.join("node_modules")
            } else {
                modules.clone()
            };
            let expected = native(&dep.join("@openai/codex-win32-x64"), &CODEX);
            // An installed package for another OS/architecture must never win.
            native(&dep.join("@openai/codex-darwin-x64"), &CODEX);
            let shim = if nested {
                prefix.join("codex.cmd")
            } else {
                modules.join(".bin/codex.cmd")
            };
            let relative = if nested { "node_modules/" } else { "../" };
            write(
                &shim,
                &format!(
                    r#"@ECHO off
"%dp0%\node.exe" "%dp0%\{relative}@openai/codex/bin/codex.js" %*
"#
                ),
            );
            assert_eq!(
                resolved(&shim, &CODEX).unwrap(),
                Some(dunce::canonicalize(expected).unwrap())
            );
        }
    }

    #[test]
    fn yarn_classic_global_shim_uses_its_entry_not_a_guessed_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let modules = dir.path().join("Data/global/node_modules");
        package(&modules.join(CODEX.package), CODEX.package, "bin/codex.js");
        let expected = native(&modules.join("@openai/codex-win32-x64"), &CODEX);
        let shim = dir.path().join("bin/codex.cmd");
        write(
            &shim,
            r#"@"%~dp0\..\Data\global\node_modules\@openai\codex\bin\codex.js" %*"#,
        );
        assert_eq!(
            resolved(&shim, &CODEX).unwrap(),
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
        let store = modules.join(".pnpm/@openai+codex@1.0.0/node_modules");
        let root = store.join(CODEX.package);
        package(&root, CODEX.package, "bin/codex.js");
        let native_root = modules
            .join(".pnpm/@openai+codex-win32-x64@1.0.0/node_modules/@openai/codex-win32-x64");
        let expected = native(&native_root, &CODEX);
        fs::create_dir_all(modules.join("@openai")).unwrap();
        std::os::unix::fs::symlink(&root, modules.join(CODEX.package)).unwrap();
        std::os::unix::fs::symlink(&native_root, store.join("@openai/codex-win32-x64")).unwrap();
        // A stale hoisted copy must not hide the package's own dependency.
        native(&modules.join("@openai/codex-win32-x64"), &CODEX);
        let shim = modules.join(".bin/codex.cmd");
        write(&shim, r#"@"%~dp0\..\@openai\codex\bin\codex.js" %*"#);
        assert_eq!(
            resolved(&shim, &CODEX).unwrap(),
            Some(dunce::canonicalize(expected).unwrap())
        );
    }

    #[test]
    fn claude_uses_its_native_platform_package_too() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("node_modules/@anthropic-ai/claude-code");
        package(&root, CLAUDE.package, "bin/claude.exe");
        let expected = native(
            &root.join("node_modules/@anthropic-ai/claude-code-win32-x64"),
            &CLAUDE,
        );
        let shim = dir.path().join("claude.cmd");
        write(
            &shim,
            r#"@"%dp0%\node_modules\@anthropic-ai\claude-code\bin\claude.exe" %*"#,
        );
        assert_eq!(
            resolved(&shim, &CLAUDE).unwrap(),
            Some(dunce::canonicalize(expected).unwrap())
        );
    }

    #[test]
    fn older_pure_javascript_claude_remains_a_launcher() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("node_modules/@anthropic-ai/claude-code");
        package(&root, CLAUDE.package, "cli.js");
        write(
            &root.join("package.json"),
            r#"{"name":"@anthropic-ai/claude-code"}"#,
        );
        let shim = dir.path().join("claude.cmd");
        write(
            &shim,
            r#"@"%dp0%\node_modules\@anthropic-ai\claude-code\cli.js" %*"#,
        );
        assert!(resolved(&shim, &CLAUDE).unwrap().is_none());
    }

    #[test]
    fn bundled_codex_and_missing_or_wrong_platform_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("node_modules/@openai/codex");
        package(&root, CODEX.package, "bin/codex.js");
        let shim = dir.path().join("codex.cmd");
        write(
            &shim,
            r#"@"%dp0%\node_modules\@openai\codex\bin\codex.js" %*"#,
        );
        native(&root.join("node_modules/@openai/codex-win32-arm64"), &CODEX);
        let error = resolved(&shim, &CODEX).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("optional dependencies"));
        let expected = native(&root, &CODEX);
        assert_eq!(
            resolved(&shim, &CODEX).unwrap(),
            Some(dunce::canonicalize(expected).unwrap())
        );
    }

    #[test]
    fn native_programs_unrelated_shims_and_pnp() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve(&dir.path().join("codex.exe"), &CODEX)
            .unwrap()
            .is_none());
        let shim = dir.path().join("codex.cmd");
        let other = dir.path().join("node_modules/other");
        package(&other, "other", "cli.js");
        write(&shim, r#"@"%dp0%\node_modules\other\cli.js" %*"#);
        assert!(resolved(&shim, &CODEX).unwrap().is_none());
        write(&dir.path().join(".pnp.cjs"), "");
        let error = resolved(&shim, &CODEX).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("nodeLinker: node-modules"));
    }
}
