//! `cargo xtask core [DIR]`: the real core the connector's tests run against — the sidevoice-core release pinned in
//! `sidevoice-platform` tag in `Cargo.toml`, for this host, checked before it is unpacked.

use std::fs;
use std::path::Path;

use serde_json::json;

use crate::archive::unpack;
use crate::publish::verify_attestation;
use crate::util::*;
use crate::Result;

pub(crate) const DEFAULT_DIR: &str = "target/sidevoice-core";
/// The manifest whose `sidevoice-platform` dependency names the core release: the one place the core version lives.
const MANIFEST_PATH: &str = "Cargo.toml";
const PLATFORM_CRATE: &str = "sidevoice-platform";
const REPOSITORY: &str = "sidevoice/sidevoice-core";
/// The archive's root directory and its entrypoint below it.
const ROOT: &str = "sidevoice-core-rust";
const ENTRYPOINT_STEM: &str = "bin/sidevoice-core-rust";

/// The core's program inside its archive for `target`: `bin/sidevoice-core-rust`, `.exe` on Windows.
fn entrypoint(target: &str) -> String {
    sidevoice_platform::target::by_name(target)
        .expect("validated release target")
        .exe_name(ENTRYPOINT_STEM)
}
const MANIFEST: &str = "native-core-manifest.json";
const ATTESTATION: &str = "attestation.sigstore.json";

/// The core's signer: its release workflow on main, for its nightlies and releases alike.
fn signer() -> String {
    format!("https://github.com/{REPOSITORY}/.github/workflows/release.yml@refs/heads/main")
}

/// A core version: `X.Y.Z` (an optional pre-release suffix), no leading `v`.
pub(crate) fn parse_version(text: &str) -> Result<String> {
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
        return Err(format!("not a core version like 0.2.0: {text:?}"));
    }
    Ok(version.to_string())
}

/// The core version the workspace manifest names: the `tag` (`vX.Y.Z`) of its `sidevoice-platform` git dependency on
/// the core's repository. Anything else (a path, a registry version, a branch, a rev, another tag shape) is an error.
pub(crate) fn core_version_from_manifest(text: &str) -> Result<String> {
    let manifest: toml::Table = text
        .parse()
        .map_err(|error| format!("{MANIFEST_PATH} is not valid TOML: {error}"))?;
    let fail = |why: &str| {
        format!(
            "{MANIFEST_PATH}: [workspace.dependencies] {PLATFORM_CRATE} {why}; it must be \
             {{ git = \"https://github.com/{REPOSITORY}\", tag = \"vX.Y.Z\" }}, the core release the connector bundles"
        )
    };
    let table = manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(|dependencies| dependencies.get(PLATFORM_CRATE))
        .ok_or_else(|| fail("is missing"))?
        .as_table()
        .ok_or_else(|| fail("is not a table"))?;
    let git = table.get("git").and_then(|git| git.as_str());
    if !git.is_some_and(|git| {
        git.trim_end_matches('/').trim_end_matches(".git")
            == format!("https://github.com/{REPOSITORY}")
    }) {
        return Err(fail("is not a git dependency on the core's repository"));
    }
    if ["rev", "branch", "path", "version"]
        .iter()
        .any(|key| table.contains_key(*key))
    {
        return Err(fail("pins by something other than the tag"));
    }
    let tag = table
        .get("tag")
        .and_then(|tag| tag.as_str())
        .ok_or_else(|| fail("has no tag"))?;
    let not_a_tag = || fail(&format!("has tag {tag:?}, not vX.Y.Z"));
    let version = tag.strip_prefix('v').ok_or_else(not_a_tag)?;
    parse_version(version).map_err(|_| not_a_tag())
}

/// The version the connector's `sidevoice-platform` tag names.
pub(crate) fn pinned_version() -> Result<String> {
    core_version_from_manifest(&String::from_utf8_lossy(&read(
        &repo().join(MANIFEST_PATH),
    )?))
}

fn asset_url(version: &str, name: &str) -> String {
    format!("https://github.com/{REPOSITORY}/releases/download/v{version}/{name}")
}

pub(crate) fn archive_name(version: &str, target: &str) -> String {
    sidevoice_platform::target::by_name(target)
        .expect("validated release target")
        .archive_name("sidevoice-core", Some(version))
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

/// Downloads the pinned core release's archive for `target` with `SHA256SUMS`, the manifest and the attestation,
/// and checks the archive is listed in both with the same digest and that the attestation binds the archive and
/// the manifest to the core's release workflow on main.
pub(crate) fn fetch(target: &str) -> Result<PinnedCore> {
    let version = pinned_version()?;
    let name = archive_name(&version, target);
    let downloaded = TempDir::new("sidevoice-core-download")?;
    // The checksums first: a core release made before a target existed does not list its archive, and that is
    // said as such, not as a failed download.
    let sums_path = downloaded.0.join("SHA256SUMS");
    write(&sums_path, &download(&asset_url(&version, "SHA256SUMS"))?)?;
    if !parse_sums(&read(&sums_path)?)?
        .iter()
        .any(|(_, listed)| *listed == name)
    {
        return Err(format!(
            "sidevoice-core {version} publishes no {target} archive ({name} is not in its SHA256SUMS): \
             the `{PLATFORM_CRATE}` tag in {MANIFEST_PATH} must name a core release built for {target}"
        ));
    }
    for asset in [name.as_str(), MANIFEST, ATTESTATION] {
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
    let entrypoint = entrypoint(&pinned.target);
    if !root.join(&entrypoint).is_file() {
        return Err(format!("{} has no {entrypoint}", pinned.name));
    }
    write(&dir.join(&pinned.name), &read(&pinned.archive())?)?;
    let record = json!({"version": pinned.version, "target": pinned.target, "archive": pinned.name,
                        "sha256": pinned.sha256, "size": pinned.size, "source_sha": pinned.source_sha,
                        "entrypoint": root.join(&entrypoint)});
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
    fn the_core_program_has_the_suffix_of_its_target() {
        assert_eq!(entrypoint("linux-x86_64"), "bin/sidevoice-core-rust");
        assert_eq!(entrypoint("macos-aarch64"), "bin/sidevoice-core-rust");
        assert_eq!(entrypoint("windows-x86_64"), "bin/sidevoice-core-rust.exe");
    }

    fn manifest(dependency: &str) -> String {
        format!("[package]\nname = \"connector\"\n\n[workspace.dependencies]\nserde = \"1\"\n{dependency}\n")
    }

    #[test]
    fn a_version_is_x_y_z() {
        assert_eq!(parse_version("0.2.0\n").unwrap(), "0.2.0");
        assert_eq!(parse_version("0.3.0-rc.1").unwrap(), "0.3.0-rc.1");
        assert!(parse_version("v0.2.0").is_err());
        assert!(parse_version("nightly").is_err());
        assert!(parse_version("0.2").is_err());
        assert!(parse_version("0.2.0\n0.3.0\n").is_err());
    }

    #[test]
    fn the_core_version_is_the_platform_tag() {
        let git = "https://github.com/sidevoice/sidevoice-core";
        let version = |dependency: &str| core_version_from_manifest(&manifest(dependency));
        let platform =
            |fields: &str| format!("sidevoice-platform = {{ git = \"{git}\", {fields} }}");
        assert_eq!(version(&platform("tag = \"v0.9.0\"")).unwrap(), "0.9.0");
        assert_eq!(
            version(&format!(
                "sidevoice-platform = {{ git = \"{git}.git\", tag = \"v0.10.1-rc.1\" }}"
            ))
            .unwrap(),
            "0.10.1-rc.1"
        );
        for bad in [
            platform("tag = \"0.9.0\""),
            platform("tag = \"nightly\""),
            platform("branch = \"main\""),
            platform("rev = \"abc123\""),
            platform("tag = \"v0.9.0\", rev = \"abc\""),
            "sidevoice-platform = { git = \"https://github.com/other/repo\", tag = \"v0.9.0\" }"
                .into(),
            "sidevoice-platform = { path = \"../core\" }".into(),
            "sidevoice-platform = \"0.9.0\"".into(),
            "sidevoice-platform = { workspace = true }".into(),
            String::new(),
        ] {
            let error = version(&bad).unwrap_err();
            assert!(error.contains("sidevoice-platform"), "{error}");
        }
    }

    #[test]
    fn the_repository_names_a_core_version() {
        pinned_version().unwrap();
    }
}
