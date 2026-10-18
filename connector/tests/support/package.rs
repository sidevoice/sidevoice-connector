//! A package as the release archive (and every npm package made from it) lays it out: `bin/sidevoice-connector`
//! (this build), `core/<the pinned core archive>` and the inventory `connector.json`.
//!
//! The core archive is the one `cargo xtask core` checked and kept in `target/sidevoice-core`, or in the private
//! directory named by `SIDEVOICE_TEST_PINNED_CORE_DIR`. Without it, package tests report an explicit local skip
//! and fail in CI, like every test that needs the real core.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use super::CONNECTOR;

/// The checked core archive and its record (`core.json`), or `None` when the default local fixture is absent.
pub fn pinned_core() -> Option<(PathBuf, Value)> {
    let explicit = std::env::var_os("SIDEVOICE_TEST_PINNED_CORE_DIR");
    let dir = explicit
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/sidevoice-core"));
    if !dir.exists() {
        assert!(
            explicit.is_none() && std::env::var_os("CI").is_none(),
            "no pinned Core fixture in {}: run `cargo xtask core` before `cargo test`",
            dir.display()
        );
        eprintln!(
            "SKIPPED_PINNED_CORE_PACKAGE: no archive in {}; passing package test names in this run are skips (run `cargo xtask core` or set SIDEVOICE_TEST_PINNED_CORE_DIR)",
            dir.display()
        );
        return None;
    }
    let record: Value = serde_json::from_slice(
        &fs::read(dir.join("core.json"))
            .unwrap_or_else(|error| panic!("{}: {error}", dir.join("core.json").display())),
    )
    .unwrap_or_else(|error| panic!("{}: {error}", dir.join("core.json").display()));
    let name = record["archive"]
        .as_str()
        .expect("pinned Core record has no archive name");
    let archive = dir.join(name);
    assert!(
        archive.is_file(),
        "pinned Core archive is missing: {}",
        archive.display()
    );
    Some((archive, record))
}

/// A package at `at/package` holding this build's connector and `archive` as its core, its inventory naming
/// `sha256` as the archive's digest.
pub fn package(at: &Path, archive: &Path, record: &Value, sha256: &str) -> PathBuf {
    let root = at.join("package");
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::create_dir_all(root.join("core")).unwrap();
    let binary = root.join("bin/sidevoice-connector");
    // A link, not a copy: a program just written can be "busy" while another test's fork holds it.
    if fs::hard_link(CONNECTOR, &binary).is_err() {
        fs::copy(CONNECTOR, &binary).unwrap();
    }
    let name = format!("core/{}", record["archive"].as_str().unwrap());
    fs::copy(archive, root.join(&name)).unwrap();
    let identity: Value = serde_json::from_slice(
        &Command::new(&binary)
            .args(["--version", "--json"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let inventory = json!({"version": identity["version"], "target": record["target"],
        "core": {"version": record["version"], "archive": name, "sha256": sha256,
                 "size": fs::metadata(archive).unwrap().len(), "source_sha": record["source_sha"]}});
    fs::write(root.join("connector.json"), inventory.to_string()).unwrap();
    root
}
