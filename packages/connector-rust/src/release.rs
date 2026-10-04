//! Native release storage. Pointer replacement is the sole selection commit point.
use anyhow::{Context, Result};
use fs2::FileExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{symlink, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
use tokio::time::{sleep, Duration};

#[derive(Clone, Debug)]
pub struct Paths {
    pub home: PathBuf,
    pub data: PathBuf,
    pub root: PathBuf,
    pub config: PathBuf,
}
impl Paths {
    pub fn environment() -> Result<Self> {
        let home = PathBuf::from(std::env::var_os("HOME").context("HOME")?);
        let data = std::env::var_os("SIDEVOICE_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".sidevoice"));
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let default_root = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"))
            .join("sidevoice");
        let record = read_json(&data.join("install.json"))?;
        let root = record["releases"]
            .as_str()
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() && p.file_name().is_some_and(|v| v == "sidevoice"))
            .unwrap_or(default_root);
        for path in [&home, &data, &config, &root] {
            if !path.is_absolute() {
                return Err(crate::release::refusal(
                    "control.absolute-installation-path-required",
                    json!({}),
                ));
            }
        }
        Ok(Self {
            home,
            data,
            root,
            config,
        })
    }
    pub fn from_profile(profile: &crate::proof::Profile) -> Self {
        Self {
            home: profile.home.clone(),
            data: profile.data.clone(),
            root: profile.root.clone(),
            config: profile.xdg_config.clone(),
        }
    }
    pub fn prepare(&self) -> Result<()> {
        for path in [
            &self.data,
            &self.data.join("core"),
            &self.root,
            &self.root.join("releases"),
        ] {
            private_directory(path)?;
        }
        Ok(())
    }
    pub fn selected(&self, name: &str) -> Result<Option<Value>> {
        if !["current", "previous", "verified"].contains(&name) {
            return Err(crate::release::refusal(
                "control.invalid-selection-name",
                json!({}),
            ));
        }
        let path = self.root.join(name);
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if !meta.file_type().is_symlink() || meta.uid() != unsafe { libc::geteuid() } {
            return Err(crate::release::refusal(
                "control.unsafe-release-pointer",
                json!({}),
            ));
        }
        let target = fs::read_link(&path)?;
        let id = target
            .strip_prefix("releases")
            .ok()
            .and_then(|p| p.to_str())
            .filter(|v| valid_id(v))
            .context("invalid release pointer")?;
        private_directory(&self.root.join("releases"))?;
        let dir = self.root.join("releases").join(id);
        crate::proof::private_dir(&dir)?;
        let release = read_json(&dir.join("release.json"))?;
        if release["id"].as_str() != Some(id) {
            return Err(crate::release::refusal(
                "control.release-record-differs-from-pointer",
                json!({}),
            ));
        }
        Ok(Some(release))
    }
    pub fn point(&self, name: &str, id: &str) -> Result<()> {
        if !valid_id(id) || !["current", "previous", "verified"].contains(&name) {
            return Err(crate::release::refusal(
                "control.invalid-release-pointer",
                json!({}),
            ));
        }
        crate::proof::private_dir(&self.root)?;
        let temp = self
            .root
            .join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
        symlink(Path::new("releases").join(id), &temp)?;
        let result = fs::rename(&temp, self.root.join(name));
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result?;
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }
    pub async fn lock(&self) -> Result<File> {
        private_directory(&self.data)?;
        let path = self.data.join("install.lock");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        crate::proof::private_file(&path)?;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => break,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    sleep(Duration::from_millis(100)).await
                }
                Err(e) => return Err(e.into()),
            }
        }
        file.set_len(0)?;
        file.write_all(
            json!({"pid":std::process::id(),"kind":"install","start":null})
                .to_string()
                .as_bytes(),
        )?;
        file.sync_all()?;
        Ok(file)
    }
}
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() < 240
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}
pub fn private_directory(path: &Path) -> Result<()> {
    directory(path, true)
}
pub fn directory(path: &Path, private: bool) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if !path.is_absolute() {
        return Err(crate::release::refusal(
            "control.private-directory-must-be-absolute",
            json!({}),
        ));
    }
    let mut current = PathBuf::from("/");
    for component in path.components() {
        if matches!(component, std::path::Component::RootDir) {
            continue;
        }
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(crate::release::refusal(
                "control.unsafe-private-directory-path",
                json!({}),
            ));
        }
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(mut meta) => {
                if meta.file_type().is_symlink()
                    && meta.uid() == 0
                    && [Path::new("/var"), Path::new("/tmp"), Path::new("/etc")]
                        .contains(&current.as_path())
                {
                    meta = fs::metadata(&current)?;
                }
                if !meta.is_dir() || meta.file_type().is_symlink() {
                    return Err(crate::release::refusal(
                        "control.private-directory-crosses-a-link",
                        json!({}),
                    ));
                }
                if meta.uid() != 0 && meta.uid() != unsafe { libc::geteuid() } {
                    return Err(crate::release::refusal(
                        "control.private-directory-has-foreign-owner",
                        json!({}),
                    ));
                }
                // System temporary roots are sticky; user-owned descendants remain private.
                if meta.mode() & 0o022 != 0 && meta.mode() & 0o1000 == 0 {
                    return Err(crate::release::refusal(
                        "control.private-directory-has-writable-ancestor",
                        json!({}),
                    ));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::DirBuilder::new().mode(0o700).create(&current)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    if private {
        crate::proof::private_dir(path)
    } else {
        crate::proof::validate_user_directory(path).map(|_| ())
    }
}
pub fn read_json(path: &Path) -> Result<Value> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Value::Null),
        Err(e) => return Err(e.into()),
        Ok(_) => {}
    }
    crate::proof::private_file(path)?;
    if fs::metadata(path)?.len() > 8 * 1024 * 1024 {
        return Err(crate::release::refusal(
            "control.private-record-too-large",
            json!({}),
        ));
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
pub fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
pub fn file_digest(path: &Path) -> Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut b = [0u8; 65536];
    loop {
        let n = f.read(&mut b)?;
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
    }
    Ok(hex::encode(h.finalize()))
}
pub fn write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let parent = path.parent().context("file parent")?;
    directory(parent, false)?;
    let temp = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
pub fn write_json(path: &Path, v: &Value) -> Result<()> {
    write(path, &serde_json::to_vec(v)?, 0o600)
}
pub fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
pub fn decide(current: Option<&Value>, next: &Value) -> &'static str {
    let Some(current) = current else {
        return "install";
    };
    let version = |v: &Value| -> Vec<u64> {
        v["connector"]
            .as_str()
            .unwrap_or("0")
            .split(['.', '-', '+'])
            .take(3)
            .map(|s| s.parse().unwrap_or(0))
            .collect()
    };
    match version(next).cmp(&version(current)) {
        std::cmp::Ordering::Greater => return "upgrade",
        std::cmp::Ordering::Less => return "noop",
        _ => {}
    }
    if next["channel"] == "nightly"
        && current["channel"] == "nightly"
        && next["build_seq"].as_u64() < current["build_seq"].as_u64()
    {
        return "noop";
    }
    if next["pair_id"] != current["pair_id"]
        || next["format"] != current["format"]
        || next["distributor_sha256"] != current["distributor_sha256"]
        || next["build_seq"].as_u64() > current["build_seq"].as_u64()
    {
        "upgrade"
    } else {
        "noop"
    }
}

/// Extract only inventoried regular files. Neither archive links nor arbitrary metadata are applied.
pub fn unpack_core(archive: &[u8], manifest: &Value, target: &str, root: &Path) -> Result<()> {
    let bundle = &manifest["bundles"][target];
    if archive.len() > 250_000_000
        || bundle["size"].as_u64() != Some(archive.len() as u64)
        || bundle["sha256"].as_str() != Some(digest(archive).as_str())
    {
        return Err(authenticity("sha256"));
    }
    let decoder =
        zstd::stream::read::Decoder::new(archive).map_err(|_| authenticity("archive-type"))?;
    let mut tar = tar::Archive::new(decoder.take(1_010_000_000));
    private_directory(root)?;
    let mut inventory = None;
    let mut paths_seen = std::collections::BTreeSet::new();
    let mut dirs_seen = std::collections::BTreeSet::new();
    let mut seen = std::collections::BTreeMap::new();
    let mut total = 0u64;
    let mut count = 0usize;
    for entry in tar.entries().map_err(|_| authenticity("archive-type"))? {
        let mut entry = entry.map_err(|_| authenticity("archive-type"))?;
        count += 1;
        if count > 2000 {
            return Err(authenticity("archive-size"));
        }
        let kind = entry.header().entry_type();
        let raw = entry.path_bytes().into_owned();
        let raw = std::str::from_utf8(&raw).map_err(|_| authenticity("archive-path"))?;
        let normalized = if kind.is_dir() {
            raw.strip_suffix('/').unwrap_or(raw)
        } else {
            raw
        };
        let mut parts = normalized.split('/');
        if parts.next() != Some("sidevoice-core-rust")
            || normalized.len() > 260
            || normalized
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            || !normalized
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._+@/-".contains(&b))
        {
            return Err(authenticity("archive-path"));
        }
        if !paths_seen.insert(normalized.to_string()) {
            return Err(authenticity("archive-path"));
        }
        let name = normalized
            .strip_prefix("sidevoice-core-rust/")
            .unwrap_or("");
        let relative = Path::new(name);
        if name.is_empty() {
            if !kind.is_dir() {
                return Err(authenticity("archive-type"));
            }
            dirs_seen.insert(normalized.to_string());
            continue;
        }
        if name != "native-core.json"
            && !matches!(
                relative
                    .components()
                    .next()
                    .and_then(|c| c.as_os_str().to_str()),
                Some("bin" | "lib" | "models" | "checks" | "notices")
            )
        {
            return Err(authenticity("archive-path"));
        }
        let file = root.join(relative);
        if kind.is_dir() {
            dirs_seen.insert(normalized.to_string());
            private_directory(&file)?;
            continue;
        }
        if !kind.is_file() {
            return Err(authenticity("archive-type"));
        }
        let size = entry.size();
        total = total
            .checked_add(size)
            .ok_or_else(|| authenticity("archive-size"))?;
        if size > 500_000_000 || total > 1_000_000_000 {
            return Err(authenticity("archive-size"));
        }
        if name == "native-core.json" {
            if inventory.is_some() || size > 4_000_000 {
                return Err(authenticity("archive-size"));
            }
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .map_err(|_| authenticity("archive-size"))?;
            let value: Value =
                serde_json::from_slice(&bytes).map_err(|_| authenticity("manifest"))?;
            let mut canonical = serde_json::to_vec(&value)?;
            canonical.push(b'\n');
            if canonical != bytes {
                return Err(authenticity("manifest"));
            }
            write(&file, &bytes, 0o600)?;
            inventory = Some(value);
            continue;
        }
        if seen.contains_key(name) {
            return Err(authenticity("archive-path"));
        }
        private_directory(file.parent().context("archive parent")?)?;
        let executable = name.starts_with("bin/");
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(if executable { 0o700 } else { 0o600 })
            .custom_flags(libc::O_NOFOLLOW)
            .open(&file)?;
        let mut h = Sha256::new();
        let mut bytes = [0u8; 65536];
        let mut copied = 0u64;
        loop {
            let n = entry
                .read(&mut bytes)
                .map_err(|_| authenticity("archive-size"))?;
            if n == 0 {
                break;
            }
            out.write_all(&bytes[..n])?;
            h.update(&bytes[..n]);
            copied += n as u64;
        }
        out.sync_all()?;
        if copied != size {
            return Err(authenticity("archive-size"));
        }
        seen.insert(name.to_string(), (size, hex::encode(h.finalize())));
    }
    let inventory = inventory.ok_or_else(|| authenticity("archive-inventory"))?;
    if !exact_keys(
        &inventory,
        &[
            "schema",
            "kind",
            "target",
            "source_sha",
            "entrypoint",
            "files",
        ],
    ) || inventory["schema"] != 1
        || inventory["kind"] != "rust-native-v1"
        || inventory["target"] != target
        || inventory["source_sha"] != manifest["source_sha"]
        || inventory["entrypoint"] != "bin/sidevoice-core-rust"
    {
        return Err(authenticity("archive-inventory"));
    }
    let files = inventory["files"]
        .as_array()
        .ok_or_else(|| authenticity("archive-inventory"))?;
    if files.len() != seen.len() {
        return Err(authenticity("archive-inventory"));
    }
    let mut expected_dirs = std::collections::BTreeSet::from(["sidevoice-core-rust".to_string()]);
    let mut previous = "";
    for file in files {
        if !exact_keys(file, &["name", "size", "sha256"]) {
            return Err(authenticity("archive-inventory"));
        }
        let name = file["name"]
            .as_str()
            .ok_or_else(|| authenticity("archive-inventory"))?;
        if name <= previous {
            return Err(authenticity("archive-inventory"));
        }
        previous = name;
        let components: Vec<_> = name.split('/').collect();
        for end in 1..components.len() {
            expected_dirs.insert(format!(
                "sidevoice-core-rust/{}",
                components[..end].join("/")
            ));
        }
        let (size, sha) = seen
            .get(name)
            .ok_or_else(|| authenticity("archive-inventory"))?;
        if file["size"].as_u64() != Some(*size) || file["sha256"].as_str() != Some(sha) {
            return Err(authenticity("archive-inventory"));
        }
    }
    for required in [
        "bin/sidevoice-core-rust",
        "checks/detector-16k.wav",
        "models/silero.onnx",
        "models/silero_vad_16k.bin",
        "models/smart_turn_weights.bin.gz",
    ] {
        if !seen.contains_key(required) {
            return Err(authenticity("archive-inventory"));
        }
    }
    if !seen.keys().any(|k| k.starts_with("notices/")) {
        return Err(authenticity("archive-inventory"));
    }
    if dirs_seen != expected_dirs {
        return Err(authenticity("archive-inventory"));
    }
    sync_tree(root)?;
    Ok(())
}
pub fn sync_tree(root: &Path) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let e = entry?;
        if e.file_type()?.is_dir() {
            sync_tree(&e.path())?;
        } else if e.file_type()?.is_file() {
            File::open(e.path())?.sync_all()?;
        } else {
            return Err(crate::release::refusal(
                "control.release-contains-unsupported-links",
                json!({}),
            ));
        }
    }
    File::open(root)?.sync_all()?;
    Ok(())
}

#[derive(Debug)]
pub struct ControlError {
    pub key: &'static str,
    pub params: Value,
}
impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&crate::agents::message(self.key, &self.params))
    }
}
impl std::error::Error for ControlError {}
pub fn refusal(key: &'static str, params: Value) -> anyhow::Error {
    ControlError { key, params }.into()
}

/// Remove interrupted staging only while the caller holds install.lock.
pub fn remove_leftovers(p: &Paths) -> Result<()> {
    for entry in fs::read_dir(p.root.join("releases"))? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some((id, suffix)) = name.rsplit_once(".tmp-") {
            if valid_id(id)
                && !suffix.is_empty()
                && suffix.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
            {
                crate::proof::private_dir(&entry.path())?;
                fs::remove_dir_all(entry.path())?;
            }
        }
    }
    for entry in fs::read_dir(&p.root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if [".current.", ".previous.", ".verified."]
            .iter()
            .any(|prefix| name.starts_with(prefix))
            && name.ends_with(".tmp")
        {
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() && metadata.uid() == unsafe { libc::geteuid() } {
                fs::remove_file(entry.path())?;
            }
        }
    }
    Ok(())
}
pub fn prune(p: &Paths) -> Result<()> {
    let mut kept = std::collections::BTreeSet::new();
    for name in ["current", "previous", "verified"] {
        if let Some(v) = p.selected(name)? {
            kept.insert(v["id"].as_str().context("release id")?.to_owned());
        }
    }
    if kept.is_empty() {
        return Ok(());
    }
    for entry in fs::read_dir(p.root.join("releases"))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if kept.contains(&name) || !valid_id(&name) || !entry.file_type()?.is_dir() {
            continue;
        }
        let record = read_json(&entry.path().join("release.json"))?;
        if record["id"].as_str() == Some(name.as_str()) {
            crate::proof::private_dir(&entry.path())?;
            fs::remove_dir_all(entry.path())?;
        }
    }
    cleanup_legacy_runtimes(p, false)?;
    Ok(())
}

pub fn authenticity(check: &'static str) -> anyhow::Error {
    refusal("install.authenticity", json!({"check":check}))
}
pub fn exact_keys(value: &Value, keys: &[&str]) -> bool {
    value.as_object().is_some_and(|object| {
        object.len() == keys.len() && keys.iter().all(|key| object.contains_key(*key))
    })
}

/// Legacy runtimes carry an installer-written identity marker. Unmarked siblings are not ours to reclaim.
fn owned_legacy_runtimes(p: &Paths) -> Result<Vec<(String, PathBuf)>> {
    let root = p.data.join("core-runtime");
    match fs::symlink_metadata(&root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    crate::proof::private_dir(&root)?;
    let mut owned = Vec::new();
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !valid_id(&name) || !entry.file_type()?.is_dir() {
            continue;
        }
        if crate::proof::private_dir(&entry.path()).is_err() {
            continue;
        }
        let marker = |name: &str| -> Option<Value> {
            read_json(&entry.path().join(name))
                .ok()
                .filter(|value| !value.is_null())
        };
        let wheel = marker("installed.json").is_some_and(|record| {
            record["id"].as_str() == Some(name.as_str())
                && record["version"].as_str().is_some_and(|v| !v.is_empty())
                && record["spec"].as_str().is_some_and(|v| !v.is_empty())
        });
        let bundle = marker(".sidevoice-runtime.json").is_some_and(|record| {
            record["kind"] == "bundle"
                && record["id"].as_str() == Some(name.as_str())
                && record["core"].as_str().is_some_and(|v| !v.is_empty())
                && record["sha256"]
                    .as_str()
                    .is_some_and(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
        });
        if wheel || bundle {
            owned.push((name, entry.path()));
        }
    }
    Ok(owned)
}
pub fn cleanup_legacy_runtimes(p: &Paths, uninstall: bool) -> Result<()> {
    let mut kept = std::collections::BTreeSet::new();
    if !uninstall {
        for name in ["current", "previous", "verified"] {
            if let Some(record) = p.selected(name)? {
                if let Some(runtime) = record["core_build"].as_str() {
                    kept.insert(runtime.to_owned());
                }
            }
        }
    }
    for (id, path) in owned_legacy_runtimes(p)? {
        if kept.contains(&id) {
            continue;
        }
        // remove_dir_all unlinks Python/venv symlinks themselves; it never follows their external targets.
        fs::remove_dir_all(path)?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nightly_order_precedes_runtime_and_format_changes() {
        let current = json!({"connector":"1.2.3","channel":"nightly","build_seq":20,"format":"sea","pair_id":"old"});
        let next = json!({"connector":"1.2.3","channel":"nightly","build_seq":19,"format":"rust-native","pair_id":"new"});
        assert_eq!(decide(Some(&current), &next), "noop");
        let mut next = next;
        next["build_seq"] = json!(20);
        assert_eq!(decide(Some(&current), &next), "upgrade");
    }
    #[test]
    fn pointers_commit_atomically_and_reject_escaping_targets() {
        let root =
            std::env::temp_dir().join(format!("sidevoice-release-test-{}", uuid::Uuid::new_v4()));
        private_directory(&root).unwrap();
        let p = Paths {
            home: root.clone(),
            data: root.join("data"),
            root: root.join("sidevoice"),
            config: root.join("config"),
        };
        p.prepare().unwrap();
        for id in ["before", "after"] {
            private_directory(&p.root.join("releases").join(id)).unwrap();
            write_json(
                &p.root.join("releases").join(id).join("release.json"),
                &json!({"id":id}),
            )
            .unwrap();
        }
        p.point("current", "before").unwrap();
        p.point("verified", "before").unwrap();
        p.point("previous", "before").unwrap();
        p.point("current", "after").unwrap();
        assert_eq!(p.selected("current").unwrap().unwrap()["id"], "after");
        assert_eq!(p.selected("verified").unwrap().unwrap()["id"], "before");
        assert!(p.point("current", "../outside").is_err());
        fs::remove_file(p.root.join("current")).unwrap();
        symlink("../outside", p.root.join("current")).unwrap();
        assert!(p.selected("current").is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn private_directory_does_not_follow_user_symlinks() {
        let root =
            std::env::temp_dir().join(format!("sidevoice-dir-test-{}", uuid::Uuid::new_v4()));
        private_directory(&root).unwrap();
        private_directory(&root.join("real")).unwrap();
        symlink(root.join("real"), root.join("linked")).unwrap();
        assert!(private_directory(&root.join("linked/child")).is_err());
        assert!(!root.join("real/child").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_runtime_cleanup_keeps_selected_and_foreign_state() {
        let root =
            std::env::temp_dir().join(format!("sidevoice-runtime-test-{}", uuid::Uuid::new_v4()));
        let p = Paths {
            home: root.join("home"),
            data: root.join("data"),
            root: root.join("sidevoice"),
            config: root.join("config"),
        };
        p.prepare().unwrap();
        for id in ["owned-old", "owned-kept", "foreign", "mismatched"] {
            private_directory(&p.data.join("core-runtime").join(id)).unwrap();
        }
        for id in ["owned-old", "owned-kept"] {
            write_json(
                &p.data.join("core-runtime").join(id).join("installed.json"),
                &json!({"id":id,"version":"0.1.0","spec":"/trusted/core.whl"}),
            )
            .unwrap();
        }
        write_json(
            &p.data.join("core-runtime/mismatched/installed.json"),
            &json!({"id":"someone-else","version":"0.1.0","spec":"external"}),
        )
        .unwrap();
        private_directory(&root.join("outside")).unwrap();
        fs::write(root.join("outside/personal.txt"), "keep").unwrap();
        symlink(
            root.join("outside"),
            p.data.join("core-runtime/owned-old/python"),
        )
        .unwrap();
        symlink(
            root.join("outside"),
            p.data.join("core-runtime/foreign-link"),
        )
        .unwrap();
        private_directory(&p.root.join("releases/legacy")).unwrap();
        write_json(
            &p.root.join("releases/legacy/release.json"),
            &json!({"id":"legacy","core_build":"owned-kept"}),
        )
        .unwrap();
        p.point("previous", "legacy").unwrap();
        cleanup_legacy_runtimes(&p, false).unwrap();
        assert!(!p.data.join("core-runtime/owned-old").exists());
        assert!(p.data.join("core-runtime/owned-kept").exists());
        assert!(root.join("outside/personal.txt").exists());
        cleanup_legacy_runtimes(&p, true).unwrap();
        assert!(!p.data.join("core-runtime/owned-kept").exists());
        for name in ["foreign", "mismatched", "foreign-link"] {
            assert!(p.data.join("core-runtime").join(name).exists());
        }
        assert!(root.join("outside/personal.txt").exists());
        fs::remove_dir_all(root).unwrap();
    }
    fn archive_entry(name: &str, kind: tar::EntryType, bytes: &[u8]) -> Vec<u8> {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o600);
        header.set_entry_type(kind);
        // Set raw names so a traversal fixture reaches our extractor, rather than the builder refusing it first.
        let field = &mut header.as_mut_bytes()[..100];
        field.fill(0);
        field[..name.len()].copy_from_slice(name.as_bytes());
        header.set_cksum();
        let mut builder = tar::Builder::new(Vec::new());
        builder.append(&header, bytes).unwrap();
        let tar = builder.into_inner().unwrap();
        zstd::stream::encode_all(tar.as_slice(), 0).unwrap()
    }
    #[test]
    fn malformed_archives_report_stable_authenticity_checks() {
        let root =
            std::env::temp_dir().join(format!("sidevoice-archive-test-{}", uuid::Uuid::new_v4()));
        private_directory(&root).unwrap();
        let cases = [
            (
                "archive-type",
                archive_entry("sidevoice-core-rust/bin/evil", tar::EntryType::Symlink, b""),
            ),
            (
                "archive-path",
                archive_entry(
                    "sidevoice-core-rust/bin/../evil",
                    tar::EntryType::Regular,
                    b"",
                ),
            ),
            (
                "archive-inventory",
                archive_entry("sidevoice-core-rust", tar::EntryType::Directory, b""),
            ),
        ];
        for (index, (check, archive)) in cases.into_iter().enumerate() {
            let manifest =
                json!({"bundles":{"test":{"size":archive.len(),"sha256":digest(&archive)}}});
            let error = unpack_core(&archive, &manifest, "test", &root.join(index.to_string()))
                .unwrap_err();
            let error = error.downcast_ref::<ControlError>().unwrap();
            assert_eq!(error.key, "install.authenticity");
            assert_eq!(error.params["check"], check);
        }
        let archive = b"not-the-expected-bytes";
        let error = unpack_core(
            archive,
            &json!({"bundles":{"test":{"size":archive.len(),"sha256":"0".repeat(64)}}}),
            "test",
            &root.join("digest"),
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<ControlError>().unwrap().params["check"],
            "sha256"
        );
        assert!(!root.join("digest").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
