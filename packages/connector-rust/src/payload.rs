//! Build-time verified native payload. Installation never retrieves Core from the network.
use anyhow::{ensure, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const CORE_VERSION: &str = "0.1.0";
pub const MANIFEST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/native-core-manifest.json"));
pub const ARCHIVE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/native-core.tar.zst"));

pub fn core_manifest() -> Result<Value> {
    ensure!(!MANIFEST.is_empty(), "install.native-payload-missing");
    Ok(serde_json::from_slice(MANIFEST)?)
}

pub fn core_archive() -> Result<&'static [u8]> {
    ensure!(!ARCHIVE.is_empty(), "install.native-payload-missing");
    Ok(ARCHIVE)
}

pub fn version_metadata() -> Value {
    json!({"ok":true,"version":env!("SIDEVOICE_CONNECTOR_VERSION"),
        "target":env!("SIDEVOICE_CONNECTOR_TARGET"),"channel":env!("SIDEVOICE_CHANNEL"),
        "connector_sha":env!("SIDEVOICE_CONNECTOR_BUILD_SHA"),
        "build_seq":env!("SIDEVOICE_BUILD_SEQ").parse::<u64>().expect("validated build sequence"),
        "format":"rust-native","sea":false})
}

pub fn connector_metadata() -> Value {
    let version = version_metadata();
    let manifest = core_manifest().ok();
    let digest = manifest
        .as_ref()
        .map(|_| hex::encode(Sha256::digest(MANIFEST)));
    json!({"ok":true,"connector":{
        "version":version["version"],"sha":version["connector_sha"],"channel":version["channel"],
        "build_seq":version["build_seq"],"target":version["target"],"format":"rust-native","sea":false,
        "link_min":2,"link_max":2},
        "embedded_core":{"version":CORE_VERSION,"manifest_sha256":digest,"assets":[],"api":1,"link":2,
            "kind":"rust-native-v1","manifest":manifest},
        "protocols":{"metadata":"sidevoice-metadata-v1","progress":"sidevoice-progress-jsonl-v1"}})
}
