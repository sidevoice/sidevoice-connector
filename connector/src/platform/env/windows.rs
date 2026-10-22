//! Windows: `USERPROFILE`, the variables a child needs from the system, and the `PATH` a login has: the machine's
//! `Path` then the user's, from the registry (`winreg`) as Explorer builds it, since a Windows session has no login
//! shell and a service or a task starts with whatever `PATH` it was given.

use super::{Env, LoginPath};
use std::ffi::OsString;
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
use winreg::RegKey;

const MACHINE: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";
const USER: &str = "Environment";

/// This OS's environment.
pub struct Os;

/// The registry's `Path` under `key` of `root`, its `%NAME%` references expanded; empty when there is none.
fn registry_path(root: RegKey, key: &str) -> String {
    root.open_subkey(key)
        .and_then(|key| key.get_value::<String, _>("Path"))
        .map(|path| Os::expand(&path, |name| std::env::var(name).ok()))
        .unwrap_or_default()
}

impl super::Env for Os {
    const HOME: &'static str = "USERPROFILE";
    const DATA_NAME: &'static str = "Sidevoice";
    // What programs started with an empty environment fail without (`SystemRoot`: sockets and the system's crypto;
    // `PATHEXT`: how a `.cmd` is found; the per-user folders and the temporary directory most programs write to).
    const SYSTEM: &'static [&'static str] = &[
        "SystemRoot",
        "SystemDrive",
        "windir",
        "ComSpec",
        "PATHEXT",
        "LOCALAPPDATA",
        "APPDATA",
        "TEMP",
        "TMP",
        "ProgramData",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "CommonProgramFiles",
        "USERNAME",
        "USERDOMAIN",
        "COMPUTERNAME",
        "NUMBER_OF_PROCESSORS",
        "PROCESSOR_ARCHITECTURE",
    ];

    // The registry's, or this process's own when the registry gives nothing usable.
    fn base_path() -> OsString {
        Self::login_path()
            .and_then(|path| match path {
                LoginPath::Known(path) => Some(path.into()),
                LoginPath::Run { .. } => None,
            })
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_default()
    }

    fn login_path() -> Option<LoginPath> {
        let joined = [
            registry_path(RegKey::predef(HKEY_LOCAL_MACHINE), MACHINE),
            registry_path(RegKey::predef(HKEY_CURRENT_USER), USER),
        ]
        .join(";");
        LoginPath::clean(joined.as_ref()).map(LoginPath::Known)
    }
}
