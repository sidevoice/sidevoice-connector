//! `cargo xtask dist`: build the release binary for this host and package it as the release archive.

use std::process::Command;

use serde_json::json;

use crate::archive::write_archive;
use crate::notices::stage_notices;
use crate::util::*;
use crate::verify::verify;
use crate::{Result, ENTRYPOINT, INVENTORY, KIND, PACKAGE, ROOT_NAME};

/// Where `dist` leaves the archive.
pub(crate) const DIST_DIR: &str = "target/dist";

/// The Rust target triple this host builds for.
fn host_triple() -> Result<String> {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    output(&rustc, &["-vV"], None)?
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_string)
        .ok_or_else(|| "rustc -vV names no host".into())
}

pub(crate) fn dist() -> Result<()> {
    let repo = repo();
    let target = host_target()?;
    let source_sha = git(&["rev-parse", "HEAD"])?;
    let epoch: u64 = git(&["show", "-s", "--format=%ct", "HEAD"])?
        .parse()
        .map_err(|_| "commit time")?;
    let version = connector_version()?;
    // The build identity the binary reports (packages/connector-rust/build.rs).
    let status = Command::new(cargo())
        .args(["build", "--locked", "--release", "--package", PACKAGE])
        .env("SIDEVOICE_CONNECTOR_BUILD_SHA", &source_sha)
        .env("SIDEVOICE_CONNECTOR_TARGET", target)
        .env("SIDEVOICE_CONNECTOR_VERSION", &version)
        .current_dir(&repo)
        .status()
        .map_err(|error| format!("cargo build: {error}"))?;
    if !status.success() {
        return Err("cargo build failed".into());
    }

    let work = TempDir::new("sidevoice-connector-dist")?;
    let stage = work.0.join(ROOT_NAME);
    mkdir(&stage.join("bin"))?;
    mkdir(&stage.join("notices"))?;
    let binary = stage.join(ENTRYPOINT);
    write(&binary, &read(&repo.join("target/release").join(PACKAGE))?)?;
    chmod(&binary, 0o755)?;
    write(&stage.join("LICENSE"), &read(&repo.join("LICENSE"))?)?;
    stage_notices(&stage.join("notices"), &host_triple()?)?;

    let mut files = Vec::new();
    for (name, is_dir) in walk(&stage)? {
        if !is_dir {
            files.push(file_record(&stage.join(&name), &name)?);
        }
    }
    let count = files.len();
    let inventory = json!({"schema": 1, "kind": KIND, "version": version, "target": target,
                           "source_sha": source_sha, "entrypoint": ENTRYPOINT, "files": files});
    write(&stage.join(INVENTORY), &canonical(&inventory))?;

    let archive = repo
        .join(DIST_DIR)
        .join(format!("{ROOT_NAME}-{target}.tar.zst"));
    write_archive(&work.0, ROOT_NAME, &[ENTRYPOINT], epoch, &archive)?;
    let bytes = read(&archive)?;
    println!(
        "{}",
        json!({"target": target, "version": version, "name": archive.file_name().map(|name| name.to_string_lossy()),
               "size": bytes.len(), "sha256": sha256(&bytes), "files": count})
    );
    verify(&archive)
}
