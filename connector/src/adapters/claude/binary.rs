//! Where Claude Code's CLI is when it is not on `PATH`. `PATH` is looked at first (`agents::find_binary`); these are
//! the fallbacks, taken as a program found on `PATH` is: the first that exists and can be run is used
//! (`agents::first_runnable`). Nothing here starts a program.

use crate::adapters::markers::Roots;
use crate::platform::env::{Env, Environment};
use crate::platform::target::{Host, Target as _};
use semver::Version;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The candidates, in the order they are tried: the native installer's (`~/.local/bin/claude`), the older local
/// install (`~/.claude/local/claude`), then the copy the Claude desktop app bundles.
pub(super) fn fallbacks(roots: &Roots) -> Vec<PathBuf> {
    let exe = Host::exe_name("claude");
    let home = |relative: &str| roots.home.join(relative).join(&exe);
    let mut found = vec![home(".local/bin"), home(".claude/local")];
    if let Some(local) = &roots.local_app_data {
        let roots = version_roots(
            Environment::store_packages(local),
            Environment::per_user_app_programs(local),
        );
        found.extend(desktop_bundles(&roots, &exe));
    }
    found
}

/// The directories holding the desktop app's `<version>\<hash>\claude.exe` builds, from the two places the app has
/// kept them: the MSIX package, `<packages>\Claude_*\LocalCache\Roaming\Claude\claude-code` (older), and the
/// per-user data folder, `<app programs>\Claude-Data\claude_*\claude-code` (current).
fn version_roots(packages: Option<PathBuf>, programs: Option<PathBuf>) -> Vec<PathBuf> {
    let named = |dir: &Path| -> Vec<PathBuf> {
        subdirs(dir)
            .into_iter()
            .filter(|path| {
                path.file_name().is_some_and(|name| {
                    name.to_string_lossy()
                        .to_ascii_lowercase()
                        .starts_with("claude_")
                })
            })
            .collect()
    };
    let mut roots = Vec::new();
    if let Some(packages) = packages {
        roots.extend(
            named(&packages)
                .into_iter()
                .map(|package| package.join("LocalCache/Roaming/Claude/claude-code")),
        );
    }
    if let Some(programs) = programs {
        roots.extend(
            named(&programs.join("Claude-Data"))
                .into_iter()
                .map(|data| data.join("claude-code")),
        );
    }
    roots
}

/// Every `<root>\<version>\<hash>\claude.exe` under the [`version_roots`]. The highest version wins (by version
/// precedence, not text); among several builds of it, the newest. All are returned, best first: the caller takes the
/// first that can be run, so a newer one that cannot does not hide an older one that can.
fn desktop_bundles(roots: &[PathBuf], exe: &str) -> Vec<PathBuf> {
    let mut found: Vec<(Version, SystemTime, PathBuf)> = Vec::new();
    for root in roots {
        for version_dir in subdirs(root) {
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
                found.push((version.clone(), modified, program));
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp_precedence(&a.0).then_with(|| b.1.cmp(&a.1)));
    found.into_iter().map(|(_, _, program)| program).collect()
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
        let ranked = desktop_bundles(&version_roots(Some(packages.to_path_buf()), None), &exe);
        assert_eq!(ranked.len(), 4, "every build of a version, not the others");
        assert_eq!(ranked[0], newest);
        assert!(
            ranked[3].to_string_lossy().contains("2.1.9"),
            "the lowest version is last"
        );
        assert!(
            desktop_bundles(&version_roots(Some(packages.join("none")), None), &exe).is_empty()
        );
    }

    #[test]
    fn the_claude_data_folder_is_searched_with_the_package_and_the_newest_version_wins() {
        let dir = tempfile::tempdir().unwrap();
        let exe = Host::exe_name("claude");
        let packages = dir.path().join("Packages");
        let programs = dir.path().to_path_buf();
        program(
            &bundle(&packages, "Claude_pzs8sxrjxfjjc", "2.1.200", "old"),
            0,
        );
        let current = programs
            .join("Claude-Data/claude_pzs8sxrjxfjjc/claude-code/2.1.308/c17a819afb40")
            .join(&exe);
        program(&current, 0);
        program(
            &programs
                .join("Claude-Data/other_abc/claude-code/9.9.9/x")
                .join(&exe),
            0,
        );
        let roots = version_roots(Some(packages.clone()), Some(programs.clone()));
        let ranked = desktop_bundles(&roots, &exe);
        assert_eq!(ranked.len(), 2, "both layouts, not the unrelated folder");
        assert_eq!(ranked[0], current);
        // Either layout alone is found.
        let only = version_roots(None, Some(programs));
        assert_eq!(desktop_bundles(&only, &exe), vec![current]);
    }
    #[test]
    fn the_native_and_local_installs_come_first() {
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
                roots.home.join(".local/bin").join(&exe),
                roots.home.join(".claude/local").join(&exe),
            ]
        );
    }
}

#[cfg(test)]
mod resolution {
    use super::*;
    use crate::platform::fs::{Create, Files, Fs, TestFiles};

    #[test]
    fn path_wins_and_a_fallback_that_can_be_run_is_used_when_path_has_none() {
        let dir = crate::test_support::temp_dir("claude-binary");
        let roots = Roots {
            home: dir.path().join("home"),
            local_app_data: None,
            program_files: None,
        };
        let native = roots.home.join(".local/bin").join(Host::exe_name("claude"));
        Files::ensure_private_dir(native.parent().unwrap()).unwrap();
        let claude = crate::adapters::claude::Claude;
        assert_eq!(crate::agents::find_binary(&claude, None, &roots), None);
        Files::open_private(&native, Create::NewProgram).unwrap();
        assert_eq!(
            crate::agents::find_binary(&claude, None, &roots).as_deref(),
            native.to_str()
        );
        // Used as one on PATH would be, whoever else can change it.
        Files::let_others_change(&native);
        assert_eq!(
            crate::agents::find_binary(&claude, None, &roots).as_deref(),
            native.to_str()
        );
        // A better candidate that is missing, or a directory, does not hide the next one.
        let ranked = vec![
            roots.home.join("missing"),
            roots.home.clone(),
            native.clone(),
        ];
        assert_eq!(
            crate::agents::first_runnable(ranked).as_deref(),
            native.to_str()
        );
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
