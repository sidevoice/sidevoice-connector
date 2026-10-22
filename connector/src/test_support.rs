//! What the crate's unit tests share.

/// A scratch directory for one test, removed when dropped (`tempfile`). Under the temporary directory's canonical
/// path (macOS: `/private/var/...`, not the `/var` link), so paths built from it equal what the code canonicalizes.
/// Not private: a test that needs a private directory makes one inside with `Files::create_private_dir`.
pub(crate) fn temp_dir(label: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("sv-{label}-"))
        .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
        .unwrap()
}

/// A symbolic link at `link` to `target` (a directory or a file), for tests of what must never be followed. The one
/// place a test names the OS for it: a link is made differently on each.
pub(crate) fn symlink(target: &std::path::Path, link: &std::path::Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link).unwrap();
    } else {
        std::os::windows::fs::symlink_file(target, link).unwrap();
    }
}

/// `path` made runnable by its owner, for a test that needs a program a lookup finds. Where a file's name makes it
/// one (Windows), nothing to do.
pub(crate) fn make_executable(path: &std::path::Path) {
    #[cfg(unix)]
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    #[cfg(windows)]
    let _ = path;
}
