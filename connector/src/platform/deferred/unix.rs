//! Unix: an open file can be unlinked, so an uninstall is never left with something to delete later.

use std::io;
use std::path::PathBuf;

/// This OS's deferred deletion: none.
pub struct Os;

impl super::Deferred for Os {
    fn at_next_sign_in(_: &[PathBuf]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "nothing is left to delete later here: a file in use can be deleted at once",
        ))
    }
}
