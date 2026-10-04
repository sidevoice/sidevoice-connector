use sha2::{Digest, Sha256};
use std::{env, fs, path::PathBuf};

fn build_value(name: &str, fallback: &str) -> String {
    println!("cargo:rerun-if-env-changed={name}");
    match env::var(name) {
        Ok(value) if !value.is_empty() && !value.contains(['\n', '\r']) => value,
        Ok(_) => panic!("{name} is malformed"),
        Err(_) => fallback.to_owned(),
    }
}

fn main() {
    let source = build_value("SIDEVOICE_CONNECTOR_BUILD_SHA", "development");
    let target = build_value("SIDEVOICE_CONNECTOR_TARGET", "development");
    let version = build_value("SIDEVOICE_CONNECTOR_VERSION", env!("CARGO_PKG_VERSION"));
    let channel = build_value("SIDEVOICE_CHANNEL", "release");
    assert!(
        matches!(channel.as_str(), "release" | "nightly"),
        "invalid channel"
    );
    let sequence = build_value("SIDEVOICE_BUILD_SEQ", "0");
    assert!(
        sequence
            .parse::<u64>()
            .is_ok_and(|n| n <= 9_007_199_254_740_991),
        "invalid build sequence"
    );
    for (key, value) in [
        ("SIDEVOICE_CONNECTOR_BUILD_SHA", &source),
        ("SIDEVOICE_CONNECTOR_TARGET", &target),
        ("SIDEVOICE_CONNECTOR_VERSION", &version),
        ("SIDEVOICE_CHANNEL", &channel),
        ("SIDEVOICE_BUILD_SEQ", &sequence),
    ] {
        println!("cargo:rustc-env={key}={value}");
    }
    println!("cargo:rerun-if-env-changed=SIDEVOICE_NATIVE_CORE_MANIFEST");
    println!("cargo:rerun-if-env-changed=SIDEVOICE_NATIVE_CORE_ARCHIVE");
    println!("cargo:rerun-if-env-changed=SIDEVOICE_REQUIRE_NATIVE_PAYLOAD");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"));
    let manifest_path = env::var_os("SIDEVOICE_NATIVE_CORE_MANIFEST");
    let archive_path = env::var_os("SIDEVOICE_NATIVE_CORE_ARCHIVE");
    let (manifest, archive) = match (manifest_path, archive_path) {
        (Some(manifest), Some(archive)) => {
            let manifest = PathBuf::from(manifest);
            let archive = PathBuf::from(archive);
            for path in [&manifest, &archive] {
                println!("cargo:rerun-if-changed={}", path.display());
                assert!(
                    fs::symlink_metadata(path).expect("native input").is_file(),
                    "native inputs must be regular files"
                );
            }
            let bytes = fs::read(&manifest).expect("native Core manifest");
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).expect("native Core manifest JSON");
            let pin_path = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap())
                .join("../connector/rust-core-production-pin.json");
            println!("cargo:rerun-if-changed={}", pin_path.display());
            let pin: serde_json::Value =
                serde_json::from_slice(&fs::read(pin_path).expect("Core source pin"))
                    .expect("Core pin JSON");
            assert_eq!(value["schema"], 1);
            assert_eq!(value["kind"], "rust-native-v1");
            assert_eq!(value["source_sha"], pin["source_sha"]);
            assert_eq!(value["cargo_lock_sha256"], pin["cargo_lock_sha256"]);
            assert_eq!(value["entrypoint"], "bin/sidevoice-core-rust");
            assert!(
                matches!(
                    target.as_str(),
                    "macos-aarch64" | "linux-x86_64" | "linux-aarch64"
                ),
                "native target required"
            );
            let rust_target = env::var("TARGET").unwrap();
            let expected = match target.as_str() {
                "macos-aarch64" => "aarch64-apple-darwin",
                "linux-x86_64" => "x86_64-unknown-linux-gnu",
                "linux-aarch64" => "aarch64-unknown-linux-gnu",
                _ => unreachable!(),
            };
            assert_eq!(
                rust_target, expected,
                "payload target differs from Rust target"
            );
            assert!(
                source.len() == 40
                    && source
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "production source SHA required"
            );
            let record = &value["bundles"][&target];
            let size = fs::metadata(&archive).unwrap().len();
            assert!(size > 0 && size <= 250_000_000, "Core archive size limit");
            assert_eq!(record["size"].as_u64(), Some(size));
            let archive_bytes = fs::read(&archive).expect("native Core archive");
            assert_eq!(
                record["sha256"].as_str(),
                Some(hex::encode(Sha256::digest(&archive_bytes)).as_str())
            );
            let expected_name = format!(
                "sidevoice-core-rust-{}-{target}.tar.zst",
                value["source_sha"].as_str().unwrap()
            );
            assert_eq!(record["name"].as_str(), Some(expected_name.as_str()));
            (bytes, archive_bytes)
        }
        (None, None) => {
            assert!(
                env::var("SIDEVOICE_REQUIRE_NATIVE_PAYLOAD").as_deref() != Ok("1"),
                "production build requires embedded native Core"
            );
            (Vec::new(), Vec::new())
        }
        _ => panic!("native Core manifest and archive must be supplied together"),
    };
    fs::write(out.join("native-core-manifest.json"), manifest).unwrap();
    fs::write(out.join("native-core.tar.zst"), archive).unwrap();
}
