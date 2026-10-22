//! Uninstall only the installer's two owned roots, using no-follow directory handles.

use crate::service::layout::Layout;
use serde_json::{json, Value};
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

const CLAIM: &str = ".uninstall-claim";
const CLAIM_REL: &str = ".sidevoice/.uninstall-claim";
const RECOVERY: &str = ".uninstall-recovery";
const DATA_KEPT: &[&str] = &[
    "install.lock",
    "connector.lock",
    "agents.lock",
    "node-stopped.json",
    "install.json",
    CLAIM,
    RECOVERY,
];

fn clean_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

pub(super) fn validate_scope(layout: &Layout) -> Result<(), &'static str> {
    let home = &layout.home;
    if !clean_absolute(home)
        || home == Path::new("/")
        || fs::canonicalize(home).ok().as_deref() != Some(home)
    {
        return Err("unsafe home for uninstall");
    }
    let data = home.join(".sidevoice");
    let data_home = home.join(".local/share");
    let releases = data_home.join("sidevoice");
    if !clean_absolute(&layout.data)
        || !clean_absolute(&layout.data_home)
        || !clean_absolute(&layout.releases)
        || layout.data != data
        || layout.data_home != data_home
        || layout.releases != releases
    {
        return Err("uninstall path is outside known Sidevoice roots");
    }
    for path in [&layout.data, &layout.releases] {
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err("uninstall root is not a real directory")
            }
            Ok(_) if fs::canonicalize(path).ok().as_deref() != Some(path) => {
                return Err("uninstall root is not canonical")
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err("cannot inspect uninstall root"),
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Mount {
    device: libc::dev_t,
    #[cfg(target_os = "linux")]
    id: u64,
    #[cfg(target_os = "macos")]
    mounted_at: Vec<u8>,
}

fn stat_fd(fd: RawFd) -> Result<libc::stat, &'static str> {
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

#[cfg(target_os = "linux")]
fn mount(fd: RawFd) -> Result<Mount, &'static str> {
    let stat = stat_fd(fd)?;
    let info = fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))
        .map_err(|_| "cannot inspect uninstall mount")?;
    let id = info
        .lines()
        .find_map(|line| line.strip_prefix("mnt_id:\t"))
        .and_then(|id| id.trim().parse().ok())
        .ok_or("cannot inspect uninstall mount")?;
    Ok(Mount {
        device: stat.st_dev,
        id,
    })
}

#[cfg(target_os = "macos")]
fn mount(fd: RawFd) -> Result<Mount, &'static str> {
    let stat = stat_fd(fd)?;
    let mut fs_stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(fd, &mut fs_stat) } != 0 {
        return Err("cannot inspect uninstall mount");
    }
    let mounted_at = unsafe { CStr::from_ptr(fs_stat.f_mntonname.as_ptr()) }
        .to_bytes()
        .to_vec();
    Ok(Mount {
        device: stat.st_dev,
        mounted_at,
    })
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
    let stream = unsafe { libc::fdopendir(std::os::fd::IntoRawFd::into_raw_fd(copy)) };
    if stream.is_null() {
        return Err("cannot enumerate uninstall directory");
    }
    let result = (|| {
        let mut found = Vec::new();
        loop {
            #[cfg(target_os = "linux")]
            unsafe {
                *libc::__errno_location() = 0
            };
            #[cfg(target_os = "macos")]
            unsafe {
                *libc::__error() = 0
            };
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                if io::Error::last_os_error().raw_os_error() == Some(0) {
                    break;
                }
                return Err("cannot enumerate uninstall directory");
            }
            let raw = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if raw != b"." && raw != b".." {
                found.push(OsStr::from_bytes(raw).to_owned());
            }
        }
        Ok(found)
    })();
    unsafe { libc::closedir(stream) };
    result
}

pub(super) struct Guard {
    home_path: PathBuf,
    home: OwnedFd,
    expected: Mount,
}

impl Guard {
    pub(super) fn new(layout: &Layout) -> Result<Self, &'static str> {
        validate_scope(layout)?;
        let home = absolute_dir(&layout.home)?;
        let expected = mount(home.as_raw_fd())?;
        Ok(Self {
            home_path: layout.home.clone(),
            home,
            expected,
        })
    }

    fn check_mount(&self, fd: RawFd) -> Result<(), &'static str> {
        if mount(fd)? == self.expected {
            Ok(())
        } else {
            Err("uninstall mount or device boundary")
        }
    }

    pub(super) fn data_for_lock(&self) -> Result<OwnedFd, &'static str> {
        let child = OsStr::new(".sidevoice");
        if !exists_at(self.home.as_raw_fd(), child)? {
            let child_name = name(child)?;
            if unsafe { libc::mkdirat(self.home.as_raw_fd(), child_name.as_ptr(), 0o700) } != 0 {
                return Err("cannot create uninstall data root");
            }
        }
        let data = open_dir(self.home.as_raw_fd(), child)?;
        self.check_mount(data.as_raw_fd())?;
        self.verify_dir(Path::new(".sidevoice"), data.as_raw_fd())?;
        let stat = stat_fd(data.as_raw_fd())?;
        if stat.st_uid != crate::platform::user::current().raw() || stat.st_mode & 0o077 != 0 {
            return Err("unsafe uninstall data root");
        }
        Ok(data)
    }

    fn open_relative(&self, path: &Path) -> Result<Option<OwnedFd>, &'static str> {
        let mut current = open_dir(self.home.as_raw_fd(), OsStr::new("."))?;
        for component in path.components() {
            let Component::Normal(part) = component else {
                return Err("unsafe uninstall path");
            };
            if !exists_at(current.as_raw_fd(), part)? {
                return Ok(None);
            }
            current = open_dir(current.as_raw_fd(), part)?;
            self.check_mount(current.as_raw_fd())?;
        }
        Ok(Some(current))
    }

    fn verify_dir(&self, path: &Path, held: RawFd) -> Result<(), &'static str> {
        let named_home = absolute_dir(&self.home_path)?;
        if !same(
            &stat_fd(named_home.as_raw_fd())?,
            &stat_fd(self.home.as_raw_fd())?,
        ) {
            return Err("uninstall home replaced during removal");
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
        if stat.st_uid != crate::platform::user::current().raw() || stat.st_mode & 0o777 != 0o700 {
            return Err("unsafe uninstall claim directory");
        }
        Ok(child)
    }

    fn claim_area(&self) -> Result<Option<(OwnedFd, OwnedFd)>, &'static str> {
        let Some(data) = self.open_relative(Path::new(".sidevoice"))? else {
            return Ok(None);
        };
        self.verify_dir(Path::new(".sidevoice"), data.as_raw_fd())?;
        let claim = self.private_child(data.as_raw_fd(), CLAIM)?;
        self.verify_dir(Path::new(CLAIM_REL), claim.as_raw_fd())?;
        Ok(Some((data, claim)))
    }

    /// Pin a copy of this running CLI inside D and atomically point install.json
    /// at it before any installed release or personal data is removed. A retry
    /// through the desktop's installed-CLI lookup can then resume the claim.
    pub(super) fn stage_recovery(&self, layout: &Layout) -> Result<(), &'static str> {
        let data = self
            .open_relative(Path::new(".sidevoice"))?
            .ok_or("uninstall data root disappeared")?;
        self.verify_dir(Path::new(".sidevoice"), data.as_raw_fd())?;
        let recovery = self.private_child(data.as_raw_fd(), RECOVERY)?;
        let executable = OsStr::new("sidevoice-connector");
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
                || stat.st_uid != crate::platform::user::current().raw()
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
        self.verify_dir(Path::new(".sidevoice"), data.as_raw_fd())?;
        self.verify_dir(
            Path::new(".sidevoice/.uninstall-recovery"),
            recovery.as_raw_fd(),
        )?;
        let mut record = if exists_at(data.as_raw_fd(), OsStr::new("install.json"))? {
            layout
                .install_record()
                .ok_or("unsafe uninstall install record")?
        } else {
            json!({})
        };
        let path = layout.data.join(RECOVERY).join("sidevoice-connector");
        record["command"] = json!([path]);
        record["releases"] = json!(layout.releases);
        self.write_install_record(data.as_raw_fd(), &record)
    }

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
        self.verify_dir(Path::new(".sidevoice"), data)?;
        if unsafe { libc::renameat(data, temporary.as_ptr(), data, c"install.json".as_ptr()) } != 0
        {
            return Err("cannot save uninstall record");
        }
        Ok(())
    }

    fn claim_entry(
        &self,
        source: RawFd,
        source_path: &Path,
        source_name: &OsStr,
        claim: RawFd,
        claimed_name: &str,
        before_rename: &impl Fn(&OsStr),
    ) -> Result<(), &'static str> {
        self.verify_dir(source_path, source)?;
        self.verify_dir(Path::new(CLAIM_REL), claim)?;
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
        self.verify_dir(Path::new(CLAIM_REL), claim)?;
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
                #[cfg(target_os = "linux")]
                {
                    let child = unsafe {
                        libc::openat(
                            fd,
                            child_name.as_ptr(),
                            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                        )
                    };
                    if child < 0 {
                        return Err("uninstall entry replaced");
                    }
                    let child = unsafe { OwnedFd::from_raw_fd(child) };
                    if !same(&before, &stat_fd(child.as_raw_fd())?) {
                        return Err("uninstall entry replaced");
                    }
                    self.check_mount(child.as_raw_fd())?;
                }
                #[cfg(target_os = "macos")]
                if before.st_mode & libc::S_IFMT == libc::S_IFREG {
                    let child = unsafe {
                        libc::openat(
                            fd,
                            child_name.as_ptr(),
                            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                        )
                    };
                    if child < 0 {
                        return Err("uninstall entry replaced");
                    }
                    let child = unsafe { OwnedFd::from_raw_fd(child) };
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

    pub(super) fn preflight(&self) -> Result<bool, &'static str> {
        let probe = |fd, _: &Path| mount(fd);
        let releases = self.tree(
            Path::new(".local/share/sidevoice"),
            false,
            &[],
            false,
            &probe,
        )?;
        self.tree(Path::new(".sidevoice"), false, super::KEPT, false, &probe)?;
        let claimed_releases = if let Some(claim) = self.open_relative(Path::new(CLAIM_REL))? {
            entries(claim.as_raw_fd())?
                .iter()
                .any(|name| name.as_bytes().starts_with(b"release-"))
        } else {
            false
        };
        Ok(releases || claimed_releases)
    }

    #[cfg(test)]
    pub(super) fn preflight_with_simulated_data_mount(&self) -> Result<bool, &'static str> {
        let probe = |fd, path: &Path| {
            let mut observed = mount(fd)?;
            if path == Path::new(".sidevoice") {
                observed.device ^= 1;
            }
            Ok(observed)
        };
        self.tree(Path::new(".sidevoice"), false, super::KEPT, false, &probe)?;
        Ok(false)
    }

    pub(super) fn remove_releases(&self) -> Result<(), &'static str> {
        let Some((_, claim)) = self.claim_area()? else {
            return Err("uninstall data root disappeared");
        };
        let release_parent = Path::new(".local/share");
        if let Some(parent) = self.open_relative(release_parent)? {
            if exists_at(parent.as_raw_fd(), OsStr::new("sidevoice"))? {
                let claimed_name = format!("release-{}", uuid::Uuid::new_v4().simple());
                self.claim_entry(
                    parent.as_raw_fd(),
                    release_parent,
                    OsStr::new("sidevoice"),
                    claim.as_raw_fd(),
                    &claimed_name,
                    &|_| {},
                )?;
            }
        }
        for entry in entries(claim.as_raw_fd())? {
            if !entry.as_bytes().starts_with(b"release-") {
                continue;
            }
            self.verify_dir(Path::new(CLAIM_REL), claim.as_raw_fd())?;
            let entry_name = name(&entry)?;
            let before = stat_at(claim.as_raw_fd(), &entry_name)?;
            if before.st_mode & libc::S_IFMT == libc::S_IFDIR {
                self.tree(
                    &Path::new(CLAIM_REL).join(&entry),
                    true,
                    &[],
                    true,
                    &|fd, _| mount(fd),
                )?;
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

    #[cfg(test)]
    pub(super) fn remove_data_contents(&self) -> Result<Vec<String>, &'static str> {
        self.remove_data_contents_with_hook(&|_| {})
    }

    pub(super) fn remove_data_contents_with_hook(
        &self,
        before_rename: &impl Fn(&OsStr),
    ) -> Result<Vec<String>, &'static str> {
        let Some((root, claim)) = self.claim_area()? else {
            return Ok(Vec::new());
        };
        let mut removed = Vec::new();
        for entry in entries(root.as_raw_fd())? {
            if DATA_KEPT.iter().any(|kept| OsStr::new(kept) == entry) {
                continue;
            }
            let claimed_name = format!("data-{}", uuid::Uuid::new_v4().simple());
            self.claim_entry(
                root.as_raw_fd(),
                Path::new(".sidevoice"),
                &entry,
                claim.as_raw_fd(),
                &claimed_name,
                before_rename,
            )?;
            removed.push(entry.to_string_lossy().into_owned());
        }
        let release_names: Vec<String> = entries(claim.as_raw_fd())?
            .into_iter()
            .filter(|name| name.as_bytes().starts_with(b"release-"))
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        let kept: Vec<&str> = release_names.iter().map(String::as_str).collect();
        self.walk(
            claim.as_raw_fd(),
            Path::new(CLAIM_REL),
            true,
            &kept,
            0,
            &|fd, _| mount(fd),
        )?;
        Ok(removed)
    }

    pub(super) fn finish_claim(&self) -> Result<(), &'static str> {
        let Some(data) = self.open_relative(Path::new(".sidevoice"))? else {
            return Ok(());
        };
        if !exists_at(data.as_raw_fd(), OsStr::new(CLAIM))? {
            return Ok(());
        }
        let claim = open_dir(data.as_raw_fd(), OsStr::new(CLAIM))?;
        self.verify_dir(Path::new(CLAIM_REL), claim.as_raw_fd())?;
        if !entries(claim.as_raw_fd())?.is_empty() {
            return Err("uninstall claim still contains data");
        }
        self.verify_dir(Path::new(".sidevoice"), data.as_raw_fd())?;
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
    pub(super) fn finish_recovery(&self) -> Result<(), &'static str> {
        if self
            .open_relative(Path::new(".local/share/sidevoice"))?
            .is_some()
        {
            return Err("uninstall releases remain");
        }
        let Some(data) = self.open_relative(Path::new(".sidevoice"))? else {
            return Ok(());
        };
        for entry in entries(data.as_raw_fd())? {
            if !DATA_KEPT.iter().any(|kept| OsStr::new(kept) == entry) {
                return Err("uninstall data remains");
            }
        }
        if exists_at(data.as_raw_fd(), OsStr::new(CLAIM))? {
            return Err("uninstall claim remains");
        }
        if exists_at(data.as_raw_fd(), OsStr::new(RECOVERY))? {
            let recovery_path = Path::new(".sidevoice/.uninstall-recovery");
            let recovery = self
                .open_relative(recovery_path)?
                .ok_or("uninstall recovery disappeared")?;
            let names = entries(recovery.as_raw_fd())?;
            if names.len() != 1 || names[0] != OsStr::new("sidevoice-connector") {
                return Err("uninstall recovery directory changed");
            }
            self.verify_dir(recovery_path, recovery.as_raw_fd())?;
            if unsafe { libc::unlinkat(recovery.as_raw_fd(), c"sidevoice-connector".as_ptr(), 0) }
                != 0
            {
                return Err("cannot remove recovery executable");
            }
            self.verify_dir(Path::new(".sidevoice"), data.as_raw_fd())?;
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
            self.verify_dir(Path::new(".sidevoice"), data.as_raw_fd())?;
            if unsafe { libc::unlinkat(data.as_raw_fd(), c"install.json".as_ptr(), 0) } != 0 {
                return Err("cannot remove uninstall record");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn layout(home: &Path) -> Layout {
        Layout::from_vars(
            |name| (name == "HOME").then(|| home.as_os_str().to_owned()),
            std::iter::empty(),
            &[],
        )
        .unwrap()
    }

    #[test]
    fn only_exact_owned_roots_can_be_uninstalled() {
        let scratch = crate::platform::fs::tests::Scratch::new("uninstall-scope");
        let home = scratch.0.join("home");
        fs::create_dir(&home).unwrap();
        let mut layout = layout(&home);
        assert!(validate_scope(&layout).is_ok());
        for unsafe_path in [
            PathBuf::new(),
            PathBuf::from("relative"),
            PathBuf::from("/"),
            home.clone(),
            home.join(".sidevoice/.."),
            home.join("elsewhere"),
        ] {
            layout.data = unsafe_path;
            assert!(validate_scope(&layout).is_err());
        }
        layout.data = home.join(".sidevoice");
        layout.releases = home.clone();
        assert!(validate_scope(&layout).is_err());
        layout.releases = home.join(".local/share/sidevoice");
        layout.data_home = home.clone();
        assert!(validate_scope(&layout).is_err());
        layout.data_home = home.join(".local/share");
        symlink(&home, &layout.data).unwrap();
        assert!(validate_scope(&layout).is_err());
    }

    #[test]
    fn links_inside_owned_roots_never_delete_their_targets() {
        let scratch = crate::platform::fs::tests::Scratch::new("uninstall-links");
        let home = scratch.0.join("home");
        let outside = scratch.0.join("outside");
        let layout = layout(&home);
        fs::create_dir_all(&layout.data).unwrap();
        fs::create_dir_all(&layout.releases).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), "untouched").unwrap();
        symlink(&outside, layout.data.join("external")).unwrap();
        symlink(&outside, layout.releases.join("current")).unwrap();
        fs::write(layout.data.join("remove"), "gone").unwrap();

        let guard = Guard::new(&layout).unwrap();
        assert!(guard.preflight().unwrap());
        guard.remove_releases().unwrap();
        let removed = guard.remove_data_contents().unwrap();
        assert_eq!(removed.len(), 2);
        assert!(!layout.releases.exists());
        assert!(layout.data.exists());
        assert_eq!(
            fs::read_to_string(outside.join("keep")).unwrap(),
            "untouched"
        );
        assert!(!guard.preflight().unwrap());
        assert!(guard.remove_data_contents().unwrap().is_empty());
    }

    #[test]
    fn injected_mount_boundary_refuses_before_deleting() {
        let scratch = crate::platform::fs::tests::Scratch::new("uninstall-mount");
        let layout = layout(&scratch.0.join("home"));
        let nested = layout.data.join("mounted");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("keep"), "untouched").unwrap();
        let guard = Guard::new(&layout).unwrap();
        let probe = |fd: RawFd, path: &Path| {
            let mut observed = mount(fd)?;
            if path == Path::new(".sidevoice/mounted") {
                observed.device ^= 1;
            }
            Ok(observed)
        };
        assert_eq!(
            guard.tree(Path::new(".sidevoice"), false, &[], false, &probe),
            Err("uninstall mount or device boundary")
        );
        assert!(nested.join("keep").exists());
    }

    #[test]
    fn directory_replaced_by_link_during_removal_is_refused() {
        let scratch = crate::platform::fs::tests::Scratch::new("uninstall-race");
        let layout = layout(&scratch.0.join("home"));
        let victim = layout.data.join("victim");
        let moved = layout.data.join("moved");
        let outside = scratch.0.join("outside");
        fs::create_dir_all(&victim).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(victim.join("inside"), "inside").unwrap();
        fs::write(outside.join("keep"), "outside").unwrap();
        let guard = Guard::new(&layout).unwrap();
        let swapped = AtomicBool::new(false);
        let probe = |fd: RawFd, path: &Path| {
            if path == Path::new(".sidevoice/victim") && !swapped.swap(true, Ordering::SeqCst) {
                fs::rename(&victim, &moved).unwrap();
                symlink(&outside, &victim).unwrap();
            }
            mount(fd)
        };
        assert!(guard
            .tree(Path::new(".sidevoice"), false, &[], true, &probe)
            .is_err());
        assert_eq!(fs::read_to_string(outside.join("keep")).unwrap(), "outside");
        assert_eq!(fs::read_to_string(moved.join("inside")).unwrap(), "inside");
    }

    #[test]
    fn moved_directory_at_final_claim_boundary_keeps_its_sentinel() {
        let scratch = crate::platform::fs::tests::Scratch::new("uninstall-claim-race");
        let layout = layout(&scratch.0.join("home"));
        let victim = layout.data.join("victim");
        let moved = scratch.0.join("moved-outside");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("keep"), "outside sentinel").unwrap();
        let guard = Guard::new(&layout).unwrap();
        let swapped = AtomicBool::new(false);
        let hook = |_: &OsStr| {
            if !swapped.swap(true, Ordering::SeqCst) {
                fs::rename(&victim, &moved).unwrap();
                symlink(&moved, &victim).unwrap();
            }
        };
        assert!(guard.remove_data_contents_with_hook(&hook).is_err());
        assert_eq!(
            fs::read_to_string(moved.join("keep")).unwrap(),
            "outside sentinel"
        );
    }

    #[test]
    fn home_replaced_after_opening_is_refused() {
        let scratch = crate::platform::fs::tests::Scratch::new("uninstall-home-race");
        let home = scratch.0.join("home");
        let moved = scratch.0.join("moved-home");
        let layout = layout(&home);
        fs::create_dir_all(&layout.data).unwrap();
        fs::write(layout.data.join("keep"), "untouched").unwrap();
        let guard = Guard::new(&layout).unwrap();
        fs::rename(&home, &moved).unwrap();
        symlink(&moved, &home).unwrap();
        assert!(guard.remove_data_contents().is_err());
        assert_eq!(
            fs::read_to_string(moved.join(".sidevoice/keep")).unwrap(),
            "untouched"
        );
    }
}
