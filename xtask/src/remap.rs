//! The release build's `--remap-path-prefix` flags: a panic's location, and the paths in the debug files, name the file
//! relative to its crate or the repository, never a directory of the machine that built it (crash reports carry them).
//!
//! Fixed roots, never an enumeration of what is on disk (a cache that restores some directories and not others would
//! change the flags from run to run, and every change rebuilds every crate):
//!
//! - the repository root: `src/main.rs`;
//! - the registry's sources, `$CARGO_HOME/registry/src/` -> `registry/`: `registry/<index>/serde-1.0.0/src/lib.rs`
//!   (`<index>` is cargo's name for the registry, a hash of its URL: the same on every machine);
//! - the git dependencies' checkouts, `$CARGO_HOME/git/checkouts/` -> `git/`: `git/<name>-<hash>/<rev>/src/lib.rs`
//!   (they need not exist when the flags are made: a checkout is made by the build itself);
//! - the toolchain's sources, `<sysroot>/lib/rustlib/src/rust/` -> `rust/` (the standard library the toolchain ships is
//!   already built as `/rustc/<hash>/...`).
//!
//! They go in as `--config` arguments of the build (`target.'cfg(all())'.rustflags`, which cargo joins to the flags of
//! `.cargo/config.toml`), not as `RUSTFLAGS`, which would replace those. When two roots nest the later one wins (rustc), so
//! the repository comes first.
//!
//! `check` then scans the shipped binary for what is left. It looks for Rust: a panic's location is a path that ends in
//! `.rs`. C and C++ sources compiled by a build script (aws-lc, whisper.cpp) put their own `__FILE__` in the binary, an
//! absolute path in a directory of this machine that `--remap-path-prefix` (rustc's flag) never touches; those name no Rust
//! panic and a crash report does not carry them, so they are not a failure.
//!
//! This file is the same in sidevoice-core, sidevoice-connector and sidevoice-desktop (xtask/src/remap.rs).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The arguments to add to the `cargo build` of the release. `toolchain_dir` is where `rustc` finds the toolchain the build
/// uses (its `rust-toolchain.toml`).
pub fn cargo_args(repo: &Path, toolchain_dir: &Path) -> Result<Vec<String>, String> {
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let output = Command::new(&rustc)
        .args(["--print", "sysroot"])
        .current_dir(toolchain_dir)
        .output()
        .map_err(|error| format!("{rustc} --print sysroot: {error}"))?;
    if !output.status.success() {
        return Err(format!("{rustc} --print sysroot failed"));
    }
    let sysroot = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    Ok(config_args(repo, &cargo_home()?, &sysroot))
}

/// What the flags are for these roots.
fn config_args(repo: &Path, cargo_home: &Path, sysroot: &Path) -> Vec<String> {
    let list: Vec<String> = prefixes(repo, cargo_home, sysroot)
        .into_iter()
        .map(|(from, to)| toml_string(&format!("--remap-path-prefix={}={to}", from.display())))
        .collect();
    vec![
        "--config".into(),
        format!("target.'cfg(all())'.rustflags=[{}]", list.join(",")),
    ]
}

/// What to strip and what to put there: always these four, in this order.
fn prefixes(repo: &Path, cargo_home: &Path, sysroot: &Path) -> Vec<(PathBuf, String)> {
    vec![
        (repo.to_path_buf(), String::new()),
        (cargo_home.join("registry").join("src"), "registry".into()),
        (cargo_home.join("git").join("checkouts"), "git".into()),
        (sysroot.join("lib").join("rustlib").join("src").join("rust"), "rust".into()),
    ]
}

/// Fails when `binary` still holds a Rust path of the repository or of the cargo home: the flags above did not reach
/// something that panics, and a crash report would name this machine.
pub fn check(binary: &Path, repo: &Path) -> Result<(), String> {
    let bytes = fs::read(binary).map_err(|error| format!("{}: {error}", binary.display()))?;
    match leak(&bytes, &[repo.to_path_buf(), cargo_home()?]) {
        Some(path) => Err(format!(
            "{} still holds the build machine's Rust path {path}: a panic location would name it (xtask/src/remap.rs)",
            binary.display()
        )),
        None => Ok(()),
    }
}

/// The first Rust source path in `bytes` that starts with one of `roots` (as written, or with `/` for `\`).
fn leak(bytes: &[u8], roots: &[PathBuf]) -> Option<String> {
    for root in roots {
        let text = root.display().to_string();
        let mut needles = vec![text.clone()];
        let slashed = text.replace('\\', "/");
        if slashed != text {
            needles.push(slashed);
        }
        for needle in needles.iter().filter(|needle| !needle.is_empty()) {
            let needle = needle.as_bytes();
            let mut from = 0;
            while let Some(at) = find(bytes, needle, from) {
                let end = at + needle.len();
                let run = &bytes[end..bytes[end..].iter().position(|&b| !is_path_byte(b)).map_or(bytes.len(), |n| end + n)];
                if names_rust_source(run) {
                    let shown = &bytes[at..end + run.len()];
                    return Some(String::from_utf8_lossy(&shown[..shown.len().min(200)]).into_owned());
                }
                from = end;
            }
        }
    }
    None
}

/// The bytes a path is made of in a binary: no NUL (a C string ends there), no space, no quote.
fn is_path_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"_-./\\+@~".contains(&byte)
}

/// Whether a run of path bytes holds a `.rs` file name. Rust puts its string literals one after the other without a
/// separator, so a file name can be followed by more text, but not by a directory separator (`x.rs/` or `x.rs-1/` is a
/// directory).
fn names_rust_source(run: &[u8]) -> bool {
    run.windows(3)
        .enumerate()
        .any(|(at, window)| window == b".rs" && !matches!(run.get(at + 3), Some(b'/' | b'\\' | b'-')))
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let (first, rest) = needle.split_first()?;
    let mut start = from;
    while start + needle.len() <= haystack.len() {
        let at = start + haystack[start..=haystack.len() - needle.len()].iter().position(|byte| byte == first)?;
        if &haystack[at + 1..at + needle.len()] == rest {
            return Some(at);
        }
        start = at + 1;
    }
    None
}

fn cargo_home() -> Result<PathBuf, String> {
    if let Some(home) = env::var_os("CARGO_HOME") {
        return Ok(PathBuf::from(home));
    }
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .ok_or("neither CARGO_HOME nor the home directory is set")?;
    Ok(Path::new(&home).join(".cargo"))
}

fn toml_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> Vec<PathBuf> {
        vec![PathBuf::from("/build/repo"), PathBuf::from("/home/ci/.cargo")]
    }

    #[test]
    fn a_windows_path_is_quoted_for_toml() {
        assert_eq!(
            toml_string(r#"--remap-path-prefix=C:\a "b"="#),
            r#""--remap-path-prefix=C:\\a \"b\"=""#
        );
    }

    #[test]
    fn the_flags_are_the_same_whatever_is_on_disk() {
        let first = config_args(Path::new("/build/repo"), Path::new("/home/ci/.cargo"), Path::new("/tc"));
        assert_eq!(first, config_args(Path::new("/build/repo"), Path::new("/home/ci/.cargo"), Path::new("/tc")));
        assert_eq!(first[0], "--config");
        assert_eq!(
            first[1],
            "target.'cfg(all())'.rustflags=[\"--remap-path-prefix=/build/repo=\",\
             \"--remap-path-prefix=/home/ci/.cargo/registry/src=registry\",\
             \"--remap-path-prefix=/home/ci/.cargo/git/checkouts=git\",\
             \"--remap-path-prefix=/tc/lib/rustlib/src/rust=rust\"]"
        );
    }

    #[test]
    fn a_rust_panic_location_is_refused() {
        assert!(leak(b"..\x00/build/repo/rust/x.rs\x00", &roots()).is_some());
        assert!(leak(b"/home/ci/.cargo/registry/src/index.crates.io-6f17d22bba15001f/serde-1.0.0/src/lib.rs", &roots()).is_some());
        // Literals are stored one after the other, with no NUL between them.
        assert!(leak(b"/build/repo/src/main.rscalled `Option::unwrap()` on a `None` value", &roots()).is_some());
        assert!(leak(br"C:\build\repo\src\main.rs".as_slice(), &[PathBuf::from(r"C:\build\repo")]).is_some());
        assert!(leak(b"C:/build/repo/src/main.rs", &[PathBuf::from(r"C:\build\repo")]).is_some());
        // Not the first occurrence only.
        assert!(leak(b"/build/repo\x00/build/repo/a/b.rs", &roots()).is_some());
    }

    #[test]
    fn a_c_source_file_name_is_not() {
        // aws-lc-sys under MSVC (OPENSSL_PUT_ERROR), whisper.cpp (GGML_ASSERT), a build script's OUT_DIR.
        for text in [
            &b"/home/ci/.cargo/registry/src/index.crates.io-6f17d22bba15001f/aws-lc-sys-0.30.0/aws-lc/crypto/err/err.c\x00"[..],
            b"/build/repo/target/release/build/whisper-rs-sys-1/out/whisper.cpp/ggml/src/ggml.c\x00GGML_ASSERT\x00",
            b"/build/repo/target/release/build/x/out/src/llama.cpp\x00",
            b"/build/repo/vendor/lib.cc\x00/build/repo/vendor/lib.h\x00",
        ] {
            assert_eq!(leak(text, &roots()), None, "{}", String::from_utf8_lossy(text));
        }
        assert_eq!(
            leak(br"C:\build\repo\target\release\build\x\out\crypto\err.c".as_slice(), &[PathBuf::from(r"C:\build\repo")]),
            None
        );
    }

    #[test]
    fn a_directory_called_rs_is_not_a_source_file() {
        assert_eq!(leak(b"/build/repo/foo.rs/x.c\x00/build/repo/bar.rs-1/y.h\x00", &roots()), None);
    }

    #[test]
    fn a_binary_without_the_roots_passes() {
        assert_eq!(leak(b"rust/x.rs\x00registry/idx/serde/src/lib.rs", &roots()), None);
        assert_eq!(leak(b"anything", &[PathBuf::new()]), None);
    }
}
