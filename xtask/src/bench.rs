//! `cargo xtask bench [ARGS...]`: the connector's test bench (`connector/src/bench/`) on this checkout: the pinned core
//! fetched when `target/sidevoice-core` does not have it, this build of the connector and the bench, then the bench
//! run with ARGS (`--help` lists them).

use std::path::PathBuf;
use std::process::Command;

use crate::util::*;
use crate::{Result, PACKAGE};

pub(crate) fn bench(args: &[&str]) -> Result<()> {
    let core = repo().join(crate::core::DEFAULT_DIR);
    if !core.join("core.json").is_file() {
        crate::core::core(&core)?;
    }
    // Both binaries: the bench installs the connector built beside it.
    let built = Command::new(cargo())
        .args(["build", "--locked", "--package", PACKAGE, "--bins"])
        .current_dir(repo())
        .status()
        .map_err(|error| format!("cargo build: {error}"))?;
    if !built.success() {
        return Err("cargo build failed".into());
    }
    let target = metadata(false, &[])?["target_directory"]
        .as_str()
        .map(PathBuf::from)
        .ok_or("cargo metadata names no target directory")?;
    let status = Command::new(target.join("debug/sidevoice-bench"))
        .arg("--core")
        .arg(&core)
        .args(args)
        .status()
        .map_err(|error| format!("sidevoice-bench: {error}"))?;
    if !status.success() {
        return Err(format!("sidevoice-bench exited with {status}"));
    }
    Ok(())
}
