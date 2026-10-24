//! A package as the release archive (and every npm package made from it) lays it out: `bin/sidevoice-connector`
//! (this build), `core/<the pinned core archive>` and the inventory `connector.json`.
//!
//! The core archive is the one `cargo xtask core` checked and kept in `target/sidevoice-core`. Without it the
//! tests that need it are skipped locally and fail in CI, like every test that needs the real core.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use sidevoice_connector::platform::service::{Service as _, Services};
use sidevoice_connector::platform::target::{Host, Target as _};

use super::{connector_program, BACKGROUND, CONNECTOR};

/// The checked core archive and its record (`core.json`), or `None` when the test must be skipped.
pub fn pinned_core() -> Option<(PathBuf, Value)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/sidevoice-core");
    let record: Option<Value> = fs::read(dir.join("core.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let found = record.and_then(|record| {
        let archive = dir.join(record["archive"].as_str()?);
        archive.is_file().then_some((archive, record))
    });
    if found.is_none() {
        assert!(
            std::env::var_os("CI").is_none(),
            "no core archive in {}: run `cargo xtask core` before `cargo test`",
            dir.display()
        );
        eprintln!(
            "skipped: no core archive in {} (run `cargo xtask core`)",
            dir.display()
        );
    }
    found
}

/// A package at `at/package` holding this build's connector and `archive` as its core, its inventory naming
/// `sha256` as the archive's digest.
pub fn package(at: &Path, archive: &Path, record: &Value, sha256: &str) -> PathBuf {
    let root = at.join("package");
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::create_dir_all(root.join("core")).unwrap();
    let binary = root.join(connector_program());
    // A link, not a copy: a program just written can be "busy" while another test's fork holds it.
    if fs::hard_link(CONNECTOR, &binary).is_err() {
        fs::copy(CONNECTOR, &binary).unwrap();
    }
    // Where the OS runs its service jobs through a windowless launcher, the package carries it beside the connector.
    if let Some(stem) = Services::BACKGROUND {
        let launcher = root.join("bin").join(Host::exe_name(stem));
        if fs::hard_link(BACKGROUND, &launcher).is_err() {
            fs::copy(BACKGROUND, &launcher).unwrap();
        }
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
