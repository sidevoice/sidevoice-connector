//! The licence notices an archive carries: the licence texts of every crate linked into the binary on this target.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use crate::util::*;
use crate::{Result, PACKAGE};

/// The crates the released binary links on `triple`: its normal dependencies, transitively (build scripts, proc
/// macros' own dependencies and dev-dependencies are not in the binary, but proc macros are listed like any crate).
fn linked_packages(metadata: &Value) -> Result<Vec<&Value>> {
    let packages = metadata["packages"].as_array().ok_or("no packages")?;
    let by_id: BTreeMap<&str, &Value> = packages
        .iter()
        .filter_map(|package| package["id"].as_str().map(|id| (id, package)))
        .collect();
    let nodes: BTreeMap<&str, &Value> = metadata["resolve"]["nodes"]
        .as_array()
        .ok_or("no resolve graph")?
        .iter()
        .filter_map(|node| node["id"].as_str().map(|id| (id, node)))
        .collect();
    let root = packages
        .iter()
        .find(|package| package["name"] == PACKAGE && package["source"].is_null())
        .and_then(|package| package["id"].as_str())
        .ok_or_else(|| format!("no {PACKAGE} in cargo metadata"))?;
    let mut seen = BTreeSet::from([root]);
    let mut queue = vec![root];
    while let Some(id) = queue.pop() {
        let node = nodes.get(id).ok_or_else(|| format!("{id}: not resolved"))?;
        for dep in node["deps"].as_array().into_iter().flatten() {
            let normal = dep["dep_kinds"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|kind| kind["kind"].is_null());
            let Some(dep_id) = dep["pkg"].as_str() else {
                continue;
            };
            if normal && seen.insert(dep_id) {
                queue.push(dep_id);
            }
        }
    }
    seen.remove(root);
    seen.into_iter()
        .map(|id| {
            by_id
                .get(id)
                .copied()
                .ok_or_else(|| format!("{id}: no package"))
        })
        .collect()
}

/// Writes `notices/rust-dependencies.json` (each linked crate, its licence expression and its licence texts by
/// digest) and `notices/licenses/<sha256>.txt`.
pub(crate) fn stage_notices(notices: &Path, triple: &str) -> Result<()> {
    let metadata = metadata(true, &["--filter-platform", triple])?;
    let licenses = notices.join("licenses");
    mkdir(&licenses)?;
    let mut packages = Vec::new();
    for package in linked_packages(&metadata)? {
        let manifest = package["manifest_path"]
            .as_str()
            .ok_or("package without manifest_path")?;
        let source = Path::new(manifest)
            .parent()
            .ok_or("manifest without directory")?;
        let mut paths: Vec<_> = fs::read_dir(source)
            .map_err(|error| format!("{}: {error}", source.display()))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_file())
            .collect();
        paths.sort();
        let mut texts = Vec::new();
        for path in paths {
            let file_name = path
                .file_name()
                .expect("a file")
                .to_string_lossy()
                .into_owned();
            let lower = file_name.to_lowercase();
            if !["license", "licence", "copying", "notice", "unlicense"]
                .iter()
                .any(|prefix| lower.starts_with(prefix))
            {
                continue;
            }
            let bytes = read(&path)?;
            if bytes.len() > 1_000_000 {
                return Err(format!(
                    "unexpectedly large licence: {} {file_name}",
                    package["name"]
                ));
            }
            let digest = sha256(&bytes);
            let destination = licenses.join(format!("{digest}.txt"));
            if !destination.exists() {
                write(&destination, &bytes)?;
            }
            texts.push(json!({"name": file_name, "sha256": digest}));
        }
        packages.push(
            json!({"name": package["name"], "version": package["version"],
                             "license": package["license"], "source": package["source"],
                             "license_texts": texts}),
        );
    }
    packages.sort_by_key(|package| {
        let field = |key: &str| package[key].as_str().unwrap_or("").to_string();
        (field("name"), field("version"), field("source"))
    });
    if packages.is_empty() {
        return Err("no linked crates found".into());
    }
    write(
        &notices.join("rust-dependencies.json"),
        &canonical(&Value::Array(packages)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_normal_dependencies_are_followed() {
        let metadata = json!({
            "packages": [
                {"id": "c", "name": PACKAGE, "source": null},
                {"id": "a", "name": "a", "source": "registry"},
                {"id": "b", "name": "b", "source": "registry"},
                {"id": "dev", "name": "dev", "source": "registry"},
                {"id": "build", "name": "build", "source": "registry"},
            ],
            "resolve": {"nodes": [
                {"id": "c", "deps": [
                    {"pkg": "a", "dep_kinds": [{"kind": null}]},
                    {"pkg": "dev", "dep_kinds": [{"kind": "dev"}]},
                    {"pkg": "build", "dep_kinds": [{"kind": "build"}]},
                ]},
                {"id": "a", "deps": [{"pkg": "b", "dep_kinds": [{"kind": null}]}]},
                {"id": "b", "deps": []},
                {"id": "dev", "deps": []},
                {"id": "build", "deps": []},
            ]},
        });
        let names: Vec<&str> = linked_packages(&metadata)
            .unwrap()
            .into_iter()
            .map(|package| package["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["a", "b"]);
    }
}
