//! Where Codex's CLI is when it is not on `PATH` because it lives inside the ChatGPT desktop app. `PATH` is looked
//! at first (`agents::find_binary`); these are the fallbacks, taken as a program found on `PATH` is: the first that
//! exists and can be run is used (`agents::first_runnable`). Nothing here starts a program.

use crate::adapters::markers::Roots;
use crate::platform::env::{Env, Environment};
use crate::platform::target::{Host, Target as _};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The candidates, in the order they are tried.
///
/// - Windows: the copy the app makes of its bundled `codex.exe` for the user, `<app programs>\OpenAI\Codex\bin\
///   <hash>\codex.exe`, newest first (that "newest" is an assumption). The Store package's own copy is not a
///   candidate: it is reported as not runnable by the user.
/// - macOS: the CLI inside the app bundle, in `/Applications` and in `~/Applications`, in the layout of the current
///   app and in the older one.
pub(super) fn fallbacks(roots: &Roots) -> Vec<PathBuf> {
    let exe = Host::exe_name("codex");
    let mut found = Vec::new();
    if let Some(programs) = roots
        .local_app_data
        .as_deref()
        .and_then(Environment::per_user_app_programs)
    {
        found.extend(cached_copies(
            &programs.join("OpenAI").join("Codex").join("bin"),
            &exe,
        ));
    }
    for apps in [
        PathBuf::from("/Applications"),
        roots.home.join("Applications"),
    ] {
        let resources = apps.join("ChatGPT.app/Contents/Resources");
        found.push(resources.join("codex-cli/bin").join(&exe));
        found.push(resources.join(&exe));
    }
    found
}

/// The smallest a finished copy of the app's `codex.exe` can be. The cache has no completion marker and its hash
/// directory names have no pattern we know, so a copy cut short by a failed relocation (openai/codex#40700) is
/// recognised by size: the real program is tens of megabytes. A heuristic, not a guarantee.
const LEAST_PROGRAM_BYTES: u64 = 4 * 1024 * 1024;

/// `<cache>\<hash>\codex.exe` for every finished cache entry, the most recently written directory first. Skipped:
/// a directory the app is still filling or gave up on (a name starting with `.`, or containing `staging`:
/// `.staging-<id>`), and a program too small to be whole ([`LEAST_PROGRAM_BYTES`]).
fn cached_copies(cache: &Path, exe: &str) -> Vec<PathBuf> {
    let mut found: Vec<(SystemTime, PathBuf)> = fs::read_dir(cache)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            !name.starts_with('.') && !name.contains("staging")
        })
        .filter_map(|entry| {
            let program = entry.path().join(exe);
            fs::metadata(&program)
                .ok()
                .filter(|meta| meta.is_file() && meta.len() >= LEAST_PROGRAM_BYTES)?;
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, program))
        })
        .collect();
    found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    found.into_iter().map(|(_, program)| program).collect()
}

/// The program an npm launcher of Codex starts. npm installs Codex as a launcher script (`codex.cmd` on Windows, which
/// is what `PATH` finds there) over `@openai/codex/bin/codex.js`, which runs the native program of the platform's
/// package: `<package>/vendor/<target>/bin/codex[.exe]`, in `@openai/codex-<platform>`, installed inside `@openai/codex`
/// or beside it. A `.cmd` cannot be given the text of a message (Rust refuses a newline in a batch file's arguments and
/// has to escape the rest for `cmd.exe`), so that program is run directly. None when `launcher` is itself the program
/// (Unix, a native install), or the package is not where npm puts it (pnpm, a different manager): the launcher is
/// then used as found.
pub(super) fn npm_native(launcher: &Path) -> Option<PathBuf> {
    let exe = Host::exe_name("codex");
    if launcher
        .file_name()?
        .to_string_lossy()
        .eq_ignore_ascii_case(&exe)
    {
        return None;
    }
    let dir = launcher.parent()?;
    // `.bin` beside the packages (a local install) or the prefix that holds `node_modules` (a global one).
    let modules = if dir.file_name().is_some_and(|name| name == ".bin") {
        dir.parent()?.to_path_buf()
    } else {
        dir.join("node_modules")
    };
    // npm's name for this machine's architecture (the platform package's suffix); the target directory starts with
    // Rust's. A package for another architecture installed too is not this machine's program.
    let arch = std::env::consts::ARCH;
    let package_suffix = match arch {
        "x86_64" => "-x64",
        "aarch64" => "-arm64",
        _ => return None,
    };
    let target_prefix = format!("{arch}-");
    let scope = modules.join("@openai");
    let codex = scope.join("codex");
    let mut packages = vec![codex.clone()];
    for parent in [codex.join("node_modules").join("@openai"), scope] {
        let mut found: Vec<PathBuf> = fs::read_dir(parent)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.starts_with("codex-") && name.ends_with(package_suffix)
            })
            .map(|entry| entry.path())
            .collect();
        found.sort();
        packages.extend(found);
    }
    packages.into_iter().find_map(|package| {
        let mut targets: Vec<PathBuf> = fs::read_dir(package.join("vendor"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&target_prefix)
            })
            .map(|entry| entry.path().join("bin").join(&exe))
            .filter(|program| program.is_file())
            .collect();
        targets.sort();
        targets.into_iter().next()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::fs::{Create, Files, Fs};
    use std::time::Duration;

    /// A program of a whole size at `path`; `age` is its own mtime, which the order must not follow.
    fn program(path: &Path, age: u64) {
        Files::ensure_private_dir(path.parent().unwrap()).unwrap();
        Files::open_private(path, Create::NewProgram).unwrap();
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_len(LEAST_PROGRAM_BYTES).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + age))
            .unwrap();
    }

    #[test]
    fn cached_copies_are_listed_by_the_directory_written_last_and_only_when_finished() {
        let dir = crate::test_support::temp_dir("codex-cache");
        let exe = Host::exe_name("codex");
        let cache = dir.path().join("bin");
        let at = |name: &str| cache.join(name).join(&exe);
        // The directory written last comes first, whatever the program inside says about its own age.
        program(&at("aaa"), 99);
        std::thread::sleep(Duration::from_millis(50));
        program(&at("bbb"), 1);
        // Not finished: a staging directory (whatever its age), a hidden one, a program cut short, no program.
        std::thread::sleep(Duration::from_millis(50));
        program(&at(".staging-1f"), 99);
        program(&at("copy-staging"), 99);
        program(&at(".hidden"), 99);
        Files::ensure_private_dir(cache.join("short").as_path()).unwrap();
        fs::write(at("short"), vec![0u8; 1024]).unwrap();
        fs::create_dir_all(cache.join("empty")).unwrap();
        fs::write(cache.join("stray"), "").unwrap();
        assert_eq!(cached_copies(&cache, &exe), vec![at("bbb"), at("aaa")]);
        assert!(cached_copies(&cache.join("none"), &exe).is_empty());
    }

    #[test]
    fn the_bundle_candidates_are_the_apps_in_both_applications_folders_and_the_cache_where_the_os_has_one(
    ) {
        let dir = crate::test_support::temp_dir("codex-roots");
        let roots = Roots {
            home: dir.path().join("home"),
            local_app_data: None,
            program_files: None,
        };
        let exe = Host::exe_name("codex");
        let resources = "ChatGPT.app/Contents/Resources";
        let system = Path::new("/Applications").join(resources);
        let home = roots.home.join("Applications").join(resources);
        assert_eq!(
            fallbacks(&roots),
            vec![
                system.join("codex-cli/bin").join(&exe),
                system.join(&exe),
                home.join("codex-cli/bin").join(&exe),
                home.join(&exe),
            ]
        );
    }

    #[test]
    fn an_npm_launcher_stands_for_the_native_program_of_its_package() {
        let dir = crate::test_support::temp_dir("codex-npm");
        let exe = Host::exe_name("codex");
        let arch = std::env::consts::ARCH;
        let (here, there) = if arch == "aarch64" {
            ("arm64", "x64")
        } else {
            ("x64", "arm64")
        };
        let launcher = |root: &Path| root.join("codex.cmd");
        // A program in `package`, under a target directory named as Rust names this machine's (or another's).
        let native = |package: &Path, target: &str| {
            package.join(format!("vendor/{target}-test/bin")).join(&exe)
        };
        let make = |path: &Path| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"").unwrap();
        };

        // A global install: the platform package inside `@openai/codex`, as npm nests it.
        let global = dir.path().join("global");
        let inner = global.join(format!(
            "node_modules/@openai/codex/node_modules/@openai/codex-test-{here}"
        ));
        assert_eq!(npm_native(&launcher(&global)), None, "nothing installed");
        make(&native(&inner, arch));
        assert_eq!(npm_native(&launcher(&global)), Some(native(&inner, arch)));

        // Hoisted beside it, and a local install (`node_modules/.bin/codex.cmd`).
        let local = dir.path().join("local/node_modules");
        let beside = local.join(format!("@openai/codex-test-{here}"));
        make(&native(&beside, arch));
        assert_eq!(
            npm_native(&local.join(".bin/codex.cmd")),
            Some(native(&beside, arch))
        );

        // Both architectures' packages installed (and the other's sorting first, or its target in this machine's
        // package): only this machine's program is taken.
        let mixed = dir.path().join("mixed/node_modules/@openai");
        make(&native(&mixed.join(format!("codex-test-{there}")), arch));
        make(&native(&mixed.join(format!("codex-test-{there}")), there));
        make(&native(&mixed.join(format!("codex-test-{here}")), there));
        make(&native(&mixed.join(format!("codex-test-{here}")), arch));
        assert_eq!(
            npm_native(&dir.path().join("mixed/codex.cmd")),
            Some(native(&mixed.join(format!("codex-test-{here}")), arch))
        );
        let only_other = dir.path().join("other/node_modules/@openai");
        make(&native(
            &only_other.join(format!("codex-test-{there}")),
            there,
        ));
        assert_eq!(npm_native(&dir.path().join("other/codex.cmd")), None);

        // The program itself, or a manager whose layout is not npm's, is left as found.
        assert_eq!(npm_native(&global.join(&exe)), None);
        let pnpm = dir.path().join("pnpm");
        assert_eq!(npm_native(&launcher(&pnpm)), None);
    }
}
