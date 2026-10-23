//! Where Claude Code's CLI is when it is not on `PATH`. `PATH` is looked at first (`agents::find_in_path`); these are
//! the fallbacks, checked by file metadata only: nothing here starts a program.

use crate::adapters::markers::Roots;
use crate::platform::target::{Host, Target as _};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The candidates, in the order they are tried: the native installer's (`~/.local/bin/claude`), the older local
/// install (`~/.claude/local/claude`), then the copy the Claude desktop app bundles.
pub(super) fn fallbacks(roots: &Roots) -> Vec<PathBuf> {
    let exe = Host::exe_name("claude");
    let mut found = vec![
        roots.home.join(".local").join("bin").join(&exe),
        roots.home.join(".claude").join("local").join(&exe),
    ];
    if let Some(local) = &roots.local_app_data {
        found.extend(desktop_bundle(local, &exe));
    }
    found
}

/// The Windows desktop app is an MSIX package (`Claude_<publisher id>`) and keeps its Claude Code under
/// `<LOCALAPPDATA>\Packages\Claude_*\LocalCache\Roaming\Claude\claude-code\<version>\<hash>\claude.exe`. The highest
/// version wins (compared as versions, not text); among several builds of it, the newest.
fn desktop_bundle(local_app_data: &Path, exe: &str) -> Option<PathBuf> {
    let mut best: Option<((semver::Version, SystemTime), PathBuf)> = None;
    for package in subdirs(&local_app_data.join("Packages")) {
        let named = package
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("Claude_"));
        if !named {
            continue;
        }
        let root = package.join("LocalCache/Roaming/Claude/claude-code");
        for version_dir in subdirs(&root) {
            let Some(version) = version_dir
                .file_name()
                .and_then(|name| semver::Version::parse(&name.to_string_lossy()).ok())
            else {
                continue;
            };
            for build in subdirs(&version_dir) {
                let program = build.join(exe);
                let Ok(modified) = fs::metadata(&program).and_then(|meta| {
                    if meta.is_file() {
                        meta.modified()
                    } else {
                        Err(std::io::ErrorKind::NotFound.into())
                    }
                }) else {
                    continue;
                };
                let key = (version.clone(), modified);
                if best.as_ref().is_none_or(|(top, _)| key > *top) {
                    best = Some((key, program));
                }
            }
        }
    }
    best.map(|(_, program)| program)
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn roots(dir: &Path) -> Roots {
        Roots {
            home: dir.join("home"),
            local_app_data: Some(dir.join("local")),
            program_files: None,
        }
    }

    fn program(path: &Path, age: u64) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + age))
            .unwrap();
    }

    fn bundle(dir: &Path, package: &str, version: &str, hash: &str) -> PathBuf {
        dir.join("local/Packages")
            .join(package)
            .join("LocalCache/Roaming/Claude/claude-code")
            .join(version)
            .join(hash)
            .join(Host::exe_name("claude"))
    }

    #[test]
    fn the_desktop_bundle_is_the_highest_version_then_the_newest_build() {
        let dir = tempfile::tempdir().unwrap();
        let exe = |path: PathBuf, age| {
            program(&path, age);
            path
        };
        exe(
            bundle(dir.path(), "Claude_pzs8sxrjxfjjc", "2.1.9", "aaa"),
            50,
        );
        exe(
            bundle(dir.path(), "Claude_pzs8sxrjxfjjc", "2.1.308", "bbb"),
            1,
        );
        exe(
            bundle(dir.path(), "Claude_pzs8sxrjxfjjc", "2.1.308", "old"),
            0,
        );
        exe(bundle(dir.path(), "Other_abc", "9.9.9", "ccc"), 99);
        let newer_build = exe(
            bundle(dir.path(), "Claude_pzs8sxrjxfjjc", "2.1.308", "new"),
            5,
        );
        // A version directory with nothing in it, and one that is not a version, are skipped.
        fs::create_dir_all(dir.path().join(
            "local/Packages/Claude_pzs8sxrjxfjjc/LocalCache/Roaming/Claude/claude-code/3.0.0/x",
        ))
        .unwrap();
        fs::create_dir_all(dir.path().join(
            "local/Packages/Claude_pzs8sxrjxfjjc/LocalCache/Roaming/Claude/claude-code/latest/x",
        ))
        .unwrap();
        assert_eq!(fallbacks(&roots(dir.path())).last(), Some(&newer_build));
    }

    #[test]
    fn the_native_and_local_installs_come_before_the_desktop_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let roots = roots(dir.path());
        let exe = Host::exe_name("claude");
        assert_eq!(
            fallbacks(&roots),
            vec![
                roots.home.join(".local/bin").join(&exe),
                roots.home.join(".claude/local").join(&exe),
            ],
            "no desktop app, no third candidate"
        );
        let desktop = bundle(dir.path(), "Claude_x", "1.0.0", "h");
        program(&desktop, 0);
        assert_eq!(fallbacks(&roots).len(), 3);
        assert_eq!(fallbacks(&roots)[2], desktop);
    }
}

#[cfg(all(test, unix))]
mod resolution {
    use super::*;
    use crate::adapters::registration::Registration as _;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn path_wins_and_the_fallback_is_used_when_path_has_none() {
        let dir = tempfile::tempdir().unwrap();
        let roots = Roots {
            home: dir.path().join("home"),
            local_app_data: None,
            program_files: None,
        };
        let native = roots.home.join(".local/bin/claude");
        fs::create_dir_all(native.parent().unwrap()).unwrap();
        fs::write(&native, "").unwrap();
        // Present but not executable: not a candidate.
        assert_eq!(
            crate::agents::find_binary(&super::super::Claude, None, &roots),
            None
        );
        fs::set_permissions(&native, fs::Permissions::from_mode(0o755)).unwrap();
        let found = crate::agents::find_binary(&super::super::Claude, None, &roots);
        assert_eq!(found.as_deref(), native.to_str());
        let on_path = dir.path().join("bin");
        fs::create_dir_all(&on_path).unwrap();
        let first = on_path.join("claude");
        fs::write(&first, "").unwrap();
        fs::set_permissions(&first, fs::Permissions::from_mode(0o755)).unwrap();
        let found = crate::agents::find_binary(&super::super::Claude, on_path.to_str(), &roots);
        assert_eq!(found.as_deref(), first.to_str());
        let _ = super::super::Claude.binary_names();
    }
}
