//! Removing the two roots the installer owns (`R`, the releases, and `D`, the data directory) without ever following
//! a link out of them, and without deleting anything that is not ours: the file-system half of `uninstall`.
//!
//! The interface is the [`Removal`] trait; [`Remover`] is the one OS implementation compiled in, chosen below.
//! Callers import both: `use crate::platform::uninstall::{Removal, Remover, Roots};` then
//! `Remover::new(&roots)?.preflight()?`.
//!
//! What every OS guarantees, and what [`Roots::validate`] and the implementations enforce:
//!
//! - **Owned roots only.** `D` is a directory below a home that is a real,
//!   canonical, absolute directory, named as the OS names a program's own directory (`Sidevoice`, `sidevoice`), and
//!   `R` is directly inside it; anything else is refused before anything is touched.
//! - **No link is followed.** A link met inside a root is removed as a link; what it points to is untouched.
//! - **No root swapped mid-step.** A root, or a directory above it, replaced by a link while a step runs is never
//!   reached through: the step is refused, or the replacement cannot happen while it runs.
//! - **Claim by rename.** An entry is first renamed into a private claim directory inside `D` and only then
//!   deleted there, so a replacement put at the old name in the meantime is never reached by the deletion.
//! - **The same volume.** Nothing on another mount or volume is entered.
//! - **Retry.** A failure part-way leaves a recovery copy of the running program in `D` and `install.json` naming
//!   it, so running `uninstall` again finishes the work.
//!
//! How they are met differs. On Unix every step is relative to directory descriptors held open from the home down
//! (`openat` with `O_NOFOLLOW`), each checked against the path again before it is used, with the mount compared at
//! every directory, walked in full by [`Removal::preflight`] before anything is removed. On Windows each step holds
//! every directory from the volume root down to the one it works in, each opened as itself (a link or junction there
//! is refused) and without letting anybody rename or delete it while held, so the paths the step uses reach those
//! directories and nothing else; the home is compared with the one seen at the start, a claimed entry is compared
//! before and after its rename, and the standard library's handle-based `remove_dir_all` (which removes a link or
//! junction itself, never what it leads to, so it never leaves the volume) does the deleting. Windows has no
//! recursive preflight and no mount compare: it never enters a reparse point, which is how another volume is mounted
//! there. Everything that does not touch the OS is one body here: the roots and the retry record.

#[cfg(unix)]
#[path = "uninstall/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "uninstall/windows.rs"]
mod imp;

pub use imp::Os as Remover;

use super::env::{Env, Environment};
use super::fs::{Files, Fs};
use serde_json::{json, Value};
use std::ffi::OsStr;
use std::fs::File;
use std::path::{Component, Path, PathBuf};

/// The private directory in `D` that entries are renamed into before they are deleted.
const CLAIM: &str = ".uninstall-claim";
/// The private directory in `D` holding the copy of the program that finishes an interrupted uninstall.
const RECOVERY: &str = ".uninstall-recovery";
/// The program inside [`RECOVERY`].
const RECOVERY_PROGRAM: &str = "sidevoice-connector";
/// `D`'s record of the installation, which names the recovery program until the very end.
const RECORD: &str = "install.json";
/// What a claimed release is called inside [`CLAIM`], before its unique suffix.
const RELEASE_PREFIX: &str = "release-";
/// What a claimed entry of `D` is called inside [`CLAIM`], before its unique suffix.
const DATA_PREFIX: &str = "data-";

/// The roots one uninstall may remove, and what stays in the data directory.
pub struct Roots {
    /// The person's home.
    pub home: PathBuf,
    /// `D`: the OS's standard place for the program's data, below the home (`data_dir`).
    pub data: PathBuf,
    /// `R`: a directory of `D`.
    pub releases: PathBuf,
    /// The names in `D` that an uninstall leaves (its permanent lock files and the stop).
    pub kept: &'static [&'static str],
}

impl Roots {
    /// That these are the installer's own roots and nothing else: absolute and plain paths, the home canonical,
    /// `D` a directory below the home named as the OS names a program's own directory ([`Env::DATA_NAME`]) and `R`
    /// directly inside it and, when they exist, real canonical directories.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !clean_absolute(&self.home) || self.home.parent().is_none() || !canonical(&self.home) {
            return Err("unsafe home for uninstall");
        }
        let home = dunce::simplified(&self.home);
        let data = dunce::simplified(&self.data);
        if !clean_absolute(&self.data)
            || !clean_absolute(&self.releases)
            || data.strip_prefix(home).is_err()
            || data == home
            || data.file_name() != Some(OsStr::new(Environment::DATA_NAME))
            || dunce::simplified(&self.releases).parent() != Some(data)
            || self.releases.file_name().is_none()
        {
            return Err("uninstall path is outside known Sidevoice roots");
        }
        for path in [&self.data, &self.releases] {
            match std::fs::symlink_metadata(path) {
                Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                    return Err("uninstall root is not a real directory")
                }
                Ok(_) if !canonical(path) => return Err("uninstall root is not canonical"),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err("cannot inspect uninstall root"),
            }
        }
        Ok(())
    }

    /// The names in `D` that are never claimed as data: what the uninstall `kept`, the retry record, the claim and the
    /// recovery directories, and `R` (removed as the releases, by itself).
    fn is_kept(&self, name: &OsStr) -> bool {
        self.kept
            .iter()
            .chain(&[RECORD, CLAIM, RECOVERY])
            .any(|kept| OsStr::new(kept) == name)
            || self.releases.file_name() == Some(name)
    }
}

/// An absolute path of plain components only: no `.`, no `..`, nothing a link or a prefix trick could hide in.
fn clean_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path.components().all(|component| {
            matches!(
                component,
                Component::Prefix(_) | Component::RootDir | Component::Normal(_)
            )
        })
}

/// Whether `path` is what it resolves to: no link, `.` or `..` resolved away. Both sides are compared in one form,
/// `dunce::simplified` (without the `\\?\` prefix Windows' `canonicalize` adds, wherever dropping it changes
/// nothing; as it is otherwise), so a path written either way compares. On Unix both sides are left as they are.
fn canonical(path: &Path) -> bool {
    std::fs::canonicalize(path)
        .is_ok_and(|real| dunce::simplified(&real) == dunce::simplified(path))
}

/// The retry record for [`Removal::stage_recovery`]: `D/install.json` as it is (a JSON object, or nothing), with
/// `command` naming the recovery program and `releases` naming `R`.
fn recovery_record(roots: &Roots) -> Result<Value, &'static str> {
    let mut record = match Files::read_trusted(&roots.data.join(RECORD), 64 * 1024) {
        Ok(None) => json!({}),
        Ok(Some(bytes)) => serde_json::from_slice::<Value>(&bytes)
            .ok()
            .filter(Value::is_object)
            .ok_or("unsafe uninstall install record")?,
        Err(_) => return Err("unsafe uninstall install record"),
    };
    record["command"] = json!([roots.data.join(RECOVERY).join(RECOVERY_PROGRAM)]);
    record["releases"] = json!(roots.releases);
    Ok(record)
}

/// What every OS implements ([`Remover`], in `uninstall/unix.rs` and `uninstall/windows.rs`). Every method that
/// can fail says why in a fixed phrase, and every one re-checks what it relies on, so a root swapped for a link
/// between two calls is refused, not followed.
pub trait Removal: Sized {
    /// Holds `roots` (validated here) so that the steps below act on them and on nothing else.
    fn new(roots: Roots) -> Result<Self, &'static str>;

    /// The roots this removal is for.
    fn roots(&self) -> &Roots;

    /// Checks both roots without removing anything, refusing at the first thing that is not safe to remove (a mount
    /// or volume boundary, an entry that is not what it was, a running program this OS cannot delete). Whether
    /// there are releases to remove.
    fn preflight(&self) -> Result<bool, &'static str>;

    /// The lock file `name` in `D` (created private when missing), opened so that a link in its place is refused;
    /// the caller then takes the lock on it (`lock::wait_lock_file` checks the rest). `D` is made when missing.
    fn lock_file(&self, name: &str) -> Result<File, &'static str>;

    /// The retry route, before any release or data is removed: a copy of the running program placed private in
    /// `D/.uninstall-recovery`, then `D/install.json` written whole: the record kept, its `command` naming that copy
    /// and `releases` naming `R`. `D` is held as one directory from the copy to the record, so a `D` replaced in
    /// between is refused, not written through. A retry through the desktop's installed-CLI lookup then resumes.
    fn stage_recovery(&self) -> Result<(), &'static str>;

    /// Renames every entry of `D` except the kept ones into the claim directory, calling `before_claim(name)` just
    /// before each rename, then deletes what was claimed (claimed releases wait for [`Removal::remove_releases`]).
    /// The names claimed.
    fn remove_data_contents(
        &self,
        before_claim: &dyn Fn(&OsStr),
    ) -> Result<Vec<String>, &'static str>;

    /// Renames `R` into the claim directory and deletes everything claimed as a release.
    fn remove_releases(&self) -> Result<(), &'static str>;

    /// Removes the claim directory, which must be empty by now.
    fn finish_claim(&self) -> Result<(), &'static str>;

    /// The tail, once nothing else remains: the recovery program and directory, then `D/install.json`. Refused while
    /// releases or data remain, so that a failure earlier always leaves the way to retry.
    fn finish_recovery(&self) -> Result<(), &'static str>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{symlink, temp_dir};
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};

    const KEPT: &[&str] = &["install.lock", "agents.lock", "node-stopped.json"];

    const DATA: &str = Environment::DATA_NAME;

    fn roots(home: &Path) -> Roots {
        let data = home.join(DATA);
        Roots {
            home: home.to_path_buf(),
            releases: data.join("install"),
            data,
            kept: KEPT,
        }
    }

    /// A home with both roots made, and a removal over it.
    fn installed(label: &str) -> (tempfile::TempDir, Roots, Remover) {
        let scratch = temp_dir(label);
        let home = scratch.path().join("home");
        let layout = roots(&home);
        Files::ensure_private_dir(&layout.data).unwrap();
        fs::create_dir_all(layout.releases.join("releases").join("1.0.0")).unwrap();
        fs::write(layout.releases.join("releases/1.0.0/binary"), "binary").unwrap();
        let removal = Remover::new(roots(&home)).unwrap();
        (scratch, layout, removal)
    }

    #[test]
    fn only_exact_owned_roots_can_be_uninstalled() {
        let scratch = temp_dir("uninstall-scope");
        let home = scratch.path().join("home");
        fs::create_dir(&home).unwrap();
        assert!(roots(&home).validate().is_ok());
        for unsafe_path in [
            PathBuf::new(),
            PathBuf::from("relative"),
            home.clone(),
            home.join(DATA).join(".."),
            home.join("elsewhere"),
            home.join("elsewhere")
                .join(DATA)
                .join("..")
                .join("elsewhere"),
        ] {
            let mut layout = roots(&home);
            layout.data = unsafe_path;
            assert!(layout.validate().is_err());
        }
        let mut layout = roots(&home);
        layout.releases = home.clone();
        assert!(layout.validate().is_err());
        let mut layout = roots(&home);
        layout.releases = home.join("install");
        assert!(layout.validate().is_err(), "R is inside D");
        let layout = Roots {
            data: home.join(".local").join("share").join(DATA),
            releases: home.join(".local").join("share").join(DATA).join("install"),
            ..roots(&home)
        };
        assert!(
            layout.validate().is_ok(),
            "D is where the OS puts it, however deep"
        );
        let mut layout = roots(&home);
        layout.home = PathBuf::from(std::path::MAIN_SEPARATOR_STR);
        assert!(layout.validate().is_err(), "never the file-system root");
        let layout = roots(&home);
        symlink(&home, &layout.data);
        assert!(layout.validate().is_err(), "a root that is a link");
    }

    #[test]
    fn links_inside_owned_roots_never_delete_their_targets() {
        let scratch = temp_dir("uninstall-links");
        let outside = scratch.path().join("outside");
        let (_scratch, layout, removal) = installed("uninstall-links-home");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), "untouched").unwrap();
        symlink(&outside, &layout.data.join("external"));
        symlink(&outside, &layout.releases.join("current"));
        fs::write(layout.data.join("remove"), "gone").unwrap();

        assert!(removal.preflight().unwrap());
        removal.remove_releases().unwrap();
        let removed = removal.remove_data_contents(&|_| {}).unwrap();
        assert_eq!(removed.len(), 2);
        assert!(!layout.releases.exists());
        assert!(layout.data.exists());
        assert_eq!(
            fs::read_to_string(outside.join("keep")).unwrap(),
            "untouched"
        );
        assert!(!removal.preflight().unwrap());
        assert!(removal.remove_data_contents(&|_| {}).unwrap().is_empty());
    }

    #[test]
    fn what_is_kept_stays_and_the_rest_goes_with_a_retry_route_until_the_end() {
        let (_scratch, layout, removal) = installed("uninstall-kept");
        for name in ["install.lock", "agents.lock", "node-stopped.json", "core"] {
            fs::write(layout.data.join(name), "x").unwrap();
        }
        fs::write(layout.data.join("install.json"), r#"{"command":["x"]}"#).unwrap();
        removal.stage_recovery().unwrap();
        let record: Value =
            serde_json::from_slice(&fs::read(layout.data.join("install.json")).unwrap()).unwrap();
        let program = layout.data.join(RECOVERY).join(RECOVERY_PROGRAM);
        assert_eq!(record["command"], json!([program]));
        assert_eq!(record["releases"], json!(layout.releases));
        assert!(program.is_file(), "a copy of this program");

        assert_eq!(removal.remove_data_contents(&|_| {}).unwrap(), ["core"]);
        assert!(removal.finish_recovery().is_err(), "releases remain");
        removal.remove_releases().unwrap();
        removal.finish_claim().unwrap();
        removal.finish_recovery().unwrap();
        let mut left: Vec<_> = fs::read_dir(&layout.data)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["agents.lock", "install.lock", "node-stopped.json"]);
    }

    #[test]
    fn a_lock_file_is_made_private_and_a_link_in_its_place_is_refused() {
        let scratch = temp_dir("uninstall-lock");
        let (_scratch, layout, removal) = installed("uninstall-lock-home");
        let lock = removal.lock_file("install.lock").unwrap();
        drop(lock);
        assert!(layout.data.join("install.lock").is_file());
        let outside = scratch.path().join("outside");
        fs::write(&outside, "sentinel").unwrap();
        symlink(&outside, &layout.data.join("agents.lock"));
        assert!(removal.lock_file("agents.lock").is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"sentinel");
    }

    #[test]
    fn an_entry_swapped_for_a_link_at_the_claim_is_moved_never_followed() {
        let scratch = temp_dir("uninstall-claim-race");
        let (_home, layout, removal) = installed("uninstall-claim-race-home");
        let victim = layout.data.join("victim");
        let moved = scratch.path().join("moved-outside");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("keep"), "outside sentinel").unwrap();
        let swapped = AtomicBool::new(false);
        let hook = |_: &OsStr| {
            if !swapped.swap(true, Ordering::SeqCst) && fs::rename(&victim, &moved).is_ok() {
                symlink(&moved, &victim);
            }
        };
        let _ = removal.remove_data_contents(&hook);
        assert_eq!(
            fs::read_to_string(moved.join("keep")).unwrap(),
            "outside sentinel"
        );
    }

    #[test]
    fn a_home_replaced_after_it_was_opened_is_refused() {
        let scratch = temp_dir("uninstall-home-race");
        let home = scratch.path().join("home");
        let moved = scratch.path().join("moved-home");
        let layout = roots(&home);
        Files::ensure_private_dir(&layout.data).unwrap();
        fs::write(layout.data.join("keep"), "untouched").unwrap();
        let removal = Remover::new(roots(&home)).unwrap();
        fs::rename(&home, &moved).unwrap();
        symlink(&moved, &home);
        assert!(removal.remove_data_contents(&|_| {}).is_err());
        assert_eq!(
            fs::read_to_string(moved.join(DATA).join("keep")).unwrap(),
            "untouched"
        );
    }

    /// A root, or the home above it, renamed away and replaced by a link to an outside directory while a removal is
    /// working in it: whether the OS refuses the rename while the step holds the directory, or the step sees the swap
    /// and stops, nothing outside is removed.
    #[test]
    fn a_root_or_the_home_swapped_for_a_link_mid_removal_never_reaches_outside() {
        for swapped in [DATA, ""] {
            let (scratch, layout, removal) = installed("uninstall-ancestor-swap");
            fs::write(layout.data.join("entry"), "ours").unwrap();
            let outside = scratch.path().join("outside");
            for root in [DATA.to_owned(), format!("{DATA}/install")] {
                fs::create_dir_all(outside.join(&root)).unwrap();
                fs::write(outside.join(&root).join("keep"), "outside").unwrap();
            }
            fs::write(outside.join("keep"), "outside").unwrap();
            let (victim, target) = if swapped.is_empty() {
                (layout.home.clone(), outside.clone())
            } else {
                (layout.home.join(swapped), outside.join(DATA))
            };
            let moved = scratch.path().join("moved");
            let done = AtomicBool::new(false);
            let hook = |_: &OsStr| {
                if !done.swap(true, Ordering::SeqCst) && fs::rename(&victim, &moved).is_ok() {
                    symlink(&target, &victim);
                }
            };
            let first = removal.remove_data_contents(&hook);
            assert!(
                done.load(Ordering::SeqCst),
                "{swapped:?}: the hook never ran"
            );
            if moved.exists() {
                assert!(
                    first.is_err(),
                    "{swapped:?}: a swap the OS allowed must be refused"
                );
            } else {
                assert!(
                    first.is_ok(),
                    "{swapped:?}: the OS held the directory, so the removal completes"
                );
            }
            let _ = removal.remove_releases();
            let _ = removal.finish_claim();
            for kept in [
                "keep".to_owned(),
                format!("{DATA}/keep"),
                format!("{DATA}/install/keep"),
            ] {
                assert_eq!(
                    fs::read_to_string(outside.join(&kept)).unwrap(),
                    "outside",
                    "{swapped:?}: {kept}"
                );
            }
        }
    }

    #[test]
    fn a_home_named_as_the_os_resolves_it_is_accepted() {
        let scratch = temp_dir("uninstall-resolved");
        let home = scratch.path().join("home");
        fs::create_dir(&home).unwrap();
        let resolved = fs::canonicalize(&home).unwrap();
        assert!(roots(&resolved).validate().is_ok());
        let mut mixed = roots(&home);
        mixed.data = resolved.join(DATA);
        assert!(mixed.validate().is_ok(), "either form, compared as one");
    }
}
