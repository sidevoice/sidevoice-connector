//! Removing the two roots the installer owns (`R`, the releases, and `D`, the data directory) without ever following
//! a link out of them, and without deleting anything that is not ours: the file-system half of `uninstall`.
//!
//! The interface is the [`Removal`] trait; [`Remover`] is the one OS implementation compiled in, chosen below.
//! Callers import both: `use crate::platform::uninstall::{Removal, Remover, Roots};` then
//! `Remover::new(&roots)?.preflight()?`.
//!
//! What every OS guarantees, and what [`Roots::validate`] and the implementations enforce:
//!
//! - **Never the home or above.** `D` is never the home directory, a directory the home is in, or a file-system
//!   or volume root, whatever it holds: no record in it makes it ours.
//! - **Owned roots only.** `D` is either exactly the OS's standard place for this user (`data_dir` with no
//!   override, wherever that is, inside the home or not), or a directory that holds an installation of ours: an
//!   `install.json` of this user's that nobody else can change, naming `R` as its releases (what `install` writes).
//!   `R` is directly inside `D`. Anything else is refused before anything is touched, so a folder that merely has
//!   the program's name is never emptied. Another directory holding nothing but what an uninstall leaves proves
//!   nothing: it is reported ([`Ownership::Leftovers`]) and the caller leaves it as it is.
//! - **The `D` that was checked.** What `D` is (its file id) is recorded when it is checked, or when the removal
//!   makes it, and every later use of `D` is refused when what is there is something else.
//! - **No link is followed.** A link met inside a root is removed as a link; what it points to is untouched.
//! - **No root swapped mid-step.** A root, or a directory above it, replaced by a link while a step runs is never
//!   reached through: the step is refused, or the replacement cannot happen while it runs.
//! - **Claim by rename.** An entry is first renamed into a private claim directory inside `D` and only then
//!   deleted there, so a replacement put at the old name in the meantime is never reached by the deletion.
//! - **The same volume.** Nothing on another mount or volume is entered.
//! - **Retry.** A failure part-way leaves a recovery copy of the running program in `D` and `install.json` naming
//!   it, so running `uninstall` again finishes the work.
//!
//! Every step starts from the anchor: the nearest directory above `D` that exists when the removal starts (the
//! directory `D` is in, unless that is still to be made), compared with the one seen at the start before each use.
//!
//! How they are met differs. On Unix every step is relative to directory descriptors held open from the anchor down
//! (`openat` with `O_NOFOLLOW`), each checked against the path again before it is used, with the mount compared at
//! every directory, walked in full by [`Removal::preflight`] before anything is removed. On Windows each step holds
//! every directory from the volume root down to the one it works in, each opened as itself (a link or junction there
//! is refused) and without letting anybody rename or delete it while held, so the paths the step uses reach those
//! directories and nothing else, a claimed entry is compared
//! before and after its rename, and the standard library's handle-based `remove_dir_all` (which removes a link or
//! junction itself, never what it leads to, so it never leaves the volume) does the deleting. Windows has no
//! recursive preflight and no mount compare: it never enters a reparse point, which is how another volume is mounted
//! there. Everything that does not touch the OS is one body here: the roots, whether `D` is ours, and the retry
//! record.

#[cfg(unix)]
#[path = "uninstall/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "uninstall/windows.rs"]
mod imp;

pub use imp::Os as Remover;

use super::fs::{Files, Fs};
use super::target::{Host, Target as _};
use serde_json::{json, Value};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::path::{Component, Path, PathBuf};

/// The private directory in `D` that entries are renamed into before they are deleted.
const CLAIM: &str = ".uninstall-claim";
/// The private directory in `D` holding the copy of the program that finishes an interrupted uninstall.
const RECOVERY: &str = ".uninstall-recovery";
/// The program inside [`RECOVERY`].
fn recovery_program() -> String {
    Host::exe_name("sidevoice-connector")
}
/// `D`'s record of the installation, which names the recovery program until the very end.
const RECORD: &str = "install.json";
/// The largest [`RECORD`] read.
const RECORD_LIMIT: u64 = 64 * 1024;
/// What a claimed release is called inside [`CLAIM`], before its unique suffix.
const RELEASE_PREFIX: &str = "release-";
/// What a claimed entry of `D` is called inside [`CLAIM`], before its unique suffix.
const DATA_PREFIX: &str = "data-";

/// The roots one uninstall may remove, and what stays in the data directory.
pub struct Roots {
    /// `D`, the data directory (`data_dir`).
    pub data: PathBuf,
    /// `R`: a directory of `D`.
    pub releases: PathBuf,
    /// `D` as the OS places it for this user with no override (`data_dir::standard`); `None` when there is none.
    pub standard: Option<PathBuf>,
    /// The names in `D` that an uninstall leaves (its permanent lock files and the stop).
    pub kept: &'static [&'static str],
    /// This user's home directory; `None` when it is not known, and then nothing is removed.
    pub home: Option<PathBuf>,
}

/// What `D` is to a removal, as [`Removal::ownership`] finds it. Anything that is not ours is refused instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ownership {
    /// The standard place, or a directory holding an installation of ours.
    Ours,
    /// Not the standard place, and not there.
    Absent,
    /// Not the standard place, no installation record, and nothing in it but what an uninstall leaves: nothing
    /// proves it is ours, so it is not written to and nothing in it is deleted.
    Leftovers,
}

/// What a removal reads through the `D` it holds, for [`Roots::check_owned`]: `D`'s names and its `install.json`
/// (read as a trusted file of this user's: a regular file, this user's, that nobody else can change), `None` when
/// `D` is not there.
type Contents = Option<(Vec<OsString>, Option<Vec<u8>>)>;

impl Roots {
    /// That these are plain roots: absolute paths of plain components, `D` not a file-system or volume root, not
    /// the home and not a directory the home is in, `R` directly inside `D`, both real canonical directories when
    /// they exist, and the anchor ([`Roots::anchor`]) one too. Whether `D` is ours is [`Roots::check_owned`]'s,
    /// which [`Removal::new`] asks through what it holds.
    pub fn validate(&self) -> Result<(), &'static str> {
        let data = dunce::simplified(&self.data);
        if !clean_absolute(&self.data)
            || !clean_absolute(&self.releases)
            || data.parent().is_none()
            || dunce::simplified(&self.releases).parent() != Some(data)
            || self.releases.file_name().is_none()
        {
            return Err("uninstall path is outside known Sidevoice roots");
        }
        self.check_below_home()?;
        if !canonical(&self.anchor()?) {
            return Err("unsafe uninstall anchor");
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

    /// That `D` is not the home nor a directory the home is in: compared as written and, when both are there, as
    /// the OS resolves them (so another spelling of the same directory, through a link or in another case, is seen
    /// too). Refused when the home is not known.
    fn check_below_home(&self) -> Result<(), &'static str> {
        const REFUSED: &str =
            "uninstall refused: the data directory is the home directory or a directory it is in";
        let home = self
            .home
            .as_deref()
            .ok_or("uninstall refused: this user's home directory is not known")?;
        if dunce::simplified(home).starts_with(dunce::simplified(&self.data)) {
            return Err(REFUSED);
        }
        if let (Ok(home), Ok(data)) = (
            std::fs::canonicalize(home),
            std::fs::canonicalize(&self.data),
        ) {
            if dunce::simplified(&home).starts_with(dunce::simplified(&data)) {
                return Err(REFUSED);
            }
        }
        Ok(())
    }

    /// The nearest directory above `D` that exists: where every step of a removal starts from. Refused when what is
    /// there is not a directory (a link there is refused by [`Roots::validate`], as not canonical).
    fn anchor(&self) -> Result<PathBuf, &'static str> {
        for dir in self.data.ancestors().skip(1) {
            match std::fs::symlink_metadata(dir) {
                Ok(meta) if meta.file_type().is_symlink() => return Err("unsafe uninstall anchor"),
                Ok(meta) if meta.is_dir() => return Ok(dir.to_path_buf()),
                Ok(_) => return Err("unsafe uninstall anchor"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err("cannot inspect uninstall anchor"),
            }
        }
        Err("unsafe uninstall anchor")
    }

    /// What `D` is: ours when it is exactly the standard place ([`Roots::standard`]) or, as `read` gives it through
    /// the `D` the removal holds, it holds an installation of ours (an `install.json` naming `R` as its releases:
    /// what `install` writes, and the retry record too); else absent, or leftovers when it holds nothing but what an
    /// uninstall leaves (`kept`); refused otherwise. `read` is only asked when `D` is not the standard place.
    fn check_owned(
        &self,
        read: impl FnOnce() -> Result<Contents, &'static str>,
    ) -> Result<Ownership, &'static str> {
        let standard = self
            .standard
            .as_deref()
            .is_some_and(|standard| dunce::simplified(standard) == dunce::simplified(&self.data));
        if standard {
            return Ok(Ownership::Ours);
        }
        let Some((names, record)) = read()? else {
            return Ok(Ownership::Absent);
        };
        let installed = record
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|record| record.get("releases") == Some(&json!(self.releases)));
        let empty = names
            .iter()
            .all(|name| self.kept.iter().any(|kept| OsStr::new(kept) == name));
        if installed {
            Ok(Ownership::Ours)
        } else if empty {
            Ok(Ownership::Leftovers)
        } else {
            Err("uninstall refused: the data directory is not the standard one and holds no installation of ours")
        }
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
    let mut record = match Files::read_trusted(&roots.data.join(RECORD), RECORD_LIMIT) {
        Ok(None) => json!({}),
        Ok(Some(bytes)) => serde_json::from_slice::<Value>(&bytes)
            .ok()
            .filter(Value::is_object)
            .ok_or("unsafe uninstall install record")?,
        Err(_) => return Err("unsafe uninstall install record"),
    };
    record["command"] = json!([roots.data.join(RECOVERY).join(recovery_program())]);
    record["releases"] = json!(roots.releases);
    Ok(record)
}

/// What every OS implements ([`Remover`], in `uninstall/unix.rs` and `uninstall/windows.rs`). Every method that
/// can fail says why in a fixed phrase, and every one re-checks what it relies on, so a root swapped for a link
/// between two calls is refused, not followed.
pub trait Removal: Sized {
    /// Holds `roots` (validated here) so that the steps below act on them and on nothing else, and refuses a `D`
    /// that is not ours ([`Roots::check_owned`], read through what it holds) before anything is touched.
    /// What `D` is (its file id) is recorded here when it is there; every later use of `D` compares it.
    fn new(roots: Roots) -> Result<Self, &'static str>;

    /// The roots this removal is for.
    fn roots(&self) -> &Roots;

    /// What `D` is now ([`Roots::check_owned`], read again through the `D` recorded by [`Removal::new`] or made by
    /// [`Removal::lock_file`]); refused when it is not ours.
    fn ownership(&self) -> Result<Ownership, &'static str>;

    /// Checks both roots without removing anything, refusing at the first thing that is not safe to remove (a mount
    /// or volume boundary, an entry that is not what it was, a running program this OS cannot delete). Whether
    /// there are releases to remove.
    fn preflight(&self) -> Result<bool, &'static str>;

    /// The lock file `name` in `D` (created private when missing), opened so that a link in its place is refused;
    /// the caller then takes the lock on it (`lock::wait_lock_file` checks the rest). `D` is made when missing and
    /// recorded as made; a `D` missing when the removal started that something else made since is refused.
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
    use crate::platform::env::{Env, Environment};
    use crate::platform::fs::{Files, TestFiles};
    use crate::test_support::temp_dir;
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};

    const KEPT: &[&str] = &["install.lock", "agents.lock", "node-stopped.json"];

    const DATA: &str = Environment::DATA_NAME;

    /// `D` at `data`, `R` inside it, `standard` as the OS's place for this user and the directory it is in as the
    /// home.
    fn at(data: PathBuf, standard: &Path) -> Roots {
        Roots {
            releases: data.join("install"),
            data,
            standard: Some(standard.to_path_buf()),
            kept: KEPT,
            home: standard.parent().map(Path::to_path_buf),
        }
    }

    /// The standard place in a scratch home.
    fn roots(home: &Path) -> Roots {
        at(home.join(DATA), &home.join(DATA))
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
    fn only_plain_roots_with_the_releases_inside_the_data_are_valid() {
        let scratch = temp_dir("uninstall-scope");
        let home = scratch.path().join("home");
        fs::create_dir(&home).unwrap();
        assert!(roots(&home).validate().is_ok());
        for unsafe_path in [
            PathBuf::new(),
            PathBuf::from("relative"),
            PathBuf::from(std::path::MAIN_SEPARATOR_STR),
            home.join(DATA).join(".."),
            home.join("elsewhere").join("..").join(DATA),
        ] {
            let layout = at(unsafe_path, &home.join(DATA));
            assert!(layout.validate().is_err(), "{}", layout.data.display());
        }
        let mut layout = roots(&home);
        layout.releases = home.clone();
        assert!(layout.validate().is_err());
        let mut layout = roots(&home);
        layout.releases = home.join("install");
        assert!(layout.validate().is_err(), "R is inside D");
        let deep = home.join(".local").join("share").join(DATA);
        assert!(
            at(deep.clone(), &deep).validate().is_ok(),
            "D is where the OS puts it, however deep"
        );
        let layout = roots(&home);
        Files::symlink(&home, &layout.data);
        assert!(layout.validate().is_err(), "a root that is a link");
    }

    /// A private `D` at `data` holding `entry`.
    fn with_entry(data: &Path) {
        Files::ensure_private_dir(data).unwrap();
        fs::write(data.join("entry"), "theirs").unwrap();
    }

    #[test]
    fn a_folder_that_only_has_the_programs_name_is_never_emptied() {
        let scratch = temp_dir("uninstall-unrelated");
        let home = scratch.path().join("home");
        let standard = home.join(".local").join("share").join(DATA);
        let unrelated = home.join("Documents").join(DATA);
        with_entry(&unrelated);
        assert!(Remover::new(at(unrelated.clone(), &standard)).is_err());
        Files::write_json(
            &unrelated.join(RECORD),
            &json!({"releases": home.join("install")}),
        )
        .unwrap();
        assert!(
            Remover::new(at(unrelated.clone(), &standard)).is_err(),
            "a record naming other releases is not ours"
        );
        assert_eq!(
            fs::read_to_string(unrelated.join("entry")).unwrap(),
            "theirs"
        );
    }

    #[test]
    fn the_standard_place_is_accepted_wherever_the_os_puts_it() {
        let scratch = temp_dir("uninstall-standard");
        let elsewhere = scratch.path().join("mnt").join("user-data").join(DATA);
        with_entry(&elsewhere);
        let removal = Remover::new(at(elsewhere.clone(), &elsewhere)).unwrap();
        assert_eq!(removal.remove_data_contents(&|_| {}).unwrap(), ["entry"]);
    }

    #[test]
    fn another_directory_is_ours_only_with_an_installation_of_ours_in_it() {
        let scratch = temp_dir("uninstall-custom");
        let standard = scratch.path().join("home").join(DATA);
        let custom = scratch.path().join("custom").join("profile");
        let ownership = |removal: Remover| removal.ownership().unwrap();
        assert_eq!(
            ownership(Remover::new(at(custom.clone(), &standard)).unwrap()),
            Ownership::Absent,
            "not there: nothing to remove"
        );
        Files::ensure_private_dir(&custom).unwrap();
        fs::write(custom.join(KEPT[2]), "theirs").unwrap();
        assert_eq!(
            ownership(Remover::new(at(custom.clone(), &standard)).unwrap()),
            Ownership::Leftovers,
            "only what an uninstall leaves proves nothing"
        );
        with_entry(&custom);
        let layout = at(custom.clone(), &standard);
        Files::write_json(
            &custom.join(RECORD),
            &json!({"command": ["x"], "releases": layout.releases}),
        )
        .unwrap();
        let removal = Remover::new(layout).unwrap();
        assert_eq!(removal.ownership().unwrap(), Ownership::Ours);
        assert_eq!(removal.remove_data_contents(&|_| {}).unwrap(), ["entry"]);
    }

    #[test]
    fn the_home_a_directory_it_is_in_or_a_root_is_never_emptied_whatever_it_holds() {
        let scratch = temp_dir("uninstall-home");
        let outer = scratch.path().join("outer");
        let home = outer.join("home");
        let standard = home.join(".local").join("share").join(DATA);
        Files::ensure_private_dir(&home).unwrap();
        fs::write(home.join("entry"), "theirs").unwrap();
        let root = home
            .ancestors()
            .last()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        for data in [home.clone(), outer, root] {
            let layout = at(data.clone(), &standard);
            if data.parent().is_some() {
                Files::write_json(
                    &data.join(RECORD),
                    &json!({"command": ["x"], "releases": layout.releases}),
                )
                .unwrap();
            }
            assert!(Remover::new(layout).is_err(), "{}", data.display());
        }
        let mut unknown = at(home.join("custom"), &standard);
        unknown.home = None;
        assert!(Remover::new(unknown).is_err(), "an unknown home");
        let mut spelled = at(home.clone(), &standard);
        spelled.home = Some(fs::canonicalize(&home).unwrap().join("."));
        assert!(Remover::new(spelled).is_err(), "the home written otherwise");
        assert_eq!(fs::read_to_string(home.join("entry")).unwrap(), "theirs");
    }

    #[test]
    fn a_data_directory_swapped_for_another_after_it_was_checked_is_refused_and_left_untouched() {
        // Between two steps (nothing is held then, on any OS), and in the middle of one.
        for during_step in [false, true] {
            let (scratch, layout, removal) = installed("uninstall-data-swap");
            fs::write(layout.data.join("entry"), "ours").unwrap();
            let moved = scratch.path().join("moved");
            let swapped = AtomicBool::new(false);
            let ran = AtomicBool::new(false);
            let swap = || {
                ran.store(true, Ordering::SeqCst);
                if fs::rename(&layout.data, &moved).is_ok() {
                    with_entry(&layout.data);
                    swapped.store(true, Ordering::SeqCst);
                }
            };
            if during_step {
                let once = AtomicBool::new(false);
                let hook = |_: &OsStr| {
                    if !once.swap(true, Ordering::SeqCst) {
                        swap();
                    }
                };
                let result = removal.remove_data_contents(&hook);
                assert!(ran.load(Ordering::SeqCst), "the hook never ran");
                assert!(
                    !swapped.load(Ordering::SeqCst) || result.is_err(),
                    "a swap the OS allowed must be refused"
                );
            } else {
                swap();
                assert!(swapped.load(Ordering::SeqCst));
            }
            if !swapped.load(Ordering::SeqCst) {
                continue;
            }
            assert!(removal.ownership().is_err());
            assert!(removal.preflight().is_err());
            assert!(removal.lock_file("install.lock").is_err());
            assert!(removal.stage_recovery().is_err());
            assert!(removal.remove_data_contents(&|_| {}).is_err());
            assert!(removal.remove_releases().is_err());
            assert!(removal.finish_claim().is_err());
            assert!(removal.finish_recovery().is_err());
            let mut left: Vec<_> = fs::read_dir(&layout.data)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            left.sort();
            assert_eq!(left, ["entry"], "the substitute is untouched");
            assert_eq!(
                fs::read_to_string(layout.data.join("entry")).unwrap(),
                "theirs"
            );
        }
    }

    #[test]
    fn links_inside_owned_roots_never_delete_their_targets() {
        let scratch = temp_dir("uninstall-links");
        let outside = scratch.path().join("outside");
        let (_scratch, layout, removal) = installed("uninstall-links-home");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), "untouched").unwrap();
        Files::symlink(&outside, layout.data.join("external"));
        Files::symlink(&outside, layout.releases.join("current"));
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
        let program = layout.data.join(RECOVERY).join(recovery_program());
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
        Files::symlink(&outside, layout.data.join("agents.lock"));
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
                Files::symlink(&moved, &victim);
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
        Files::symlink(&moved, &home);
        assert!(removal.remove_data_contents(&|_| {}).is_err());
        assert_eq!(
            fs::read_to_string(moved.join(DATA).join("keep")).unwrap(),
            "untouched"
        );
    }

    /// A root, or the directory above it (the anchor), renamed away and replaced by a link to an outside directory while a removal is
    /// working in it: whether the OS refuses the rename while the step holds the directory, or the step sees the swap
    /// and stops, nothing outside is removed.
    #[test]
    fn a_root_or_the_directory_above_it_swapped_for_a_link_mid_removal_never_reaches_outside() {
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
                (scratch.path().join("home"), outside.clone())
            } else {
                (layout.data.clone(), outside.join(DATA))
            };
            let moved = scratch.path().join("moved");
            let done = AtomicBool::new(false);
            let hook = |_: &OsStr| {
                if !done.swap(true, Ordering::SeqCst) && fs::rename(&victim, &moved).is_ok() {
                    Files::symlink(&target, &victim);
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
