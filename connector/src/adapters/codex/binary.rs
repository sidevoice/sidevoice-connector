//! Where Codex's CLI is when it is not on `PATH` because it lives inside the ChatGPT desktop app. `PATH` is looked
//! at first (`agents::find_binary`); these are the fallbacks, taken as a program found on `PATH` is: the first that
//! exists and can be run is used (`agents::first_runnable`). Nothing here starts a program.

use crate::adapters::markers::Roots;
use crate::platform::env::{Env, Environment};
use crate::platform::target;
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
    let exe = target::exe_name("codex");
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
        let exe = target::exe_name("codex");
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
        let exe = target::exe_name("codex");
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
}
