//! Windows: every step holds the directories it works in, and every directory above them up to the volume root, for
//! as long as it runs. Each is opened as itself (`FILE_FLAG_OPEN_REPARSE_POINT`: a link or junction there is opened,
//! seen and refused, never followed) and without `FILE_SHARE_DELETE`, so while it is held nobody can rename it,
//! delete it or put a link in its place: the paths the step then uses can only lead through those directories. The
//! anchor (the nearest directory above `D` there at the start) is compared (volume and file id, `winapi-util`) with
//! the one seen when the removal started, and `D` with the one recorded when it was checked or made; an entry is
//! claimed by renaming it into the private claim directory and
//! compared before and after; the standard library's
//! handle-based `remove_dir_all` deletes what was claimed (it removes a link or junction itself and never follows one,
//! so it never leaves the volume). `D` and the claim directory are checked private with
//! [`platform::fs`](crate::platform::fs).
//!
//! What Unix has and this does not: no recursive preflight walk (nothing is entered before it is claimed) and no
//! mount compare (a volume mounted inside a root is a reparse point, removed as one, never entered).
//!
//! A running program cannot be deleted on Windows: an uninstall run from inside what it removes (a release, or the
//! recovery copy on a retry) is refused by [`Removal::preflight`] before anything is touched, with what to do instead.
//!
//! No `unsafe` here: `std`'s `OpenOptionsExt` sets the sharing and the flags, `winapi-util` reads the file id.

use super::{
    foreign, recovery_program, recovery_record, DataDir, Removal, Roots, CLAIM, DATA_PREFIX,
    RECORD, RECORD_LIMIT, RECOVERY, RELEASE_PREFIX,
};
use crate::platform::fs::{Create, Files, Fs};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

/// Access denied alone can also mean ACLs or attributes; only call it held when
/// an exclusive attribute handle confirms another open handle conflicts.
fn in_use(error: &io::Error, path: &Path) -> bool {
    match error.raw_os_error() {
        Some(32 | 33) => true,
        Some(5) => OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .err()
            .is_some_and(|error| matches!(error.raw_os_error(), Some(32 | 33))),
        _ => false,
    }
}

/// What a file or directory is, whatever it is called: its volume and its file id.
#[derive(PartialEq)]
struct Identity(u64, u64);

/// What is at `path` itself (a link is not followed), opened only to read its attributes and sharing everything, so
/// holding it stops nothing (it can still be renamed). Kept open by the caller while the identity is compared, so
/// the id cannot be reused in between.
fn identity(path: &Path) -> Result<(File, Identity), &'static str> {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|_| "uninstall entry changed during removal")?;
    let id = identity_of(&file)?;
    Ok((file, id))
}

fn identity_of(file: &File) -> Result<Identity, &'static str> {
    let info =
        winapi_util::file::information(file).map_err(|_| "cannot inspect uninstall entry")?;
    Ok(Identity(info.volume_serial_number(), info.file_index()))
}

/// The directory `path`, held: opened for reading (so the sharing rules apply to the handle) without
/// `FILE_SHARE_DELETE`, so that it cannot be renamed or deleted while held, and refused when it is a link, a junction
/// or not a directory.
fn hold(path: &Path) -> Result<File, &'static str> {
    let dir = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|_| "uninstall directory missing or in use")?;
    let meta = dir
        .metadata()
        .map_err(|_| "cannot inspect uninstall directory")?;
    if !meta.is_dir() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err("uninstall directory is a link or junction");
    }
    Ok(dir)
}

/// Whether something (a link included) is at `path`.
fn exists(path: &Path) -> Result<bool, &'static str> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("cannot inspect uninstall path"),
    }
}

/// The names in `dir`.
fn names(dir: &Path) -> Result<Vec<OsString>, &'static str> {
    fs::read_dir(dir)
        .and_then(|entries| {
            entries
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect()
        })
        .map_err(|_| "cannot enumerate uninstall directory")
}

/// Whether `name` is a claimed release.
fn is_release(name: &OsStr) -> bool {
    name.to_string_lossy().starts_with(RELEASE_PREFIX)
}

/// `path` deleted: a link is removed itself, a directory with all it holds, a file.
fn remove_entry(path: &Path) -> Result<(), &'static str> {
    let kind = fs::symlink_metadata(path)
        .map_err(|_| "uninstall entry changed during removal")?
        .file_type();
    let done = if kind.is_symlink() {
        fs::remove_file(path).or_else(|_| fs::remove_dir(path))
    } else if kind.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    done.map_err(|_| "cannot remove uninstall entry")
}

/// Renames `source` to `target` (in a held directory), refused when what arrived is not what was there just before.
fn claim_entry(
    source: &Path,
    target: &Path,
    name: &OsStr,
    before_claim: &dyn Fn(&OsStr),
) -> Result<(), &'static str> {
    if exists(target)? {
        return Err("uninstall claim name already exists");
    }
    let (_held, before) = identity(source)?;
    before_claim(name);
    fs::rename(source, target).map_err(|_| "cannot claim uninstall entry")?;
    if identity(target)?.1 != before {
        return Err("uninstall entry replaced during claim");
    }
    Ok(())
}

/// Directories held for one step; released when dropped.
struct Held(Vec<File>);

/// This OS's removal: the roots, and the anchor as it was when the removal started.
pub struct Os {
    roots: Roots,
    anchor: (PathBuf, File, Identity),
    /// What `D` is: recorded when the removal starts with `D` there, or when it makes `D`.
    data_id: std::sync::OnceLock<Identity>,
}

impl Os {
    /// `path` (the anchor or below it) and every directory above it, held, with the anchor checked to be the one
    /// seen when the removal started and `D`, when `path` is in it, the one recorded.
    fn hold_down_to(&self, path: &Path) -> Result<Held, &'static str> {
        let held = self.hold_unchecked(path)?;
        if path.starts_with(&self.roots.data) {
            let data = held
                .0
                .get(self.roots.data.ancestors().count() - 1)
                .ok_or("unsafe uninstall path")?;
            match self.data_id.get() {
                Some(id) if *id == identity_of(data)? => {}
                Some(_) => return Err("uninstall data directory replaced during removal"),
                None => return Err("uninstall data directory appeared during removal"),
            }
        }
        Ok(held)
    }

    /// [`Os::hold_down_to`] without comparing `D`: for recording it.
    fn hold_unchecked(&self, path: &Path) -> Result<Held, &'static str> {
        self.roots.validate()?;
        let mut chain: Vec<&Path> = path.ancestors().collect();
        chain.reverse();
        let held = chain.into_iter().map(hold).collect::<Result<Vec<_>, _>>()?;
        let anchor = held
            .get(self.anchor.0.ancestors().count() - 1)
            .ok_or("unsafe uninstall path")?;
        if identity_of(anchor)? != self.anchor.2 {
            return Err("uninstall anchor replaced during removal");
        }
        Ok(Held(held))
    }

    /// `D` as it is now, recorded as the one every later use compares.
    fn record_data(&self) -> Result<(), &'static str> {
        let held = self.hold_unchecked(&self.roots.data)?;
        let id = identity_of(held.0.last().ok_or("unsafe uninstall path")?)?;
        let _ = self.data_id.set(id);
        Ok(())
    }

    /// `D` made by this removal (with any missing directories above it, each private) and recorded; refused when
    /// something else is already there.
    fn make_data(&self) -> Result<(), &'static str> {
        let data = &self.roots.data;
        let parent = data.parent().ok_or("unsafe uninstall path")?;
        if !exists(parent)? {
            Files::ensure_private_dir(parent).map_err(|_| "unsafe uninstall data root")?;
        }
        match Files::create_private_dir(data) {
            Ok(()) => self.record_data(),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                Err("uninstall data directory appeared during removal")
            }
            Err(_) => Err("cannot create uninstall data root"),
        }
    }

    /// `D`'s names and its record, read with `D` held, for [`Roots::check_owned`].
    fn contents(&self) -> Result<super::Contents, &'static str> {
        let Some(_held) = self.data()? else {
            return Ok(None);
        };
        let record = Files::read_trusted(&self.roots.data.join(RECORD), RECORD_LIMIT)
            .map_err(|_| "unsafe uninstall install record")?;
        Ok(Some((names(&self.roots.data)?, record)))
    }

    /// `D` held (with all above it) when it is there, checked: this user's, open to nobody else.
    fn data(&self) -> Result<Option<Held>, &'static str> {
        self.roots.validate()?;
        if !exists(&self.roots.data)? {
            return Ok(None);
        }
        let held = self.hold_down_to(&self.roots.data)?;
        Files::check_private(held.0.last().ok_or("unsafe uninstall path")?)
            .map_err(|_| "unsafe uninstall data root")?;
        Ok(Some(held))
    }

    /// `D`'s private child `name`, made when missing, held with `D` and all above.
    fn private_child(&self, name: &str) -> Result<(Held, PathBuf), &'static str> {
        let mut held = self.data()?.ok_or("uninstall data root disappeared")?;
        let child = self.roots.data.join(name);
        Files::ensure_private_dir(&child).map_err(|_| "unsafe uninstall claim directory")?;
        let dir = hold(&child)?;
        Files::check_private(&dir).map_err(|_| "unsafe uninstall claim directory")?;
        held.0.push(dir);
        Ok((held, child))
    }

    /// Refused when the running program is inside what an uninstall removes: Windows cannot delete it while it
    /// This OS's removal: the anchor held open, and the mount every directory below it must be on.
    fn check_not_running_inside(&self) -> Result<(), &'static str> {
        let program = std::env::current_exe()
            .and_then(fs::canonicalize)
            .map_err(|_| "cannot find the running program")?;
        let roots = [&self.roots.data, &self.roots.releases];
        let inside = |root: &Path| -> Result<bool, &'static str> {
            if !exists(root)? {
                return Ok(false);
            }
            let root = identity(root)?.1;
            Ok(program
                .ancestors()
                .skip(1)
                .any(|dir| identity(dir).is_ok_and(|(_, id)| id == root)))
        };
        for root in roots {
            if inside(root)? {
                return Err(
                    "uninstall is running from inside what it removes, and Windows cannot \
                    delete a running program: copy sidevoice-connector.exe to a folder outside the \
                    Sidevoice folders and run `uninstall` from that copy",
                );
            }
        }
        Ok(())
    }
}

impl Removal for Os {
    fn new(roots: Roots) -> Result<Self, &'static str> {
        roots.validate()?;
        let path = roots.anchor()?;
        let (file, id) = identity(&path)?;
        let removal = Self {
            roots,
            anchor: (path, file, id),
            data_id: std::sync::OnceLock::new(),
        };
        if exists(&removal.roots.data)? {
            removal.record_data()?;
        }
        removal.ownership()?;
        Ok(removal)
    }

    fn roots(&self) -> &Roots {
        &self.roots
    }

    fn ownership(&self) -> Result<super::Ownership, &'static str> {
        // `D` compared with the one recorded even where its contents are not read (the standard place).
        self.data()?;
        self.roots.check_owned(|| self.contents())
    }

    fn preflight(&self) -> Result<bool, &'static str> {
        let data = self.data()?;
        self.check_not_running_inside()?;
        let releases = &self.roots.releases;
        let present = exists(releases)?;
        if present {
            self.hold_down_to(releases)?;
        }
        let claim = self.roots.data.join(CLAIM);
        let claimed =
            data.is_some() && exists(&claim)? && names(&claim)?.iter().any(|name| is_release(name));
        Ok(present || claimed)
    }

    fn lock_file(&self, name: &str) -> Result<File, &'static str> {
        self.roots.validate()?;
        if self.data_id.get().is_none() {
            self.make_data()?;
        }
        let _held = self.data()?.ok_or("uninstall data root disappeared")?;
        Files::open_private(&self.roots.data.join(name), Create::ReadWrite)
            .map_err(|_| "cannot open uninstall lock")
    }

    fn stage_recovery(&self) -> Result<(), &'static str> {
        let (_held, recovery) = self.private_child(RECOVERY)?;
        let program = recovery.join(recovery_program());
        match Files::open_read(&program) {
            Ok(file) => {
                Files::check_private(&file).map_err(|_| "unsafe uninstall recovery executable")?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let source =
                    std::env::current_exe().map_err(|_| "cannot find recovery executable")?;
                let mut source =
                    File::open(source).map_err(|_| "cannot read recovery executable")?;
                let mut target = Files::open_private(&program, Create::NewProgram)
                    .map_err(|_| "cannot create recovery executable")?;
                io::copy(&mut source, &mut target)
                    .map_err(|_| "cannot copy recovery executable")?;
                target
                    .sync_all()
                    .map_err(|_| "cannot sync recovery executable")?;
            }
            Err(_) => return Err("unsafe uninstall recovery executable"),
        }
        let record = recovery_record(&self.roots)?;
        Files::write_json(&self.roots.data.join(RECORD), &record)
            .map_err(|_| "cannot save uninstall record")
    }

    fn remove_data_contents(
        &self,
        before_claim: &dyn Fn(&OsStr),
    ) -> Result<Vec<String>, &'static str> {
        if self.data()?.is_none() {
            return Ok(Vec::new());
        }
        let (_held, claim) = self.private_child(CLAIM)?;
        let data = &self.roots.data;
        let mut removed = Vec::new();
        for entry in names(data)? {
            if self.roots.is_kept(&entry) {
                continue;
            }
            let claimed = claim.join(format!("{DATA_PREFIX}{}", uuid::Uuid::new_v4().simple()));
            claim_entry(&data.join(&entry), &claimed, &entry, before_claim)?;
            removed.push(entry.to_string_lossy().into_owned());
        }
        for entry in names(&claim)? {
            if !is_release(&entry) {
                remove_entry(&claim.join(entry))?;
            }
        }
        Ok(removed)
    }

    fn remove_releases(&self) -> Result<(), &'static str> {
        let (_held, claim) = self.private_child(CLAIM)?;
        let releases = &self.roots.releases;
        if exists(releases)? {
            let parent = releases.parent().ok_or("unsafe uninstall root")?;
            let _parents = self.hold_down_to(parent)?;
            let claimed = claim.join(format!("{RELEASE_PREFIX}{}", uuid::Uuid::new_v4().simple()));
            claim_entry(
                releases,
                &claimed,
                releases.file_name().ok_or("unsafe uninstall root")?,
                &|_| {},
            )?;
        }
        for entry in names(&claim)? {
            if is_release(&entry) {
                remove_entry(&claim.join(entry))?;
            }
        }
        Ok(())
    }

    fn finish_claim(&self) -> Result<(), &'static str> {
        let Some(_held) = self.data()? else {
            return Ok(());
        };
        let claim = self.roots.data.join(CLAIM);
        if !exists(&claim)? {
            return Ok(());
        }
        {
            let _claim = hold(&claim)?;
            if !names(&claim)?.is_empty() {
                return Err("uninstall claim still contains data");
            }
        }
        fs::remove_dir(&claim).map_err(|_| "cannot finish uninstall claim")
    }

    fn finish_recovery(&self) -> Result<(), &'static str> {
        if exists(&self.roots.releases)? {
            return Err("uninstall releases remain");
        }
        let Some(_held) = self.data()? else {
            return Ok(());
        };
        let data = &self.roots.data;
        if names(data)?.iter().any(|name| !self.roots.is_kept(name)) {
            return Err("uninstall data remains");
        }
        if exists(&data.join(CLAIM))? {
            return Err("uninstall claim remains");
        }
        let recovery = data.join(RECOVERY);
        if exists(&recovery)? {
            {
                let _recovery = hold(&recovery)?;
                if names(&recovery)? != [OsStr::new(&recovery_program())] {
                    return Err("uninstall recovery directory changed");
                }
                fs::remove_file(recovery.join(recovery_program()))
                    .map_err(|_| "cannot remove recovery executable")?;
            }
            fs::remove_dir(&recovery).map_err(|_| "cannot remove recovery directory")?;
        }
        let record = data.join(RECORD);
        if exists(&record)? {
            fs::remove_file(record).map_err(|_| "cannot remove uninstall record")?;
        }
        Ok(())
    }

    fn remove_data_dir(&self, before_remove: &dyn Fn(&OsStr)) -> Result<DataDir, &'static str> {
        let data = &self.roots.data;
        let mut held = Vec::new();
        {
            // `D` held while its files go: nobody can swap it. A file open elsewhere (a process holds it without
            // sharing delete: a sharing violation) stays, and is reported.
            let Some(_held) = self.data()? else {
                return Ok(DataDir {
                    removed: true,
                    ..DataDir::default()
                });
            };
            let mut found = names(data)?;
            found.sort_by_key(|entry| entry == "install.lock");
            let kept = foreign(&self.roots, &found);
            if !kept.is_empty() {
                return Ok(DataDir {
                    kept,
                    ..DataDir::default()
                });
            }
            for entry in found {
                if entry == "node-stopped.json" {
                    continue;
                }
                before_remove(&entry);
                // A link is removed itself; a directory of that name is not entered.
                let path = data.join(&entry);
                if let Err(error) = fs::remove_file(&path) {
                    if in_use(&error, &path) && !path.is_dir() {
                        held.push(entry.to_string_lossy().into_owned());
                    } else {
                        return Err("cannot remove uninstall file");
                    }
                }
            }
        }
        if data.join("node-stopped.json").exists() {
            return Ok(DataDir {
                kept: vec!["node-stopped.json".into()],
                held,
                ..DataDir::default()
            });
        }
        if held.is_empty() {
            // `D` itself cannot be deleted while held, so its parent is held instead. Only an empty directory goes.
            let _parent = self.hold_down_to(data.parent().ok_or("unsafe uninstall path")?)?;
            if let Err(error) = fs::remove_dir(data) {
                if !in_use(&error, data) {
                    return Err("cannot remove uninstall directory");
                }
                held.push(
                    data.file_name()
                        .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
                );
            }
        }
        Ok(DataDir {
            removed: held.is_empty(),
            held,
            ..DataDir::default()
        })
    }
}
