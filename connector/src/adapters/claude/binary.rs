//! Where Claude Code's CLI is when it is not on `PATH`. `PATH` is looked at first (`agents::find_binary`); these are
//! the fallbacks, each paired with the directory of this user's it must be under. They are only candidates: the
//! caller judges them (`Env::trusted_program`) before one is used. Nothing here starts a program.

use crate::adapters::markers::Roots;
use crate::platform::env::{Env, Environment};
use crate::platform::target::{Host, Target as _};
use semver::Version;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The candidates as (root, program), in the order they are tried: the native installer's (`~/.local/bin/claude`),
/// the older local install (`~/.claude/local/claude`), then the copy the Claude desktop app bundles.
pub(super) fn fallbacks(roots: &Roots) -> Vec<(PathBuf, PathBuf)> {
    let exe = Host::exe_name("claude");
    let home = |relative: &str| (roots.home.clone(), roots.home.join(relative).join(&exe));
    let mut found = vec![home(".local/bin"), home(".claude/local")];
    if let Some(local) = &roots.local_app_data {
        let bundle =
            Environment::store_packages(local).and_then(|packages| desktop_bundle(&packages, &exe));
        found.extend(bundle.map(|program| (local.clone(), program)));
    }
    found
}

/// The desktop app is an MSIX package (`Claude_<publisher id>`) and keeps its Claude Code under
/// `<packages>\Claude_*\LocalCache\Roaming\Claude\claude-code\<version>\<hash>\claude.exe`. The highest version wins
/// (by version precedence, not text); among several builds of it, the newest.
fn desktop_bundle(packages: &Path, exe: &str) -> Option<PathBuf> {
    let mut best: Option<(Version, SystemTime, PathBuf)> = None;
    for package in subdirs(packages) {
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
                .and_then(|name| Version::parse(&name.to_string_lossy()).ok())
            else {
                continue;
            };
            for build in subdirs(&version_dir) {
                let program = build.join(exe);
                let Some(modified) = fs::metadata(&program)
                    .ok()
                    .filter(fs::Metadata::is_file)
                    .and_then(|meta| meta.modified().ok())
                else {
                    continue;
                };
                let better = best.as_ref().is_none_or(|(top, time, _)| {
                    version
                        .cmp_precedence(top)
                        .then_with(|| modified.cmp(time))
                        .is_gt()
                });
                if better {
                    best = Some((version.clone(), modified, program));
                }
            }
        }
    }
    best.map(|(_, _, program)| program)
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

    fn program(path: &Path, age: u64) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + age))
            .unwrap();
    }

    fn bundle(packages: &Path, package: &str, version: &str, hash: &str) -> PathBuf {
        packages
            .join(package)
            .join("LocalCache/Roaming/Claude/claude-code")
            .join(version)
            .join(hash)
            .join(Host::exe_name("claude"))
    }

    #[test]
    fn the_desktop_bundle_is_the_highest_version_then_the_newest_build() {
        let dir = tempfile::tempdir().unwrap();
        let packages = dir.path();
        let exe = Host::exe_name("claude");
        program(
            &bundle(packages, "Claude_pzs8sxrjxfjjc", "2.1.9", "aaa"),
            50,
        );
        program(
            &bundle(packages, "Claude_pzs8sxrjxfjjc", "2.1.308", "old"),
            0,
        );
        // Build metadata is no precedence: the build time decides between these two.
        program(
            &bundle(packages, "Claude_pzs8sxrjxfjjc", "2.1.308+z", "x"),
            1,
        );
        let newest = bundle(packages, "Claude_pzs8sxrjxfjjc", "2.1.308", "new");
        program(&newest, 5);
        program(&bundle(packages, "Other_abc", "9.9.9", "ccc"), 99);
        for empty in ["3.0.0/x", "latest/x"] {
            fs::create_dir_all(
                packages
                    .join("Claude_pzs8sxrjxfjjc/LocalCache/Roaming/Claude/claude-code")
                    .join(empty),
            )
            .unwrap();
        }
        assert_eq!(desktop_bundle(packages, &exe), Some(newest));
        assert_eq!(desktop_bundle(&packages.join("none"), &exe), None);
    }

    #[test]
    fn the_native_and_local_installs_come_first_each_under_the_home() {
        let dir = tempfile::tempdir().unwrap();
        let roots = Roots {
            home: dir.path().join("home"),
            local_app_data: None,
            program_files: None,
        };
        let exe = Host::exe_name("claude");
        assert_eq!(
            fallbacks(&roots),
            vec![
                (roots.home.clone(), roots.home.join(".local/bin").join(&exe)),
                (
                    roots.home.clone(),
                    roots.home.join(".claude/local").join(&exe)
                ),
            ]
        );
    }
}

#[cfg(test)]
mod resolution {
    use super::*;
    use crate::platform::fs::{Create, Files, Fs, TestFiles};

    #[test]
    fn path_wins_and_a_trusted_fallback_is_used_when_path_has_none() {
        let dir = crate::test_support::temp_dir("claude-binary");
        let roots = Roots {
            home: dir.path().join("home"),
            local_app_data: None,
            program_files: None,
        };
        let native = roots.home.join(".local/bin").join(Host::exe_name("claude"));
        Files::ensure_private_dir(native.parent().unwrap()).unwrap();
        let claude = crate::adapters::claude::Claude;
        Files::open_private(&native, Create::NewProgram).unwrap();
        assert_eq!(
            crate::agents::find_binary(&claude, None, &roots).as_deref(),
            native.to_str()
        );
        // Somebody else can change it: ignored.
        Files::let_others_change(&native);
        assert_eq!(crate::agents::find_binary(&claude, None, &roots), None);
        Files::shut_others_out(&native);
        // PATH comes first.
        let bin = dir.path().join("bin");
        let first = bin.join(Host::exe_name("claude"));
        Files::ensure_private_dir(&bin).unwrap();
        Files::open_private(&first, Create::NewProgram).unwrap();
        assert_eq!(
            crate::agents::find_binary(&claude, bin.to_str(), &roots).as_deref(),
            first.to_str()
        );
    }
}
