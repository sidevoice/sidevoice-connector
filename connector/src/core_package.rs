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
//! The core is a `rust-native-v2` archive: its program in `bin/` and its licence notices in `notices/`, nothing
//! else.
//!
//! The core never runs with a dynamic loader variable of ours ([`environment`]): it loads only the system's
//! libraries.

use crate::messages::Keyed;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// The package's inventory, at its root.
pub const INVENTORY: &str = "connector.json";

/// Variables a dynamic loader reads to load a library from elsewhere: never passed to the core.
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
const CORE_KIND: &str = "rust-native-v2";
const ENTRYPOINT: &str = "bin/sidevoice-core-rust";
/// The top-level directories a core may have; its entrypoint is the one file it must.
const TOP_LEVEL: &[&str] = &["bin", "notices"];

const MAX_INVENTORY: u64 = 1_000_000;
const MAX_CORE_INVENTORY: u64 = 4_000_000;
const MAX_ARCHIVE: u64 = 250_000_000;
const MAX_UNPACKED: u64 = 1_000_000_000;
const MAX_ENTRIES: usize = 5_000;
const MAX_PATH: usize = 240;

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

/// The core the package at `root` carries, from its inventory: it must be the package of connector `version` for
/// this target, and name a core archive inside it.
pub fn packaged_as(root: &Path, version: &str) -> Result<PackagedCore> {
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
    if inventory["version"] != version || inventory["target"] != target {
        return Err(mismatch(format!(
            "{INVENTORY} is version {} for {}; this connector is {version} for {target}",
            inventory["version"], inventory["target"],
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

/// The core's environment: `base` with no loader variable.
pub fn environment(base: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut environment = base.clone();
    for name in LOADER_VARIABLES {
        environment.remove(*name);
    }
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
    if !listed.contains_key(ENTRYPOINT) {
        return Err(mismatch(format!("the core has no {ENTRYPOINT}")));
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

/// Whether the core staged at `staged` (`<release>/core`) is still the one `core` names: its saved inventory is that
/// core's, and its files are exactly the inventory's, with their sizes and digests. A release whose core is not is
/// staged again by the installer.
pub fn staged_intact(core: &PackagedCore, staged: &Path) -> Result<()> {
    let inventory = read_bounded(&staged.join(CORE_INVENTORY), MAX_CORE_INVENTORY)?;
    let mut files = Files::new();
    staged_files(staged, staged, &mut files)?;
    check_inventory(core, &inventory, &files)
}

/// Every plain file below `dir` but the inventory, by its path below `root`, with its size and digest.
fn staged_files(root: &Path, dir: &Path, files: &mut Files) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(root)?.to_string_lossy().into_owned();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            staged_files(root, &path, files)?;
            continue;
        }
        if !kind.is_file() {
            return Err(mismatch(format!("{relative} is not a plain file")));
        }
        if relative == CORE_INVENTORY {
            continue;
        }
        if files.len() >= MAX_ENTRIES {
            return Err(mismatch("the staged core has too many files"));
        }
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        let mut hasher = Sha256::new();
        let length = std::io::copy(&mut file, &mut hasher)?;
        files.insert(relative, (length, hex::encode(hasher.finalize())));
    }
    Ok(())
}

/// What [`install`] staged.
#[derive(Debug)]
pub struct Installed {
    pub core: PackagedCore,
    pub path: PathBuf,
}

/// The installer's step for the core: stages the core the package at `package` carries into `<release>/core`.
pub fn install(package: &Path, release: &Path) -> Result<Installed> {
    install_as(package, release, crate::identity::VERSION)
}

/// [`install`], for a package of connector version `version`.
pub fn install_as(package: &Path, release: &Path, version: &str) -> Result<Installed> {
    let core = packaged_as(package, version)?;
    let path = stage(&core, release)?;
    Ok(Installed { core, path })
}

impl Installed {
    /// The answer of `stage-core --json`.
    pub fn value(&self) -> Value {
        json!({"ok": true, "core": {"version": self.core.version, "target": self.core.target,
            "sha256": self.core.sha256, "size": self.core.size, "source_sha": self.core.source_sha,
            "path": self.path}})
    }
}

#[cfg(test)]
pub(crate) mod tests {
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

    /// A core: its program (a script) and a notice.
    pub(crate) fn core_files() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            (ENTRYPOINT, b"#!/bin/sh\nexit 0\n".to_vec()),
            ("notices/LICENSE", b"licence".to_vec()),
        ]
    }

    pub(crate) fn core_archive(files: &[(&str, Vec<u8>)], extra: &[(&str, &[u8])]) -> Vec<u8> {
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
    fn the_packaged_core_is_staged_into_the_release() {
        let scratch = Scratch::new("stage");
        let root = package(&scratch.0, &core_archive(&core_files(), &[]), None);
        let release = release(&scratch.0);
        let installed = install(&root, &release).unwrap();
        assert_eq!(installed.path, release.join("core"));
        assert_eq!(installed.core.version, "0.2.0");
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
            &core_archive(&core_files(), &[]),
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
        // Listed with one notice, shipped with another.
        let archive = core_archive_mismatched(
            &core_files(),
            &[("notices/LICENSE", b"another notice".to_vec())],
        );
        let root = package(&scratch.0, &archive, None);
        let error = install(&root, &release(&scratch.0)).unwrap_err();
        assert_eq!(key(&error), "core.package-mismatch", "{error}");
        assert!(error.to_string().contains("notices/LICENSE"), "{error}");
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
        let archive = core_archive(&core_files(), &[("sidevoice-core-rust/../escape", b"x")]);
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
    fn a_package_without_a_core_or_for_another_build_is_refused() {
        let scratch = Scratch::new("missing");
        let error = packaged_as(&scratch.0, crate::identity::VERSION).unwrap_err();
        assert_eq!(key(&error), "core.package-missing", "{error}");
        fs::write(
            scratch.0.join(INVENTORY),
            json!({"version": "0.0.1", "target": crate::identity::target()}).to_string(),
        )
        .unwrap();
        assert_eq!(
            key(&packaged_as(&scratch.0, crate::identity::VERSION).unwrap_err()),
            "core.package-mismatch"
        );
        fs::write(
            scratch.0.join(INVENTORY),
            json!({"version": crate::identity::VERSION, "target": crate::identity::target()})
                .to_string(),
        )
        .unwrap();
        assert_eq!(
            key(&packaged_as(&scratch.0, crate::identity::VERSION).unwrap_err()),
            "core.package-missing"
        );
    }

    #[test]
    fn the_core_runs_with_no_loader_variable() {
        let base = BTreeMap::from([
            ("LD_PRELOAD".to_owned(), "/x.so".to_owned()),
            ("DYLD_INSERT_LIBRARIES".to_owned(), "/x.dylib".to_owned()),
            ("HOME".to_owned(), "/h".to_owned()),
        ]);
        assert_eq!(environment(&base).keys().collect::<Vec<_>>(), ["HOME"]);
    }

    #[test]
    fn only_a_rust_native_v2_core_with_bin_and_notices_is_taken() {
        let scratch = Scratch::new("kind");
        // A core of the old layout: its kind and its voice pipeline's files are refused.
        let old = core_archive_kind(&core_files(), "rust-native-v1");
        let root = package(&scratch.0, &old, None);
        let error = install(&root, &release(&scratch.0)).unwrap_err();
        assert_eq!(key(&error), "core.package-mismatch", "{error}");
        assert!(error.to_string().contains("rust-native-v2"), "{error}");
        for directory in ["models", "checks", "lib"] {
            assert!(
                member(&format!("sidevoice-core-rust/{directory}/x")).is_none(),
                "{directory}"
            );
        }
        let mut files = core_files();
        files.retain(|(name, _)| *name != ENTRYPOINT);
        let scratch = Scratch::new("entrypoint");
        let root = package(&scratch.0, &core_archive(&files, &[]), None);
        let error = install(&root, &release(&scratch.0)).unwrap_err();
        assert!(error.to_string().contains(ENTRYPOINT), "{error}");
    }

    /// An honest archive of `files` whose inventory names `kind`.
    fn core_archive_kind(files: &[(&str, Vec<u8>)], kind: &str) -> Vec<u8> {
        let honest = core_archive(files, &[]);
        let decoded = zstd::decode_all(honest.as_slice()).unwrap();
        let mut archive = tar::Archive::new(decoded.as_slice());
        let mut builder = tar::Builder::new(Vec::new());
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let name = entry.path().unwrap().to_string_lossy().into_owned();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            if name == format!("{ARCHIVE_ROOT}/{CORE_INVENTORY}") {
                let mut inventory: Value = serde_json::from_slice(&bytes).unwrap();
                inventory["kind"] = json!(kind);
                bytes = serde_json::to_vec(&inventory).unwrap();
            }
            let mut header = entry.header().clone();
            header.set_size(bytes.len() as u64);
            header.set_cksum();
            builder.append(&header, bytes.as_slice()).unwrap();
        }
        zstd::encode_all(builder.into_inner().unwrap().as_slice(), 3).unwrap()
    }
}
