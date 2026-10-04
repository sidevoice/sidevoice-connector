//! Native release storage. Pointer replacement is the sole selection commit point.
use anyhow::{bail, Context, Result};
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
        return Err(crate::release::refusal(
            "control.native-core-archive-digest-mismatch",
            json!({}),
        ));
    }
    let decoder = zstd::stream::read::Decoder::new(archive)?;
    let mut tar = tar::Archive::new(decoder.take(1_010_000_000));
    private_directory(root)?;
    let mut inventory = None;
    let mut seen = std::collections::BTreeMap::new();
    let mut total = 0u64;
    let mut count = 0usize;
    for entry in tar.entries()? {
        let mut entry = entry?;
        count += 1;
        if count > 2000 {
            return Err(crate::release::refusal(
                "control.native-core-archive-entry-limit",
                json!({}),
            ));
        }
        let path = entry.path()?.into_owned();
        let relative = path
            .strip_prefix("sidevoice-core-rust")
            .context("native Core archive root")?;
        if path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(crate::release::refusal(
                "control.unsafe-native-core-archive-path",
                json!({}),
            ));
        }
        let name = relative
            .to_str()
            .context("native Core archive path encoding")?;
        if name.len() > 240
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._+@/-".contains(&c))
        {
            return Err(crate::release::refusal(
                "control.unsafe-native-core-archive-path",
                json!({}),
            ));
        }
        let kind = entry.header().entry_type();
        if name.is_empty() {
            if !kind.is_dir() {
                return Err(crate::release::refusal(
                    "control.archive-root-is-not-a-directory",
                    json!({}),
                ));
            }
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
            return Err(crate::release::refusal(
                "control.native-core-archive-unexpected-root",
                json!({}),
            ));
        }
        let file = root.join(relative);
        if kind.is_dir() {
            private_directory(&file)?;
            continue;
        }
        if !kind.is_file() {
            return Err(crate::release::refusal(
                "control.native-core-archive-links-or-special-files-are-forbidden",
                json!({}),
            ));
        }
        let size = entry.size();
        total = total.checked_add(size).context("archive size overflow")?;
        if size > 500_000_000 || total > 1_000_000_000 {
            return Err(crate::release::refusal(
                "control.native-core-archive-size-limit",
                json!({}),
            ));
        }
        if name == "native-core.json" {
            if inventory.is_some() || size > 4_000_000 {
                return Err(crate::release::refusal(
                    "control.native-core-inventory-limit",
                    json!({}),
                ));
            }
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            let value: Value = serde_json::from_slice(&bytes)?;
            let mut canonical = serde_json::to_vec(&value)?;
            canonical.push(b'\n');
            if canonical != bytes {
                return Err(crate::release::refusal(
                    "control.native-core-inventory-is-not-canonical",
                    json!({}),
                ));
            }
            write(&file, &bytes, 0o600)?;
            inventory = Some(value);
            continue;
        }
        if seen.contains_key(name) {
            return Err(crate::release::refusal(
                "control.duplicate-native-core-archive-file",
                json!({}),
            ));
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
            let n = entry.read(&mut bytes)?;
            if n == 0 {
                break;
            }
            out.write_all(&bytes[..n])?;
            h.update(&bytes[..n]);
            copied += n as u64;
        }
        out.sync_all()?;
        if copied != size {
            return Err(crate::release::refusal(
                "control.native-core-truncated-archive-file",
                json!({}),
            ));
        }
        seen.insert(name.to_string(), (size, hex::encode(h.finalize())));
    }
    let inventory = inventory.context("native Core inventory missing")?;
    if inventory["schema"] != 1
        || inventory["kind"] != "rust-native-v1"
        || inventory["target"] != target
        || inventory["source_sha"] != manifest["source_sha"]
        || inventory["entrypoint"] != "bin/sidevoice-core-rust"
    {
        return Err(crate::release::refusal(
            "control.native-core-inventory-identity-mismatch",
            json!({}),
        ));
    }
    let files = inventory["files"]
        .as_array()
        .context("native Core inventory files")?;
    if files.len() != seen.len() {
        return Err(crate::release::refusal(
            "control.native-core-inventory-differs-from-archive",
            json!({}),
        ));
    }
    let mut previous = "";
    for file in files {
        let name = file["name"]
            .as_str()
            .context("native Core inventory file name")?;
        if name <= previous {
            return Err(crate::release::refusal(
                "control.native-core-inventory-files-not-unique-and-sorted",
                json!({}),
            ));
        }
        previous = name;
        let (size, sha) = seen
            .get(name)
            .context("native Core inventory omitted file")?;
        if file["size"].as_u64() != Some(*size) || file["sha256"].as_str() != Some(sha) {
            return Err(crate::release::refusal(
                "control.native-core-inventory-digest-mismatch",
                json!({}),
            ));
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
            return Err(crate::release::refusal(
                "control.native-core-required-file-missing",
                json!({}),
            ));
        }
    }
    if !seen.keys().any(|k| k.starts_with("notices/")) {
        return Err(crate::release::refusal(
            "control.native-core-notices-missing",
            json!({}),
        ));
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
}
