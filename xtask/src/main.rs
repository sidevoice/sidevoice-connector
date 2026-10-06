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
//! - `fixtures`: `core` and `codex`, into their default directories: what `cargo test` runs against.

mod archive;
mod codex;
mod core;
mod dist;
mod glibc;
mod manifest;
mod notices;
mod publish;
mod util;
mod verify;

use std::env;
use std::path::Path;

pub(crate) type Result<T> = std::result::Result<T, String>;

pub(crate) const TARGETS: [&str; 3] = ["linux-aarch64", "linux-x86_64", "macos-aarch64"];
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
    "usage: cargo xtask dist | verify ARCHIVE | verify-floor ARCHIVE | manifest DIR [--tag vX.Y.Z] | publish DIR TAG | fixtures | core [DIR] | codex [DIR]";

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
        _ => Err(USAGE.into()),
    };
    if let Err(error) = result {
        eprintln!("xtask: {error}");
        std::process::exit(1);
    }
}
