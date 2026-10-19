//! `cargo xtask dist`: build the release binary for this host and package it as the release archive.
//!
//! On Linux the binary is linked against glibc [`glibc::FLOOR`] (`cargo zigbuild`, zig's glibc stubs), not against
//! the build machine's, so it runs on every distribution from that release on; the inventory records the floor and
//! `verify` checks the binary needs nothing newer.

use std::process::Command;

use serde_json::json;

use crate::archive::write_archive;
use crate::glibc;
use crate::notices::stage_notices;
use crate::util::*;
use crate::verify::verify;
use crate::{core, Result, CORE_DIR, ENTRYPOINT, INVENTORY, KIND, PACKAGE, ROOT_NAME};

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
    let triple = host_triple()?;
    let linux = target.starts_with("linux-");
    // On Linux, cargo-zigbuild's `<triple>.<glibc>` target links against that glibc release; the binary lands in
    // target/<triple>/release.
    let (subcommand, build_target, built) = if linux {
        let built = repo.join("target").join(&triple).join("release");
        (
            "zigbuild",
            Some(format!("{triple}.{}", glibc::FLOOR)),
            built,
        )
    } else {
        ("build", None, repo.join("target/release"))
    };
    let mut build = Command::new(cargo());
    // Only the connector: the package's other binary, the test bench, is not released.
    build.args([
        subcommand,
        "--locked",
        "--release",
        "--package",
        PACKAGE,
        "--bin",
        PACKAGE,
    ]);
    if let Some(build_target) = &build_target {
        build.args(["--target", build_target]);
    }
    // The build identity the binary reports (connector/build.rs).
    let status = build
        .env("SIDEVOICE_CONNECTOR_BUILD_SHA", &source_sha)
        .env("SIDEVOICE_CONNECTOR_TARGET", target)
        .env("SIDEVOICE_CONNECTOR_VERSION", &version)
        .current_dir(&repo)
        .status()
        .map_err(|error| format!("cargo {subcommand}: {error}"))?;
    if !status.success() {
        return Err(format!("cargo {subcommand} failed"));
    }

    let work = TempDir::new("sidevoice-connector-dist")?;
    let stage = work.0.join(ROOT_NAME);
    mkdir(&stage.join("bin"))?;
    mkdir(&stage.join("notices"))?;
    let binary = stage.join(ENTRYPOINT);
    write(&binary, &read(&built.join(PACKAGE))?)?;
    chmod(&binary, 0o755)?;
    write(&stage.join("LICENSE"), &read(&repo.join("LICENSE"))?)?;
    stage_notices(&stage.join("notices"), &triple)?;
    // The core travels inside: the pinned release's archive for this target, checked against its attestation.
    let pinned = core::fetch(target)?;
    let core_archive = format!("{CORE_DIR}/{}", pinned.name);
    mkdir(&stage.join(CORE_DIR))?;
    write(&stage.join(&core_archive), &read(&pinned.archive())?)?;

    let mut files = Vec::new();
    for (name, is_dir) in walk(&stage)? {
        if !is_dir {
            files.push(file_record(&stage.join(&name), &name)?);
        }
    }
    let count = files.len();
    let mut inventory = json!({"schema": 1, "kind": KIND, "version": version, "target": target,
                               "source_sha": source_sha, "entrypoint": ENTRYPOINT, "files": files,
                               "core": {"version": pinned.version, "archive": core_archive,
                                        "sha256": pinned.sha256, "size": pinned.size,
                                        "source_sha": pinned.source_sha}});
    if linux {
        inventory["glibc"] = glibc::FLOOR.into();
    }
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
    verify(&archive, None)
}
