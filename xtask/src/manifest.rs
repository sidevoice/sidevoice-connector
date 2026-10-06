//! `cargo xtask manifest DIR [--tag vX.Y.Z]`: the published names, `sidevoice-connector-manifest.json` and
//! `SHA256SUMS` for every target's archive.

use std::env;
use std::fs;
use std::path::Path;

use serde_json::json;

use crate::archive::unpack_checked;
use crate::util::*;
use crate::{Result, ENTRYPOINT, KIND, MANIFEST, ROOT_NAME, TARGETS};

/// The name an archive is published under: the version for a release, `nightly` for the nightly (fixed names, so
/// its download URLs never change). The commit is in the manifest and in every archive's inventory.
pub(crate) fn published_name(label: &str, target: &str) -> String {
    format!("{ROOT_NAME}-{label}-{target}.tar.zst")
}

pub(crate) fn manifest(dir: &Path, tag: Option<&str>) -> Result<()> {
    let source_sha = git(&["rev-parse", "HEAD"])?;
    if let Ok(expected) = env::var("GITHUB_SHA") {
        // The attestation names GITHUB_SHA as its source: the archives must come from that very commit.
        if expected != source_sha {
            return Err(format!(
                "checked out {source_sha}, but this run is for {expected}"
            ));
        }
    }
    let lock = read(&repo().join("Cargo.lock"))?;
    let bytes = write_manifest(dir, tag, &source_sha, &connector_version()?, &lock)?;
    print!("{}", String::from_utf8_lossy(&bytes));
    Ok(())
}

/// Renames every target's archive in `dir` to its published name, checks each is that target's build of
/// `source_sha` at `version`, and writes the manifest and `SHA256SUMS`. Returns the manifest's bytes.
pub(crate) fn write_manifest(
    dir: &Path,
    tag: Option<&str>,
    source_sha: &str,
    version: &str,
    lock: &[u8],
) -> Result<Vec<u8>> {
    if let Some(tag) = tag {
        if format!("v{version}") != tag {
            return Err(format!("Cargo.toml says {version}, the release is {tag}"));
        }
    }
    let label = tag
        .map(|tag| tag.trim_start_matches('v'))
        .unwrap_or("nightly");
    let mut bundles = serde_json::Map::new();
    let mut sums = Vec::new();
    for target in TARGETS {
        let built = dir.join(format!("{ROOT_NAME}-{target}.tar.zst"));
        let name = published_name(label, target);
        if built.exists() {
            fs::rename(&built, dir.join(&name)).map_err(|error| format!("{name}: {error}"))?;
        }
        let work = TempDir::new(&format!("sidevoice-connector-manifest-check-{target}"))?;
        let (_, inventory) = unpack_checked(&dir.join(&name), &work.0)?;
        if inventory["target"] != target
            || inventory["source_sha"] != source_sha
            || inventory["version"] != version
        {
            return Err(format!("wrong archive identity: {name}"));
        }
        let record = file_record(&dir.join(&name), &name)?;
        sums.push(format!(
            "{}  {name}\n",
            record["sha256"].as_str().unwrap_or("")
        ));
        bundles.insert(target.into(), record);
    }
    let value = json!({"schema": 1, "kind": KIND, "version": version, "source_sha": source_sha,
                       "cargo_lock_sha256": sha256(lock), "entrypoint": ENTRYPOINT, "bundles": bundles});
    let bytes = canonical(&value);
    write(&dir.join(MANIFEST), &bytes)?;
    sums.push(format!("{}  {MANIFEST}\n", sha256(&bytes)));
    sums.sort_by(|left, right| left[66..].cmp(&right[66..]));
    write(&dir.join("SHA256SUMS"), sums.concat().as_bytes())?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::write_archive;
    use crate::INVENTORY;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    /// A minimal archive of `target` built from `sha` at `version`, as `dist` names it, in `dir`.
    fn built(dir: &Path, target: &str, sha: &str, version: &str) {
        let work = TempDir::new(&format!("xtask-manifest-stage-{target}-{version}")).unwrap();
        let root = work.0.join(ROOT_NAME);
        mkdir(&root.join("bin")).unwrap();
        write(&root.join(ENTRYPOINT), target.as_bytes()).unwrap();
        let files = vec![file_record(&root.join(ENTRYPOINT), ENTRYPOINT).unwrap()];
        let mut inventory = json!({"schema": 1, "kind": KIND, "version": version, "target": target,
                                   "source_sha": sha, "entrypoint": ENTRYPOINT, "files": files});
        if target.starts_with("linux-") {
            inventory["glibc"] = "2.28".into();
        }
        write(&root.join(INVENTORY), &canonical(&inventory)).unwrap();
        let archive = dir.join(format!("{ROOT_NAME}-{target}.tar.zst"));
        write_archive(&work.0, ROOT_NAME, &[ENTRYPOINT], 1, &archive).unwrap();
    }

    #[test]
    fn nightly_names_are_fixed_and_release_names_carry_the_version() {
        assert_eq!(
            published_name("nightly", "linux-x86_64"),
            "sidevoice-connector-nightly-linux-x86_64.tar.zst"
        );
        assert_eq!(
            published_name("0.7.0", "macos-aarch64"),
            "sidevoice-connector-0.7.0-macos-aarch64.tar.zst"
        );
    }

    #[test]
    fn a_release_lists_every_target_under_its_version() {
        let dir = TempDir::new("xtask-manifest-release").unwrap();
        for target in TARGETS {
            built(&dir.0, target, SHA, "0.7.0");
        }
        let bytes = write_manifest(&dir.0, Some("v0.7.0"), SHA, "0.7.0", b"lock").unwrap();
        let manifest = parse_json(&bytes, "manifest").unwrap();
        assert_eq!(manifest["version"], "0.7.0");
        for target in TARGETS {
            let name = published_name("0.7.0", target);
            assert_eq!(manifest["bundles"][target]["name"], name.as_str());
            assert!(dir.0.join(&name).is_file());
        }
        let sums = parse_sums(&read(&dir.0.join("SHA256SUMS")).unwrap()).unwrap();
        let names: Vec<&str> = sums.iter().map(|(_, name)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "sidevoice-connector-0.7.0-linux-aarch64.tar.zst",
                "sidevoice-connector-0.7.0-linux-x86_64.tar.zst",
                "sidevoice-connector-0.7.0-macos-aarch64.tar.zst",
                MANIFEST,
            ]
        );
        for (digest, name) in sums {
            assert_eq!(sha256(&read(&dir.0.join(name)).unwrap()), digest);
        }
    }

    #[test]
    fn a_tag_other_than_the_crate_version_is_refused() {
        let dir = TempDir::new("xtask-manifest-tag").unwrap();
        let error = write_manifest(&dir.0, Some("v0.8.0"), SHA, "0.7.0", b"lock").unwrap_err();
        assert!(error.contains("0.8.0"), "{error}");
    }

    #[test]
    fn an_archive_of_another_commit_is_refused() {
        let dir = TempDir::new("xtask-manifest-commit").unwrap();
        for target in TARGETS {
            built(&dir.0, target, SHA, "0.7.0");
        }
        let other = "1111111111111111111111111111111111111111";
        let error = write_manifest(&dir.0, None, other, "0.7.0", b"lock").unwrap_err();
        assert!(error.contains("wrong archive identity"), "{error}");
    }
}
