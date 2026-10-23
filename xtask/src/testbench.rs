//! `cargo xtask testbench [ARGS...]` (also `bench`): the test bench (`testbench/`) on this checkout: the pinned core
//! fetched when `target/sidevoice-core` does not have it, this build of the connector and the test bench, then the
//! test bench run with ARGS (`--help` lists them).

use std::path::PathBuf;
use std::process::Command;

use crate::util::*;
use crate::{Result, PACKAGE};

const TESTBENCH_PACKAGE: &str = "sidevoice-testbench";

pub(crate) fn testbench(args: &[&str]) -> Result<()> {
    let core = repo().join(crate::core::DEFAULT_DIR);
    if !core.join("core.json").is_file() {
        crate::core::core(&core)?;
    }
    // Both packages: the test bench installs the connector built beside it.
    let built = Command::new(cargo())
        .args([
            "build",
            "--locked",
            "--package",
            PACKAGE,
            "--package",
            TESTBENCH_PACKAGE,
        ])
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
    let status = Command::new(target.join(format!("debug/{TESTBENCH_PACKAGE}")))
        .arg("--core")
        .arg(&core)
        .args(args)
        .status()
        .map_err(|error| format!("{TESTBENCH_PACKAGE}: {error}"))?;
    if !status.success() {
        return Err(format!("{TESTBENCH_PACKAGE} exited with {status}"));
    }
    Ok(())
}
