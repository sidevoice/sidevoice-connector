//! The archive format: a reproducible tar of one root directory, zstd-compressed. Writing it, and unpacking one
//! (ours or the core's) with every bound and path check.

use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::glibc;
use crate::util::*;
use crate::{Result, ENTRYPOINT, INVENTORY, KIND, ROOT_NAME, TARGETS};

const MAX_COMPRESSED: u64 = 250_000_000;
const MAX_TOTAL: u64 = 1_000_000_000;
const MAX_ENTRIES: usize = 5_000;

/// Compresses the tree `<work>/<root>` as a reproducible archive: owner root, time `epoch`, 0755 for directories and
/// for the files in `executables`, 0644 for everything else.
pub(crate) fn write_archive(
    work: &Path,
    root: &str,
    executables: &[&str],
    epoch: u64,
    archive: &Path,
) -> Result<()> {
    let stage = work.join(root);
    let mut entries = vec![(String::new(), true)];
    entries.extend(walk(&stage)?);
    let mut builder = tar::Builder::new(Vec::new());
    for (relative, is_dir) in entries {
        let name = if relative.is_empty() {
            root.to_string()
        } else {
            format!("{root}/{relative}")
        };
        let mut header = tar::Header::new_ustar();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(epoch);
        if is_dir {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            builder.append_data(&mut header, format!("{name}/"), std::io::empty())
        } else {
            let bytes = read(&work.join(&name))?;
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(if executables.contains(&relative.as_str()) {
                0o755
            } else {
                0o644
            });
            header.set_size(bytes.len() as u64);
            builder.append_data(&mut header, &name, bytes.as_slice())
        }
        .map_err(|error| format!("tar {name}: {error}"))?;
    }
    let tar = builder.into_inner().map_err(|error| error.to_string())?;
    let compressed = zstd::encode_all(tar.as_slice(), 19).map_err(|error| error.to_string())?;
    mkdir(archive.parent().expect("has a parent"))?;
    write(archive, &compressed)
}

/// Unpacks `archive` into `destination`, refusing anything but plain files and directories below the single
/// directory `root`, and anything past the size and count bounds. Returns `<destination>/<root>`.
pub(crate) fn unpack(archive: &Path, destination: &Path, root: &str) -> Result<PathBuf> {
    let compressed =
        fs::File::open(archive).map_err(|error| format!("{}: {error}", archive.display()))?;
    if compressed
        .metadata()
        .map_err(|error| error.to_string())?
        .len()
        > MAX_COMPRESSED
    {
        return Err("compressed archive exceeds bound".into());
    }
    let decoder = zstd::Decoder::new(compressed).map_err(|error| error.to_string())?;
    let mut tar = tar::Archive::new(decoder.take(MAX_TOTAL + 1_000_000));
    let mut seen = BTreeSet::new();
    let mut total: u64 = 0;
    for entry in tar.entries().map_err(|error| error.to_string())? {
        let mut entry = entry.map_err(|error| error.to_string())?;
        let name = entry
            .path()
            .map_err(|error| error.to_string())?
            .to_string_lossy()
            .trim_end_matches('/')
            .to_string();
        if name.len() > 240 || seen.len() >= MAX_ENTRIES {
            return Err("archive path or entry count exceeds bound".into());
        }
        let kind = entry.header().entry_type();
        if name == root {
            if !kind.is_dir() {
                return Err("archive root must be a directory".into());
            }
        } else {
            validate_name(&name, root)?;
        }
        if !seen.insert(name.clone()) || !(kind.is_dir() || kind.is_file()) {
            return Err(format!("duplicate or forbidden tar entry: {name}"));
        }
        let path = destination.join(&name);
        if kind.is_dir() {
            mkdir(&path)?;
            continue;
        }
        let size = entry.header().size().map_err(|error| error.to_string())?;
        total += size;
        if total > MAX_TOTAL {
            return Err("archive size exceeds bound".into());
        }
        mkdir(path.parent().expect("below root"))?;
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        write(&path, &bytes)?;
        chmod(
            &path,
            entry.header().mode().map_err(|error| error.to_string())? & 0o755,
        )?;
    }
    let root = destination.join(root);
    if !root.is_dir() {
        return Err("archive has no root directory".into());
    }
    Ok(root)
}

pub(crate) fn validate_name(name: &str, root: &str) -> Result<()> {
    if name.is_empty()
        || name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(format!("unsafe member path: {name}"));
    }
    if !name.starts_with(&format!("{root}/")) {
        return Err(format!("wrong archive root: {name}"));
    }
    Ok(())
}

/// Unpacks one of our archives and checks its inventory against what it holds: every file listed with its size
/// and digest, nothing else, the fields and identity it must have. Returns its root and inventory.
pub(crate) fn unpack_checked(archive: &Path, destination: &Path) -> Result<(PathBuf, Value)> {
    let root = unpack(archive, destination, ROOT_NAME)?;
    let bytes = read(&root.join(INVENTORY))?;
    let inventory = parse_json(&bytes, INVENTORY)?;
    if canonical(&inventory) != bytes {
        return Err(format!("{INVENTORY} is not canonical"));
    }
    let fields: BTreeSet<&str> = inventory
        .as_object()
        .ok_or("inventory")?
        .keys()
        .map(String::as_str)
        .collect();
    let target = inventory["target"].as_str().unwrap_or("");
    let mut expected = BTreeSet::from([
        "schema",
        "kind",
        "version",
        "target",
        "source_sha",
        "entrypoint",
        "files",
    ]);
    // A Linux binary's glibc floor: the oldest C library it runs on (crate::glibc).
    if target.starts_with("linux-") {
        expected.insert("glibc");
        if glibc::parse(inventory["glibc"].as_str().unwrap_or("")).is_none() {
            return Err("wrong glibc floor".into());
        }
    }
    if fields != expected {
        return Err("wrong inventory fields".into());
    }
    if inventory["schema"] != 1
        || inventory["kind"] != KIND
        || inventory["entrypoint"] != ENTRYPOINT
    {
        return Err("wrong kind or entrypoint".into());
    }
    if !TARGETS.contains(&target)
        || !is_commit(inventory["source_sha"].as_str().unwrap_or(""))
        || inventory["version"].as_str().unwrap_or("").is_empty()
    {
        return Err("wrong target, source commit or version".into());
    }
    let records = inventory["files"].as_array().ok_or("inventory files")?;
    let names: Vec<&str> = records
        .iter()
        .map(|record| record["name"].as_str().unwrap_or(""))
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    if names != sorted {
        return Err("unsorted or duplicated inventory".into());
    }
    let mut listed: BTreeSet<String> = names.iter().map(|name| name.to_string()).collect();
    listed.insert(INVENTORY.into());
    let present: BTreeSet<String> = walk(&root)?
        .into_iter()
        .filter(|(_, is_dir)| !is_dir)
        .map(|(name, _)| name)
        .collect();
    if present != listed {
        return Err(format!(
            "unexpected or missing files: {:?}",
            present.symmetric_difference(&listed)
        ));
    }
    if !names.contains(&ENTRYPOINT) {
        return Err("the inventory does not list the entrypoint".into());
    }
    for record in records {
        let name = record["name"].as_str().unwrap_or("");
        validate_name(&format!("{ROOT_NAME}/{name}"), ROOT_NAME)?;
        if record.as_object().map(|object| object.len()) != Some(3)
            || *record != file_record(&root.join(name), name)?
        {
            return Err(format!("inventory mismatch: {name}"));
        }
    }
    Ok((root, inventory))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stage(work: &Path) -> PathBuf {
        let root = work.join(ROOT_NAME);
        mkdir(&root.join("bin")).unwrap();
        write(&root.join(ENTRYPOINT), b"#!/bin/sh\n").unwrap();
        write(&root.join("LICENSE"), b"licence").unwrap();
        let files = vec![
            file_record(&root.join(ENTRYPOINT), ENTRYPOINT).unwrap(),
            file_record(&root.join("LICENSE"), "LICENSE").unwrap(),
        ];
        let mut files = files;
        files.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        let inventory = json!({"schema": 1, "kind": KIND, "version": "1.2.3", "target": "linux-x86_64",
            "glibc": "2.28", "source_sha": "0123456789abcdef0123456789abcdef01234567", "entrypoint": ENTRYPOINT,
            "files": files});
        write(&root.join(INVENTORY), &canonical(&inventory)).unwrap();
        root
    }

    #[test]
    fn an_archive_round_trips_and_is_reproducible() {
        let work = TempDir::new("xtask-archive-test").unwrap();
        stage(&work.0);
        let first = work.0.join("first.tar.zst");
        let second = work.0.join("second.tar.zst");
        write_archive(&work.0, ROOT_NAME, &[ENTRYPOINT], 1, &first).unwrap();
        write_archive(&work.0, ROOT_NAME, &[ENTRYPOINT], 1, &second).unwrap();
        assert_eq!(read(&first).unwrap(), read(&second).unwrap());

        let out = TempDir::new("xtask-archive-test-out").unwrap();
        let (root, inventory) = unpack_checked(&first, &out.0).unwrap();
        assert_eq!(inventory["version"], "1.2.3");
        let mode = fs::metadata(root.join(ENTRYPOINT)).unwrap().permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o755
        );
    }

    #[test]
    fn a_file_the_inventory_does_not_list_is_refused() {
        let work = TempDir::new("xtask-archive-extra").unwrap();
        let root = stage(&work.0);
        write(&root.join("stowaway"), b"x").unwrap();
        let archive = work.0.join("a.tar.zst");
        write_archive(&work.0, ROOT_NAME, &[ENTRYPOINT], 1, &archive).unwrap();
        let out = TempDir::new("xtask-archive-extra-out").unwrap();
        let error = unpack_checked(&archive, &out.0).unwrap_err();
        assert!(error.contains("stowaway"), "{error}");
    }

    #[test]
    fn a_changed_file_is_refused() {
        let work = TempDir::new("xtask-archive-changed").unwrap();
        let root = stage(&work.0);
        write(&root.join("LICENSE"), b"another licence").unwrap();
        let archive = work.0.join("a.tar.zst");
        write_archive(&work.0, ROOT_NAME, &[ENTRYPOINT], 1, &archive).unwrap();
        let out = TempDir::new("xtask-archive-changed-out").unwrap();
        let error = unpack_checked(&archive, &out.0).unwrap_err();
        assert!(error.contains("inventory mismatch: LICENSE"), "{error}");
    }

    #[test]
    fn a_linux_archive_must_record_its_glibc_floor() {
        for floor in [None, Some("latest")] {
            let work = TempDir::new("xtask-archive-floor").unwrap();
            let root = stage(&work.0);
            let mut inventory = parse_json(&read(&root.join(INVENTORY)).unwrap(), "").unwrap();
            match floor {
                Some(floor) => inventory["glibc"] = floor.into(),
                None => drop(inventory.as_object_mut().unwrap().remove("glibc")),
            }
            write(&root.join(INVENTORY), &canonical(&inventory)).unwrap();
            let archive = work.0.join("a.tar.zst");
            write_archive(&work.0, ROOT_NAME, &[ENTRYPOINT], 1, &archive).unwrap();
            let out = TempDir::new("xtask-archive-floor-out").unwrap();
            let error = unpack_checked(&archive, &out.0).unwrap_err();
            assert!(
                error.contains("inventory fields") || error.contains("glibc floor"),
                "{error}"
            );
        }
    }

    #[test]
    fn member_paths_stay_below_the_root() {
        assert!(validate_name("sidevoice-connector/bin/x", ROOT_NAME).is_ok());
        assert!(validate_name("sidevoice-connector/../x", ROOT_NAME).is_err());
        assert!(validate_name("sidevoice-connector//x", ROOT_NAME).is_err());
        assert!(validate_name("elsewhere/x", ROOT_NAME).is_err());
    }
}
