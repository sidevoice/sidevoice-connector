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
pub(crate) fn symlink(target: impl AsRef<std::path::Path>, link: impl AsRef<std::path::Path>) {
    let (target, link) = (target.as_ref(), link.as_ref());
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link).unwrap();
    } else {
        std::os::windows::fs::symlink_file(target, link).unwrap();
    }
}

/// Makes `path` something a test may run (a script that stands in for an agent): executable by its owner on Unix.
/// Windows has no executable bit, the extension decides, so there is nothing to do there.
pub(crate) fn executable(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    #[cfg(windows)]
    let _ = path;
}

/// Makes `path`, created in a test's scratch directory, the owner's alone: Unix gives a new file or directory the
/// umask's access, so it is narrowed; on Windows a new one inherits the scratch directory's access, which is
/// already the owner's (and SYSTEM's and the administrators'), so there is nothing to do there.
pub(crate) fn private(path: &std::path::Path) {
    #[cfg(unix)]
    crate::platform::fs::shut_others_out(path);
    #[cfg(windows)]
    let _ = path;
}
