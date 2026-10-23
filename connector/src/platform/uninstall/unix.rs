//! Unix: every step relative to directory descriptors held open from the anchor down, no link followed
//! (`O_NOFOLLOW`), each directory checked against its path again before it is used and its mount compared with the
//! anchor's (the device and the mount id on Linux, the device and where it is mounted on macOS).

use super::{
    clean_absolute, recovery_program, recovery_record, Removal, Roots, CLAIM, DATA_PREFIX, RECORD,
    RECORD_LIMIT, RECOVERY, RELEASE_PREFIX,
};
use crate::platform::user::{User, UserId};
use serde_json::Value;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use super::os::{mount, open_entry, Mount};

/// Whether this user owns what `fd` is (`platform::user`).
fn ours(fd: RawFd) -> Result<bool, &'static str> {
    // SAFETY: the callers hold `fd` open for the call.
    let owned = unsafe { BorrowedFd::borrow_raw(fd) }
        .try_clone_to_owned()
        .map_err(|_| "cannot read uninstall owner")?;
    UserId::current()
        .owns_file(&File::from(owned))
        .map_err(|_| "cannot read uninstall owner")
}

pub(super) fn stat_fd(fd: RawFd) -> Result<libc::stat, &'static str> {
    let mut stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut stat) } == 0 {
        Ok(stat)
    } else {
        Err("cannot inspect uninstall entry")
    }
}

fn stat_at(fd: RawFd, name: &CStr) -> Result<libc::stat, &'static str> {
    let mut stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(fd, name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) } == 0 {
        Ok(stat)
    } else {
        Err("uninstall entry changed during removal")
    }
}

fn same(a: &libc::stat, b: &libc::stat) -> bool {
    a.st_dev == b.st_dev
        && a.st_ino == b.st_ino
        && a.st_mode & libc::S_IFMT == b.st_mode & libc::S_IFMT
}

fn name(name: &OsStr) -> Result<CString, &'static str> {
    CString::new(name.as_bytes()).map_err(|_| "unsafe uninstall entry name")
}

fn open_dir(parent: RawFd, child: &OsStr) -> Result<OwnedFd, &'static str> {
    let child = name(child)?;
    let fd = unsafe {
        libc::openat(
            parent,
            child.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err("uninstall directory changed or is a link")
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn absolute_dir(path: &Path) -> Result<OwnedFd, &'static str> {
    if !clean_absolute(path) {
        return Err("unsafe uninstall path");
    }
    let fd = unsafe {
        libc::open(
            c"/".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err("cannot open filesystem root");
    }
    let mut current = unsafe { OwnedFd::from_raw_fd(fd) };
    for component in path.components().skip(1) {
        let Component::Normal(part) = component else {
            return Err("unsafe uninstall path");
        };
        current = open_dir(current.as_raw_fd(), part)?;
    }
    Ok(current)
}

fn exists_at(parent: RawFd, child: &OsStr) -> Result<bool, &'static str> {
    let child = name(child)?;
    let mut stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(parent, child.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) } == 0 {
        return Ok(true);
    }
    if io::Error::last_os_error().kind() == io::ErrorKind::NotFound {
        Ok(false)
    } else {
        Err("cannot inspect uninstall path")
    }
}

fn entries(fd: RawFd) -> Result<Vec<OsString>, &'static str> {
    // A fresh open has its own directory offset; dup() would share the first pass's offset.
    let copy = open_dir(fd, OsStr::new("."))?;
    let dir = rustix::fs::Dir::new(copy).map_err(|_| "cannot enumerate uninstall directory")?;
    let mut found = Vec::new();
    for entry in dir {
        let entry = entry.map_err(|_| "cannot enumerate uninstall directory")?;
        let raw = entry.file_name().to_bytes();
        if raw != b"." && raw != b".." {
            found.push(OsStr::from_bytes(raw).to_owned());
        }
    }
    Ok(found)
}

/// This OS's removal: the anchor held open, and the mount every directory below it must be on.
pub struct Os {
    roots: Roots,
    anchor: OwnedFd,
    /// Where `anchor` was opened.
    anchor_path: PathBuf,
    expected: Mount,
    /// `D`, `D`'s claim and recovery directories and `R`, below the anchor.
    data_rel: PathBuf,
    claim_rel: PathBuf,
    recovery_rel: PathBuf,
    releases_rel: PathBuf,
    /// `D` as opened for the first lock; the same descriptor serves every lock of this removal.
    lock_data: std::sync::OnceLock<OwnedFd>,
    /// What `D` is (device and inode): recorded when the removal starts with `D` there, or when it makes `D`.
    data_id: std::sync::OnceLock<(libc::dev_t, libc::ino_t)>,
}

impl Os {
    /// That `data`, opened as `D`, is the `D` recorded: refused when it is another directory, or when `D` was not
    /// there at the start and this removal did not make it.
    fn check_data(&self, data: RawFd) -> Result<(), &'static str> {
        let stat = stat_fd(data)?;
        match self.data_id.get() {
            Some(&id) if id == (stat.st_dev, stat.st_ino) => Ok(()),
            Some(_) => Err("uninstall data directory replaced during removal"),
            None => Err("uninstall data directory appeared during removal"),
        }
    }

    fn check_mount(&self, fd: RawFd) -> Result<(), &'static str> {
        if mount(fd)? == self.expected {
            Ok(())
        } else {
            Err("uninstall mount or device boundary")
        }
    }

    fn data_for_lock(&self) -> Result<&OwnedFd, &'static str> {
        if let Some(data) = self.lock_data.get() {
            return Ok(data);
        }
        // Each missing directory on the way down from the anchor is made private, as the XDG rules ask.
        let mut data = open_dir(self.anchor.as_raw_fd(), OsStr::new("."))?;
        let mut made = false;
        for component in self.data_rel.components() {
            let Component::Normal(part) = component else {
                return Err("unsafe uninstall path");
            };
            made = false;
            if !exists_at(data.as_raw_fd(), part)? {
                let part_name = name(part)?;
                if unsafe { libc::mkdirat(data.as_raw_fd(), part_name.as_ptr(), 0o700) } != 0 {
                    return Err("cannot create uninstall data root");
                }
                made = true;
            }
            data = open_dir(data.as_raw_fd(), part)?;
            self.check_mount(data.as_raw_fd())?;
        }
        if made {
            // `D` made here, now: recorded as it is opened, so every later use compares it.
            let stat = stat_fd(data.as_raw_fd())?;
            let _ = self.data_id.set((stat.st_dev, stat.st_ino));
        }
        self.check_data(data.as_raw_fd())?;
        self.verify_dir(&self.data_rel, data.as_raw_fd())?;
        let stat = stat_fd(data.as_raw_fd())?;
        if !ours(data.as_raw_fd())? || stat.st_mode & 0o077 != 0 {
            return Err("unsafe uninstall data root");
        }
        Ok(self.lock_data.get_or_init(|| data))
    }

    /// `path` below the anchor, opened one directory at a time; `D`, on the way, compared with the one recorded.
    fn open_relative(&self, path: &Path) -> Result<Option<OwnedFd>, &'static str> {
        self.open_below(path, true)
    }

    fn open_below(&self, path: &Path, check_data: bool) -> Result<Option<OwnedFd>, &'static str> {
        let mut current = open_dir(self.anchor.as_raw_fd(), OsStr::new("."))?;
        let mut walked = PathBuf::new();
        for component in path.components() {
            let Component::Normal(part) = component else {
                return Err("unsafe uninstall path");
            };
            if !exists_at(current.as_raw_fd(), part)? {
                return Ok(None);
            }
            current = open_dir(current.as_raw_fd(), part)?;
            self.check_mount(current.as_raw_fd())?;
            walked.push(part);
            if check_data && walked == self.data_rel {
                self.check_data(current.as_raw_fd())?;
            }
        }
        Ok(Some(current))
    }

    fn verify_dir(&self, path: &Path, held: RawFd) -> Result<(), &'static str> {
        let named_anchor = absolute_dir(&self.anchor_path)?;
        if !same(
            &stat_fd(named_anchor.as_raw_fd())?,
            &stat_fd(self.anchor.as_raw_fd())?,
        ) {
            return Err("uninstall anchor replaced during removal");
        }
        let current = self
            .open_relative(path)?
            .ok_or("uninstall path disappeared")?;
        if !same(&stat_fd(current.as_raw_fd())?, &stat_fd(held)?) {
            return Err("uninstall directory replaced during removal");
        }
        self.check_mount(held)
    }

    fn private_child(&self, parent: RawFd, child: &str) -> Result<OwnedFd, &'static str> {
        let child_name = name(OsStr::new(child))?;
        if unsafe { libc::mkdirat(parent, child_name.as_ptr(), 0o700) } != 0
            && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
        {
            return Err("cannot create uninstall claim directory");
        }
        let child = open_dir(parent, OsStr::new(child))?;
        self.check_mount(child.as_raw_fd())?;
        let stat = stat_fd(child.as_raw_fd())?;
        if !ours(child.as_raw_fd())? || stat.st_mode & 0o777 != 0o700 {
            return Err("unsafe uninstall claim directory");
        }
        Ok(child)
    }

    fn claim_area(&self) -> Result<Option<(OwnedFd, OwnedFd)>, &'static str> {
        let Some(data) = self.open_relative(&self.data_rel)? else {
            return Ok(None);
        };
        self.verify_dir(&self.data_rel, data.as_raw_fd())?;
        let claim = self.private_child(data.as_raw_fd(), CLAIM)?;
        self.verify_dir(&self.claim_rel, claim.as_raw_fd())?;
        Ok(Some((data, claim)))
    }

    /// Pin a copy of this running CLI inside D and atomically point install.json
    /// at it before any installed release or personal data is removed. A retry
    fn claim_entry(
        &self,
        source: RawFd,
        source_path: &Path,
        source_name: &OsStr,
        claim: RawFd,
        claimed_name: &str,
        before_rename: &dyn Fn(&OsStr),
    ) -> Result<(), &'static str> {
        self.verify_dir(source_path, source)?;
        self.verify_dir(&self.claim_rel, claim)?;
        let source_name = name(source_name)?;
        let dest_name = name(OsStr::new(claimed_name))?;
        if exists_at(claim, OsStr::new(claimed_name))? {
            return Err("uninstall claim name already exists");
        }
        let before = stat_at(source, &source_name)?;
        if before.st_dev != self.expected.device {
            return Err("uninstall device boundary");
        }
        if before.st_mode & libc::S_IFMT == libc::S_IFDIR {
            let child = open_dir(source, OsStr::from_bytes(source_name.to_bytes()))?;
            self.check_mount(child.as_raw_fd())?;
            if !same(&before, &stat_fd(child.as_raw_fd())?) {
                return Err("uninstall entry replaced");
            }
        }
        // The rename is the ownership claim. A replacement at the old name is
        // moved as a link/inode, never followed. Validate that it was the same
        // inode before traversing anything in the private claim directory.
        before_rename(OsStr::from_bytes(source_name.to_bytes()));
        if unsafe { libc::renameat(source, source_name.as_ptr(), claim, dest_name.as_ptr()) } != 0 {
            return Err("cannot claim uninstall entry");
        }
        self.verify_dir(&self.claim_rel, claim)?;
        if !same(&before, &stat_at(claim, &dest_name)?) {
            return Err("uninstall entry replaced during claim");
        }
        Ok(())
    }

    fn walk(
        &self,
        fd: RawFd,
        relative: &Path,
        remove: bool,
        kept: &[&str],
        depth: usize,
        probe: &impl Fn(RawFd, &Path) -> Result<Mount, &'static str>,
    ) -> Result<Vec<String>, &'static str> {
        if depth > 64 {
            return Err("uninstall tree is too deep");
        }
        self.verify_dir(relative, fd)?;
        if probe(fd, relative)? != self.expected {
            return Err("uninstall mount or device boundary");
        }
        let mut removed = Vec::new();
        for entry in entries(fd)? {
            if depth == 0 && kept.iter().any(|item| OsStr::new(item) == entry) {
                continue;
            }
            self.verify_dir(relative, fd)?;
            let child_path = relative.join(&entry);
            let child_name = name(&entry)?;
            let before = stat_at(fd, &child_name)?;
            if before.st_dev != self.expected.device {
                return Err("uninstall device boundary");
            }
            if before.st_mode & libc::S_IFMT == libc::S_IFDIR {
                let child = open_dir(fd, &entry)?;
                self.check_mount(child.as_raw_fd())?;
                if !same(&before, &stat_fd(child.as_raw_fd())?) {
                    return Err("uninstall entry replaced");
                }
                self.walk(
                    child.as_raw_fd(),
                    &child_path,
                    remove,
                    &[],
                    depth + 1,
                    probe,
                )?;
                if remove {
                    self.verify_dir(relative, fd)?;
                    if !same(&before, &stat_at(fd, &child_name)?) {
                        return Err("uninstall entry replaced");
                    }
                    self.check_mount(child.as_raw_fd())?;
                    if unsafe { libc::unlinkat(fd, child_name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
                        return Err("cannot remove uninstall directory");
                    }
                }
            } else {
                if let Some(child) = open_entry(fd, &child_name, &before)? {
                    if !same(&before, &stat_fd(child.as_raw_fd())?) {
                        return Err("uninstall entry replaced");
                    }
                    self.check_mount(child.as_raw_fd())?;
                }
                if remove {
                    self.verify_dir(relative, fd)?;
                    if !same(&before, &stat_at(fd, &child_name)?) {
                        return Err("uninstall entry replaced");
                    }
                    if unsafe { libc::unlinkat(fd, child_name.as_ptr(), 0) } != 0 {
                        return Err("cannot remove uninstall entry");
                    }
                }
            }
            if depth == 0 && remove {
                removed.push(entry.to_string_lossy().into_owned());
            }
        }
        Ok(removed)
    }

    fn tree(
        &self,
        relative: &Path,
        remove_root: bool,
        kept: &[&str],
        remove: bool,
        probe: &impl Fn(RawFd, &Path) -> Result<Mount, &'static str>,
    ) -> Result<bool, &'static str> {
        let Some(root) = self.open_relative(relative)? else {
            return Ok(false);
        };
        self.walk(root.as_raw_fd(), relative, remove, kept, 0, probe)?;
        if remove && remove_root {
            let parent = relative.parent().ok_or("unsafe uninstall root")?;
            let root_name = relative.file_name().ok_or("unsafe uninstall root")?;
            let parent_fd = self
                .open_relative(parent)?
                .ok_or("uninstall root parent disappeared")?;
            self.verify_dir(parent, parent_fd.as_raw_fd())?;
            let name = name(root_name)?;
            if !same(
                &stat_fd(root.as_raw_fd())?,
                &stat_at(parent_fd.as_raw_fd(), &name)?,
            ) {
                return Err("uninstall root replaced");
            }
            self.check_mount(root.as_raw_fd())?;
            if unsafe { libc::unlinkat(parent_fd.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) }
                != 0
            {
                return Err("cannot remove uninstall root");
            }
        }
        Ok(true)
    }

    /// `D`'s names and its record, read through `D` as held from the anchor, for [`Roots::check_owned`].
    fn contents(&self) -> Result<super::Contents, &'static str> {
        let Some(data) = self.open_relative(&self.data_rel)? else {
            return Ok(None);
        };
        self.verify_dir(&self.data_rel, data.as_raw_fd())?;
        let names = entries(data.as_raw_fd())?;
        if !exists_at(data.as_raw_fd(), OsStr::new(RECORD))? {
            return Ok(Some((names, None)));
        }
        let fd = unsafe {
            libc::openat(
                data.as_raw_fd(),
                c"install.json".as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err("unsafe uninstall install record");
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let stat = stat_fd(file.as_raw_fd())?;
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG
            || !ours(file.as_raw_fd())?
            || stat.st_nlink != 1
            || stat.st_mode & 0o022 != 0
        {
            return Err("unsafe uninstall install record");
        }
        self.check_mount(file.as_raw_fd())?;
        let mut bytes = Vec::new();
        io::Read::read_to_end(&mut io::Read::take(&file, RECORD_LIMIT + 1), &mut bytes)
            .map_err(|_| "cannot read uninstall install record")?;
        if bytes.len() as u64 > RECORD_LIMIT {
            return Err("unsafe uninstall install record");
        }
        Ok(Some((names, Some(bytes))))
    }

    /// `record` written as `install.json` in the held `data`, whole: a temporary file, synced, then renamed over it
    /// once `data` is checked to be `D` still.
    fn write_install_record(&self, data: RawFd, record: &Value) -> Result<(), &'static str> {
        let temporary = format!(".uninstall-record-{}", uuid::Uuid::new_v4().simple());
        let temporary = name(OsStr::new(&temporary))?;
        let fd = unsafe {
            libc::openat(
                data,
                temporary.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err("cannot create uninstall record");
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        let bytes = serde_json::to_vec(record).map_err(|_| "cannot encode uninstall record")?;
        file.write_all(&bytes)
            .map_err(|_| "cannot write uninstall record")?;
        file.sync_all()
            .map_err(|_| "cannot sync uninstall record")?;
        self.verify_dir(&self.data_rel, data)?;
        if unsafe { libc::renameat(data, temporary.as_ptr(), data, c"install.json".as_ptr()) } != 0
        {
            return Err("cannot save uninstall record");
        }
        Ok(())
    }
}

impl Removal for Os {
    fn new(roots: Roots) -> Result<Self, &'static str> {
        roots.validate()?;
        let anchor_path = roots.anchor()?;
        let anchor = absolute_dir(&anchor_path)?;
        let expected = mount(anchor.as_raw_fd())?;
        let below = |path: &Path| {
            path.strip_prefix(&anchor_path)
                .map(Path::to_path_buf)
                .map_err(|_| "uninstall path is outside known Sidevoice roots")
        };
        let (data_rel, releases_rel) = (below(&roots.data)?, below(&roots.releases)?);
        let removal = Self {
            claim_rel: data_rel.join(CLAIM),
            recovery_rel: data_rel.join(RECOVERY),
            data_rel,
            releases_rel,
            roots,
            anchor,
            anchor_path,
            expected,
            lock_data: std::sync::OnceLock::new(),
            data_id: std::sync::OnceLock::new(),
        };
        if let Some(data) = removal.open_below(&removal.data_rel, false)? {
            let stat = stat_fd(data.as_raw_fd())?;
            let _ = removal.data_id.set((stat.st_dev, stat.st_ino));
        }
        removal.ownership()?;
        Ok(removal)
    }

    fn roots(&self) -> &Roots {
        &self.roots
    }

    fn ownership(&self) -> Result<super::Ownership, &'static str> {
        // `D` compared with the one recorded even where its contents are not read (the standard place).
        self.open_relative(&self.data_rel)?;
        self.roots.check_owned(|| self.contents())
    }

    fn lock_file(&self, entry: &str) -> Result<File, &'static str> {
        let data = self.data_for_lock()?;
        let entry = name(OsStr::new(entry))?;
        let fd = unsafe {
            libc::openat(
                data.as_raw_fd(),
                entry.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        };
        if fd < 0 {
            Err("cannot open uninstall lock")
        } else {
            Ok(File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
        }
    }

    fn stage_recovery(&self) -> Result<(), &'static str> {
        let data = self
            .open_relative(&self.data_rel)?
            .ok_or("uninstall data root disappeared")?;
        self.verify_dir(&self.data_rel, data.as_raw_fd())?;
        let recovery = self.private_child(data.as_raw_fd(), RECOVERY)?;
        let program = recovery_program();
        let executable = OsStr::new(&program);
        let executable_name = name(executable)?;
        if exists_at(recovery.as_raw_fd(), executable)? {
            let fd = unsafe {
                libc::openat(
                    recovery.as_raw_fd(),
                    executable_name.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err("unsafe uninstall recovery executable");
            }
            let file = unsafe { File::from_raw_fd(fd) };
            let stat = stat_fd(file.as_raw_fd())?;
            if stat.st_mode & libc::S_IFMT != libc::S_IFREG
                || !ours(file.as_raw_fd())?
                || stat.st_nlink != 1
                || stat.st_mode & 0o022 != 0
                || stat.st_mode & 0o100 == 0
            {
                return Err("unsafe uninstall recovery executable");
            }
            self.check_mount(file.as_raw_fd())?;
        } else {
            let source = std::env::current_exe().map_err(|_| "cannot find recovery executable")?;
            let mut source = File::open(source).map_err(|_| "cannot read recovery executable")?;
            let fd = unsafe {
                libc::openat(
                    recovery.as_raw_fd(),
                    executable_name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o700,
                )
            };
            if fd < 0 {
                return Err("cannot create recovery executable");
            }
            let mut target = unsafe { File::from_raw_fd(fd) };
            io::copy(&mut source, &mut target).map_err(|_| "cannot copy recovery executable")?;
            target
                .flush()
                .map_err(|_| "cannot finish recovery executable")?;
            target
                .sync_all()
                .map_err(|_| "cannot sync recovery executable")?;
        }
        self.verify_dir(&self.data_rel, data.as_raw_fd())?;
        self.verify_dir(&self.recovery_rel, recovery.as_raw_fd())?;
        let record = recovery_record(&self.roots)?;
        self.write_install_record(data.as_raw_fd(), &record)
    }

    fn preflight(&self) -> Result<bool, &'static str> {
        let probe = |fd, _: &Path| mount(fd);
        let releases = self.tree(&self.releases_rel, false, &[], false, &probe)?;
        self.tree(&self.data_rel, false, self.roots.kept, false, &probe)?;
        let claimed_releases = if let Some(claim) = self.open_relative(&self.claim_rel)? {
            entries(claim.as_raw_fd())?
                .iter()
                .any(|name| name.as_bytes().starts_with(RELEASE_PREFIX.as_bytes()))
        } else {
            false
        };
        Ok(releases || claimed_releases)
    }

    fn remove_releases(&self) -> Result<(), &'static str> {
        let Some((_, claim)) = self.claim_area()? else {
            return Err("uninstall data root disappeared");
        };
        let release_parent = self.releases_rel.parent().ok_or("unsafe uninstall root")?;
        let release_name = self
            .releases_rel
            .file_name()
            .ok_or("unsafe uninstall root")?;
        if let Some(parent) = self.open_relative(release_parent)? {
            if exists_at(parent.as_raw_fd(), release_name)? {
                let claimed_name = format!("{RELEASE_PREFIX}{}", uuid::Uuid::new_v4().simple());
                self.claim_entry(
                    parent.as_raw_fd(),
                    release_parent,
                    release_name,
                    claim.as_raw_fd(),
                    &claimed_name,
                    &|_| {},
                )?;
            }
        }
        for entry in entries(claim.as_raw_fd())? {
            if !entry.as_bytes().starts_with(RELEASE_PREFIX.as_bytes()) {
                continue;
            }
            self.verify_dir(&self.claim_rel, claim.as_raw_fd())?;
            let entry_name = name(&entry)?;
            let before = stat_at(claim.as_raw_fd(), &entry_name)?;
            if before.st_mode & libc::S_IFMT == libc::S_IFDIR {
                self.tree(&self.claim_rel.join(&entry), true, &[], true, &|fd, _| {
                    mount(fd)
                })?;
            } else {
                // A raced replacement may have been claimed as a link. Its
                // external target is untouched; retry can discard the link.
                if before.st_dev != self.expected.device {
                    return Err("uninstall device boundary");
                }
                if !same(&before, &stat_at(claim.as_raw_fd(), &entry_name)?) {
                    return Err("uninstall entry replaced");
                }
                if unsafe { libc::unlinkat(claim.as_raw_fd(), entry_name.as_ptr(), 0) } != 0 {
                    return Err("cannot remove claimed release link");
                }
            }
        }
        Ok(())
    }

    fn remove_data_contents(
        &self,
        before_rename: &dyn Fn(&OsStr),
    ) -> Result<Vec<String>, &'static str> {
        let Some((root, claim)) = self.claim_area()? else {
            return Ok(Vec::new());
        };
        let mut removed = Vec::new();
        for entry in entries(root.as_raw_fd())? {
            if self.roots.is_kept(&entry) {
                continue;
            }
            let claimed_name = format!("{DATA_PREFIX}{}", uuid::Uuid::new_v4().simple());
            self.claim_entry(
                root.as_raw_fd(),
                &self.data_rel,
                &entry,
                claim.as_raw_fd(),
                &claimed_name,
                before_rename,
            )?;
            removed.push(entry.to_string_lossy().into_owned());
        }
        let release_names: Vec<String> = entries(claim.as_raw_fd())?
            .into_iter()
            .filter(|name| name.as_bytes().starts_with(RELEASE_PREFIX.as_bytes()))
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        let kept: Vec<&str> = release_names.iter().map(String::as_str).collect();
        self.walk(
            claim.as_raw_fd(),
            &self.claim_rel,
            true,
            &kept,
            0,
            &|fd, _| mount(fd),
        )?;
        Ok(removed)
    }

    fn finish_claim(&self) -> Result<(), &'static str> {
        let Some(data) = self.open_relative(&self.data_rel)? else {
            return Ok(());
        };
        if !exists_at(data.as_raw_fd(), OsStr::new(CLAIM))? {
            return Ok(());
        }
        let claim = open_dir(data.as_raw_fd(), OsStr::new(CLAIM))?;
        self.verify_dir(&self.claim_rel, claim.as_raw_fd())?;
        if !entries(claim.as_raw_fd())?.is_empty() {
            return Err("uninstall claim still contains data");
        }
        self.verify_dir(&self.data_rel, data.as_raw_fd())?;
        if unsafe {
            libc::unlinkat(
                data.as_raw_fd(),
                name(OsStr::new(CLAIM))?.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        } != 0
        {
            return Err("cannot finish uninstall claim");
        }
        Ok(())
    }

    /// Final metadata tail. The executable and record remain usable through
    /// every failure in substantive data/release cleanup; only this final,
    /// already empty state can retire them.
    fn finish_recovery(&self) -> Result<(), &'static str> {
        if self.open_relative(&self.releases_rel)?.is_some() {
            return Err("uninstall releases remain");
        }
        let Some(data) = self.open_relative(&self.data_rel)? else {
            return Ok(());
        };
        for entry in entries(data.as_raw_fd())? {
            if !self.roots.is_kept(&entry) {
                return Err("uninstall data remains");
            }
        }
        if exists_at(data.as_raw_fd(), OsStr::new(CLAIM))? {
            return Err("uninstall claim remains");
        }
        if exists_at(data.as_raw_fd(), OsStr::new(RECOVERY))? {
            let recovery_path = &self.recovery_rel;
            let recovery = self
                .open_relative(recovery_path)?
                .ok_or("uninstall recovery disappeared")?;
            let names = entries(recovery.as_raw_fd())?;
            if names.len() != 1 || names[0] != OsStr::new(&recovery_program()) {
                return Err("uninstall recovery directory changed");
            }
            self.verify_dir(recovery_path, recovery.as_raw_fd())?;
            if unsafe { libc::unlinkat(recovery.as_raw_fd(), c"sidevoice-connector".as_ptr(), 0) }
                != 0
            {
                return Err("cannot remove recovery executable");
            }
            self.verify_dir(&self.data_rel, data.as_raw_fd())?;
            if unsafe {
                libc::unlinkat(
                    data.as_raw_fd(),
                    c".uninstall-recovery".as_ptr(),
                    libc::AT_REMOVEDIR,
                )
            } != 0
            {
                return Err("cannot remove recovery directory");
            }
        }
        if exists_at(data.as_raw_fd(), OsStr::new("install.json"))? {
            self.verify_dir(&self.data_rel, data.as_raw_fd())?;
            if unsafe { libc::unlinkat(data.as_raw_fd(), c"install.json".as_ptr(), 0) } != 0 {
                return Err("cannot remove uninstall record");
            }
        }
        Ok(())
    }
}

/// What only the descriptor-based removal can be asked: its mount probe and the exact refusals. The rest of its
/// behaviour is tested against the interface, in `uninstall.rs`.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::env::{Env, Environment};
    use crate::platform::fs::TestFiles;
    use crate::platform::fs::{Files, Fs};
    use crate::test_support::temp_dir;
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};

    const KEPT: &[&str] = &["install.lock"];

    /// `D` below the anchor (the scratch home it is made in).
    fn data_rel() -> &'static Path {
        Path::new(Environment::DATA_NAME)
    }

    /// The preflight walk of `D`, with the mount probe made to report a different mount for `D` itself.
    fn preflight_with_simulated_data_mount(guard: &Os) -> Result<bool, &'static str> {
        let probe = |fd, path: &Path| {
            let mut observed = mount(fd)?;
            if path == data_rel() {
                observed.device ^= 1;
            }
            Ok(observed)
        };
        guard.tree(data_rel(), false, KEPT, false, &probe)?;
        Ok(false)
    }

    fn removal(home: &Path) -> Os {
        let data = home.join(data_rel());
        Files::ensure_private_dir(&data).unwrap();
        let releases = data.join("install");
        Os::new(Roots {
            standard: Some(data.clone()),
            data,
            releases,
            kept: KEPT,
            home: Some(home.to_path_buf()),
        })
        .unwrap()
    }

    #[test]
    fn injected_mount_boundary_refuses_before_deleting() {
        let scratch = temp_dir("uninstall-mount");
        let home = scratch.path().join("home");
        let guard = removal(&home);
        let nested = home.join(data_rel()).join("mounted");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("keep"), "untouched").unwrap();
        let probe = |fd: RawFd, path: &Path| {
            let mut observed = mount(fd)?;
            if path == data_rel().join("mounted") {
                observed.device ^= 1;
            }
            Ok(observed)
        };
        assert_eq!(
            guard.tree(data_rel(), false, &[], false, &probe),
            Err("uninstall mount or device boundary")
        );
        assert!(nested.join("keep").exists());
        assert_eq!(
            preflight_with_simulated_data_mount(&guard),
            Err("uninstall mount or device boundary")
        );
    }

    #[test]
    fn directory_replaced_by_link_during_removal_is_refused() {
        let scratch = temp_dir("uninstall-race");
        let home = scratch.path().join("home");
        let guard = removal(&home);
        let victim = home.join(data_rel()).join("victim");
        let moved = home.join(data_rel()).join("moved");
        let outside = scratch.path().join("outside");
        fs::create_dir_all(&victim).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(victim.join("inside"), "inside").unwrap();
        fs::write(outside.join("keep"), "outside").unwrap();
        let swapped = AtomicBool::new(false);
        let probe = |fd: RawFd, path: &Path| {
            if path == data_rel().join("victim") && !swapped.swap(true, Ordering::SeqCst) {
                fs::rename(&victim, &moved).unwrap();
                Files::symlink(&outside, &victim);
            }
            mount(fd)
        };
        assert!(guard.tree(data_rel(), false, &[], true, &probe).is_err());
        assert_eq!(fs::read_to_string(outside.join("keep")).unwrap(), "outside");
        assert_eq!(fs::read_to_string(moved.join("inside")).unwrap(), "inside");
    }

    #[test]
    fn moved_directory_at_final_claim_boundary_is_an_error_and_keeps_its_sentinel() {
        let scratch = temp_dir("uninstall-claim-race");
        let home = scratch.path().join("home");
        let guard = removal(&home);
        let victim = home.join(data_rel()).join("victim");
        let moved = scratch.path().join("moved-outside");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("keep"), "outside sentinel").unwrap();
        let swapped = AtomicBool::new(false);
        let hook = |_: &OsStr| {
            if !swapped.swap(true, Ordering::SeqCst) {
                fs::rename(&victim, &moved).unwrap();
                Files::symlink(&moved, &victim);
            }
        };
        assert!(guard.remove_data_contents(&hook).is_err());
        assert_eq!(
            fs::read_to_string(moved.join("keep")).unwrap(),
            "outside sentinel"
        );
    }
}
