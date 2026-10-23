//! Where Codex's CLI is when it is not on `PATH` because it lives inside the ChatGPT desktop app. `PATH` is looked
//! at first (`agents::find_binary`); these are the fallbacks, each paired with the directory of this user's it must
//! be under, judged by `Env::trusted_program` before one is used. Nothing here starts a program.

use crate::adapters::markers::Roots;
use crate::platform::env::{Env, Environment};
use crate::platform::target::{Host, Target as _};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The candidates as (root, program), in the order they are tried.
///
/// - Windows: the copy the app makes of its bundled `codex.exe` for the user, `<app programs>\OpenAI\Codex\bin\
///   <hash>\codex.exe`, newest first (that "newest" is an assumption). The Store package's own copy is not a
///   candidate: it is reported as not runnable by the user.
/// - macOS: the CLI inside the app bundle, in `/Applications` and in `~/Applications`, in the layout of the current
///   app and in the older one.
pub(super) fn fallbacks(roots: &Roots) -> Vec<(PathBuf, PathBuf)> {
    let exe = Host::exe_name("codex");
    let mut found = Vec::new();
    if let Some(programs) = roots
        .local_app_data
        .as_deref()
        .and_then(Environment::per_user_app_programs)
    {
        let cache = programs.join("OpenAI").join("Codex").join("bin");
        found.extend(
            cached_copies(&cache, &exe)
                .into_iter()
                .map(|program| (programs.clone(), program)),
        );
    }
    let home_apps = roots.home.join("Applications");
    for (root, apps) in [
        (
            PathBuf::from("/Applications"),
            PathBuf::from("/Applications"),
        ),
        (roots.home.clone(), home_apps),
    ] {
        let resources = apps.join("ChatGPT.app/Contents/Resources");
        found.push((root.clone(), resources.join("codex-cli/bin").join(&exe)));
        found.push((root, resources.join(&exe)));
    }
    found
}

/// `<cache>\<hash>\codex.exe` for every hash directory that has one, the most recently written first.
fn cached_copies(cache: &Path, exe: &str) -> Vec<PathBuf> {
    let mut found: Vec<(SystemTime, PathBuf)> = fs::read_dir(cache)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| {
            let program = entry.path().join(exe);
            let modified = fs::metadata(&program)
                .ok()
                .filter(fs::Metadata::is_file)?
                .modified()
                .ok()?;
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

    fn program(path: &Path, age: u64) {
        Files::ensure_private_dir(path.parent().unwrap()).unwrap();
        Files::open_private(path, Create::NewProgram).unwrap();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + age))
            .unwrap();
    }

    #[test]
    fn cached_copies_are_listed_newest_first_and_only_where_there_is_a_program() {
        let dir = crate::test_support::temp_dir("codex-cache");
        let exe = Host::exe_name("codex");
        let cache = dir.path().join("bin");
        let (old, new) = (cache.join("aaa").join(&exe), cache.join("bbb").join(&exe));
        program(&old, 1);
        program(&new, 9);
        fs::create_dir_all(cache.join("empty")).unwrap();
        fs::write(cache.join("stray"), "").unwrap();
        assert_eq!(cached_copies(&cache, &exe), vec![new, old]);
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
        let found = fallbacks(&roots);
        let resources = "ChatGPT.app/Contents/Resources";
        assert_eq!(found.len(), 4);
        assert_eq!(found[0].0, Path::new("/Applications"));
        assert_eq!(
            found[0].1,
            Path::new("/Applications")
                .join(resources)
                .join("codex-cli/bin")
                .join(&exe)
        );
        assert_eq!(found[2].0, roots.home);
        assert_eq!(
            found[3].1,
            roots.home.join("Applications").join(resources).join(&exe)
        );
    }

    #[test]
    fn a_trusted_cached_copy_is_used_when_the_better_one_is_refused() {
        let dir = crate::test_support::temp_dir("codex-resolve");
        let exe = Host::exe_name("codex");
        let programs = dir.path().to_path_buf();
        let cache = programs.join("OpenAI/Codex/bin");
        let (old, new) = (cache.join("aaa").join(&exe), cache.join("bbb").join(&exe));
        program(&old, 1);
        program(&new, 9);
        use crate::platform::fs::TestFiles;
        Files::let_others_change(&new);
        let candidates = cached_copies(&cache, &exe)
            .into_iter()
            .map(|program| (programs.clone(), program))
            .collect();
        assert_eq!(
            crate::agents::first_trusted(candidates).as_deref(),
            old.to_str()
        );
    }
}
