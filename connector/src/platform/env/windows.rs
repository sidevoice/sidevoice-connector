//! Windows: `USERPROFILE`, the variables a child needs from the system, and the `PATH` a login has: the machine's
//! `Path` then the user's, from the registry (`winreg`) as Explorer builds it, since a Windows session has no login
//! shell and a service or a task starts with whatever `PATH` it was given.
//!
//! A `Path` of type `REG_EXPAND_SZ` names other variables (`%NVM_HOME%`, `%SystemRoot%`); they are expanded against
//! the registry's environment (the user's over the machine's), then this process's own, so a variable a version
//! manager added after this process started is still found. A `REG_SZ` value is taken as written. Expansion is own
//! code (`expand`): `ExpandEnvironmentStringsW`, and the crates that wrap it, only know this process's environment.

use super::LoginPath;
use std::collections::HashMap;
use std::ffi::OsString;
use winreg::enums::{RegType, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
use winreg::types::FromRegValue;
use winreg::{RegKey, HKEY};

const MACHINE: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";
const USER: &str = "Environment";

/// This OS's environment.
pub struct Os;

/// One variable of the registry's environment: its text, and whether it names other variables (`REG_EXPAND_SZ`).
struct Variable {
    text: String,
    expand: bool,
}

/// The string variables under `key` of `root`, by upper-case name (Windows' names ignore case).
fn variables(root: HKEY, key: &str) -> HashMap<String, Variable> {
    let Ok(key) = RegKey::predef(root).open_subkey(key) else {
        return HashMap::new();
    };
    key.enum_values()
        .filter_map(Result::ok)
        .filter_map(|(name, value)| {
            let expand = match value.vtype {
                RegType::REG_EXPAND_SZ => true,
                RegType::REG_SZ => false,
                _ => return None,
            };
            let text = String::from_reg_value(&value).ok()?;
            Some((name.to_uppercase(), Variable { text, expand }))
        })
        .collect()
}

/// `text` with every `%NAME%` that `var` knows replaced by its value, as `ExpandEnvironmentStringsW` does: an
/// unknown or empty name is left as written, both signs included, and the scan goes on after its closing sign; a
/// `%` with no closing one is kept as it is.
fn expand(text: &str, var: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = &after[..end];
        match Some(name).filter(|name| !name.is_empty()).and_then(&var) {
            Some(value) => out.push_str(&value),
            None => out.push_str(&rest[start..start + end + 2]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// What `variable` stands for: expanded with `var` when it is a `REG_EXPAND_SZ`, as written otherwise.
fn value(variable: &Variable, var: impl Fn(&str) -> Option<String>) -> String {
    if variable.expand {
        expand(&variable.text, var)
    } else {
        variable.text.clone()
    }
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

    // A variable `Path` names is looked up in the registry's environment, the user's over the machine's (its own
    // `REG_EXPAND_SZ` value expanded against this process's environment, one level, so no loop), then in this
    // process's.
    fn login_path() -> Option<LoginPath> {
        let machine = variables(HKEY_LOCAL_MACHINE, MACHINE);
        let user = variables(HKEY_CURRENT_USER, USER);
        let process = |name: &str| std::env::var(name).ok();
        let var = |name: &str| {
            let name = name.to_uppercase();
            match user.get(&name).or_else(|| machine.get(&name)) {
                Some(variable) => Some(value(variable, process)),
                None => process(&name),
            }
        };
        let path = |variables: &HashMap<String, Variable>| {
            variables
                .get("PATH")
                .map(|path| value(path, var))
                .unwrap_or_default()
        };
        let joined = [path(&machine), path(&user)].join(";");
        LoginPath::clean(joined.as_ref()).map(LoginPath::Known)
    }
}

/// Unit tests here, not against the interface in `env.rs`: `%NAME%` expansion is how the Windows registry writes a
/// value, private to this file, with nothing it stands for on another OS.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_variables_are_expanded_and_the_rest_left_as_written_signs_included() {
        let var = |name: &str| (name == "ROOT").then(|| r"C:\Windows".to_owned());
        assert_eq!(
            expand(r"%ROOT%\System32;%NOPE%;100%;%;%%;50%", var),
            r"C:\Windows\System32;%NOPE%;100%;%;%%;50%"
        );
        assert_eq!(
            expand("%NOPE%ROOT%", var),
            "%NOPE%ROOT%",
            "an unknown name keeps its closing sign"
        );
        assert_eq!(expand("%NOPE%%ROOT%", var), r"%NOPE%C:\Windows");
    }

    #[test]
    fn only_an_expandable_value_is_expanded() {
        let var = |_: &str| Some("x".to_owned());
        let text = "%A%".to_owned();
        assert_eq!(
            value(
                &Variable {
                    text: text.clone(),
                    expand: true
                },
                var
            ),
            "x"
        );
        assert_eq!(
            value(
                &Variable {
                    text,
                    expand: false
                },
                var
            ),
            "%A%"
        );
    }
}
