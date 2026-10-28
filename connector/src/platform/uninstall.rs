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
//! Claim publication and final empty-root/lock cleanup remain OS-specific here. Recursive preflight and
//! payload deletion have one owner: `sidevoice_platform::removal::Removal` on `HeldDirectory`. It verifies
//! held ancestry, refuses unsafe directories and mount boundaries, and never traverses links/reparse points.
//! The connector selects claim names and retains recovery until every payload has been removed.

#[cfg(unix)]
#[path = "uninstall/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "uninstall/windows.rs"]
mod imp;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
use linux as os;
#[cfg(target_os = "macos")]
use macos as os;

pub use imp::Os as Remover;

use super::data_dir;
use super::fs::{Files, Fs};
use super::target;
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
    target::exe_name("sidevoice-connector")
}
/// `D`'s record of the installation, which names the recovery program until the very end.
const RECORD: &str = "install.json";
/// The largest [`RECORD`] read.
const RECORD_LIMIT: u64 = 64 * 1024;
/// What a claimed release is called inside [`CLAIM`], before its unique suffix.
const RELEASE_PREFIX: &str = "release-";
/// What a claimed entry of `D` is called inside [`CLAIM`], before its unique suffix.
const DATA_PREFIX: &str = "data-";

const MODELS_UNRESOLVED: &str =
    "uninstall refused: cannot tell where the shared models folder is relative to the data directory";
/// `path` as the OS resolves it: its longest existing ancestor canonicalized, the components that do not exist yet
/// appended as written. Fails closed: only an entry that is not there walks up to the parent (a link whose target is
/// missing is refused); any other error (permission, a link
/// loop...) or a tail that cannot be placed without the OS (`..`) is an error, never a guess.
fn resolved(path: &Path, unresolved: &'static str) -> Result<PathBuf, &'static str> {
    let mut missing = Vec::new();
    let mut existing = path;
    loop {
        match std::fs::canonicalize(existing) {
            Ok(real) => {
                return Ok(missing
                    .iter()
                    .rev()
                    .fold(real, |real, part| real.join(part)));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Not found is also what a link whose target is missing says: only an entry that is truly not there is
                // walked past; a dangling link (symlink, junction) cannot be placed, so it refuses.
                match std::fs::symlink_metadata(existing) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    _ => return Err(unresolved),
                }
            }
            Err(_) => return Err(unresolved),
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name);
                existing = parent;
            }
            _ => return Err(unresolved),
        }
    }
}

/// A live service may replace entries during preflight. Retry the read-only shared check at most five
/// times, always rerunning every safety check. No deletion or claim is retried by this helper.
fn check_live_contents(mut check: impl FnMut() -> std::io::Result<()>) -> Result<(), &'static str> {
    for _ in 0..5 {
        if check().is_ok() {
            return Ok(());
        }
    }
    Err("cannot preflight uninstall contents")
}

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
    /// The names in `D` that the connector (or the core it runs) creates, and so the only ones an uninstall removes:
    /// `D` can be shared with a program that is not ours (the desktop app installs into the standard place), and what
    /// is not listed is left where it is and reported. `None` removes every entry (a `D` known to be only ours).
    pub owned: Option<&'static [&'static str]>,
    /// Every home directory the code knows: the profile's (`HOME`, `USERPROFILE`) and the OS's, which differ on Windows
    /// (the Known Folder). `D` may be none of them nor a directory one is in; none known, and nothing is removed.
    pub homes: Vec<PathBuf>,
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
        self.check_models_outside()?;
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

    /// That `D` is not the home nor a directory the home is in: compared as written and as
    /// the OS resolves them (so another spelling of the same directory, through a link or in another case, is seen
    /// too). Refused when the home is not known.
    fn check_below_home(&self) -> Result<(), &'static str> {
        const REFUSED: &str =
            "uninstall refused: the data directory is the home directory or a directory it is in";
        const UNRESOLVED_HOME: &str =
            "uninstall refused: cannot tell where the home directory is relative to the data directory";
        if self.homes.is_empty() {
            return Err("uninstall refused: this user's home directory is not known");
        }
        for home in &self.homes {
            if dunce::simplified(home).starts_with(dunce::simplified(&self.data)) {
                return Err(REFUSED);
            }
            // As the OS resolves them: a home that does not exist yet is placed by its longest existing ancestor (it may
            // be a link into `D`), and a path that cannot be resolved refuses.
            let home = resolved(home, UNRESOLVED_HOME)?;
            let data = resolved(&self.data, UNRESOLVED_HOME)?;
            if dunce::simplified(&home).starts_with(dunce::simplified(&data)) {
                return Err(REFUSED);
            }
        }
        Ok(())
    }

    /// That the shared models folder (`Models` in the standard place: `standard/Models`, wherever `D` is) is not inside `D`
    /// deeper than `D/Models` itself, nor holds `D`: `D/Models` is kept by name, but a store further down (an overridden `D`
    /// above the standard place) would be deleted with its parent. Refused before anything is touched, rather than
    /// emptying around it: the least surprising, since the person chose a `D` that contains another program's data.
    fn check_models_outside(&self) -> Result<(), &'static str> {
        const REFUSED: &str = "uninstall refused: the shared models folder is inside the data directory (or holds it); \
                               choose a data directory that does not contain it";
        let Some(models) = self
            .standard
            .as_ref()
            .map(|standard| standard.join(data_dir::MODELS))
        else {
            return Ok(());
        };
        let overlaps = |models: &Path, data: &Path| {
            let (models, data) = (dunce::simplified(models), dunce::simplified(data));
            data.starts_with(models)
                || (models.starts_with(data) && models != data.join(data_dir::MODELS))
        };
        if overlaps(&models, &self.data) {
            return Err(REFUSED);
        }
        // As the OS resolves them, whether or not the store exists yet (another instance may create it later).
        if overlaps(
            &resolved(&models, MODELS_UNRESOLVED)?,
            &resolved(&self.data, MODELS_UNRESOLVED)?,
        ) {
            return Err(REFUSED);
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

    /// Whether `name` is a file the connector itself creates in `D`, and so may be removed with `D`: the names it
    /// `kept`, except the shared models folder, which is the engine's.
    fn is_own(&self, name: &OsStr) -> bool {
        self.kept.iter().any(|kept| {
            OsStr::new(kept) == name && *kept != data_dir::MODELS && *kept != "node-stopped.json"
        })
    }

    /// Whether `name` in `D` is not the connector's: not in [`Roots::owned`] (the retry record, the claim and recovery
    /// directories and `R` are ours and never foreign).
    fn is_foreign(&self, name: &OsStr) -> bool {
        self.owned.is_some_and(|owned| {
            !owned.iter().any(|own| OsStr::new(own) == name)
                && ![RECORD, CLAIM, RECOVERY]
                    .iter()
                    .any(|own| OsStr::new(own) == name)
                && self.releases.file_name() != Some(name)
        })
    }

    /// The names in `D` that are never claimed as data: what the uninstall `kept`, the retry record, the claim and the
    /// recovery directories, and `R` (removed as the releases, by itself).
    fn is_kept(&self, name: &OsStr) -> bool {
        self.is_foreign(name)
            || self
                .kept
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

/// What [`Removal::remove_data_dir`] found and did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DataDir {
    /// `D` itself is gone.
    pub removed: bool,
    /// Preserved entries: the stop marker, or foreign entries which keep everything beside them.
    pub kept: Vec<String>,
    /// Entries of ours that could not be removed because something holds them (an open file, a process whose
    /// working directory it is); they and `D` are left. `D`'s own name when it is `D` that is held.
    pub held: Vec<String>,
}

/// The names in `names` that are not ours, sorted.
fn foreign(roots: &Roots, names: &[OsString]) -> Vec<String> {
    let mut kept: Vec<String> = names
        .iter()
        .filter(|name| !roots.is_own(name) && *name != "node-stopped.json")
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    kept.sort();
    kept
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

    /// The very last step, once [`Removal::finish_recovery`] is done while the install lock is still held:
    /// `before_remove` runs before each unlink, for deterministic interleaving checks. The install lock is unlinked last.
    /// When `D` holds only files of ours ([`Roots::is_own`]: its locks) they are removed, preserving the stop marker;
    /// when it holds anything else, nothing is removed and the other names are reported as kept. Nothing is
    /// followed: a link among them is removed as a link, a directory of that name is never entered. A file that
    /// in use is reported as held; other cleanup errors are non-fatal to uninstall; nothing outside `D` is touched.
    fn remove_data_dir(&self, before_remove: &dyn Fn(&OsStr)) -> Result<DataDir, &'static str>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::data_dir;
    use crate::platform::fs::{Files, TestFiles};
    use crate::test_support::temp_dir;
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};

    const KEPT: &[&str] = &[
        "install.lock",
        "connector.lock",
        "agents.lock",
        "node-stopped.json",
    ];

    /// The program's own directory name inside the standard place (what the shared rule picks for this OS).
    fn data_name() -> &'static str {
        static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        NAME.get_or_init(|| {
            data_dir::standard()
                .and_then(|path| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .expect("a standard data directory")
        })
    }

    /// `D` at `data`, `R` inside it, `standard` as the OS's place for this user and the directory it is in as the
    /// home.
    fn at(data: PathBuf, standard: &Path) -> Roots {
        Roots {
            releases: data.join("install"),
            data,
            standard: Some(standard.to_path_buf()),
            kept: KEPT,
            owned: None,
            homes: standard
                .parent()
                .map(Path::to_path_buf)
                .into_iter()
                .collect(),
        }
    }

    /// The standard place in a scratch home.
    fn roots(home: &Path) -> Roots {
        at(home.join(data_name()), &home.join(data_name()))
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
        // `..` spelled from the home's plain form: below a `\\?\` path (Windows' canonical form) `join` resolves it
        // instead of keeping it.
        let plain = dunce::simplified(&home);
        for unsafe_path in [
            PathBuf::new(),
            PathBuf::from("relative"),
            PathBuf::from(std::path::MAIN_SEPARATOR_STR),
            plain.join(data_name()).join(".."),
            plain.join("elsewhere").join("..").join(data_name()),
        ] {
            let layout = at(unsafe_path, &home.join(data_name()));
            assert!(layout.validate().is_err(), "{}", layout.data.display());
        }
        let mut layout = roots(&home);
        layout.releases = home.clone();
        assert!(layout.validate().is_err());
        let mut layout = roots(&home);
        layout.releases = home.join("install");
        assert!(layout.validate().is_err(), "R is inside D");
        let deep = home.join(".local").join("share").join(data_name());
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
        let standard = home.join(".local").join("share").join(data_name());
        let unrelated = home.join("Documents").join(data_name());
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
        let elsewhere = scratch
            .path()
            .join("mnt")
            .join("user-data")
            .join(data_name());
        with_entry(&elsewhere);
        let removal = Remover::new(at(elsewhere.clone(), &elsewhere)).unwrap();
        assert_eq!(removal.remove_data_contents(&|_| {}).unwrap(), ["entry"]);
    }

    #[test]
    fn another_directory_is_ours_only_with_an_installation_of_ours_in_it() {
        let scratch = temp_dir("uninstall-custom");
        let standard = scratch.path().join("home").join(data_name());
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

    #[cfg(unix)]
    #[test]
    fn a_standard_place_reaching_into_the_data_directory_through_a_link_is_refused_before_the_store_exists(
    ) {
        let scratch = temp_dir("uninstall-models-alias");
        let home = scratch.path().join("home");
        let data = home.join("data");
        Files::ensure_private_dir(&data).unwrap();
        fs::create_dir_all(data.join("nested/sidevoice")).unwrap();
        let alias = home.join("xdg-alias");
        Files::symlink(data.join("nested"), &alias);
        // `Models` does not exist yet: another instance creates it later, inside `D`.
        let mut layout = at(data, &alias.join("sidevoice"));
        layout.homes = vec![home];
        let refused = layout.validate().unwrap_err();
        assert!(refused.contains("shared models folder"), "{refused}");
    }

    #[cfg(unix)]
    #[test]
    fn a_standard_place_that_cannot_be_resolved_is_refused() {
        let scratch = temp_dir("uninstall-models-unresolved");
        let home = scratch.path().join("home");
        let data = home.join("data");
        Files::ensure_private_dir(&data).unwrap();
        fs::create_dir_all(data.join("nested")).unwrap();
        let alias = home.join("xdg-alias");
        Files::symlink(data.join("nested"), &alias);
        let looped = home.join("loop");
        Files::symlink(looped.clone(), &looped);
        // A missing tail with `..` leads back into `D` through the alias; a link loop cannot be resolved at all.
        for standard in [
            alias.join("missing").join("..").join("sidevoice"),
            alias.join("missing").join(".."),
            looped.join("sidevoice"),
        ] {
            let mut layout = at(data.clone(), &standard);
            layout.homes = vec![home.clone()];
            let refused = layout.validate().unwrap_err();
            assert!(
                refused.contains("shared models folder"),
                "{}: {refused}",
                standard.display()
            );
        }
    }

    #[test]
    fn an_overridden_data_directory_that_contains_the_shared_models_is_refused_untouched() {
        let scratch = temp_dir("uninstall-models");
        let home = scratch.path().join("home");
        let share = home.join(".local").join("share");
        let standard = share.join(data_name());
        Files::ensure_private_dir(&share).unwrap();
        let models = standard.join(data_dir::MODELS);
        Files::ensure_private_dir(&standard).unwrap();
        fs::create_dir_all(models.join("profile")).unwrap();
        fs::create_dir_all(models.join("other-program")).unwrap();
        fs::write(models.join("other-program").join("sentinel"), "theirs").unwrap();
        // `D` above the standard place holds the store two levels down; `D` inside the store holds `D`.
        for data in [share.clone(), models.join("profile")] {
            let mut layout = at(data.clone(), &standard);
            layout.homes = vec![home.clone()];
            let refused = layout.validate().unwrap_err();
            assert!(
                refused.contains("shared models folder"),
                "{}: {refused}",
                data.display()
            );
        }
        // `D` as the standard place itself keeps `Models` by name: accepted.
        let mut layout = at(standard.clone(), &standard);
        layout.homes = vec![home];
        assert!(layout.validate().is_ok());
        assert_eq!(
            fs::read_to_string(models.join("other-program").join("sentinel")).unwrap(),
            "theirs"
        );
    }

    #[test]
    fn the_home_a_directory_it_is_in_or_a_root_is_never_emptied_whatever_it_holds() {
        let scratch = temp_dir("uninstall-home");
        let outer = scratch.path().join("outer");
        let home = outer.join("home");
        let standard = home.join(".local").join("share").join(data_name());
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
        unknown.homes.clear();
        assert!(Remover::new(unknown).is_err(), "an unknown home");
        let mut spelled = at(home.clone(), &standard);
        spelled.homes = vec![fs::canonicalize(&home).unwrap().join(".")];
        assert!(Remover::new(spelled).is_err(), "the home written otherwise");
        // A home that does not exist yet, below a link into `D`: placed by its longest existing ancestor, so it is `D`'s.
        let inside = home.join("sub");
        fs::create_dir_all(&inside).unwrap();
        let alias = scratch.path().join("alias");
        Files::symlink(&inside, &alias);
        let mut future = at(home.clone(), &standard);
        future.homes = vec![alias.join("future")];
        let refused = Remover::new(future)
            .err()
            .expect("a home below a link into D");
        assert!(
            refused.contains("the home directory or a directory it is in"),
            "{refused}"
        );
        // The same through a link whose target is missing: it cannot be placed, so it refuses.
        let dangling = scratch.path().join("dangling");
        Files::symlink(home.join("missing"), &dangling);
        let mut gone = at(home.clone(), &standard);
        gone.homes = vec![dangling.join("future")];
        assert!(Remover::new(gone).is_err(), "a home below a dangling link");
        // The profile's home and the OS's are two places on Windows: either one is protected, whichever the data
        // directory is compared with.
        let elsewhere = scratch.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        for homes in [
            vec![home.clone(), elsewhere.clone()],
            vec![elsewhere.clone(), home.clone()],
        ] {
            let mut mismatch = at(home.clone(), &standard);
            mismatch.homes = homes;
            assert!(Remover::new(mismatch).is_err(), "one of two homes");
        }
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

    // A backslash is an ordinary filename character on Unix, but a separator on Windows.
    #[cfg(unix)]
    #[test]
    fn unix_backslashes_in_existing_data_names_are_removed() {
        for directory in [false, true] {
            let (_home, layout, removal) = installed("uninstall-backslash");
            let entry = layout.data.join(r"session\backup");
            if directory {
                fs::create_dir(&entry).unwrap();
                fs::write(entry.join("payload"), "session data").unwrap();
            } else {
                fs::write(&entry, "session data").unwrap();
            }

            assert!(removal.preflight().unwrap());
            emptied(&removal);
            assert!(!entry.exists());
            assert!(removal.remove_data_dir(&|_| {}).unwrap().removed);
            assert!(!layout.data.exists());
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
    fn live_preflight_retries_but_remains_bounded() {
        let mut attempts = 0;
        check_live_contents(|| {
            attempts += 1;
            if attempts < 5 {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "service replaced an entry",
                ))
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(attempts, 5);
        attempts = 0;
        assert!(check_live_contents(|| {
            attempts += 1;
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "unsafe directory",
            ))
        })
        .is_err());
        assert_eq!(attempts, 5);
    }

    #[test]
    fn claimed_data_retry_preserves_releases_recovery_models_and_locks() {
        let (_home, layout, _) = installed("uninstall-shared-retry");
        let mut selected = at(layout.data.clone(), &layout.data);
        selected.kept = &[
            "install.lock",
            "connector.lock",
            "agents.lock",
            "node-stopped.json",
            data_dir::MODELS,
        ];
        let removal = Remover::new(selected).unwrap();
        fs::create_dir(layout.data.join(data_dir::MODELS)).unwrap();
        fs::write(layout.data.join(data_dir::MODELS).join("model"), "keep").unwrap();
        fs::write(layout.data.join("install.lock"), "keep").unwrap();
        removal.stage_recovery().unwrap();
        Files::ensure_private_dir(&layout.data.join(CLAIM)).unwrap();
        let release = layout.data.join(CLAIM).join("release-interrupted");
        fs::create_dir(&release).unwrap();
        fs::write(release.join("payload"), "release").unwrap();
        let data = layout.data.join(CLAIM).join("data-interrupted");
        fs::create_dir(&data).unwrap();
        fs::write(data.join("payload"), "data").unwrap();
        removal.remove_data_contents(&|_| {}).unwrap();
        assert!(!data.exists());
        assert!(release.join("payload").exists());
        assert!(layout.data.join(RECOVERY).join(recovery_program()).exists());
        assert!(layout.data.join(RECORD).exists());
        assert!(layout.data.join(data_dir::MODELS).join("model").exists());
        assert!(layout.data.join("install.lock").exists());
        removal.remove_releases().unwrap();
        removal.finish_claim().unwrap();
        removal.finish_recovery().unwrap();
        assert!(layout.data.join(data_dir::MODELS).join("model").exists());
    }

    // These fixtures require Windows sharing modes; the assertions use the removal interface.
    #[cfg(windows)]
    #[test]
    fn busy_claimed_payload_keeps_recovery_and_retries_after_handle_closes() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        let (_home, layout, removal) = installed("uninstall-busy-payload");
        removal.stage_recovery().unwrap();
        Files::ensure_private_dir(&layout.data.join(CLAIM)).unwrap();
        let claimed = layout.data.join(CLAIM).join("data-interrupted");
        fs::create_dir(&claimed).unwrap();
        let busy = claimed.join("busy");
        fs::write(&busy, "payload").unwrap();
        let handle = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&busy)
            .unwrap();
        assert!(removal.remove_data_contents(&|_| {}).is_err());
        assert!(removal.finish_claim().is_err());
        assert!(removal.finish_recovery().is_err());
        assert!(layout.data.join(RECOVERY).join(recovery_program()).exists());
        assert!(layout.data.join(RECORD).exists());
        drop(handle);
        removal.remove_data_contents(&|_| {}).unwrap();
        removal.remove_releases().unwrap();
        removal.finish_claim().unwrap();
        removal.finish_recovery().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn final_cleanup_reports_a_lock_held_without_delete_sharing() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        let (_home, layout, removal) = installed("uninstall-busy-lock");
        let busy = layout.data.join("connector.lock");
        fs::write(&busy, "held").unwrap();
        let handle = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&busy)
            .unwrap();
        emptied(&removal);
        assert_eq!(
            removal.remove_data_dir(&|_| {}).unwrap().held,
            ["connector.lock"]
        );
        drop(handle);
        assert!(removal.remove_data_dir(&|_| {}).unwrap().held.is_empty());
    }

    /// `D` emptied the way an uninstall does, up to the last step.
    fn emptied(removal: &Remover) {
        removal.stage_recovery().unwrap();
        removal.remove_data_contents(&|_| {}).unwrap();
        removal.remove_releases().unwrap();
        removal.finish_claim().unwrap();
        removal.finish_recovery().unwrap();
    }

    #[test]
    fn cleanup_retires_the_install_lock_after_every_other_lock() {
        use crate::lock::try_lock;
        let (_home, layout, removal) = installed("uninstall-lock-order");
        fs::write(layout.data.join("node-stopped.json"), "{}").unwrap();
        let install = layout.data.join("install.lock");
        let connector = layout.data.join("connector.lock");
        let agents = layout.data.join("agents.lock");
        let install_held = try_lock(&install, "uninstall").unwrap().unwrap();
        let connector_held = try_lock(&connector, "connector").unwrap().unwrap();
        let agents_held = try_lock(&agents, "agents").unwrap().unwrap();
        for path in [&install, &connector, &agents] {
            assert!(
                try_lock(path, "replacement").unwrap().is_none(),
                "original still holds {}",
                path.display()
            );
        }
        // The service is stopped before cleanup; installers must take install.lock
        // before they can start a replacement daemon or agents operation.
        drop(connector_held);
        emptied(&removal);
        let order = std::cell::RefCell::new(Vec::new());
        removal
            .remove_data_dir(&|name| {
                assert!(try_lock(&install, "replacement-install").unwrap().is_none());
                if name == "install.lock" {
                    assert!(
                        !connector.exists(),
                        "daemon lock retired before installs can resume"
                    );
                    assert!(
                        !agents.exists(),
                        "agents lock retired before installs can resume"
                    );
                }
                order.borrow_mut().push(name.to_os_string());
            })
            .unwrap();
        assert_eq!(order.borrow().last().unwrap(), "install.lock");
        drop(agents_held);
        drop(install_held);
        let _replacement_install = try_lock(&install, "install").unwrap().unwrap();
        let _replacement_connector = try_lock(&connector, "connector").unwrap().unwrap();
        assert!(try_lock(&connector, "second-connector").unwrap().is_none());
    }

    #[test]
    fn cleanup_preserves_the_stop_and_removes_a_link_only_as_a_link() {
        let scratch = temp_dir("uninstall-own");
        let (_home, layout, removal) = installed("uninstall-own-home");
        let outside = scratch.path().join("outside");
        fs::write(&outside, "sentinel").unwrap();
        fs::write(layout.data.join("install.lock"), "x").unwrap();
        fs::write(layout.data.join("node-stopped.json"), "{}").unwrap();
        Files::symlink(&outside, layout.data.join("agents.lock"));
        emptied(&removal);
        assert_eq!(
            removal.remove_data_dir(&|_| {}).unwrap(),
            DataDir {
                kept: vec!["node-stopped.json".into()],
                ..DataDir::default()
            }
        );
        assert!(layout.data.join("node-stopped.json").exists());
        assert_eq!(fs::read(&outside).unwrap(), b"sentinel");
        assert!(layout.data.parent().unwrap().is_dir(), "only D goes");
    }

    #[test]
    fn a_file_that_is_not_ours_keeps_the_directory_and_everything_in_it() {
        let (_home, layout, removal) = installed("uninstall-foreign");
        fs::write(layout.data.join("install.lock"), "x").unwrap();
        emptied(&removal);
        fs::write(layout.data.join("stranger"), "theirs").unwrap();
        let left = removal.remove_data_dir(&|_| {}).unwrap();
        assert_eq!(
            left,
            DataDir {
                kept: vec!["stranger".into()],
                ..DataDir::default()
            }
        );
        assert_eq!(
            fs::read_to_string(layout.data.join("stranger")).unwrap(),
            "theirs"
        );
        assert!(
            layout.data.join("install.lock").exists(),
            "ours stay beside it"
        );
    }

    #[test]
    fn cleanup_never_enters_a_directory_under_an_own_file_name() {
        let (_home, layout, removal) = installed("uninstall-held");
        fs::write(layout.data.join("agents.lock"), "x").unwrap();
        // A name of ours that the OS will not remove as a file (any host): a directory with something in it.
        fs::create_dir(layout.data.join("install.lock")).unwrap();
        fs::write(layout.data.join("install.lock").join("inside"), "x").unwrap();
        emptied(&removal);
        if let Ok(left) = removal.remove_data_dir(&|_| {}) {
            assert_eq!(left.held, ["install.lock"]);
        }
        assert!(layout.data.is_dir());
        assert!(
            layout.data.join("install.lock").join("inside").exists(),
            "never entered"
        );
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
            fs::read_to_string(moved.join(data_name()).join("keep")).unwrap(),
            "untouched"
        );
    }

    /// A root, or the directory above it (the anchor), renamed away and replaced by a link to an outside directory while a removal is
    /// working in it: whether the OS refuses the rename while the step holds the directory, or the step sees the swap
    /// and stops, nothing outside is removed.
    #[test]
    fn a_root_or_the_directory_above_it_swapped_for_a_link_mid_removal_never_reaches_outside() {
        for swapped in [data_name(), ""] {
            let (scratch, layout, removal) = installed("uninstall-ancestor-swap");
            fs::write(layout.data.join("entry"), "ours").unwrap();
            let outside = scratch.path().join("outside");
            for root in [data_name().to_owned(), format!("{}/install", data_name())] {
                fs::create_dir_all(outside.join(&root)).unwrap();
                fs::write(outside.join(&root).join("keep"), "outside").unwrap();
            }
            fs::write(outside.join("keep"), "outside").unwrap();
            let (victim, target) = if swapped.is_empty() {
                (scratch.path().join("home"), outside.clone())
            } else {
                (layout.data.clone(), outside.join(data_name()))
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
                format!("{}/keep", data_name()),
                format!("{}/install/keep", data_name()),
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
        mixed.data = resolved.join(data_name());
        assert!(mixed.validate().is_ok(), "either form, compared as one");
    }
}
