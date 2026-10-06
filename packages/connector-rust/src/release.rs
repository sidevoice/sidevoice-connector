//! The installed release this binary runs from, when it is started as one (`--installed`): the release format the
//! JavaScript installer writes, `R/releases/<id>/{dist/sidevoice-rust, dist/sidevoice, core/, release.json}` with
//! `R/current` naming it. At startup the release record, the paths, the executables' digests, the target and the
//! pair identity are checked again; a binary that is not the current release's refuses to run as it.

use crate::secure_fs::{digest_file, private_dir, private_file, safe_executable};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize)]
pub struct InstalledRelease {
    pub id: String,
    pub connector: String,
    pub format: Option<String>,
    pub core_kind: Option<String>,
    pub core_build: Option<String>,
    pub core_target: Option<String>,
    pub core_source_sha: Option<String>,
    pub core_cargo_lock_sha256: Option<String>,
    pub core_manifest_sha256: Option<String>,
    pub core_archive_sha256: Option<String>,
    pub core_archive_size: Option<u64>,
    pub core_entrypoint: Option<String>,
    pub runtime_kind: String,
    pub runtime_build_sha: Option<String>,
    pub runtime_target: Option<String>,
    pub runtime_sha256: Option<String>,
    pub runtime_size: Option<u64>,
    pub distributor_sha256: Option<String>,
    pub distributor_size: Option<u64>,
    pub pair_id: String,
}

fn compiled_target() -> Option<&'static str> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return Some("macos-aarch64");
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        return Some("linux-x86_64");
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        return Some("linux-aarch64");
    }
    #[allow(unreachable_code)]
    None
}

fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The release root and the release record of the current release this binary is, or a refusal.
pub fn selected() -> Result<(PathBuf, InstalledRelease)> {
    let executable = std::env::current_exe()?.canonicalize()?;
    let dist = executable.parent().context("installed dist directory")?;
    let release_dir = dist
        .parent()
        .context("installed release directory")?
        .to_path_buf();
    if executable.file_name().and_then(|name| name.to_str()) != Some("sidevoice-rust")
        || dist.file_name().and_then(|name| name.to_str()) != Some("dist")
        || release_dir
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            != Some("releases")
    {
        bail!("Rust Connector is not inside a selected Sidevoice release");
    }
    let releases_dir = release_dir
        .parent()
        .context("release collection")?
        .to_path_buf();
    let root = releases_dir
        .parent()
        .context("Sidevoice release root")?
        .to_path_buf();
    private_dir(&root)?;
    private_dir(&releases_dir)?;
    private_dir(&release_dir)?;
    private_dir(dist)?;
    let current_link = root.join("current");
    if !fs::symlink_metadata(&current_link)?
        .file_type()
        .is_symlink()
        || current_link.canonicalize()? != release_dir
    {
        bail!("Rust Connector is not the current selected release");
    }
    let release_file = release_dir.join("release.json");
    private_file(&release_file)?;
    if fs::metadata(&release_file)?.len() > 65536 {
        bail!("selected release record is oversized");
    }
    let selected: InstalledRelease = serde_json::from_slice(&fs::read(&release_file)?)?;
    let target = compiled_target().context("unsupported installed Rust Connector target")?;
    let runtime_sha = selected
        .runtime_sha256
        .as_deref()
        .context("selected runtime digest missing")?;
    let runtime_source = selected
        .runtime_build_sha
        .as_deref()
        .context("selected Rust Connector source missing")?;
    let core_target = selected
        .core_target
        .as_deref()
        .context("selected Core target missing")?;
    let core_source = selected
        .core_source_sha
        .as_deref()
        .context("selected Core source missing")?;
    let core_archive_sha = selected
        .core_archive_sha256
        .as_deref()
        .context("selected Core archive digest missing")?;
    let core_id = format!("rust-native-v1-{core_target}-{core_source}-{core_archive_sha}");
    if selected.id
        != release_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
        || selected.format.as_deref() != Some("sea")
        || selected.runtime_kind != "rust-native-v1"
        || selected.runtime_target.as_deref() != Some(target)
        || runtime_source != env!("SIDEVOICE_CONNECTOR_BUILD_SHA")
        || selected.connector != env!("SIDEVOICE_CONNECTOR_VERSION")
        || env!("SIDEVOICE_CONNECTOR_TARGET") != target
        || selected.core_kind.as_deref() != Some("rust-native-v1")
        || core_target != target
        || !lower_hex(runtime_sha, 64)
        || !lower_hex(runtime_source, 40)
        || !lower_hex(core_source, 40)
        || !lower_hex(core_archive_sha, 64)
        || !selected
            .core_cargo_lock_sha256
            .as_deref()
            .is_some_and(|value| lower_hex(value, 64))
        || !selected
            .core_manifest_sha256
            .as_deref()
            .is_some_and(|value| lower_hex(value, 64))
        || !selected.core_archive_size.is_some_and(|size| size > 0)
        || selected.core_entrypoint.as_deref() != Some("bin/sidevoice-core-rust")
        || selected.core_build.as_deref() != Some(core_id.as_str())
        || selected.pair_id != format!("pair-v1:rust-native-v1:{runtime_sha}:core:{core_id}")
    {
        bail!("selected Rust Connector and Core release identity is invalid");
    }
    let runtime_size = selected
        .runtime_size
        .context("selected runtime size missing")?;
    let runtime_info = safe_executable(&executable)?;
    if runtime_info.len() != runtime_size || digest_file(&executable)? != runtime_sha {
        bail!("selected Rust Connector bytes differ from the release record");
    }
    let control = dist.join("sidevoice");
    let control_info = safe_executable(&control)?;
    if control_info.len()
        != selected
            .distributor_size
            .context("selected control SEA size missing")?
        || digest_file(&control)?
            != selected
                .distributor_sha256
                .as_deref()
                .context("selected control SEA digest missing")?
    {
        bail!("selected Sidevoice control executable differs from the release record");
    }
    let core_binary = release_dir.join("core/bin/sidevoice-core-rust");
    let _ = safe_executable(&core_binary)?;
    Ok((root, selected))
}
