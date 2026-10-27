//! Unix: an advertised endpoint is a socket path. It is trusted only where a connector of this data directory would
//! put it, in a directory private to this user; the path is judged as written, no link followed.

use crate::platform::fs::{Files, Fs};
use crate::platform::ipc::{Channel, Channels, Endpoint};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};

/// This OS's advertised endpoints.
pub struct Os;

impl super::AdvertisedEndpoint for Os {
    fn trust(endpoint: &Endpoint, dir: &Path, name: &str) -> io::Result<()> {
        judge(Path::new(&endpoint.to_string()), dir, name, &roots())
    }
}

/// The name of the directory the endpoints of the data directory `dir` are in, below a runtime directory:
/// `sidevoice-<first 8 bytes of SHA-256(dir), in hex>`. `sidevoice-platform` names it so (`ipc::endpoint_dir`)
/// and keeps the name to itself; a test holds the two to the same name.
fn folder(dir: &Path) -> String {
    let hash = Sha256::digest(dir.as_os_str().as_encoded_bytes());
    format!("sidevoice-{}", hex::encode(&hash[..8]))
}

/// The directories a connector's endpoint directory may be right below: wherever a process of this user's would
/// find its runtime or temporary directory, whatever this process's own environment says.
fn roots() -> Vec<PathBuf> {
    let mut roots = vec![std::env::temp_dir(), PathBuf::from("/tmp")];
    roots.extend(Channels::runtime_dir());
    roots.extend(user_roots());
    roots
}

/// macOS: the per-user temporary directory launchd gives a job as `$TMPDIR` (`/var/folders/…/T/`), and `/tmp`'s
/// real path.
#[cfg(target_os = "macos")]
fn user_roots() -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut roots = vec![PathBuf::from("/private/tmp")];
    let mut buffer = vec![0u8; 1024];
    // SAFETY: `confstr` writes at most `buffer.len()` bytes, NUL included, into the buffer it is given, and returns
    // the length the whole value needs (NUL included), 0 when there is none.
    let needed = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_TEMP_DIR,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    if needed > 1 && needed <= buffer.len() {
        buffer.truncate(needed - 1);
        roots.push(PathBuf::from(std::ffi::OsString::from_vec(buffer)));
    }
    roots
}

/// Linux: the runtime directory systemd gives a user's session and its services, `/run/user/<uid>`.
#[cfg(not(target_os = "macos"))]
fn user_roots() -> Vec<PathBuf> {
    vec![PathBuf::from(format!(
        "/run/user/{}",
        rustix::process::geteuid().as_raw()
    ))]
}

/// Whether the socket `path` is the endpoint `name` of the data directory `dir`, in its own directory below one of
/// `roots` or in `dir` itself, that directory a real one, this user's and open to nobody else.
fn judge(path: &Path, dir: &Path, name: &str, roots: &[PathBuf]) -> io::Result<()> {
    let refuse = |why: &dyn std::fmt::Display| super::untrusted(path.display(), why);
    if !path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
        return Err(refuse(&"not a plain absolute path"));
    }
    let socket = format!("{name}.sock");
    if path.file_name() != Some(OsStr::new(&socket)) {
        return Err(refuse(&format!("not named {socket}")));
    }
    let parent = path.parent().ok_or_else(|| refuse(&"no directory"))?;
    let folder = folder(dir);
    let placed = parent == dir
        || (parent.file_name() == Some(OsStr::new(&folder))
            && parent
                .parent()
                .is_some_and(|root| roots.iter().any(|known| known == root)));
    if !placed {
        return Err(refuse(&format!(
            "not in {} nor in {folder} below a temporary or runtime directory",
            dir.display()
        )));
    }
    // Opened as itself (a link is refused, not followed), and judged on that handle.
    let handle =
        Files::open_read(parent).map_err(|error| refuse(&format!("its directory: {error}")))?;
    if !handle.metadata()?.is_dir() {
        return Err(refuse(&"its directory is not one"));
    }
    Files::check_private(&handle).map_err(|error| refuse(&format!("its directory: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::fs::TestFiles;
    use crate::platform::ipc::Address;

    /// A root, a data directory, and the connector's endpoint directory of it below the root, private.
    struct Place {
        _scratch: tempfile::TempDir,
        root: PathBuf,
        data: PathBuf,
        folder: PathBuf,
    }

    fn place() -> Place {
        let scratch = crate::test_support::temp_dir("advertised");
        let root = scratch.path().join("runtime");
        let data = scratch.path().join("data");
        Files::ensure_private_dir(&root).unwrap();
        Files::ensure_private_dir(&data).unwrap();
        let folder = root.join(folder(&data));
        Files::create_private_dir(&folder).unwrap();
        Place {
            _scratch: scratch,
            root,
            data,
            folder,
        }
    }

    fn judged(place: &Place, path: &Path) -> io::Result<()> {
        judge(
            path,
            &place.data,
            "connector",
            std::slice::from_ref(&place.root),
        )
    }

    fn refused(result: io::Result<()>) -> bool {
        result.is_err_and(|error| error.kind() == io::ErrorKind::PermissionDenied)
    }

    #[test]
    fn the_directory_name_is_the_one_the_shared_ipc_gives_a_data_directory() {
        // Only where this host has a runtime directory does the shared `ipc` name one.
        let Some(runtime) = Channels::runtime_dir() else {
            return;
        };
        let data = Path::new("/Users/someone/Library/Application Support/Sidevoice");
        let endpoint = Endpoint::new(data, "connector").to_string();
        assert_eq!(
            Path::new(&endpoint),
            runtime.join(folder(data)).join("connector.sock")
        );
    }

    #[test]
    fn the_socket_in_the_private_endpoint_directory_below_a_root_or_in_the_data_directory_is_trusted(
    ) {
        let place = place();
        judged(&place, &place.folder.join("connector.sock")).unwrap();
        judged(&place, &place.data.join("connector.sock")).unwrap();
    }

    #[test]
    fn another_name_another_data_directory_or_another_place_is_refused() {
        let place = place();
        for path in [
            place.folder.join("core.sock"),
            place.folder.join("connector.sock.d"),
            place.root.join("connector.sock"),
            place.root.join(folder(&place.root)).join("connector.sock"),
            place
                .folder
                .join("..")
                .join(place.folder.file_name().unwrap())
                .join("connector.sock"),
            PathBuf::from("connector.sock"),
        ] {
            assert!(refused(judged(&place, &path)), "{}", path.display());
        }
        // The same name below a directory that is no temporary or runtime one.
        let elsewhere = place.data.join(folder(&place.data));
        Files::create_private_dir(&elsewhere).unwrap();
        assert!(refused(judged(&place, &elsewhere.join("connector.sock"))));
    }

    #[test]
    fn a_directory_open_to_others_is_refused() {
        let place = place();
        Files::let_others_read(&place.folder);
        assert!(refused(judged(
            &place,
            &place.folder.join("connector.sock")
        )));
        Files::let_others_change(&place.folder);
        assert!(refused(judged(
            &place,
            &place.folder.join("connector.sock")
        )));
    }

    #[test]
    fn a_link_in_the_directory_s_place_is_refused_even_to_a_private_one() {
        let place = place();
        let real = place.root.join("real");
        Files::create_private_dir(&real).unwrap();
        std::fs::remove_dir(&place.folder).unwrap();
        Files::symlink(&real, &place.folder);
        assert!(refused(judged(
            &place,
            &place.folder.join("connector.sock")
        )));
    }

    #[test]
    fn a_directory_of_another_user_is_refused() {
        // `/` is root's: as a data directory, its socket would be `/connector.sock`, in a directory not this user's
        // (unless this is root, which owns it).
        if rustix::process::geteuid().is_root() {
            return;
        }
        let root = Path::new("/");
        assert!(refused(judge(
            &root.join("connector.sock"),
            root,
            "connector",
            &[]
        )));
        // `/tmp`, root's and open to all (sticky), as the data directory the socket is in.
        assert!(refused(judge(
            Path::new("/tmp/connector.sock"),
            Path::new("/tmp"),
            "connector",
            &[]
        )));
    }
}
