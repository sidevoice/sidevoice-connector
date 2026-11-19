//! The release build's `--remap-path-prefix` flags: a panic's location, and the paths in the debug files, name the file
//! relative to its crate or the repository, never a directory of the machine that built it (crash reports carry them).
//!
//! - the repository root: `connector/src/main.rs`;
//! - the registry's sources, `$CARGO_HOME/registry/src/<index>/`: `serde-1.0.0/src/lib.rs`;
//! - the git dependencies' checkouts, `$CARGO_HOME/git/checkouts/<name>-<hash>/<rev>/`: `<name>/src/lib.rs`;
//! - the toolchain's sources, `<sysroot>/lib/rustlib/src/rust/`: `rust/library/...` (the standard library the toolchain ships
//!   is already built as `/rustc/<hash>/...`).
//!
//! They go in as `--config` arguments of the build (`target.'cfg(all())'.rustflags`, which cargo joins to the flags of
//! `.cargo/config.toml`), not as `RUSTFLAGS`, which would replace those. Only `dist` passes them: development and test builds
//! keep their absolute paths, which their editors and debuggers follow.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::util::output;
use crate::Result;

/// The arguments to add to the `cargo build` / `cargo zigbuild` of the release.
pub(crate) fn cargo_args(repo: &Path) -> Result<Vec<String>> {
    let list: Vec<String> = prefixes(repo)?
        .into_iter()
        .map(|(from, to)| toml_string(&format!("--remap-path-prefix={}={to}", from.display())))
        .collect();
    Ok(vec![
        "--config".into(),
        format!("target.'cfg(all())'.rustflags=[{}]", list.join(",")),
    ])
}

/// What to strip and what to put there.
fn prefixes(repo: &Path) -> Result<Vec<(PathBuf, String)>> {
    let mut found = vec![(repo.to_path_buf(), String::new())];
    let home = cargo_home()?;
    for source in children(&home.join("registry").join("src")) {
        found.push((source, String::new()));
    }
    for checkout in children(&home.join("git").join("checkouts")) {
        let name = checkout
            .file_name()
            .map(|name| unhashed(&name.to_string_lossy()))
            .unwrap_or_default();
        for revision in children(&checkout) {
            found.push((revision, name.clone()));
        }
    }
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let sysroot = PathBuf::from(output(&rustc, &["--print", "sysroot"], None)?.trim());
    let sources = sysroot.join("lib").join("rustlib").join("src").join("rust");
    if sources.is_dir() {
        found.push((sources, "rust".into()));
    }
    Ok(found)
}

/// Fails when `binary` still holds the repository's or the cargo home's path: the flags above did not reach something that
/// panics, and a crash report would name this machine.
pub(crate) fn check(binary: &Path, repo: &Path) -> Result<()> {
    let bytes = fs::read(binary).map_err(|error| format!("{}: {error}", binary.display()))?;
    for root in [repo.to_path_buf(), cargo_home()?] {
        let text = root.display().to_string();
        for needle in [text.clone(), text.replace('\\', "/")] {
            if contains(&bytes, needle.as_bytes()) {
                return Err(format!(
                    "{} still holds the build machine's path {needle}: a panic location would name it (xtask/src/remap.rs)",
                    binary.display()
                ));
            }
        }
    }
    Ok(())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn cargo_home() -> Result<PathBuf> {
    if let Some(home) = env::var_os("CARGO_HOME") {
        return Ok(PathBuf::from(home));
    }
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .ok_or("neither CARGO_HOME nor the home directory is set")?;
    Ok(Path::new(&home).join(".cargo"))
}

/// The directories directly inside `dir`; none when it does not exist (nothing was downloaded there).
fn children(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    found.sort();
    found
}

/// Cargo names a checkout's directory `<name>-<16 hex digits>`: the name alone.
fn unhashed(directory: &str) -> String {
    match directory.rsplit_once('-') {
        Some((name, hash))
            if hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
        {
            name.into()
        }
        _ => directory.into(),
    }
}

fn toml_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_windows_path_is_quoted_for_toml() {
        assert_eq!(
            toml_string(r#"--remap-path-prefix=C:\a "b"="#),
            r#""--remap-path-prefix=C:\\a \"b\"=""#
        );
    }

    #[test]
    fn a_checkout_loses_the_hash_cargo_gave_its_directory() {
        assert_eq!(
            unhashed("sidevoice-engine-3178c43051c01349"),
            "sidevoice-engine"
        );
        assert_eq!(unhashed("plain"), "plain");
    }

    #[test]
    fn a_binary_holding_a_path_is_refused() {
        assert!(contains(b"..\x00/build/repo/rust/x.rs\x00", b"/build/repo"));
        assert!(!contains(b"rust/x.rs", b"/build/repo"));
        assert!(!contains(b"anything", b""));
    }

    #[test]
    fn the_repository_comes_first_and_maps_to_nothing() {
        let args = cargo_args(Path::new("/build/repo")).unwrap();
        assert_eq!(args[0], "--config");
        assert!(args[1]
            .starts_with("target.'cfg(all())'.rustflags=[\"--remap-path-prefix=/build/repo=\""));
    }
}
