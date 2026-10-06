//! `cargo xtask core [DIR]`: the real core the connector's tests run against — sidevoice-core's published
//! `nightly` for this host, checked before it is unpacked.

use std::fs;
use std::path::Path;

use serde_json::json;

use crate::archive::unpack;
use crate::publish::verify_attestation;
use crate::util::*;
use crate::Result;

pub(crate) const DEFAULT_DIR: &str = "target/sidevoice-core";
const REPOSITORY: &str = "sidevoice/sidevoice-core";
const RELEASE: &str = "nightly";
/// The archive's root directory and its entrypoint below it.
const ROOT: &str = "sidevoice-core-rust";
const ENTRYPOINT: &str = "bin/sidevoice-core-rust";
const MANIFEST: &str = "native-core-manifest.json";
const ATTESTATION: &str = "attestation.sigstore.json";

/// The core's signer: its release workflow on main, for its nightlies and releases alike.
fn signer() -> String {
    format!("https://github.com/{REPOSITORY}/.github/workflows/release.yml@refs/heads/main")
}

fn asset_url(name: &str) -> String {
    format!("https://github.com/{REPOSITORY}/releases/download/{RELEASE}/{name}")
}

pub(crate) fn archive_name(target: &str) -> String {
    format!("sidevoice-core-{RELEASE}-{target}.tar.zst")
}

/// Downloads the core's nightly archive for this host with `SHA256SUMS`, the manifest and the attestation; checks
/// the archive is listed in both with the same digest and that the attestation binds it to the core's release
/// workflow on main; then unpacks it as `DIR/sidevoice-core-rust` and records what it is in `DIR/core.json`.
pub(crate) fn core(dir: &Path) -> Result<()> {
    let target = host_target()?;
    let name = archive_name(target);
    let download_dir = TempDir::new("sidevoice-core-download")?;
    for asset in [name.as_str(), "SHA256SUMS", MANIFEST, ATTESTATION] {
        write(&download_dir.0.join(asset), &download(&asset_url(asset))?)?;
    }
    let archive = download_dir.0.join(&name);
    let digest = sha256(&read(&archive)?);
    let listed = parse_sums(&read(&download_dir.0.join("SHA256SUMS"))?)?
        .into_iter()
        .find(|(_, listed)| *listed == name)
        .map(|(digest, _)| digest)
        .ok_or_else(|| format!("{name} is not in the core's SHA256SUMS"))?;
    let manifest = parse_json(&read(&download_dir.0.join(MANIFEST))?, MANIFEST)?;
    let bundle = &manifest["bundles"][target];
    if listed != digest || bundle["sha256"] != digest.as_str() || bundle["name"] != name.as_str() {
        return Err(format!(
            "{name}: its digest {digest} is not the one SHA256SUMS and the manifest list"
        ));
    }
    verify_attestation(
        &archive,
        REPOSITORY,
        &download_dir.0.join(ATTESTATION),
        &signer(),
    )?;

    if dir.exists() {
        fs::remove_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    }
    mkdir(dir)?;
    let root = unpack(&archive, dir, ROOT)?;
    if !root.join(ENTRYPOINT).is_file() {
        return Err(format!("{name} has no {ENTRYPOINT}"));
    }
    let record = json!({"release": RELEASE, "target": target, "archive": name, "sha256": digest,
                        "source_sha": manifest["source_sha"], "entrypoint": root.join(ENTRYPOINT)});
    write(&dir.join("core.json"), &canonical(&record))?;
    print!("{}", String::from_utf8_lossy(&canonical(&record)));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_core_comes_from_its_nightly_release() {
        assert_eq!(
            asset_url(&archive_name("linux-x86_64")),
            "https://github.com/sidevoice/sidevoice-core/releases/download/nightly/sidevoice-core-nightly-linux-x86_64.tar.zst"
        );
        assert_eq!(
            signer(),
            "https://github.com/sidevoice/sidevoice-core/.github/workflows/release.yml@refs/heads/main"
        );
    }
}
