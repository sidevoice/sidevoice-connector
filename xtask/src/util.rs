//! Helpers shared by every command: files, JSON, processes, downloads, temporary directories.

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{Result, PACKAGE};

pub(crate) fn repo() -> PathBuf {
    // The tooling is the workspace's root package.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn read(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).map_err(|error| format!("{}: {error}", path.display()))
}

pub(crate) fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::write(path, bytes).map_err(|error| format!("{}: {error}", path.display()))
}

pub(crate) fn mkdir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|error| format!("{}: {error}", path.display()))
}

pub(crate) fn chmod(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| format!("{}: {error}", path.display()))
}

pub(crate) fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| format!("{}: not UTF-8", path.display()))
}

/// Compact JSON with sorted keys and a final newline: the one byte form of every JSON file we write.
pub(crate) fn canonical(value: &Value) -> Vec<u8> {
    // Keys are sorted here, not left to serde_json's map: another crate in the workspace may turn on its
    // `preserve_order` feature.
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                Value::Object(
                    keys.into_iter()
                        .map(|key| (key.clone(), sorted(&map[key])))
                        .collect(),
                )
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    let mut bytes = serde_json::to_vec(&sorted(value)).expect("JSON values serialize");
    bytes.push(b'\n');
    bytes
}

pub(crate) fn parse_json(bytes: &[u8], what: &str) -> Result<Value> {
    serde_json::from_slice(bytes).map_err(|error| format!("{what}: {error}"))
}

/// Runs a program to completion and returns its standard output; a failure is an error with its standard error.
pub(crate) fn output(program: &str, args: &[&str], dir: Option<&Path>) -> Result<String> {
    let mut command = Command::new(program);
    command.args(args);
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    let result = command
        .output()
        .map_err(|error| format!("{program}: {error}"))?;
    if !result.status.success() {
        return Err(format!(
            "{program} {}: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&result.stderr),
            String::from_utf8_lossy(&result.stdout)
        ));
    }
    String::from_utf8(result.stdout).map_err(|_| format!("{program}: output is not UTF-8"))
}

pub(crate) fn download(url: &str) -> Result<Vec<u8>> {
    let result = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--retry",
            "3",
            url,
        ])
        .output()
        .map_err(|error| format!("curl: {error}"))?;
    if !result.status.success() {
        return Err(format!(
            "download {url}: {}",
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    Ok(result.stdout)
}

/// This machine as a release target name.
pub(crate) fn host_target() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("linux-x86_64"),
        ("linux", "aarch64") => Ok("linux-aarch64"),
        ("macos", "aarch64") => Ok("macos-aarch64"),
        (os, arch) => Err(format!("unsupported build host {os}-{arch}")),
    }
}

pub(crate) fn cargo() -> String {
    env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

pub(crate) fn git(args: &[&str]) -> Result<String> {
    Ok(output("git", args, Some(&repo()))?.trim().to_string())
}

pub(crate) fn is_commit(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// `cargo metadata` of the workspace (`--no-deps` unless `deps`).
pub(crate) fn metadata(deps: bool, extra: &[&str]) -> Result<Value> {
    let mut args = vec!["metadata", "--locked", "--format-version", "1"];
    if !deps {
        args.push("--no-deps");
    }
    args.extend(extra);
    parse_json(
        output(&cargo(), &args, Some(&repo()))?.as_bytes(),
        "cargo metadata",
    )
}

/// The released crate's version, from its Cargo.toml.
pub(crate) fn connector_version() -> Result<String> {
    metadata(false, &[])?["packages"]
        .as_array()
        .and_then(|packages| packages.iter().find(|package| package["name"] == PACKAGE))
        .and_then(|package| package["version"].as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("no {PACKAGE} in the workspace"))
}

pub(crate) fn file_record(path: &Path, name: &str) -> Result<Value> {
    let bytes = read(path)?;
    Ok(json!({"name": name, "size": bytes.len(), "sha256": sha256(&bytes)}))
}

/// Every entry below `root`, as paths relative to it with whether each is a directory, sorted.
pub(crate) fn walk(root: &Path) -> Result<Vec<(String, bool)>> {
    fn visit(root: &Path, dir: &Path, found: &mut Vec<(String, bool)>) -> Result<()> {
        for entry in fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))? {
            let path = entry.map_err(|error| error.to_string())?.path();
            let relative = path
                .strip_prefix(root)
                .expect("below root")
                .to_string_lossy()
                .into_owned();
            let is_dir = path.is_dir();
            found.push((relative, is_dir));
            if is_dir {
                visit(root, &path, found)?;
            }
        }
        Ok(())
    }
    let mut found = Vec::new();
    visit(root, root, &mut found)?;
    found.sort();
    Ok(found)
}

/// Lines of a `SHA256SUMS` file as (digest, name).
pub(crate) fn parse_sums(bytes: &[u8]) -> Result<Vec<(String, String)>> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|line| {
            line.split_once("  ")
                .map(|(digest, name)| (digest.to_string(), name.to_string()))
                .ok_or_else(|| format!("malformed SHA256SUMS line: {line}"))
        })
        .collect()
}

pub(crate) struct TempDir(pub(crate) PathBuf);

impl TempDir {
    pub(crate) fn new(label: &str) -> Result<Self> {
        Self::new_in(&env::temp_dir(), label)
    }

    pub(crate) fn new_in(base: &Path, label: &str) -> Result<Self> {
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
        let path = base.join(format!("{label} {}-{serial}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        mkdir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_json_sorts_keys_and_ends_in_a_newline() {
        let value = json!({"b": 1, "a": {"d": 2, "c": 3}});
        assert_eq!(canonical(&value), b"{\"a\":{\"c\":3,\"d\":2},\"b\":1}\n");
    }

    #[test]
    fn sums_are_two_space_separated() {
        let sums = parse_sums(b"abc  one.tar.zst\ndef  two.json\n").unwrap();
        assert_eq!(
            sums,
            vec![
                ("abc".into(), "one.tar.zst".into()),
                ("def".into(), "two.json".into())
            ]
        );
        assert!(parse_sums(b"abc one\n").is_err());
    }

    #[test]
    fn commits_are_forty_lowercase_hex_digits() {
        assert!(is_commit("0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_commit("0123456789ABCDEF0123456789abcdef01234567"));
        assert!(!is_commit("development"));
    }
}
