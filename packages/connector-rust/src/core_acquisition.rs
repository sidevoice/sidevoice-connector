//! Signed acquisition of the current Python Core bundle for macOS arm64.
//!
//! This module only creates an unselected, self-tested runtime. Release selection and the
//! user-facing install transaction belong to the caller.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use sidevoice_core_attestation::{core_policy, verify_file};
use sigstore_trust_root::{TrustedRoot, TufConfig};
use sigstore_tuf::reqwest::{self, redirect::Policy, Client, Url};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::timeout,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const RELEASE_PREFIX: &str = "/sidevoice/sidevoice-core/releases/download/";
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_SIDECAR_BYTES: usize = 4 * 1024 * 1024;
const MAX_UNPACKED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_ARCHIVE_ENTRIES: usize = 100_000;
const MAX_LINK_HOPS: usize = 4096;
const VERIFY_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const SELF_TEST_TIMEOUT: Duration = Duration::from_secs(120);
const OUTPUT_TAIL_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CoreChannel {
    Nightly,
    Release,
}

impl CoreChannel {
    fn tag(&self, version: &str) -> String {
        match self {
            Self::Nightly => "nightly".to_owned(),
            Self::Release => format!("v{version}"),
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Nightly => "nightly",
            Self::Release => "release",
        }
    }
}

/// Immutable inputs that bind an install to one producer manifest and Core version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CorePin {
    version: String,
    channel: CoreChannel,
    manifest_sha256: String,
}

impl CorePin {
    pub fn new(
        version: impl Into<String>,
        channel: CoreChannel,
        manifest_sha256: impl Into<String>,
    ) -> Result<Self, CoreAcquisitionError> {
        let pin = Self {
            version: version.into(),
            channel,
            manifest_sha256: manifest_sha256.into(),
        };
        if !valid_version(&pin.version) {
            return Err(CoreAcquisitionError::Pin(
                "Core version must be a numeric x.y.z version".into(),
            ));
        }
        if !valid_sha256(&pin.manifest_sha256) {
            return Err(CoreAcquisitionError::Pin(
                "manifest pin must be lowercase SHA-256".into(),
            ));
        }
        Ok(pin)
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn channel(&self) -> &CoreChannel {
        &self.channel
    }

    pub fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }

    fn manifest_url(&self) -> String {
        format!(
            "https://github.com{RELEASE_PREFIX}{}{}",
            self.channel.tag(&self.version),
            "/core-manifest.json"
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CoreAcquisitionError {
    #[error("Core acquisition cancelled")]
    Cancelled,
    #[error("Core pin refused: {0}")]
    Pin(String),
    #[error("Core manifest refused: {0}")]
    Manifest(String),
    #[error("Core authenticity refused: {0}")]
    Authenticity(String),
    #[error("Core download failed: {0}")]
    Download(String),
    #[error("Core archive refused: {0}")]
    Archive(String),
    #[error("Core self-test refused: {0}")]
    SelfTest(String),
    #[error("Core trust-root verification failed: {0}")]
    Trust(String),
    #[error("Core staging failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoreIdentity {
    pub version: String,
    pub channel: CoreChannel,
    pub manifest_sha256: String,
    pub bundle_sha256: String,
    pub bundle_size: u64,
}

/// A private acquisition directory. Dropping it removes the downloaded inputs and extracted runtime.
pub struct StagedCore {
    stage_root: PathBuf,
    release_stage_root: PathBuf,
    runtime_root: PathBuf,
    identity: CoreIdentity,
}

impl StagedCore {
    pub fn runtime_root(&self) -> &Path {
        &self.runtime_root
    }

    pub fn identity(&self) -> &CoreIdentity {
        &self.identity
    }

    /// Move the checked runtime into the still-unselected candidate release directory.
    pub fn promote_into_release_stage(
        self,
        release_stage_root: &Path,
        cancel: &CancellationToken,
    ) -> Result<VerifiedCoreRuntime, CoreAcquisitionError> {
        check_cancel(cancel)?;
        let expected = canonical_private_dir(&self.release_stage_root)?;
        let supplied = canonical_private_dir(release_stage_root)?;
        if expected != supplied {
            return Err(CoreAcquisitionError::Pin(
                "Core must be promoted into the release stage that owns its temporary files".into(),
            ));
        }
        let destination = supplied.join("core");
        if fs::symlink_metadata(&destination).is_ok() {
            return Err(CoreAcquisitionError::Pin(
                "release stage already contains a Core runtime".into(),
            ));
        }
        let mut renamed = false;
        let promoted = (|| {
            fs::rename(&self.runtime_root, &destination)?;
            renamed = true;
            sync_tree(&destination)?;
            sync_dir(&supplied)?;
            check_cancel(cancel)?;
            Ok(VerifiedCoreRuntime {
                root: destination.clone(),
                identity: self.identity.clone(),
            })
        })();
        if promoted.is_err() && renamed {
            fs::remove_dir_all(&destination)?;
            sync_dir(&supplied)?;
        }
        promoted
    }
}

impl Drop for StagedCore {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.stage_root);
    }
}

/// A verified and self-tested Python Core tree inside a candidate release, before selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedCoreRuntime {
    root: PathBuf,
    identity: CoreIdentity,
}

impl VerifiedCoreRuntime {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn identity(&self) -> &CoreIdentity {
        &self.identity
    }

    /// Entry point of the current signed Python bundle producer. T7 may replace this contract.
    pub fn current_python_entrypoint(&self) -> PathBuf {
        self.root.join("python/bin/python3")
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CoreManifest {
    bundles: Vec<CoreBundle>,
    wheel: CoreWheel,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CoreBundle {
    os: String,
    arch: String,
    url: String,
    sha256: String,
    size: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CoreWheel {
    url: String,
    sha256: String,
}

/// Owns the released Sigstore trust policy and bounded HTTPS client used for acquisition.
pub struct CoreAcquirer {
    client: Client,
    trust_cache: PathBuf,
}

impl CoreAcquirer {
    pub fn new(trust_cache: &Path) -> Result<Self, CoreAcquisitionError> {
        let trust_cache = ensure_private_dir(trust_cache, true)?;
        let client = Client::builder()
            .timeout(DOWNLOAD_TIMEOUT)
            .redirect(Policy::custom(|attempt| {
                if attempt.previous().len() >= 10 || !allowed_redirect(attempt.url()) {
                    attempt.error("refused Core release redirect")
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .map_err(|error| CoreAcquisitionError::Download(error.to_string()))?;
        Ok(Self {
            client,
            trust_cache,
        })
    }

    /// Download and stage the currently published `{bundles,wheel}` Core bundle for macOS arm64.
    pub async fn stage_macos_arm64(
        &self,
        pin: &CorePin,
        release_stage_root: &Path,
        cancel: &CancellationToken,
    ) -> Result<StagedCore, CoreAcquisitionError> {
        if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            return Err(CoreAcquisitionError::Pin(
                "this acquisition slice supports macOS arm64 only".into(),
            ));
        }
        check_cancel(cancel)?;
        let release_stage_root = canonical_private_dir(release_stage_root)?;
        let stage_root = create_stage_dir(&release_stage_root)?;
        let stage = StageGuard::new(stage_root.clone());

        let manifest_url = checked_manifest_url(pin)?;
        let manifest = fetch_bytes(&self.client, &manifest_url, MAX_MANIFEST_BYTES, cancel).await?;
        let manifest_sidecar = fetch_bytes(
            &self.client,
            &append_sidecar(&manifest_url),
            MAX_SIDECAR_BYTES,
            cancel,
        )
        .await?;
        let manifest_path = stage_root.join("core-manifest.json");
        write_private_new(&manifest_path, &manifest)?;
        let root = self.trusted_root(cancel).await?;
        verify_file_cancellable(
            manifest_path.clone(),
            manifest_sidecar,
            root.clone(),
            pin.manifest_sha256.clone(),
            None,
            pin.channel.as_str().to_owned(),
            cancel.clone(),
        )
        .await?;
        let parsed = parse_manifest(pin, &manifest)?;
        let bundle = macos_arm64_bundle(&parsed)?;

        let asset_url = checked_bundle_url(pin, bundle)?;
        let sidecar = fetch_bytes(
            &self.client,
            &append_sidecar(&asset_url),
            MAX_SIDECAR_BYTES,
            cancel,
        )
        .await?;
        let archive_path = stage_root.join("core-bundle.tar.zst");
        fetch_file(&self.client, &asset_url, &archive_path, bundle.size, cancel).await?;
        verify_file_cancellable(
            archive_path.clone(),
            sidecar,
            root,
            bundle.sha256.clone(),
            Some(bundle.size),
            pin.channel.as_str().to_owned(),
            cancel.clone(),
        )
        .await?;
        check_cancel(cancel)?;

        let payload = stage_root.join("payload");
        let archive_for_extract = archive_path.clone();
        let cancel_for_extract = cancel.clone();
        tokio::task::spawn_blocking(move || {
            extract_verified_archive(&archive_for_extract, &payload, &cancel_for_extract)
        })
        .await
        .map_err(|error| CoreAcquisitionError::Archive(error.to_string()))??;
        check_cancel(cancel)?;
        self_test_current_python(&payload, cancel).await?;
        check_cancel(cancel)?;

        fs::remove_file(manifest_path)?;
        fs::remove_file(archive_path)?;
        sync_dir(&stage_root)?;
        let identity = CoreIdentity {
            version: pin.version.clone(),
            channel: pin.channel.clone(),
            manifest_sha256: pin.manifest_sha256.clone(),
            bundle_sha256: bundle.sha256.clone(),
            bundle_size: bundle.size,
        };
        let staged = StagedCore {
            stage_root: stage.into_path(),
            release_stage_root,
            runtime_root: payload,
            identity,
        };
        Ok(staged)
    }

    async fn trusted_root(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Arc<TrustedRoot>, CoreAcquisitionError> {
        check_cancel(cancel)?;
        let http = reqwest::Client::builder()
            .timeout(VERIFY_TIMEOUT)
            .build()
            .map_err(|error| CoreAcquisitionError::Trust(error.to_string()))?;
        let config = TufConfig::production()
            .with_cache_dir(self.trust_cache.clone())
            .with_http_client(http);
        tokio::select! {
            _ = cancel.cancelled() => Err(CoreAcquisitionError::Cancelled),
            result = TrustedRoot::from_tuf(config) => result
                .map(Arc::new)
                .map_err(|error| CoreAcquisitionError::Trust(error.to_string())),
        }
    }

    #[cfg(test)]
    async fn stage_snapshot_for_test(
        &self,
        pin: &CorePin,
        release_stage_root: &Path,
        snapshot: &Path,
        cancel: &CancellationToken,
    ) -> Result<StagedCore, CoreAcquisitionError> {
        if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            return Err(CoreAcquisitionError::Pin(
                "macOS arm64 fixture required".into(),
            ));
        }
        check_cancel(cancel)?;
        let release_stage_root = canonical_private_dir(release_stage_root)?;
        let stage_root = create_stage_dir(&release_stage_root)?;
        let stage = StageGuard::new(stage_root.clone());
        let manifest = read_bounded(&snapshot.join("core-manifest.json"), MAX_MANIFEST_BYTES)?;
        let manifest_sidecar = read_bounded(
            &snapshot.join("core-manifest.json.sigstore.json"),
            MAX_SIDECAR_BYTES,
        )?;
        let manifest_path = stage_root.join("core-manifest.json");
        write_private_new(&manifest_path, &manifest)?;
        let root = self.trusted_root(cancel).await?;
        verify_file_cancellable(
            manifest_path.clone(),
            manifest_sidecar,
            root.clone(),
            pin.manifest_sha256.clone(),
            None,
            pin.channel.as_str().to_owned(),
            cancel.clone(),
        )
        .await?;
        let parsed = parse_manifest(pin, &manifest)?;
        let bundle = macos_arm64_bundle(&parsed)?;
        let archive_name = "core-macos-aarch64.tar.zst";
        let archive_sidecar_name = "core-macos-aarch64.tar.zst.sigstore.json";
        let archive_path = stage_root.join(archive_name);
        copy_private_new(&snapshot.join(archive_name), &archive_path, bundle.size)?;
        let asset_sidecar = read_bounded(&snapshot.join(archive_sidecar_name), MAX_SIDECAR_BYTES)?;
        verify_file_cancellable(
            archive_path.clone(),
            asset_sidecar,
            root,
            bundle.sha256.clone(),
            Some(bundle.size),
            pin.channel.as_str().to_owned(),
            cancel.clone(),
        )
        .await?;
        check_cancel(cancel)?;
        let payload = stage_root.join("payload");
        let archive_for_extract = archive_path.clone();
        let cancel_for_extract = cancel.clone();
        tokio::task::spawn_blocking(move || {
            extract_verified_archive(&archive_for_extract, &payload, &cancel_for_extract)
        })
        .await
        .map_err(|error| CoreAcquisitionError::Archive(error.to_string()))??;
        check_cancel(cancel)?;
        self_test_current_python(&payload, cancel).await?;
        check_cancel(cancel)?;
        fs::remove_file(manifest_path)?;
        fs::remove_file(archive_path)?;
        sync_dir(&stage_root)?;
        let staged = StagedCore {
            stage_root: stage.into_path(),
            release_stage_root,
            runtime_root: payload,
            identity: CoreIdentity {
                version: pin.version.clone(),
                channel: pin.channel.clone(),
                manifest_sha256: pin.manifest_sha256.clone(),
                bundle_sha256: bundle.sha256.clone(),
                bundle_size: bundle.size,
            },
        };
        Ok(staged)
    }
}

struct StageGuard(Option<PathBuf>);

impl StageGuard {
    fn new(path: PathBuf) -> Self {
        Self(Some(path))
    }

    fn into_path(mut self) -> PathBuf {
        self.0.take().expect("stage guard owns its path")
    }
}

impl Drop for StageGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn valid_version(version: &str) -> bool {
    let mut parts = version.split('.');
    let numbers = [parts.next(), parts.next(), parts.next()];
    numbers.into_iter().all(|part| {
        part.is_some_and(|value| {
            !value.is_empty()
                && (value == "0" || !value.starts_with('0'))
                && value.bytes().all(|byte| byte.is_ascii_digit())
        })
    }) && parts.next().is_none()
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn checked_manifest_url(pin: &CorePin) -> Result<Url, CoreAcquisitionError> {
    let raw = pin.manifest_url();
    let url =
        Url::parse(&raw).map_err(|_| CoreAcquisitionError::Pin("invalid manifest URL".into()))?;
    if !allowed_release_url(&url)
        || url.path()
            != format!(
                "{RELEASE_PREFIX}{}/core-manifest.json",
                pin.channel.tag(&pin.version)
            )
    {
        return Err(CoreAcquisitionError::Pin(
            "manifest URL is outside the pinned Core release".into(),
        ));
    }
    Ok(url)
}

fn allowed_release_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some("github.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.path().starts_with(RELEASE_PREFIX)
}

fn allowed_redirect(url: &Url) -> bool {
    url.scheme() == "https"
        && matches!(
            url.host_str(),
            Some("github.com" | "release-assets.githubusercontent.com")
        )
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

fn append_sidecar(url: &Url) -> Url {
    let mut sidecar = url.clone();
    sidecar.set_path(&format!("{}.sigstore.json", url.path()));
    sidecar
}

fn parse_manifest(pin: &CorePin, bytes: &[u8]) -> Result<CoreManifest, CoreAcquisitionError> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(CoreAcquisitionError::Manifest(
            "manifest exceeds size limit".into(),
        ));
    }
    if sha256(bytes) != pin.manifest_sha256 {
        return Err(CoreAcquisitionError::Authenticity(
            "manifest bytes do not match the pinned SHA-256".into(),
        ));
    }
    let manifest: CoreManifest = serde_json::from_slice(bytes)
        .map_err(|error| CoreAcquisitionError::Manifest(format!("producer schema: {error}")))?;
    if manifest.bundles.is_empty() {
        return Err(CoreAcquisitionError::Manifest(
            "no Core bundles in producer manifest".into(),
        ));
    }
    let mut targets = HashSet::new();
    for bundle in &manifest.bundles {
        let target = format!("{}/{}", bundle.os, bundle.arch);
        if !matches!(
            target.as_str(),
            "macos/aarch64" | "linux/x86_64" | "linux/aarch64"
        ) {
            return Err(CoreAcquisitionError::Manifest(format!(
                "unsupported bundle target {target}"
            )));
        }
        if !targets.insert(target.clone()) {
            return Err(CoreAcquisitionError::Manifest(format!(
                "duplicate bundle target {target}"
            )));
        }
        if bundle.size == 0 || bundle.size > MAX_SAFE_INTEGER || !valid_sha256(&bundle.sha256) {
            return Err(CoreAcquisitionError::Manifest(format!(
                "incomplete bundle record for {target}"
            )));
        }
        let expected = format!(
            "sidevoice-core-{}-{}-{}.tar.zst",
            pin.version, bundle.os, bundle.arch
        );
        validate_asset_url(pin, &bundle.url, &expected)?;
    }
    if !valid_sha256(&manifest.wheel.sha256) {
        return Err(CoreAcquisitionError::Manifest(
            "invalid wheel SHA-256".into(),
        ));
    }
    let wheel_name = format!("sidevoice_core-{}-py3-none-any.whl", pin.version);
    validate_asset_url(pin, &manifest.wheel.url, &wheel_name)?;
    Ok(manifest)
}

fn validate_asset_url(pin: &CorePin, raw: &str, name: &str) -> Result<Url, CoreAcquisitionError> {
    let url = Url::parse(raw)
        .map_err(|_| CoreAcquisitionError::Manifest("invalid Core asset URL".into()))?;
    let expected = format!("{RELEASE_PREFIX}{}/{name}", pin.channel.tag(&pin.version));
    if !allowed_release_url(&url) || url.path() != expected {
        return Err(CoreAcquisitionError::Manifest(
            "Core asset URL does not match its release channel, version and filename".into(),
        ));
    }
    Ok(url)
}

fn macos_arm64_bundle(manifest: &CoreManifest) -> Result<&CoreBundle, CoreAcquisitionError> {
    manifest
        .bundles
        .iter()
        .find(|bundle| bundle.os == "macos" && bundle.arch == "aarch64")
        .ok_or_else(|| {
            CoreAcquisitionError::Manifest("pinned manifest has no macOS arm64 bundle".into())
        })
}

fn checked_bundle_url(pin: &CorePin, bundle: &CoreBundle) -> Result<Url, CoreAcquisitionError> {
    let name = format!("sidevoice-core-{}-macos-aarch64.tar.zst", pin.version);
    validate_asset_url(pin, &bundle.url, &name)
}

async fn fetch_bytes(
    client: &Client,
    url: &Url,
    limit: usize,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, CoreAcquisitionError> {
    check_cancel(cancel)?;
    let response = send_checked(client, url, cancel).await?;
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(CoreAcquisitionError::Download(
            "response exceeds size limit".into(),
        ));
    }
    let mut response = response;
    let mut bytes = Vec::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Err(CoreAcquisitionError::Cancelled),
            next = response.chunk() => match next {
                Ok(Some(chunk)) => {
                    if bytes.len().saturating_add(chunk.len()) > limit {
                        return Err(CoreAcquisitionError::Download("response exceeds size limit".into()));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => return Ok(bytes),
                Err(error) => return Err(CoreAcquisitionError::Download(error.to_string())),
            }
        }
    }
}

async fn fetch_file(
    client: &Client,
    url: &Url,
    path: &Path,
    expected_size: u64,
    cancel: &CancellationToken,
) -> Result<(), CoreAcquisitionError> {
    check_cancel(cancel)?;
    let mut response = send_checked(client, url, cancel).await?;
    if response
        .content_length()
        .is_some_and(|length| length > expected_size)
    {
        return Err(CoreAcquisitionError::Download(
            "archive response exceeds signed size".into(),
        ));
    }
    let file = create_private_file(path)?;
    let mut file = tokio::fs::File::from_std(file);
    let mut size = 0_u64;
    let result = loop {
        tokio::select! {
            _ = cancel.cancelled() => break Err(CoreAcquisitionError::Cancelled),
            next = response.chunk() => match next {
                Ok(Some(chunk)) => {
                    let Some(next_size) = size.checked_add(chunk.len() as u64) else {
                        break Err(CoreAcquisitionError::Download("archive size overflow".into()));
                    };
                    size = next_size;
                    if size > expected_size {
                        break Err(CoreAcquisitionError::Download("archive exceeds signed size".into()));
                    }
                    if let Err(error) = file.write_all(&chunk).await {
                        break Err(CoreAcquisitionError::Io(error));
                    }
                }
                Ok(None) => break Ok(()),
                Err(error) => break Err(CoreAcquisitionError::Download(error.to_string())),
            }
        }
    };
    if let Err(error) = result {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    if size != expected_size {
        drop(file);
        fs::remove_file(path)?;
        return Err(CoreAcquisitionError::Download(
            "archive size differs from signed manifest".into(),
        ));
    }
    file.sync_all().await?;
    Ok(())
}

async fn send_checked(
    client: &Client,
    url: &Url,
    cancel: &CancellationToken,
) -> Result<reqwest::Response, CoreAcquisitionError> {
    if !allowed_redirect(url) {
        return Err(CoreAcquisitionError::Pin(
            "refused non-GitHub Core URL".into(),
        ));
    }
    let request = client.get(url.clone()).send();
    let response = tokio::select! {
        _ = cancel.cancelled() => return Err(CoreAcquisitionError::Cancelled),
        result = request => result.map_err(|error| CoreAcquisitionError::Download(error.to_string()))?,
    };
    let final_url = response.url();
    if !allowed_redirect(final_url) {
        return Err(CoreAcquisitionError::Download(
            "Core release redirected outside GitHub Releases".into(),
        ));
    }
    if !response.status().is_success() {
        return Err(CoreAcquisitionError::Download(format!(
            "Core release request returned HTTP {}",
            response.status().as_u16()
        )));
    }
    Ok(response)
}

async fn verify_file_cancellable(
    path: PathBuf,
    sidecar: Vec<u8>,
    root: Arc<TrustedRoot>,
    expected_sha256: String,
    expected_size: Option<u64>,
    channel: String,
    cancel: CancellationToken,
) -> Result<(), CoreAcquisitionError> {
    check_cancel(&cancel)?;
    let mut task = tokio::task::spawn_blocking(move || {
        let policy = core_policy();
        verify_file(
            &path,
            &sidecar,
            &root,
            &policy,
            Some(&channel),
            Some(&expected_sha256),
            expected_size,
        )
        .map(|_| ())
        .map_err(CoreAcquisitionError::Authenticity)
    });
    tokio::select! {
        _ = cancel.cancelled() => {
            let _ = (&mut task).await;
            Err(CoreAcquisitionError::Cancelled)
        }
        result = &mut task => result.map_err(|error| CoreAcquisitionError::Authenticity(error.to_string()))?,
    }
}

async fn self_test_current_python(
    payload: &Path,
    cancel: &CancellationToken,
) -> Result<(), CoreAcquisitionError> {
    check_cancel(cancel)?;
    let interpreter = payload.join("python/bin/python3");
    let metadata = fs::symlink_metadata(&interpreter).map_err(|error| {
        CoreAcquisitionError::SelfTest(format!("missing current Python Core entrypoint: {error}"))
    })?;
    if !metadata.file_type().is_file() && !metadata.file_type().is_symlink() {
        return Err(CoreAcquisitionError::SelfTest(
            "current Python Core entrypoint is not a file".into(),
        ));
    }
    if fs::metadata(&interpreter)?.permissions().mode() & 0o111 == 0 {
        return Err(CoreAcquisitionError::SelfTest(
            "current Python Core entrypoint is not executable".into(),
        ));
    }

    let mut command = Command::new(&interpreter);
    command
        .args(["-I", "-B", "-m", "sidevoice_core.server", "--self-test"])
        .current_dir(payload)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", payload)
        .env("TMPDIR", payload)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| CoreAcquisitionError::SelfTest(error.to_string()))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_task = tokio::spawn(read_tail(stdout));
    let stderr_task = tokio::spawn(read_tail(stderr));
    let waited = tokio::select! {
        _ = cancel.cancelled() => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(CoreAcquisitionError::Cancelled);
        }
        result = timeout(SELF_TEST_TIMEOUT, child.wait()) => match result {
            Ok(result) => result.map_err(|error| CoreAcquisitionError::SelfTest(error.to_string()))?,
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                return Err(CoreAcquisitionError::SelfTest("Core self-test timed out".into()));
            }
        }
    };
    let stdout = stdout_task
        .await
        .map_err(|error| CoreAcquisitionError::SelfTest(error.to_string()))?
        .map_err(|error| CoreAcquisitionError::SelfTest(error.to_string()))?;
    let stderr = stderr_task
        .await
        .map_err(|error| CoreAcquisitionError::SelfTest(error.to_string()))?
        .map_err(|error| CoreAcquisitionError::SelfTest(error.to_string()))?;
    if !waited.success() {
        return Err(CoreAcquisitionError::SelfTest(format!(
            "Core self-test exited with {}: {}",
            waited
                .code()
                .map_or_else(|| "signal".into(), |code| code.to_string()),
            tail_text(&stderr)
        )));
    }
    let last_line = stdout
        .split(|byte| *byte == b'\n')
        .rev()
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let report: serde_json::Value = serde_json::from_slice(last_line).map_err(|_| {
        CoreAcquisitionError::SelfTest("Core self-test returned no JSON result".into())
    })?;
    if report.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(CoreAcquisitionError::SelfTest(
            "Core self-test result did not report ok".into(),
        ));
    }
    Ok(())
}

async fn read_tail<R: tokio::io::AsyncRead + Unpin>(mut reader: R) -> std::io::Result<Vec<u8>> {
    let mut tail = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            return Ok(tail);
        }
        tail.extend_from_slice(&chunk[..count]);
        if tail.len() > OUTPUT_TAIL_BYTES {
            tail.drain(..tail.len() - OUTPUT_TAIL_BYTES);
        }
    }
}

fn tail_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_owned()
}

fn extract_verified_archive(
    archive_path: &Path,
    destination: &Path,
    cancel: &CancellationToken,
) -> Result<(), CoreAcquisitionError> {
    check_cancel(cancel)?;
    fs::create_dir(destination)?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o700))?;
    let root = destination.canonicalize()?;
    let compressed = File::open(archive_path)?;
    let decoder = zstd::stream::read::Decoder::new(compressed)
        .map_err(|error| CoreAcquisitionError::Archive(error.to_string()))?;
    let mut archive = tar::Archive::new(decoder);
    let mut entries = archive
        .entries()
        .map_err(|error| CoreAcquisitionError::Archive(error.to_string()))?;
    let mut seen = HashSet::new();
    let mut links = Vec::new();
    let mut entry_count = 0_usize;
    let mut unpacked = 0_u64;
    while let Some(entry) = entries.next() {
        check_cancel(cancel)?;
        let mut entry = entry.map_err(|error| CoreAcquisitionError::Archive(error.to_string()))?;
        entry_count += 1;
        if entry_count > MAX_ARCHIVE_ENTRIES {
            return Err(CoreAcquisitionError::Archive(
                "too many archive entries".into(),
            ));
        }
        let (raw_path, raw_link, has_pax_path, has_pax_linkpath) = raw_entry_names(&mut entry)?;
        let path_bytes = entry.path_bytes();
        if !has_pax_path && path_bytes.as_ref() != raw_path.as_slice() {
            return Err(CoreAcquisitionError::Archive(
                "unsupported extended archive path".into(),
            ));
        }
        let name = std::str::from_utf8(&path_bytes)
            .map_err(|_| CoreAcquisitionError::Archive("archive path is not UTF-8".into()))?;
        let (normalized, parts) = clean_archive_path(name)?;
        if !seen.insert(normalized.clone()) {
            return Err(CoreAcquisitionError::Archive(format!(
                "duplicate archive path {normalized}"
            )));
        }
        ensure_real_parents(&root, &parts[..parts.len() - 1])?;
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            ensure_real_directory(&root.join(&normalized))?;
            drain_entry(&mut entry, cancel, &mut unpacked)?;
        } else if entry_type.is_symlink() {
            let target = entry
                .link_name_bytes()
                .map(|target| target.into_owned())
                .ok_or_else(|| {
                    CoreAcquisitionError::Archive(format!("symlink target missing at {normalized}"))
                })?;
            if !has_pax_linkpath && target.as_slice() != raw_link.as_slice() {
                return Err(CoreAcquisitionError::Archive(
                    "unsupported extended symlink target".into(),
                ));
            }
            let target = std::str::from_utf8(&target)
                .map_err(|_| CoreAcquisitionError::Archive("symlink target is not UTF-8".into()))?;
            validate_link_target(target)?;
            drain_entry(&mut entry, cancel, &mut unpacked)?;
            links.push(ArchiveLink {
                path: normalized,
                target: target.to_owned(),
            });
        } else if entry_type.is_file() && !entry_type.is_contiguous() {
            let size = entry.size();
            unpacked = unpacked
                .checked_add(size)
                .ok_or_else(|| CoreAcquisitionError::Archive("unpacked size overflow".into()))?;
            if unpacked > MAX_UNPACKED_BYTES {
                return Err(CoreAcquisitionError::Archive(
                    "unpacked Core exceeds size limit".into(),
                ));
            }
            let output = root.join(&normalized);
            let mode = if entry.header().mode().unwrap_or(0) & 0o111 != 0 {
                0o755
            } else {
                0o644
            };
            let mut file = create_archive_file(&output, mode)?;
            copy_entry(&mut entry, &mut file, cancel)?;
            file.sync_all()?;
            if fs::metadata(&output)?.len() != size {
                return Err(CoreAcquisitionError::Archive(format!(
                    "truncated archive file {normalized}"
                )));
            }
        } else {
            return Err(CoreAcquisitionError::Archive(format!(
                "unsupported archive entry type at {normalized}"
            )));
        }
    }
    validate_link_targets(&root, &links)?;
    for link in links {
        check_cancel(cancel)?;
        let (normalized, parts) = clean_archive_path(&link.path)?;
        ensure_real_parents(&root, &parts[..parts.len() - 1])?;
        let full = root.join(normalized);
        if fs::symlink_metadata(&full).is_ok() {
            return Err(CoreAcquisitionError::Archive(
                "symlink collides with an archive entry".into(),
            ));
        }
        std::os::unix::fs::symlink(&link.target, full)?;
    }
    sync_tree(destination)?;
    Ok(())
}

struct ArchiveLink {
    path: String,
    target: String,
}

fn clean_archive_path(name: &str) -> Result<(String, Vec<String>), CoreAcquisitionError> {
    if name.is_empty() || name.contains('\0') || name.contains('\\') || name.starts_with('/') {
        return Err(CoreAcquisitionError::Archive("unsafe archive path".into()));
    }
    let normalized = name.strip_suffix('/').unwrap_or(name);
    let parts: Vec<String> = normalized.split('/').map(str::to_owned).collect();
    if parts.is_empty()
        || parts
            .iter()
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(CoreAcquisitionError::Archive("unsafe archive path".into()));
    }
    Ok((parts.join("/"), parts))
}

fn raw_entry_names<R: Read>(
    entry: &mut tar::Entry<'_, R>,
) -> Result<(Vec<u8>, Vec<u8>, bool, bool), CoreAcquisitionError> {
    let header = entry.header().as_bytes();
    let name = header_field(&header[..100])?;
    let prefix = if &header[257..263] == b"ustar\0" {
        header_field(&header[345..500])?
    } else {
        &[]
    };
    let mut raw_path = Vec::with_capacity(prefix.len() + name.len() + 1);
    if !prefix.is_empty() {
        raw_path.extend_from_slice(prefix);
        raw_path.push(b'/');
    }
    raw_path.extend_from_slice(name);
    let raw_link = header_field(&header[157..257])?.to_vec();
    reject_raw_name(&raw_path)?;
    reject_raw_name(&raw_link)?;
    let mut has_pax_path = false;
    let mut has_pax_linkpath = false;
    if let Some(extensions) = entry
        .pax_extensions()
        .map_err(|error| CoreAcquisitionError::Archive(error.to_string()))?
    {
        for extension in extensions {
            let extension =
                extension.map_err(|error| CoreAcquisitionError::Archive(error.to_string()))?;
            match extension.key_bytes() {
                b"path" => {
                    has_pax_path = true;
                    reject_raw_name(extension.value_bytes())?;
                }
                b"linkpath" => {
                    has_pax_linkpath = true;
                    reject_raw_name(extension.value_bytes())?;
                }
                _ => {}
            }
        }
    }
    Ok((raw_path, raw_link, has_pax_path, has_pax_linkpath))
}

fn header_field(field: &[u8]) -> Result<&[u8], CoreAcquisitionError> {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    if field[end..].iter().any(|byte| *byte != 0) {
        return Err(CoreAcquisitionError::Archive(
            "malformed tar header field".into(),
        ));
    }
    Ok(&field[..end])
}

fn reject_raw_name(value: &[u8]) -> Result<(), CoreAcquisitionError> {
    if value.contains(&b'\\') {
        return Err(CoreAcquisitionError::Archive(
            "archive path or symlink target contains a forbidden byte".into(),
        ));
    }
    Ok(())
}

fn ensure_real_parents(root: &Path, parts: &[String]) -> Result<(), CoreAcquisitionError> {
    let mut current = root.to_path_buf();
    for part in parts {
        current.push(part);
        ensure_real_directory(&current)?;
    }
    Ok(())
}

fn ensure_real_directory(path: &Path) -> Result<(), CoreAcquisitionError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(CoreAcquisitionError::Archive(
            "archive parent is not a real directory".into(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn validate_link_target(target: &str) -> Result<(), CoreAcquisitionError> {
    if target.is_empty()
        || target.contains('\0')
        || target.contains('\\')
        || target.starts_with('/')
    {
        return Err(CoreAcquisitionError::Archive(
            "absolute or malformed symlink target".into(),
        ));
    }
    Ok(())
}

fn validate_link_targets(root: &Path, links: &[ArchiveLink]) -> Result<(), CoreAcquisitionError> {
    let by_path: HashMap<&str, &str> = links
        .iter()
        .map(|link| (link.path.as_str(), link.target.as_str()))
        .collect();
    for link in links {
        let mut parent: Vec<String> = link.path.split('/').map(str::to_owned).collect();
        parent.pop();
        let mut pending: Vec<String> = link.target.split('/').map(str::to_owned).collect();
        let mut visited = HashSet::from([link.path.clone()]);
        let mut hops = 0;
        while let Some(part) = pending.first().cloned() {
            pending.remove(0);
            match part.as_str() {
                "" | "." => continue,
                ".." => {
                    if parent.pop().is_none() {
                        return Err(CoreAcquisitionError::Archive(
                            "symlink escapes staging root".into(),
                        ));
                    }
                }
                _ => {
                    parent.push(part);
                    let resolved = parent.join("/");
                    if let Some(target) = by_path.get(resolved.as_str()) {
                        hops += 1;
                        if hops > MAX_LINK_HOPS || !visited.insert(resolved.clone()) {
                            return Err(CoreAcquisitionError::Archive(
                                "symlink cycle or excessive chain".into(),
                            ));
                        }
                        parent.pop();
                        let mut expansion: Vec<String> =
                            resolved.split('/').map(str::to_owned).collect();
                        expansion.pop();
                        expansion.extend(target.split('/').map(str::to_owned));
                        expansion.extend(pending);
                        pending = expansion;
                    }
                }
            }
        }
        let mut resolved = root.to_path_buf();
        for part in &parent {
            if part == ".." || part == "." || part.is_empty() {
                return Err(CoreAcquisitionError::Archive(
                    "symlink escapes staging root".into(),
                ));
            }
            resolved.push(part);
        }
        if !resolved.starts_with(root) {
            return Err(CoreAcquisitionError::Archive(
                "symlink escapes staging root".into(),
            ));
        }
    }
    Ok(())
}

fn create_archive_file(path: &Path, mode: u32) -> Result<File, CoreAcquisitionError> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW);
    Ok(options.open(path)?)
}

fn drain_entry<R: Read>(
    reader: &mut R,
    cancel: &CancellationToken,
    unpacked: &mut u64,
) -> Result<(), CoreAcquisitionError> {
    let mut buffer = [0; 64 * 1024];
    loop {
        check_cancel(cancel)?;
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        *unpacked = unpacked
            .checked_add(count as u64)
            .ok_or_else(|| CoreAcquisitionError::Archive("unpacked size overflow".into()))?;
        if *unpacked > MAX_UNPACKED_BYTES {
            return Err(CoreAcquisitionError::Archive(
                "unpacked Core exceeds size limit".into(),
            ));
        }
    }
}

fn copy_entry<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    cancel: &CancellationToken,
) -> Result<(), CoreAcquisitionError> {
    let mut buffer = [0; 64 * 1024];
    loop {
        check_cancel(cancel)?;
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        writer.write_all(&buffer[..count])?;
    }
}

fn create_stage_dir(parent: &Path) -> Result<PathBuf, CoreAcquisitionError> {
    let name = format!(".core-acquisition-{}", Uuid::new_v4());
    let path = parent.join(name);
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    builder.create(&path)?;
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        let _ = fs::remove_dir_all(&path);
        return Err(CoreAcquisitionError::Pin(
            "unsafe Core acquisition stage".into(),
        ));
    }
    Ok(path)
}

fn create_private_file(path: &Path) -> Result<File, CoreAcquisitionError> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW);
    Ok(options.open(path)?)
}

fn write_private_new(path: &Path, bytes: &[u8]) -> Result<(), CoreAcquisitionError> {
    let mut file = create_private_file(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn copy_private_new(
    source: &Path,
    destination: &Path,
    expected_size: u64,
) -> Result<(), CoreAcquisitionError> {
    let source_meta = fs::symlink_metadata(source)?;
    if !source_meta.is_file()
        || source_meta.file_type().is_symlink()
        || source_meta.len() != expected_size
    {
        return Err(CoreAcquisitionError::Authenticity(
            "frozen fixture does not match signed size".into(),
        ));
    }
    let mut input = File::open(source)?;
    let mut output = create_private_file(destination)?;
    std::io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    Ok(())
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, CoreAcquisitionError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit as u64 {
        return Err(CoreAcquisitionError::Manifest(
            "fixture input is unsafe or oversized".into(),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(CoreAcquisitionError::Manifest(
            "fixture input is oversized".into(),
        ));
    }
    Ok(bytes)
}

fn ensure_private_dir(path: &Path, create: bool) -> Result<PathBuf, CoreAcquisitionError> {
    if !path.is_absolute() {
        return Err(CoreAcquisitionError::Pin(
            "private directory must be absolute".into(),
        ));
    }
    let mut missing = Vec::new();
    let mut current = path.to_path_buf();
    let mut result = loop {
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if (current == path && metadata.file_type().is_symlink())
                    || !fs::metadata(&current)?.is_dir()
                {
                    return Err(CoreAcquisitionError::Pin(
                        "private directory path is not a directory".into(),
                    ));
                }
                break current.canonicalize()?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !create {
                    return Err(CoreAcquisitionError::Pin(
                        "private directory is missing".into(),
                    ));
                }
                let name = current.file_name().ok_or_else(|| {
                    CoreAcquisitionError::Pin("private directory has no final component".into())
                })?;
                missing.push(name.to_os_string());
                current = current
                    .parent()
                    .ok_or_else(|| {
                        CoreAcquisitionError::Pin(
                            "private directory has no existing ancestor".into(),
                        )
                    })?
                    .to_path_buf();
            }
            Err(error) => return Err(error.into()),
        }
    };
    for name in missing.iter().rev() {
        result.push(name);
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder.create(&result)?;
    }
    let metadata = fs::symlink_metadata(&result)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(CoreAcquisitionError::Pin(
            "private directory ownership or mode is unsafe".into(),
        ));
    }
    Ok(result)
}

fn canonical_private_dir(path: &Path) -> Result<PathBuf, CoreAcquisitionError> {
    ensure_private_dir(path, false)
}

fn sync_tree(path: &Path) -> Result<(), CoreAcquisitionError> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let child = entry.path();
        let metadata = fs::symlink_metadata(&child)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            sync_tree(&child)?;
        } else if metadata.is_file() {
            File::open(&child)?.sync_all()?;
        }
    }
    sync_dir(path)
}

fn sync_dir(path: &Path) -> Result<(), CoreAcquisitionError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), CoreAcquisitionError> {
    if cancel.is_cancelled() {
        Err(CoreAcquisitionError::Cancelled)
    } else {
        Ok(())
    }
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Cursor,
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn scratch() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "sidevoice-core-acquisition-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn manifest(version: &str, channel: &str) -> Vec<u8> {
        let tag = if channel == "release" {
            format!("v{version}")
        } else {
            "nightly".into()
        };
        let base = format!("https://github.com{RELEASE_PREFIX}{tag}");
        serde_json::to_vec(&serde_json::json!({
            "bundles": [
                {"os":"macos","arch":"aarch64","url":format!("{base}/sidevoice-core-{version}-macos-aarch64.tar.zst"),"sha256":"a".repeat(64),"size":123},
                {"os":"linux","arch":"x86_64","url":format!("{base}/sidevoice-core-{version}-linux-x86_64.tar.zst"),"sha256":"b".repeat(64),"size":456},
                {"os":"linux","arch":"aarch64","url":format!("{base}/sidevoice-core-{version}-linux-aarch64.tar.zst"),"sha256":"c".repeat(64),"size":789}
            ],
            "wheel":{"url":format!("{base}/sidevoice_core-{version}-py3-none-any.whl"),"sha256":"d".repeat(64)}
        }))
        .unwrap()
    }

    fn pin(bytes: &[u8], channel: CoreChannel) -> CorePin {
        CorePin::new("0.1.0", channel, sha256(bytes)).unwrap()
    }

    fn archive(entries: &[(&str, &[u8], bool)]) -> PathBuf {
        let path = scratch().join("test.tar.zst");
        let file = File::create(&path).unwrap();
        let encoder = zstd::Encoder::new(file, 1).unwrap();
        let mut builder = tar::Builder::new(encoder);
        for (name, bytes, executable) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(if *executable { 0o755 } else { 0o644 });
            header.set_cksum();
            builder
                .append_data(&mut header, name, Cursor::new(*bytes))
                .unwrap();
        }
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();
        path
    }

    fn archive_with_raw_path(name: &[u8]) -> PathBuf {
        let path = scratch().join("raw-path.tar.zst");
        let file = File::create(&path).unwrap();
        let encoder = zstd::Encoder::new(file, 1).unwrap();
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.as_mut_bytes()[..100].fill(0);
        header.as_mut_bytes()[..name.len()].copy_from_slice(name);
        header.set_size(3);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, Cursor::new(b"bad")).unwrap();
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();
        path
    }

    #[test]
    fn manifest_requires_exact_producer_schema_and_pinned_assets() {
        let bytes = manifest("0.1.0", "nightly");
        let valid_pin = pin(&bytes, CoreChannel::Nightly);
        let parsed = parse_manifest(&valid_pin, &bytes).unwrap();
        assert_eq!(macos_arm64_bundle(&parsed).unwrap().size, 123);

        let mut extra = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap();
        extra["version"] = serde_json::json!("0.1.0");
        let extra = serde_json::to_vec(&extra).unwrap();
        let extra_pin = pin(&extra, CoreChannel::Nightly);
        assert!(parse_manifest(&extra_pin, &extra).is_err());

        let mut changed = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap();
        changed["bundles"][0]["url"] = serde_json::json!("https://example.com/core.tar.zst");
        let changed = serde_json::to_vec(&changed).unwrap();
        let changed_pin = pin(&changed, CoreChannel::Nightly);
        assert!(parse_manifest(&changed_pin, &changed).is_err());

        let mut oversized = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap();
        oversized["bundles"][0]["size"] = serde_json::json!(MAX_SAFE_INTEGER + 1);
        let oversized = serde_json::to_vec(&oversized).unwrap();
        let oversized_pin = pin(&oversized, CoreChannel::Nightly);
        assert!(parse_manifest(&oversized_pin, &oversized).is_err());
    }

    #[test]
    fn manifest_url_binds_channel_and_version() {
        let bytes = manifest("0.1.0", "release");
        let release = pin(&bytes, CoreChannel::Release);
        assert!(checked_manifest_url(&release).is_ok());
        let bad = CorePin::new("0.1.0", CoreChannel::Release, "A".repeat(64));
        assert!(bad.is_err());
        assert!(CorePin::new("0.1.0/../../other", CoreChannel::Nightly, "a".repeat(64)).is_err());
    }

    #[test]
    fn archive_refuses_escape_duplicate_and_hard_link_entries() {
        let cancel = CancellationToken::new();
        let outside = scratch();
        let archive_path = archive(&[("../escape", b"bad", false)]);
        assert!(
            extract_verified_archive(&archive_path, &outside.join("payload"), &cancel).is_err()
        );
        assert!(!outside.join("escape").exists());

        let backslash_path = archive_with_raw_path(b"python\\..\\escape");
        assert!(
            extract_verified_archive(&backslash_path, &outside.join("backslash"), &cancel).is_err()
        );

        let archive_path = archive(&[("python/a", b"one", false), ("python/a", b"two", false)]);
        assert!(
            extract_verified_archive(&archive_path, &outside.join("duplicate"), &cancel).is_err()
        );

        let mut builder = tar::Builder::new(
            zstd::Encoder::new(File::create(outside.join("link.tar.zst")).unwrap(), 1).unwrap(),
        );
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Link);
        header.set_size(0);
        header.set_link_name("python/target").unwrap();
        header.set_cksum();
        builder.append(&header, std::io::empty()).unwrap();
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();
        assert!(extract_verified_archive(
            &outside.join("link.tar.zst"),
            &outside.join("hardlink"),
            &cancel
        )
        .is_err());
    }

    #[test]
    fn archive_allows_in_root_links_and_rejects_link_escape_or_cycle() {
        let outside = scratch();
        let cancel = CancellationToken::new();
        let link = ArchiveLink {
            path: "python/bin/python3".into(),
            target: "../lib/python".into(),
        };
        assert!(validate_link_targets(&outside, &[link]).is_ok());
        let escape = ArchiveLink {
            path: "python/link".into(),
            target: "../../../outside".into(),
        };
        assert!(validate_link_targets(&outside, &[escape]).is_err());
        let cycle = [
            ArchiveLink {
                path: "python/a".into(),
                target: "b".into(),
            },
            ArchiveLink {
                path: "python/b".into(),
                target: "a".into(),
            },
        ];
        assert!(validate_link_targets(&outside, &cycle).is_err());

        let path = archive(&[("python/file", b"safe", false)]);
        let payload = outside.join("payload");
        extract_verified_archive(&path, &payload, &cancel).unwrap();
        assert_eq!(fs::read(payload.join("python/file")).unwrap(), b"safe");
    }

    #[test]
    fn cancelled_promotion_discards_the_staged_core_without_a_candidate() {
        let release_stage_root = scratch();
        let stage_root = release_stage_root.join(".core-acquisition-test");
        let runtime_root = stage_root.join("payload");
        fs::create_dir_all(&runtime_root).unwrap();
        fs::write(runtime_root.join("fixture-marker"), b"not executable").unwrap();
        let staged = StagedCore {
            stage_root: stage_root.clone(),
            release_stage_root: release_stage_root.clone(),
            runtime_root,
            identity: CoreIdentity {
                version: "0.1.0".into(),
                channel: CoreChannel::Nightly,
                manifest_sha256: "a".repeat(64),
                bundle_sha256: "b".repeat(64),
                bundle_size: 1,
            },
        };
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            staged.promote_into_release_stage(&release_stage_root, &cancel),
            Err(CoreAcquisitionError::Cancelled)
        ));
        assert!(!release_stage_root.join("core").exists());
        assert!(!stage_root.exists());
    }

    #[tokio::test]
    #[ignore = "hosted macOS arm64 run uses one coherent signed Core nightly snapshot"]
    async fn real_signed_macos_core_snapshot_verifies_extracts_and_self_tests() {
        let snapshot = PathBuf::from(std::env::var_os("SIDEVOICE_CORE_SNAPSHOT").unwrap());
        let manifest =
            read_bounded(&snapshot.join("core-manifest.json"), MAX_MANIFEST_BYTES).unwrap();
        let manifest_sha = sha256(&manifest);
        let stage_parent = scratch();
        let cache = stage_parent.join("sigstore-tuf");
        let acquirer = CoreAcquirer::new(&cache).unwrap();
        let pin = CorePin::new("0.1.0", CoreChannel::Nightly, manifest_sha).unwrap();
        let cancel = CancellationToken::new();
        let staged = acquirer
            .stage_snapshot_for_test(&pin, &stage_parent, &snapshot, &cancel)
            .await
            .unwrap();
        assert_eq!(staged.identity().version, "0.1.0");
        assert!(staged.runtime_root().join("python/bin/python3").exists());
        let promoted = staged
            .promote_into_release_stage(&stage_parent, &cancel)
            .unwrap();
        assert!(promoted.current_python_entrypoint().exists());
    }

    #[tokio::test]
    #[ignore = "hosted macOS arm64 run uses the frozen nightly snapshot for representative refusals"]
    async fn real_core_snapshot_refuses_wrong_digest_and_transparency_mutation() {
        let snapshot = PathBuf::from(std::env::var_os("SIDEVOICE_CORE_SNAPSHOT").unwrap());
        let manifest =
            read_bounded(&snapshot.join("core-manifest.json"), MAX_MANIFEST_BYTES).unwrap();
        let sidecar = read_bounded(
            &snapshot.join("core-manifest.json.sigstore.json"),
            MAX_SIDECAR_BYTES,
        )
        .unwrap();
        let root = {
            let cache = PathBuf::from(std::env::var_os("SIDEVOICE_CORE_TUF_CACHE").unwrap());
            let client = CoreAcquirer::new(&cache).unwrap();
            client
                .trusted_root(&CancellationToken::new())
                .await
                .unwrap()
        };
        let artifact = snapshot.join("core-manifest.json");
        let wrong_digest = "0".repeat(64);
        let digest_error = verify_file(
            &artifact,
            &sidecar,
            &root,
            &core_policy(),
            Some("nightly"),
            Some(&wrong_digest),
            None,
        )
        .unwrap_err();
        assert!(
            digest_error.contains("manifest digest mismatch"),
            "{digest_error}"
        );

        let mut wire: serde_json::Value = serde_json::from_slice(&sidecar).unwrap();
        wire["verificationMaterial"]["tlogEntries"][0]["inclusionProof"]["rootHash"] =
            serde_json::json!("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=");
        let mutated = serde_json::to_vec(&wire).unwrap();
        let error = verify_file(
            &artifact,
            &mutated,
            &root,
            &core_policy(),
            Some("nightly"),
            Some(&sha256(&manifest)),
            None,
        )
        .unwrap_err();
        assert!(error.contains("cryptographic verification"), "{error}");

        let npm = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/connector/test/fixtures/r4/sigstore-5.0.0.tgz");
        let npm_sidecar = include_bytes!(
            "../../../packages/connector/test/fixtures/r4/sigstore-5.0.0.sigstore.json"
        );
        let identity_error = verify_file(
            &npm,
            npm_sidecar,
            &root,
            &core_policy(),
            Some("nightly"),
            None,
            None,
        )
        .unwrap_err();
        assert!(
            identity_error.contains("cryptographic verification: identity mismatch:"),
            "{identity_error}"
        );
    }

    #[tokio::test]
    #[ignore = "hosted macOS arm64 run uses the frozen nightly snapshot for cleanup refusals"]
    async fn failed_pin_and_cancellation_leave_no_candidate_core() {
        let snapshot = PathBuf::from(std::env::var_os("SIDEVOICE_CORE_SNAPSHOT").unwrap());
        let manifest =
            read_bounded(&snapshot.join("core-manifest.json"), MAX_MANIFEST_BYTES).unwrap();
        let stage_parent = scratch();
        let cache = stage_parent.join("sigstore-tuf");
        let acquirer = CoreAcquirer::new(&cache).unwrap();
        let wrong_pin = CorePin::new("0.1.0", CoreChannel::Nightly, "0".repeat(64)).unwrap();
        let cancel = CancellationToken::new();
        assert!(matches!(
            acquirer
                .stage_snapshot_for_test(&wrong_pin, &stage_parent, &snapshot, &cancel)
                .await,
            Err(CoreAcquisitionError::Authenticity(_))
        ));
        assert!(!stage_parent.read_dir().unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".core-acquisition-")));

        let valid_pin = CorePin::new("0.1.0", CoreChannel::Nightly, sha256(&manifest)).unwrap();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(matches!(
            acquirer
                .stage_snapshot_for_test(&valid_pin, &stage_parent, &snapshot, &cancelled)
                .await,
            Err(CoreAcquisitionError::Cancelled)
        ));
        assert!(!stage_parent.read_dir().unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".core-acquisition-")));
    }

    #[test]
    fn redirect_policy_refuses_http_foreign_hosts_and_credentials() {
        for raw in [
            "http://github.com/sidevoice/sidevoice-core/releases/download/nightly/core.tar.zst",
            "https://github.com.evil.test/sidevoice/sidevoice-core/releases/download/nightly/core.tar.zst",
            "https://user@github.com/sidevoice/sidevoice-core/releases/download/nightly/core.tar.zst",
            "https://github.com:444/sidevoice/sidevoice-core/releases/download/nightly/core.tar.zst",
        ] {
            assert!(!allowed_redirect(&Url::parse(raw).unwrap()), "{raw}");
        }
    }
}
