//! `cargo xtask core [DIR]`: the real core the connector's tests run against — the sidevoice-core release pinned in
//! `core.pin`, for this host, checked before it is unpacked.

use std::fs;
use std::path::Path;

use serde_json::json;

use crate::archive::unpack;
use crate::publish::verify_attestation;
use crate::util::*;
use crate::Result;

pub(crate) const DEFAULT_DIR: &str = "target/sidevoice-core";
/// The pin: one line, the core's version (`X.Y.Z`), nothing else, so a dependency bot can move it.
pub(crate) const PIN: &str = "core.pin";
const REPOSITORY: &str = "sidevoice/sidevoice-core";
/// The archive's root directory and its entrypoint below it.
const ROOT: &str = "sidevoice-core-rust";
const ENTRYPOINT: &str = "bin/sidevoice-core-rust";
const MANIFEST: &str = "native-core-manifest.json";
const ATTESTATION: &str = "attestation.sigstore.json";

/// The core's signer: its release workflow on main, for its nightlies and releases alike.
fn signer() -> String {
    format!("https://github.com/{REPOSITORY}/.github/workflows/release.yml@refs/heads/main")
}

/// The pinned version, from the pin file's text: one `X.Y.Z` (an optional pre-release suffix), no leading `v`.
pub(crate) fn parse_pin(text: &str) -> Result<String> {
    let version = text.trim();
    let (core, suffix) = version.split_once('-').unwrap_or((version, ""));
    let numeric = core.split('.').count() == 3
        && core
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    let suffix_ok = suffix
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.');
    if !numeric || !suffix_ok || version.lines().count() != 1 {
        return Err(format!(
            "{PIN} must hold one core version like 0.2.0, not {text:?}"
        ));
    }
    Ok(version.to_string())
}

fn asset_url(version: &str, name: &str) -> String {
    format!("https://github.com/{REPOSITORY}/releases/download/v{version}/{name}")
}

pub(crate) fn archive_name(version: &str, target: &str) -> String {
    format!("sidevoice-core-{version}-{target}.tar.zst")
}

/// The pinned core release's archive for a target, downloaded and checked: listed in the release's `SHA256SUMS`
/// and manifest with the same digest, and attested together with the manifest by the core's release workflow on
/// main. The archive is `<download>/<name>` and lives as long as this value.
pub(crate) struct PinnedCore {
    pub(crate) version: String,
    pub(crate) target: String,
    /// The archive's published name, `sidevoice-core-<version>-<target>.tar.zst`.
    pub(crate) name: String,
    pub(crate) sha256: String,
    pub(crate) size: u64,
    pub(crate) source_sha: String,
    download: TempDir,
}

impl PinnedCore {
    pub(crate) fn archive(&self) -> std::path::PathBuf {
        self.download.0.join(&self.name)
    }
}

/// The version `core.pin` names.
pub(crate) fn pinned_version() -> Result<String> {
    parse_pin(&String::from_utf8_lossy(&read(&repo().join(PIN))?))
}

/// Downloads the pinned core release's archive for `target` with `SHA256SUMS`, the manifest and the attestation,
/// and checks the archive is listed in both with the same digest and that the attestation binds the archive and
/// the manifest to the core's release workflow on main.
pub(crate) fn fetch(target: &str) -> Result<PinnedCore> {
    let version = pinned_version()?;
    let name = archive_name(&version, target);
    let downloaded = TempDir::new("sidevoice-core-download")?;
    for asset in [name.as_str(), "SHA256SUMS", MANIFEST, ATTESTATION] {
        write(
            &downloaded.0.join(asset),
            &download(&asset_url(&version, asset))?,
        )?;
    }
    let archive = downloaded.0.join(&name);
    let manifest_path = downloaded.0.join(MANIFEST);
    let sums = parse_sums(&read(&downloaded.0.join("SHA256SUMS"))?)?;
    let listed = |file: &str| {
        sums.iter()
            .find(|(_, listed)| listed == file)
            .map(|(digest, _)| digest.clone())
            .ok_or_else(|| format!("{file} is not in the core's SHA256SUMS"))
    };
    let bytes = read(&archive)?;
    let digest = sha256(&bytes);
    let manifest_bytes = read(&manifest_path)?;
    if listed(MANIFEST)? != sha256(&manifest_bytes) {
        return Err(format!("{MANIFEST} is not the one SHA256SUMS lists"));
    }
    let manifest = parse_json(&manifest_bytes, MANIFEST)?;
    let bundle = &manifest["bundles"][target];
    if listed(&name)? != digest
        || bundle["sha256"] != digest.as_str()
        || bundle["name"] != name.as_str()
    {
        return Err(format!(
            "{name}: its digest {digest} is not the one SHA256SUMS and the manifest list"
        ));
    }
    let attestation = downloaded.0.join(ATTESTATION);
    for file in [&archive, &manifest_path] {
        verify_attestation(file, REPOSITORY, &attestation, &signer())?;
    }
    let source_sha = manifest["source_sha"].as_str().unwrap_or("").to_string();
    if !is_commit(&source_sha) {
        return Err(format!("{MANIFEST} names no source commit"));
    }
    Ok(PinnedCore {
        version,
        target: target.to_string(),
        name,
        sha256: digest,
        size: bytes.len() as u64,
        source_sha,
        download: downloaded,
    })
}

/// Fetches the pinned core for this host ([`fetch`]) and unpacks it as `DIR/sidevoice-core-rust`, keeps the checked
/// archive as `DIR/<name>`, and records what it is in `DIR/core.json`.
pub(crate) fn core(dir: &Path) -> Result<()> {
    let pinned = fetch(host_target()?)?;
    if dir.exists() {
        fs::remove_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    }
    mkdir(dir)?;
    let root = unpack(&pinned.archive(), dir, ROOT)?;
    if !root.join(ENTRYPOINT).is_file() {
        return Err(format!("{} has no {ENTRYPOINT}", pinned.name));
    }
    write(&dir.join(&pinned.name), &read(&pinned.archive())?)?;
    let record = json!({"version": pinned.version, "target": pinned.target, "archive": pinned.name,
                        "sha256": pinned.sha256, "size": pinned.size, "source_sha": pinned.source_sha,
                        "entrypoint": root.join(ENTRYPOINT)});
    write(&dir.join("core.json"), &canonical(&record))?;
    print!("{}", String::from_utf8_lossy(&canonical(&record)));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_core_comes_from_its_pinned_release() {
        assert_eq!(
            asset_url("0.2.0", &archive_name("0.2.0", "linux-x86_64")),
            "https://github.com/sidevoice/sidevoice-core/releases/download/v0.2.0/sidevoice-core-0.2.0-linux-x86_64.tar.zst"
        );
        assert_eq!(
            signer(),
            "https://github.com/sidevoice/sidevoice-core/.github/workflows/release.yml@refs/heads/main"
        );
    }

    #[test]
    fn the_pin_is_one_version() {
        assert_eq!(parse_pin("0.2.0\n").unwrap(), "0.2.0");
        assert_eq!(parse_pin("0.3.0-rc.1").unwrap(), "0.3.0-rc.1");
        assert!(parse_pin("v0.2.0").is_err());
        assert!(parse_pin("nightly").is_err());
        assert!(parse_pin("0.2").is_err());
        assert!(parse_pin("0.2.0\n0.3.0\n").is_err());
    }

    #[test]
    fn the_repository_pin_is_valid() {
        let text = std::fs::read_to_string(repo().join(PIN)).unwrap();
        parse_pin(&text).unwrap();
    }
}
