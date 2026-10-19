//! The core inside the connector's package, with the real core: a package laid out as the release archive lays it
//! out (`bin/sidevoice-connector`, `core/<the pinned core archive>`, `connector.json`) stages its core into a
//! release with `stage-core`, checked against the core's own inventory; a package whose core archive is not the
//! one its inventory names is refused with a stable key and stages nothing.
//!
//! The core archive is the one `cargo xtask core` checked and kept in `target/sidevoice-core`. Without it the test
//! is skipped locally and fails in CI, like every test that needs the real core.

mod support;

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use support::package::{package, pinned_core};

fn scratch(label: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("core-package-{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn release(at: &Path) -> PathBuf {
    let release = at.join("release");
    fs::DirBuilder::new().mode(0o700).create(&release).unwrap();
    release
}

fn stage_core(package: &Path, release: &Path) -> (bool, Value) {
    let output = Command::new(package.join("bin/sidevoice-connector"))
        .arg("stage-core")
        .arg(release)
        .arg("--json")
        .env("LD_LIBRARY_PATH", "/nonexistent")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    let value = serde_json::from_str(text.trim()).unwrap_or_else(|_| {
        panic!(
            "stage-core printed {text:?} {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status.success(), value)
}

#[test]
fn the_packaged_core_is_staged_into_a_release() {
    let Some((archive, record)) = pinned_core() else {
        return;
    };
    let at = scratch("staged");
    let root = package(&at, &archive, &record, record["sha256"].as_str().unwrap());
    let release = release(&at);
    let (ok, answer) = stage_core(&root, &release);
    assert!(ok, "{answer}");
    assert_eq!(answer["core"]["version"], record["version"], "{answer}");
    assert_eq!(answer["core"]["sha256"], record["sha256"], "{answer}");
    let program = release.join("core/bin/sidevoice-core-rust");
    let mode = fs::metadata(&program).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700, "{}", program.display());
    assert!(release.join("core/notices").is_dir());
    assert!(!release.join("core/models").exists());

    let (ok, again) = stage_core(&root, &release);
    assert!(!ok, "a release has one core: {again}");
    fs::remove_dir_all(&at).unwrap();
}

#[test]
fn a_core_archive_that_is_not_the_inventorys_is_refused() {
    let Some((archive, record)) = pinned_core() else {
        return;
    };
    let at = scratch("mismatch");
    let root = package(&at, &archive, &record, &"0".repeat(64));
    let release = release(&at);
    let (ok, answer) = stage_core(&root, &release);
    assert!(!ok, "{answer}");
    assert_eq!(answer["error"]["key"], "core.package-mismatch", "{answer}");
    assert_eq!(fs::read_dir(&release).unwrap().count(), 0, "nothing staged");
    fs::remove_dir_all(&at).unwrap();
}
