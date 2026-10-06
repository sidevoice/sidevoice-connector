//! Build tooling for the connector, run as `cargo xtask <command>` (alias in `.cargo/config.toml`).
//!
//! - `dist`: build the release binary for this host (Linux: against glibc `glibc::FLOOR`, with cargo-zigbuild)
//!   and package it, with its licence notices and the pinned core release's archive for this host (checked as
//!   `core` checks it), as the archive `target/dist/sidevoice-connector-<target>.tar.zst`; then `verify` it.
//! - `verify ARCHIVE`: unpack it somewhere else, check its inventory (the core inside included), what the binary
//!   links and (Linux) that it needs no glibc newer than the floor the inventory records, and run it from there: it
//!   must report the version and build identity the inventory names, and stage the core it carries into a release,
//!   where the core passes its own self-test.
//! - `verify-floor ARCHIVE` (Linux, needs Docker): `verify`, running the binary in a container of the oldest
//!   distribution it supports, whose glibc is the floor (`glibc::FLOOR_IMAGE`). The core is not run there: the
//!   pinned core release has a floor of its own.
//! - `manifest DIR [--tag vX.Y.Z]`: check every target's archive in DIR, give each its published name and write
//!   `sidevoice-connector-manifest.json` and `SHA256SUMS`; with a tag, the crate version must be that release.
//! - `publish DIR TAG`: attach every file in DIR to the release TAG (for `nightly`, move the tag here first and drop
//!   older assets), download them back, check them against `SHA256SUMS` and the attestation, and publish.
//! - `core [DIR]`: download the sidevoice-core release pinned in `core.pin` for this host, check it against that
//!   release's `SHA256SUMS`, manifest and attestation, and unpack it into DIR (default `target/sidevoice-core`),
//!   with the checked archive beside it: where the connector's tests find the real core.
//! - `codex [DIR]`: install the pinned Codex CLI from npm into DIR (default `target/codex`), where the registration
//!   test finds it.
//! - `npm ARCHIVE...`: the npm packages of those archives (`@sidevoice/sidevoice-<os>-<cpu>` each) and the launcher
//!   `sidevoice`, packed into `target/npm` (xtask/src/npm.rs).
//! - `npm-smoke`: install the launcher and this machine's package from `target/npm` as a person does and run
//!   `npx sidevoice --version --json` from there.
//! - `npm-publish TAG`: the npm packages of the GitHub release TAG's checked assets, published by trusted publishing:
//!   the platform packages, then the launcher as a staged version a maintainer approves.
//! - `fixtures`: `core` and `codex`, into their default directories: what `cargo test` runs against.

mod archive;
mod codex;
mod core;
mod dist;
mod glibc;
mod manifest;
mod notices;
mod npm;
mod publish;
mod util;
mod verify;

use std::env;
use std::path::Path;

pub(crate) type Result<T> = std::result::Result<T, String>;

/// A release target: its name in archive names and inventories, the `std::env::consts::OS` and `ARCH` of the machine
/// that builds it (each target is built on its own runner), and npm's `os` and `cpu` for its package.
pub(crate) struct Target {
    pub(crate) name: &'static str,
    pub(crate) os: &'static str,
    pub(crate) arch: &'static str,
    pub(crate) npm_os: &'static str,
    pub(crate) npm_cpu: &'static str,
}

/// Every release target: the one list the tooling reads (archives, manifest, npm packages, the launcher's
/// dependencies). Adding a target is a line here and its runner in the `ci.yml` and `release.yml` matrices
/// (RELEASING.md, "npm: what is published and how to add a platform").
pub(crate) const TARGETS: &[Target] = &[
    Target {
        name: "linux-aarch64",
        os: "linux",
        arch: "aarch64",
        npm_os: "linux",
        npm_cpu: "arm64",
    },
    Target {
        name: "linux-x86_64",
        os: "linux",
        arch: "x86_64",
        npm_os: "linux",
        npm_cpu: "x64",
    },
    Target {
        name: "macos-aarch64",
        os: "macos",
        arch: "aarch64",
        npm_os: "darwin",
        npm_cpu: "arm64",
    },
];

/// The names of [`TARGETS`], in order.
pub(crate) fn target_names() -> impl Iterator<Item = &'static str> {
    TARGETS.iter().map(|target| target.name)
}
/// The crate whose binary is released.
pub(crate) const PACKAGE: &str = "sidevoice-connector";
/// The archive's single root directory.
pub(crate) const ROOT_NAME: &str = "sidevoice-connector";
pub(crate) const ENTRYPOINT: &str = "bin/sidevoice-connector";
/// Where the archive carries the pinned core release archive, as the core published it.
pub(crate) const CORE_DIR: &str = "core";
/// The inventory every archive carries at its root.
pub(crate) const INVENTORY: &str = "connector.json";
pub(crate) const MANIFEST: &str = "sidevoice-connector-manifest.json";
pub(crate) const KIND: &str = "sidevoice-connector-v1";

const USAGE: &str =
    "usage: cargo xtask dist | verify ARCHIVE | verify-floor ARCHIVE | manifest DIR [--tag vX.Y.Z] | publish DIR TAG | npm ARCHIVE... | npm-smoke | npm-publish TAG | fixtures | core [DIR] | codex [DIR]";

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["dist"] => dist::dist(),
        ["verify", archive] => verify::verify(Path::new(archive), None),
        ["verify-floor", archive] => verify::verify(Path::new(archive), Some(glibc::FLOOR_IMAGE)),
        ["manifest", dir] => manifest::manifest(Path::new(dir), None),
        ["manifest", dir, "--tag", tag] => manifest::manifest(Path::new(dir), Some(tag)),
        ["publish", dir, tag] => publish::publish(Path::new(dir), tag),
        ["core"] => core::core(&util::repo().join(core::DEFAULT_DIR)),
        ["core", dir] => core::core(Path::new(dir)),
        ["codex"] => codex::codex(&util::repo().join(codex::DEFAULT_DIR)),
        ["codex", dir] => codex::codex(Path::new(dir)),
        ["fixtures"] => core::core(&util::repo().join(core::DEFAULT_DIR))
            .and_then(|()| codex::codex(&util::repo().join(codex::DEFAULT_DIR))),
        ["npm", archives @ ..] if !archives.is_empty() => {
            let archives: Vec<&Path> = archives.iter().map(Path::new).collect();
            npm::npm_packages(&archives)
        }
        ["npm-smoke"] => npm::smoke(),
        ["npm-publish", tag] => npm::publish(tag),
        _ => Err(USAGE.into()),
    };
    if let Err(error) = result {
        eprintln!("xtask: {error}");
        std::process::exit(1);
    }
}
