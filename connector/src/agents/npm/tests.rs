use super::*;

use crate::adapters::registration::Registration;
use crate::platform::fs::{Files, TestFiles};
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
    resolve_for(shim, spec, windows_targets(Cpu::X64))
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
        let text = include_str!("tests/npm.cmd");
        let text = if nested {
            text.to_owned()
        } else {
            text.replace(r"\node_modules\", r"\..\")
        };
        write(&shim, &text);
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
    write(&shim, include_str!("tests/yarn.cmd"));
    assert_eq!(
        resolved(&shim, &CODEX).unwrap(),
        Some(dunce::canonicalize(expected).unwrap())
    );
}

#[test]
fn pnpm_follows_package_symlink_and_resolves_optional_dependency_in_store() {
    let dir = tempfile::tempdir().unwrap();
    let modules = dir.path().join("node_modules");
    let store = modules.join(".pnpm/@openai+codex@1.0.0/node_modules");
    let root = store.join(CODEX.package);
    package(&root, CODEX.package, "bin/codex.js");
    let native_root =
        modules.join(".pnpm/@openai+codex-win32-x64@1.0.0/node_modules/@openai/codex-win32-x64");
    let expected = native(&native_root, &CODEX);
    fs::create_dir_all(modules.join("@openai")).unwrap();
    Files::symlink(&root, modules.join(CODEX.package));
    Files::symlink(&native_root, store.join("@openai/codex-win32-x64"));
    // A stale hoisted copy must not hide the package's own dependency.
    native(&modules.join("@openai/codex-win32-x64"), &CODEX);
    let shim = modules.join(".bin/codex.cmd");
    write(&shim, include_str!("tests/pnpm.cmd"));
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
fn missing_native_or_wrong_architecture_is_a_diagnostic_not_a_bundled_guess() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("node_modules/@openai/codex");
    package(&root, CODEX.package, "bin/codex.js");
    let shim = dir.path().join("codex.cmd");
    write(&shim, include_str!("tests/npm.cmd"));
    native(&root.join("node_modules/@openai/codex-win32-arm64"), &CODEX);
    // Older releases had different layouts; neither is guessed by this resolver.
    write(
        &root.join(format!("vendor/{TARGET}/codex/codex.exe")),
        "legacy",
    );
    write(&root.join(format!("bin/codex-{TARGET}.exe")), "legacy");
    let error = resolved(&shim, &CODEX).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert!(error.to_string().contains("native program is missing"));
}

#[test]
fn arm64_host_prefers_native_packages_then_falls_back_to_x64() {
    let dir = tempfile::tempdir().unwrap();
    let modules = dir.path().join("node_modules");
    package(&modules.join(CODEX.package), CODEX.package, "bin/codex.js");
    let shim = dir.path().join("codex.cmd");
    write(&shim, include_str!("tests/npm.cmd"));
    let x64 = native(&modules.join("@openai/codex-win32-x64"), &CODEX);
    assert_eq!(
        resolve_for(&shim, &CODEX, windows_targets(Cpu::Arm64)).unwrap(),
        Some(dunce::canonicalize(&x64).unwrap())
    );
    let arm64 =
        modules.join("@openai/codex-win32-arm64/vendor/aarch64-pc-windows-msvc/bin/codex.exe");
    write(&arm64, "native arm64");
    assert_eq!(
        resolve_for(&shim, &CODEX, windows_targets(Cpu::Arm64)).unwrap(),
        Some(dunce::canonicalize(&arm64).unwrap())
    );
    fs::remove_file(&x64).unwrap();
    assert_eq!(
        resolve_for(&shim, &CODEX, windows_targets(Cpu::Arm64)).unwrap(),
        Some(dunce::canonicalize(&arm64).unwrap())
    );
    assert!(resolve_for(&shim, &CODEX, windows_targets(Cpu::X64)).is_err());
}

#[test]
fn native_programs_unrelated_shims_and_pnp() {
    let dir = tempfile::tempdir().unwrap();
    // Bun installs executable launchers: leave their argument handling to the executable.
    let executable = dir.path().join("codex.exe");
    write(&executable, "MZ");
    assert!(resolve(&executable, &CODEX).unwrap().is_none());
    let shim = dir.path().join("codex.cmd");
    let other = dir.path().join("node_modules/other");
    package(&other, "other", "cli.js");
    write(&shim, r#"@"%dp0%\node_modules\other\cli.js" %*"#);
    assert!(resolved(&shim, &CODEX).unwrap().is_none());
    write(&dir.path().join("package.json"), "{}");
    write(&dir.path().join(".pnp.cjs"), "");
    let error = resolved(&shim, &CODEX).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert!(error.to_string().contains("nodeLinker: node-modules"));
}

#[test]
fn unrelated_ancestor_pnp_does_not_classify_a_separate_project() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("package.json"), "{}");
    write(&dir.path().join(".pnp.cjs"), "");
    let project = dir.path().join("project");
    write(&project.join("package.json"), "{}");
    let shim = project.join("node_modules/.bin/codex.cmd");
    write(&shim, "@echo unrelated launcher");
    assert!(resolved(&shim, &CODEX).unwrap().is_none());
}
