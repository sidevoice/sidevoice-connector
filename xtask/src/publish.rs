//! `cargo xtask publish DIR TAG`: attach, read back, verify and publish a release.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::Path;

use crate::util::*;
use crate::Result;

/// The signer every asset must carry: the release workflow on main, for nightlies and releases alike.
pub(crate) fn signer(repository: &str) -> String {
    format!("https://github.com/{repository}/.github/workflows/release.yml@refs/heads/main")
}

pub(crate) fn gh(args: &[&str]) -> Result<String> {
    output("gh", args, None)
}

/// `gh attestation verify` of one file against a Sigstore bundle, signed by `identity` on a GitHub-hosted runner.
pub(crate) fn verify_attestation(
    file: &Path,
    repository: &str,
    bundle: &Path,
    identity: &str,
) -> Result<()> {
    gh(&[
        "attestation",
        "verify",
        path_str(file)?,
        "--repo",
        repository,
        "--bundle",
        path_str(bundle)?,
        "--cert-identity",
        identity,
        "--deny-self-hosted-runners",
    ])
    .map(|_| ())
    .map_err(|error| format!("{}: attestation does not verify: {error}", file.display()))
}

fn nightly_notes(sha: &str) -> String {
    format!(
        "Snapshot of `main` at {sha}. Not a version: the `nightly` tag moves to every commit on `main` whose \
         build passes, and these assets are replaced each time. Pin a `vX.Y.Z` release instead."
    )
}

pub(crate) fn publish(dir: &Path, tag: &str) -> Result<()> {
    let repository = env::var("GH_REPO").map_err(|_| "GH_REPO is not set")?;
    let mut assets: Vec<String> = Vec::new();
    for entry in fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))? {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.is_file() {
            assets.push(path_str(&path)?.to_string());
        }
    }
    assets.sort();
    let files: Vec<&str> = assets.iter().map(String::as_str).collect();

    if tag == "nightly" {
        let sha = env::var("GITHUB_SHA").map_err(|_| "GITHUB_SHA is not set")?;
        if gh(&["api", &format!("repos/{repository}/git/ref/tags/nightly")]).is_ok() {
            gh(&[
                "api",
                "-X",
                "PATCH",
                &format!("repos/{repository}/git/refs/tags/nightly"),
                "-f",
                &format!("sha={sha}"),
                "-F",
                "force=true",
            ])?;
        } else {
            gh(&[
                "api",
                "-X",
                "POST",
                &format!("repos/{repository}/git/refs"),
                "-f",
                "ref=refs/tags/nightly",
                "-f",
                &format!("sha={sha}"),
            ])?;
        }
        let notes = nightly_notes(&sha);
        if gh(&["release", "view", "nightly"]).is_ok() {
            gh(&[
                "release",
                "edit",
                "nightly",
                "--title",
                "Nightly (main)",
                "--notes",
                &notes,
                "--prerelease",
                "--latest=false",
            ])?;
            let mut upload = vec!["release", "upload", "nightly"];
            upload.extend(&files);
            upload.push("--clobber");
            gh(&upload)?;
            let names: BTreeSet<String> = files
                .iter()
                .filter_map(|file| {
                    Path::new(file)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .collect();
            // Anything left from an older snapshot that this one did not replace.
            for old in gh(&[
                "release",
                "view",
                "nightly",
                "--json",
                "assets",
                "-q",
                ".assets[].name",
            ])?
            .lines()
            {
                if !names.contains(old) {
                    gh(&["release", "delete-asset", "nightly", old, "--yes"])?;
                }
            }
        } else {
            let mut create = vec!["release", "create", "nightly"];
            create.extend(&files);
            create.extend([
                "--verify-tag",
                "--title",
                "Nightly (main)",
                "--notes",
                &notes,
                "--draft",
                "--prerelease",
                "--latest=false",
            ]);
            gh(&create)?;
        }
    } else {
        let mut upload = vec!["release", "upload", tag];
        upload.extend(&files);
        upload.push("--clobber");
        gh(&upload)?;
    }

    // Read every asset back from the release: same bytes, listed in SHA256SUMS, signed by the release workflow.
    let check = TempDir::new("sidevoice-connector-release-check")?;
    gh(&["release", "download", tag, "--dir", path_str(&check.0)?])?;
    let bundle = check.0.join("attestation.sigstore.json");
    for (digest, name) in parse_sums(&read(&check.0.join("SHA256SUMS"))?)? {
        let downloaded = read(&check.0.join(&name))?;
        if sha256(&downloaded) != digest || downloaded != read(&dir.join(&name))? {
            return Err(format!(
                "{name}: the release holds other bytes than this build"
            ));
        }
        verify_attestation(
            &check.0.join(&name),
            &repository,
            &bundle,
            &signer(&repository),
        )?;
    }

    if tag == "nightly" || tag.contains('-') {
        gh(&[
            "release",
            "edit",
            tag,
            "--draft=false",
            "--prerelease",
            "--latest=false",
        ])?;
    } else {
        gh(&[
            "release",
            "edit",
            tag,
            "--draft=false",
            "--prerelease=false",
            "--latest",
        ])?;
    }
    println!("published {tag}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_signer_is_the_release_workflow_on_main() {
        assert_eq!(
            signer("sidevoice/sidevoice-connector"),
            "https://github.com/sidevoice/sidevoice-connector/.github/workflows/release.yml@refs/heads/main"
        );
    }
}
