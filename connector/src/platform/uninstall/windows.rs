//! Windows: the standard library's handle-based `remove_dir_all` deletes (it never follows a symbolic link or a
//! junction, so it removes the link itself and never leaves the volume), behind the claim rename, over directories
//! that [`platform::fs`](crate::platform::fs) has checked: this user's, open to nobody else, no link on the way.
//! Every step checks the roots again ([`Roots::validate`]), so a home swapped for a link is refused, not followed.
//!
//! Windows has no descriptor-relative open (`openat`) in the standard library, and no maintained crate offers one
//! that keeps the Unix guarantees as they are (see the pull request that brought this file); a running program
//! cannot be deleted, so an uninstall started from inside the releases or the recovery copy fails at that file and
//! is retried from outside.

use super::{Removal, Roots, CLAIM, DATA_PREFIX, RECOVERY, RECOVERY_PROGRAM, RELEASE_PREFIX};
use crate::platform::fs::{Create, Files, Fs};
use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

/// This OS's removal.
pub struct Os {
    roots: Roots,
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

impl Os {
    /// `D` when it is there, checked: a real directory of this user's that nobody else can change.
    fn data(&self) -> Result<Option<PathBuf>, &'static str> {
        self.roots.validate()?;
        let data = &self.roots.data;
        if !exists(data)? {
            return Ok(None);
        }
        Files::open_read(data)
            .map_err(Into::into)
            .and_then(|dir| Files::check_private(&dir))
            .map_err(|_| "unsafe uninstall data root")?;
        Ok(Some(data.clone()))
    }

    /// The claim directory in `D`, made private when missing, and `D`.
    fn claim(&self) -> Result<(PathBuf, PathBuf), &'static str> {
        let data = self.data()?.ok_or("uninstall data root disappeared")?;
        let claim = data.join(CLAIM);
        Files::ensure_private_dir(&claim).map_err(|_| "unsafe uninstall claim directory")?;
        Ok((data, claim))
    }
}

impl Removal for Os {
    fn new(roots: Roots) -> Result<Self, &'static str> {
        roots.validate()?;
        Ok(Self { roots })
    }

    fn roots(&self) -> &Roots {
        &self.roots
    }

    fn preflight(&self) -> Result<bool, &'static str> {
        let data = self.data()?;
        let releases = &self.roots.releases;
        let present = exists(releases)?;
        if present {
            Files::open_read(releases)
                .ok()
                .filter(|dir| dir.metadata().is_ok_and(|meta| meta.is_dir()))
                .ok_or("uninstall root is not a real directory")?;
        }
        let claimed = match data {
            Some(data) if exists(&data.join(CLAIM))? => names(&data.join(CLAIM))?
                .iter()
                .any(|name| is_release(name)),
            _ => false,
        };
        Ok(present || claimed)
    }

    fn lock_file(&self, name: &str) -> Result<File, &'static str> {
        self.roots.validate()?;
        Files::ensure_private_dir(&self.roots.data).map_err(|_| "unsafe uninstall data root")?;
        Files::open_private(&self.roots.data.join(name), Create::ReadWrite)
            .map_err(|_| "cannot open uninstall lock")
    }

    fn place_recovery_program(&self) -> Result<(), &'static str> {
        let data = self.data()?.ok_or("uninstall data root disappeared")?;
        let recovery = data.join(RECOVERY);
        Files::ensure_private_dir(&recovery).map_err(|_| "unsafe uninstall claim directory")?;
        let program = recovery.join(RECOVERY_PROGRAM);
        match Files::open_read(&program) {
            Ok(file) => {
                Files::check_private(&file).map_err(|_| "unsafe uninstall recovery executable")
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
                    .map_err(|_| "cannot sync recovery executable")
            }
            Err(_) => Err("unsafe uninstall recovery executable"),
        }
    }

    fn write_install_record(&self, record: &Value) -> Result<(), &'static str> {
        let data = self.data()?.ok_or("uninstall data root disappeared")?;
        Files::write_json(&data.join("install.json"), record)
            .map_err(|_| "cannot save uninstall record")
    }

    fn remove_data_contents(
        &self,
        before_claim: &dyn Fn(&OsStr),
    ) -> Result<Vec<String>, &'static str> {
        if self.data()?.is_none() {
            return Ok(Vec::new());
        }
        let (data, claim) = self.claim()?;
        let mut removed = Vec::new();
        for entry in names(&data)? {
            if self.roots.is_kept(&entry) {
                continue;
            }
            before_claim(&entry);
            let claimed = claim.join(format!("{DATA_PREFIX}{}", uuid::Uuid::new_v4().simple()));
            fs::rename(data.join(&entry), claimed).map_err(|_| "cannot claim uninstall entry")?;
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
        let (_, claim) = self.claim()?;
        let releases = &self.roots.releases;
        if exists(releases)? {
            let claimed = claim.join(format!("{RELEASE_PREFIX}{}", uuid::Uuid::new_v4().simple()));
            fs::rename(releases, claimed).map_err(|_| "cannot claim uninstall entry")?;
        }
        for entry in names(&claim)? {
            if is_release(&entry) {
                remove_entry(&claim.join(entry))?;
            }
        }
        Ok(())
    }

    fn finish_claim(&self) -> Result<(), &'static str> {
        let Some(data) = self.data()? else {
            return Ok(());
        };
        let claim = data.join(CLAIM);
        if !exists(&claim)? {
            return Ok(());
        }
        if !names(&claim)?.is_empty() {
            return Err("uninstall claim still contains data");
        }
        fs::remove_dir(&claim).map_err(|_| "cannot finish uninstall claim")
    }

    fn finish_recovery(&self) -> Result<(), &'static str> {
        if exists(&self.roots.releases)? {
            return Err("uninstall releases remain");
        }
        let Some(data) = self.data()? else {
            return Ok(());
        };
        if names(&data)?.iter().any(|name| !self.roots.is_kept(name)) {
            return Err("uninstall data remains");
        }
        if exists(&data.join(CLAIM))? {
            return Err("uninstall claim remains");
        }
        let recovery = data.join(RECOVERY);
        if exists(&recovery)? {
            if names(&recovery)? != [OsStr::new(RECOVERY_PROGRAM)] {
                return Err("uninstall recovery directory changed");
            }
            fs::remove_file(recovery.join(RECOVERY_PROGRAM))
                .map_err(|_| "cannot remove recovery executable")?;
            fs::remove_dir(&recovery).map_err(|_| "cannot remove recovery directory")?;
        }
        let record = data.join("install.json");
        if exists(&record)? {
            fs::remove_file(record).map_err(|_| "cannot remove uninstall record")?;
        }
        Ok(())
    }
}
