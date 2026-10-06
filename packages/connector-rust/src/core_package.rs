//! The core this connector's package carries, and how it is put into a release.
//!
//! The release archive (and every package made from it) holds, next to `bin/sidevoice-connector`, the pinned
//! sidevoice-core release archive for the same target, exactly as the core's release published it, and names it in
//! the package's inventory, `connector.json`:
//!
//! ```json
//! {"core": {"version": "0.2.0", "archive": "core/sidevoice-core-0.2.0-linux-x86_64.tar.zst",
//!           "sha256": "…", "size": 25000000, "source_sha": "…"}, "target": "linux-x86_64", "version": "0.7.0", …}
//! ```
//!
//! (`cargo xtask dist` writes it; the core was checked there against its release's attestation, manifest and
//! `SHA256SUMS`.) Nothing is downloaded at install. [`install`] stages that archive into `<release>/core`
//! (the release layout's `CORE_DIRECTORY`): its size and digest must be the inventory's, it is unpacked below one
//! fixed root with only plain files and directories, and what it unpacks to must be exactly the core's own
//! inventory (`native-core.json`: every file with its size and digest, for this target and this source commit).
//! Then the core runs its own self-test (`--self-test <wav> <models>`) from where it was staged; a core that fails
//! it is removed and the failure is keyed (`core.self-test`).
//!
//! The core never runs with a dynamic loader variable of ours ([`environment`]): its libraries are its own.

use crate::messages::Keyed;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The package's inventory, at its root.
pub const INVENTORY: &str = "connector.json";

/// Variables a dynamic loader reads: never passed to the core, whose libraries are its own.
pub const LOADER_VARIABLES: &[&str] = &[
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
    "LD_AUDIT",
    "LD_DEBUG",
    "DYLD_LIBRARY_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_VERSIONED_LIBRARY_PATH",
    "DYLD_ROOT_PATH",
];

/// The core archive's single root directory, and what is below it.
const ARCHIVE_ROOT: &str = "sidevoice-core-rust";
const CORE_INVENTORY: &str = "native-core.json";
const CORE_KIND: &str = "rust-native-v1";
const ENTRYPOINT: &str = "bin/sidevoice-core-rust";
const SELF_TEST_WAV: &str = "checks/detector-16k.wav";
const MODELS: &str = "models";
/// The top-level directories a core may have, and the files it must.
const TOP_LEVEL: &[&str] = &["bin", "lib", "models", "checks", "notices"];
const REQUIRED: &[&str] = &[
    ENTRYPOINT,
    SELF_TEST_WAV,
    "models/silero.onnx",
    "models/silero_vad_16k.bin",
    "models/smart_turn_weights.bin.gz",
];

const MAX_INVENTORY: u64 = 1_000_000;
const MAX_CORE_INVENTORY: u64 = 4_000_000;
const MAX_ARCHIVE: u64 = 250_000_000;
const MAX_UNPACKED: u64 = 1_000_000_000;
const MAX_ENTRIES: usize = 5_000;
const MAX_PATH: usize = 240;
const SELF_TEST_LIMIT: Duration = Duration::from_secs(120);

/// The core a package carries, as its inventory names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackagedCore {
    pub version: String,
    pub target: String,
    /// The archive, an absolute path inside the package.
    pub archive: PathBuf,
    pub sha256: String,
    pub size: u64,
    pub source_sha: String,
}

fn mismatch(detail: impl std::fmt::Display) -> anyhow::Error {
    Keyed::new(
        "core.package-mismatch",
        json!({"detail": detail.to_string()}),
    )
    .into()
}

fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    if !file.metadata()?.is_file() {
        bail!("{} is not a file", path.display());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("{} is larger than {limit} bytes", path.display());
    }
    Ok(bytes)
}

/// The package this binary is part of: the directory above its `bin/`.
pub fn package_root() -> Result<PathBuf> {
    let executable = std::env::current_exe()?.canonicalize()?;
    let bin = executable.parent().context("the executable's directory")?;
    if bin.file_name().and_then(|name| name.to_str()) != Some("bin") {
        bail!(Keyed::new(
            "core.package-missing",
            json!({"detail": format!("{} is not in a package's bin directory", executable.display())})
        ));
    }
    Ok(bin.parent().context("the package's root")?.to_path_buf())
}

/// The core the package at `root` carries, from its inventory: it must be this build's package (version, target)
/// and name a core archive inside it.
pub fn packaged(root: &Path) -> Result<PackagedCore> {
    let path = root.join(INVENTORY);
    if !path.exists() {
        bail!(Keyed::new(
            "core.package-missing",
            json!({"detail": format!("no {}", path.display())})
        ));
    }
    let inventory: Value = serde_json::from_slice(&read_bounded(&path, MAX_INVENTORY)?)
        .map_err(|error| mismatch(format!("{INVENTORY}: {error}")))?;
    let target = crate::identity::target().context("unsupported target")?;
    if inventory["version"] != crate::identity::VERSION || inventory["target"] != target {
        return Err(mismatch(format!(
            "{INVENTORY} is version {} for {}; this connector is {} for {target}",
            inventory["version"],
            inventory["target"],
            crate::identity::VERSION
        )));
    }
    let core = &inventory["core"];
    if core.is_null() {
        bail!(Keyed::new(
            "core.package-missing",
            json!({"detail": format!("{INVENTORY} names no core")})
        ));
    }
    let text = |field: &str| core[field].as_str().unwrap_or("").to_owned();
    let (version, archive, sha256, source_sha) = (
        text("version"),
        text("archive"),
        text("sha256"),
        text("source_sha"),
    );
    let size = core["size"].as_u64().unwrap_or(0);
    let expected_name = format!("core/sidevoice-core-{version}-{target}.tar.zst");
    if version.is_empty()
        || archive != expected_name
        || !lower_hex(&sha256, 64)
        || !lower_hex(&source_sha, 40)
        || size == 0
        || size > MAX_ARCHIVE
    {
        return Err(mismatch(format!(
            "{INVENTORY}: malformed core record {core}"
        )));
    }
    Ok(PackagedCore {
        version,
        target: target.to_owned(),
        archive: root.join(archive),
        sha256,
        size,
        source_sha,
    })
}

/// The core's environment: `base` with no loader variable and its models named.
pub fn environment(base: &BTreeMap<String, String>, core: &Path) -> BTreeMap<String, String> {
    let mut environment = base.clone();
    for name in LOADER_VARIABLES {
        environment.remove(*name);
    }
    environment.insert(
        "RUSTVANI_CACHE_DIR".into(),
        core.join(MODELS).to_string_lossy().into_owned(),
    );
    environment
}

/// Files by their path below the core's root, with their size and digest.
type Files = BTreeMap<String, (u64, String)>;

/// A member's path below the archive root, when it is one a core may have.
fn member(name: &str) -> Option<&str> {
    let relative = name.strip_prefix(ARCHIVE_ROOT)?.strip_prefix('/')?;
    let safe = !relative.is_empty()
        && relative.len() <= MAX_PATH
        && relative.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'@' | b'/' | b'-')
        })
        && relative
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && (relative == CORE_INVENTORY
            || TOP_LEVEL.contains(&relative.split('/').next().unwrap_or("")));
    safe.then_some(relative)
}

/// Unpacks the core archive `bytes` into the fresh directory `into` (the archive root's contents), refusing
/// anything but plain files and directories below the one root. Returns every file written with its size and
/// digest, and the core's inventory.
fn unpack(bytes: &[u8], into: &Path) -> Result<(Files, Vec<u8>)> {
    let decoder = zstd::Decoder::new(bytes).map_err(|error| mismatch(format!("zstd: {error}")))?;
    let mut archive = tar::Archive::new(decoder.take(MAX_UNPACKED + 1_000_000));
    let mut seen = BTreeSet::new();
    let mut written = BTreeMap::new();
    let mut inventory = None;
    let mut total = 0u64;
    let entries = archive
        .entries()
        .map_err(|error| mismatch(format!("tar: {error}")))?;
    for entry in entries {
        let mut entry = entry.map_err(|error| mismatch(format!("tar: {error}")))?;
        let raw = entry
            .path()
            .map_err(|error| mismatch(format!("tar path: {error}")))?
            .to_string_lossy()
            .into_owned();
        let name = raw.trim_end_matches('/').to_owned();
        let kind = entry.header().entry_type();
        if seen.len() >= MAX_ENTRIES || !seen.insert(name.clone()) {
            return Err(mismatch(format!("too many or duplicate entries ({name})")));
        }
        if name == ARCHIVE_ROOT {
            if !kind.is_dir() {
                return Err(mismatch("the archive root is not a directory"));
            }
            continue;
        }
        let relative = member(&name).ok_or_else(|| mismatch(format!("unsafe path {raw:?}")))?;
        let path = into.join(relative);
        if kind.is_dir() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&path)?;
            continue;
        }
        if !kind.is_file() {
            return Err(mismatch(format!("{relative} is not a plain file")));
        }
        let size = entry.header().size()?;
        total += size;
        if total > MAX_UNPACKED {
            return Err(mismatch("the core exceeds its unpacked size bound"));
        }
        if relative == CORE_INVENTORY {
            if size > MAX_CORE_INVENTORY {
                return Err(mismatch(format!("{CORE_INVENTORY} is too large")));
            }
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            inventory = Some(bytes);
            continue;
        }
        let parent = path.parent().context("member parent")?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
        let mode = if relative == ENTRYPOINT { 0o700 } else { 0o600 };
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        let mut hasher = Sha256::new();
        let mut block = [0u8; 65536];
        let mut length = 0u64;
        loop {
            let read = entry.read(&mut block)?;
            if read == 0 {
                break;
            }
            hasher.update(&block[..read]);
            file.write_all(&block[..read])?;
            length += read as u64;
        }
        file.sync_all()?;
        written.insert(
            relative.to_owned(),
            (length, hex::encode(hasher.finalize())),
        );
    }
    let inventory = inventory.ok_or_else(|| mismatch(format!("no {CORE_INVENTORY}")))?;
    Ok((written, inventory))
}

/// Checks what was unpacked against the core's inventory: this target, this source commit, the fixed entrypoint,
/// and exactly its files with their sizes and digests.
fn check_inventory(core: &PackagedCore, inventory: &[u8], written: &Files) -> Result<()> {
    let inventory: Value = serde_json::from_slice(inventory)
        .map_err(|error| mismatch(format!("{CORE_INVENTORY}: {error}")))?;
    if inventory["schema"] != 1
        || inventory["kind"] != CORE_KIND
        || inventory["target"] != core.target.as_str()
        || inventory["source_sha"] != core.source_sha.as_str()
        || inventory["entrypoint"] != ENTRYPOINT
    {
        return Err(mismatch(format!(
            "{CORE_INVENTORY} is not the {CORE_KIND} core {} for {}",
            core.source_sha, core.target
        )));
    }
    let mut listed = BTreeMap::new();
    for record in inventory["files"]
        .as_array()
        .ok_or_else(|| mismatch(format!("{CORE_INVENTORY} lists no files")))?
    {
        let name = record["name"].as_str().unwrap_or("");
        let size = record["size"].as_u64();
        let sha256 = record["sha256"].as_str().unwrap_or("");
        let (Some(size), true) = (size, lower_hex(sha256, 64)) else {
            return Err(mismatch(format!(
                "{CORE_INVENTORY}: malformed record {record}"
            )));
        };
        if listed
            .insert(name.to_owned(), (size, sha256.to_owned()))
            .is_some()
        {
            return Err(mismatch(format!("{CORE_INVENTORY} lists {name} twice")));
        }
    }
    if let Some(missing) = REQUIRED.iter().find(|name| !listed.contains_key(**name)) {
        return Err(mismatch(format!("the core has no {missing}")));
    }
    if &listed != written {
        let differ: Vec<&String> = listed
            .keys()
            .chain(written.keys())
            .filter(|name| listed.get(*name) != written.get(*name))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        return Err(mismatch(format!(
            "the core's files differ from {CORE_INVENTORY}: {differ:?}"
        )));
    }
    Ok(())
}

/// Stages the packaged core into `<release>/core`: the archive must be the inventory's (size, digest), and what it
/// unpacks to the core's own inventory. It is unpacked beside, checked, then renamed into place, so `<release>/core`
/// is there whole or not at all. `release` must be a private directory with no core yet. Returns `<release>/core`.
pub fn stage(core: &PackagedCore, release: &Path) -> Result<PathBuf> {
    if !release.is_absolute() {
        bail!("{} is not an absolute path", release.display());
    }
    crate::secure_fs::private_dir(release)?;
    let destination = release.join(crate::service::layout::CORE_DIRECTORY);
    if fs::symlink_metadata(&destination).is_ok() {
        bail!("{} already exists", destination.display());
    }
    let bytes = match read_bounded(&core.archive, MAX_ARCHIVE) {
        Ok(bytes) => bytes,
        Err(error) if !core.archive.exists() => {
            bail!(Keyed::new(
                "core.package-missing",
                json!({"detail": format!("{error}")})
            ))
        }
        Err(error) => return Err(error),
    };
    let digest = hex::encode(Sha256::digest(&bytes));
    if bytes.len() as u64 != core.size || digest != core.sha256 {
        return Err(mismatch(format!(
            "{} is {} bytes with digest {digest}; the inventory says {} bytes with digest {}",
            core.archive.display(),
            bytes.len(),
            core.size,
            core.sha256
        )));
    }
    let partial = release.join(format!(".core-{}.partial", uuid::Uuid::new_v4()));
    fs::DirBuilder::new().mode(0o700).create(&partial)?;
    let result = (|| -> Result<()> {
        let (written, inventory) = unpack(&bytes, &partial)?;
        check_inventory(core, &inventory, &written)?;
        crate::secure_fs::write_private(&partial.join(CORE_INVENTORY), &inventory)?;
        fs::rename(&partial, &destination)?;
        fs::File::open(release)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&partial);
    }
    result.map(|()| destination)
}

fn self_test_failure(detail: impl std::fmt::Display) -> anyhow::Error {
    let detail: String = detail
        .to_string()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(240)
        .collect();
    Keyed::new("core.self-test", json!({"detail": detail})).into()
}

/// Whether a self-test report is a passing one: the detectors heard the voice in the check file and the turn end,
/// at 16 kHz, and Opus decoded a 20 ms frame (the criteria of the core's own release check).
fn report_passes(report: &Value) -> bool {
    let detectors = &report["detectors"];
    detectors["sample_rate"] == 16_000
        && detectors["frames"]
            .as_u64()
            .is_some_and(|frames| frames > 0)
        && detectors["max_voice_confidence"]
            .as_f64()
            .is_some_and(|confidence| confidence > 0.5)
        && detectors["smart_turn_complete"] == true
        && report["opus_decoded_samples"] == 320
}

/// Runs the staged core's own self-test (`--self-test <core>/checks/detector-16k.wav <core>/models`) and returns
/// its report; a failure, a timeout or a report that does not pass is `core.self-test`.
pub fn self_test(core: &Path) -> Result<Value> {
    let program = core.join(ENTRYPOINT);
    let current: BTreeMap<String, String> = std::env::vars().collect();
    let mut command = Command::new(&program);
    command
        .arg("--self-test")
        .arg(core.join(SELF_TEST_WAV))
        .arg(core.join(MODELS))
        .env_clear()
        .envs(environment(&current, core))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A program just written can be "busy" for a moment while another thread's fork still holds it open.
    let mut attempts = 0;
    let mut child = loop {
        match command.spawn() {
            Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) && attempts < 20 => {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            spawned => {
                break spawned.map_err(|error| {
                    self_test_failure(format!("{}: {error}", program.display()))
                })?
            }
        }
    };
    let mut stdout = child.stdout.take().context("stdout")?;
    let mut stderr = child.stderr.take().context("stderr")?;
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = (&mut stdout).take(1_000_000).read_to_end(&mut bytes);
        bytes
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = (&mut stderr).take(1_000_000).read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + SELF_TEST_LIMIT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(self_test_failure(format!(
                "no answer after {} s",
                SELF_TEST_LIMIT.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = String::from_utf8_lossy(&out.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err.join().unwrap_or_default()).into_owned();
    let last = |text: &str| text.trim().lines().last().unwrap_or("").to_owned();
    if !status.success() {
        // The core says why as one JSON line: `{"error_key", "detail"}`.
        let said = serde_json::from_str::<Value>(&last(&stderr)).ok();
        let detail = said
            .as_ref()
            .and_then(|said| {
                let key = said["error_key"].as_str()?;
                Some(match said["detail"].as_str() {
                    Some(detail) => format!("{key}: {detail}"),
                    None => key.to_owned(),
                })
            })
            .unwrap_or_else(|| format!("{status}: {}", last(&stderr)));
        return Err(self_test_failure(detail));
    }
    let report: Value = serde_json::from_str(&last(&stdout))
        .map_err(|_| self_test_failure(format!("unreadable report {:?}", last(&stdout))))?;
    if !report_passes(&report) {
        return Err(self_test_failure(format!("report {report}")));
    }
    Ok(report)
}

/// What [`install`] staged.
#[derive(Debug)]
pub struct Installed {
    pub core: PackagedCore,
    pub path: PathBuf,
    pub self_test: Value,
}

/// The installer's step for the core: stages the core the package at `package` carries into `<release>/core`
/// and runs its self-test there. A core that fails its self-test is removed again.
pub fn install(package: &Path, release: &Path) -> Result<Installed> {
    let core = packaged(package)?;
    let path = stage(&core, release)?;
    match self_test(&path) {
        Ok(self_test) => Ok(Installed {
            core,
            path,
            self_test,
        }),
        Err(error) => {
            let _ = fs::remove_dir_all(&path);
            Err(error)
        }
    }
}

impl Installed {
    /// The answer of `stage-core --json`.
    pub fn value(&self) -> Value {
        json!({"ok": true, "core": {"version": self.core.version, "target": self.core.target,
            "sha256": self.core.sha256, "size": self.core.size, "source_sha": self.core.source_sha,
            "path": self.path}, "self_test": self.self_test})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "sidevoice-core-package-{label}-{}",
                uuid::Uuid::new_v4()
            ));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const SOURCE: &str = "0123456789abcdef0123456789abcdef01234567";

    /// A core whose program is a script: it passes its self-test, or fails it the way the core does.
    fn core_files(passes: bool) -> Vec<(&'static str, Vec<u8>)> {
        let program = if passes {
            "#!/bin/sh\necho '{\"detectors\":{\"sample_rate\":16000,\"frames\":3,\"max_voice_confidence\":0.9,\
             \"smart_turn_probability\":0.8,\"smart_turn_complete\":true},\"opus_decoded_samples\":320}'\n"
        } else {
            "#!/bin/sh\necho '{\"error_key\":\"rust_core_t0_detector_failed\",\"detail\":\"no model\"}' >&2\nexit 1\n"
        };
        vec![
            (ENTRYPOINT, program.as_bytes().to_vec()),
            (SELF_TEST_WAV, b"RIFF".to_vec()),
            ("models/silero.onnx", b"onnx".to_vec()),
            ("models/silero_vad_16k.bin", b"vad".to_vec()),
            ("models/smart_turn_weights.bin.gz", b"turn".to_vec()),
            ("notices/LICENSE", b"licence".to_vec()),
        ]
    }

    fn core_archive(files: &[(&str, Vec<u8>)], extra: &[(&str, &[u8])]) -> Vec<u8> {
        let mut records: Vec<Value> = files
            .iter()
            .map(|(name, bytes)| {
                json!({"name": name, "size": bytes.len(), "sha256": hex::encode(Sha256::digest(bytes))})
            })
            .collect();
        records.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        let target = crate::identity::target().unwrap();
        let inventory = json!({"schema": 1, "kind": CORE_KIND, "target": target, "source_sha": SOURCE,
            "entrypoint": ENTRYPOINT, "files": records});
        let mut builder = tar::Builder::new(Vec::new());
        let mut add = |name: String, bytes: &[u8], mode: u32| {
            let mut header = tar::Header::new_ustar();
            header.set_mode(mode);
            header.set_size(bytes.len() as u64);
            header.set_entry_type(tar::EntryType::Regular);
            // The name as given, even a hostile one (`set_path` would refuse `..`).
            let field = &mut header.as_ustar_mut().unwrap().name;
            field[..name.len()].copy_from_slice(name.as_bytes());
            header.set_cksum();
            builder.append(&header, bytes).unwrap();
        };
        let inventory = serde_json::to_vec(&inventory).unwrap();
        add(
            format!("{ARCHIVE_ROOT}/{CORE_INVENTORY}"),
            &inventory,
            0o644,
        );
        for (name, bytes) in files {
            add(format!("{ARCHIVE_ROOT}/{name}"), bytes, 0o755);
        }
        for (name, bytes) in extra {
            add((*name).to_owned(), bytes, 0o644);
        }
        zstd::encode_all(builder.into_inner().unwrap().as_slice(), 3).unwrap()
    }

    /// A package holding `archive` as its core, with the inventory naming `listed` as its digest.
    fn package(scratch: &Path, archive: &[u8], listed: Option<String>) -> PathBuf {
        let root = scratch.join("package");
        let target = crate::identity::target().unwrap();
        let name = format!("core/sidevoice-core-0.2.0-{target}.tar.zst");
        fs::create_dir_all(root.join("core")).unwrap();
        fs::write(root.join(&name), archive).unwrap();
        let inventory = json!({"version": crate::identity::VERSION, "target": target,
            "core": {"version": "0.2.0", "archive": name, "size": archive.len(), "source_sha": SOURCE,
                     "sha256": listed.unwrap_or_else(|| hex::encode(Sha256::digest(archive)))}});
        fs::write(root.join(INVENTORY), inventory.to_string()).unwrap();
        root
    }

    fn release(scratch: &Path) -> PathBuf {
        let release = scratch.join("release");
        fs::DirBuilder::new().mode(0o700).create(&release).unwrap();
        release
    }

    fn key(error: &anyhow::Error) -> &'static str {
        crate::messages::keyed(error).key
    }

    #[test]
    fn the_packaged_core_is_staged_into_the_release_and_passes_its_self_test() {
        let scratch = Scratch::new("stage");
        let root = package(&scratch.0, &core_archive(&core_files(true), &[]), None);
        let release = release(&scratch.0);
        let installed = install(&root, &release).unwrap();
        assert_eq!(installed.path, release.join("core"));
        assert_eq!(installed.core.version, "0.2.0");
        assert_eq!(installed.self_test["opus_decoded_samples"], 320);
        let program = release.join(crate::service::layout::CORE_ENTRYPOINT);
        let mode = fs::metadata(&program).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "only the entrypoint is executable");
        assert!(release.join("core/native-core.json").is_file());
        let value = installed.value();
        assert_eq!(value["ok"], true);
        assert_eq!(value["core"]["sha256"], installed.core.sha256);
        let leftovers: Vec<_> = fs::read_dir(&release).unwrap().collect();
        assert_eq!(
            leftovers.len(),
            1,
            "nothing but core/ is left: {leftovers:?}"
        );

        let again = stage(&installed.core, &release).unwrap_err();
        assert!(again.to_string().contains("already exists"), "{again}");
    }

    #[test]
    fn an_archive_whose_digest_is_not_the_inventorys_is_refused() {
        let scratch = Scratch::new("digest");
        let root = package(
            &scratch.0,
            &core_archive(&core_files(true), &[]),
            Some("0".repeat(64)),
        );
        let release = release(&scratch.0);
        let error = install(&root, &release).unwrap_err();
        assert_eq!(key(&error), "core.package-mismatch", "{error}");
        assert_eq!(fs::read_dir(&release).unwrap().count(), 0, "nothing staged");
    }

    #[test]
    fn a_core_that_differs_from_its_own_inventory_is_refused() {
        let scratch = Scratch::new("inventory");
        // Listed with one model, shipped with another.
        let archive = core_archive_mismatched(
            &core_files(true),
            &[("models/silero.onnx", b"another model".to_vec())],
        );
        let root = package(&scratch.0, &archive, None);
        let error = install(&root, &release(&scratch.0)).unwrap_err();
        assert_eq!(key(&error), "core.package-mismatch", "{error}");
        assert!(error.to_string().contains("models/silero.onnx"), "{error}");
    }

    /// An archive whose inventory lists `listed` while it holds `held` in place of the same names.
    fn core_archive_mismatched(listed: &[(&str, Vec<u8>)], held: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let honest = core_archive(listed, &[]);
        let decoded = zstd::decode_all(honest.as_slice()).unwrap();
        let mut archive = tar::Archive::new(decoded.as_slice());
        let mut builder = tar::Builder::new(Vec::new());
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let name = entry.path().unwrap().to_string_lossy().into_owned();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            if let Some((_, replacement)) = held
                .iter()
                .find(|(file, _)| name == format!("{ARCHIVE_ROOT}/{file}"))
            {
                bytes = replacement.clone();
            }
            let mut header = entry.header().clone();
            header.set_size(bytes.len() as u64);
            header.set_cksum();
            builder.append(&header, bytes.as_slice()).unwrap();
        }
        zstd::encode_all(builder.into_inner().unwrap().as_slice(), 3).unwrap()
    }

    #[test]
    fn a_path_outside_the_core_is_refused() {
        let scratch = Scratch::new("escape");
        let archive = core_archive(
            &core_files(true),
            &[("sidevoice-core-rust/../escape", b"x")],
        );
        let root = package(&scratch.0, &archive, None);
        let release = release(&scratch.0);
        let error = install(&root, &release).unwrap_err();
        assert_eq!(key(&error), "core.package-mismatch", "{error}");
        assert!(!scratch.0.join("escape").exists());
        assert_eq!(fs::read_dir(&release).unwrap().count(), 0, "nothing staged");
        assert!(member("sidevoice-core-rust/bin/x").is_some());
        assert!(member("sidevoice-core-rust/etc/x").is_none());
        assert!(member("sidevoice-core-rust//x").is_none());
        assert!(member("elsewhere/bin/x").is_none());
    }

    #[test]
    fn a_failed_self_test_is_keyed_and_leaves_no_core() {
        let scratch = Scratch::new("self-test");
        let root = package(&scratch.0, &core_archive(&core_files(false), &[]), None);
        let release = release(&scratch.0);
        let error = install(&root, &release).unwrap_err();
        let keyed = crate::messages::keyed(&error);
        assert_eq!(keyed.key, "core.self-test", "{error}");
        assert_eq!(
            keyed.params["detail"],
            "rust_core_t0_detector_failed: no model"
        );
        assert!(!release.join("core").exists(), "a failing core is removed");
    }

    #[test]
    fn a_package_without_a_core_or_for_another_build_is_refused() {
        let scratch = Scratch::new("missing");
        let error = packaged(&scratch.0).unwrap_err();
        assert_eq!(key(&error), "core.package-missing", "{error}");
        fs::write(
            scratch.0.join(INVENTORY),
            json!({"version": "0.0.1", "target": crate::identity::target()}).to_string(),
        )
        .unwrap();
        assert_eq!(
            key(&packaged(&scratch.0).unwrap_err()),
            "core.package-mismatch"
        );
        fs::write(
            scratch.0.join(INVENTORY),
            json!({"version": crate::identity::VERSION, "target": crate::identity::target()})
                .to_string(),
        )
        .unwrap();
        assert_eq!(
            key(&packaged(&scratch.0).unwrap_err()),
            "core.package-missing"
        );
    }

    #[test]
    fn the_core_runs_with_no_loader_variable_and_its_models_named() {
        let base = BTreeMap::from([
            ("LD_PRELOAD".to_owned(), "/x.so".to_owned()),
            ("DYLD_INSERT_LIBRARIES".to_owned(), "/x.dylib".to_owned()),
            ("HOME".to_owned(), "/h".to_owned()),
        ]);
        let environment = environment(&base, Path::new("/r/core"));
        assert_eq!(
            environment.keys().collect::<Vec<_>>(),
            ["HOME", "RUSTVANI_CACHE_DIR"]
        );
        assert_eq!(environment["RUSTVANI_CACHE_DIR"], "/r/core/models");
    }

    #[test]
    fn only_a_passing_report_passes() {
        let report: Value = serde_json::from_str(
            "{\"detectors\":{\"sample_rate\":16000,\"frames\":3,\"max_voice_confidence\":0.9,\
             \"smart_turn_complete\":true},\"opus_decoded_samples\":320}",
        )
        .unwrap();
        assert!(report_passes(&report));
        let mut quiet = report.clone();
        quiet["detectors"]["max_voice_confidence"] = json!(0.1);
        assert!(!report_passes(&quiet));
        let mut silent = report;
        silent["opus_decoded_samples"] = json!(0);
        assert!(!report_passes(&silent));
    }
}
