//! `D`, the connector's data directory, and `R`, where its releases are: one rule, used by every place that needs
//! them (`profile`, `service/layout`, the uninstall's allowlist).
//!
//! `D` is one directory for the program, in the OS's standard place for a program's data (`dirs`):
//!
//! | OS | `D` |
//! |---|---|
//! | macOS | `~/Library/Application Support/Sidevoice` |
//! | Linux | `$XDG_DATA_HOME/sidevoice`, else `~/.local/share/sidevoice` |
//! | Windows | `%LOCALAPPDATA%\Sidevoice` |
//!
//! `$SIDEVOICE_DATA_DIR` overrides it (tests, profiles). `R` is `D/install`: the releases are inside `D`.

use crate::platform::env::{Env, Environment};
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The variable that names `D`.
pub const VARIABLE: &str = "SIDEVOICE_DATA_DIR";

/// `R` inside `D`: `R/current.json` and `R/releases/<id>`.
const RELEASES: &str = "install";

/// `D` as the environment `var` names it: `$SIDEVOICE_DATA_DIR` when set (absolute), else this OS's standard place.
pub fn data_dir(var: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf> {
    match var(VARIABLE).filter(|value| !value.is_empty()) {
        Some(value) => {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                bail!("{VARIABLE} must be absolute");
            }
            Ok(path)
        }
        None => dirs::data_local_dir()
            .map(|place| place.join(Environment::DATA_NAME))
            .filter(|path| path.is_absolute())
            .context("this user has no standard data directory"),
    }
}

/// `R`, the releases' directory, for the data directory `data`.
pub fn releases_dir(data: &Path) -> PathBuf {
    data.join(RELEASES)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> Option<OsString> {
        None
    }

    #[test]
    fn the_default_is_one_directory_in_the_standard_place_and_the_override_wins() {
        let default = data_dir(none).unwrap();
        assert_eq!(default.file_name().unwrap(), Environment::DATA_NAME);
        assert_eq!(
            default.parent().unwrap(),
            dirs::data_local_dir().unwrap(),
            "the program's own directory, directly in the place for data"
        );
        let elsewhere = std::env::temp_dir().join("elsewhere");
        assert_eq!(
            data_dir(|name| (name == VARIABLE).then(|| elsewhere.clone().into_os_string()))
                .unwrap(),
            elsewhere
        );
        assert_eq!(
            data_dir(|name| (name == VARIABLE).then(OsString::new)).unwrap(),
            default,
            "an empty override is no override"
        );
        assert!(data_dir(|name| (name == VARIABLE).then(|| "relative".into())).is_err());
    }

    #[test]
    fn the_releases_are_inside_the_data_directory() {
        let data = std::env::temp_dir().join("data");
        assert_eq!(releases_dir(&data).parent().unwrap(), data);
    }
}
