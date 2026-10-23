//! Windows: `USERPROFILE`, the variables a child needs from the system, and the `PATH` a login has: the machine's
//! `Path` then the user's, from the registry (`winreg`) as Explorer builds it, since a Windows session has no login
//! shell and a service or a task starts with whatever `PATH` it was given.
//!
//! A `Path` of type `REG_EXPAND_SZ` names other variables (`%NVM_HOME%`, `%SystemRoot%`); they are expanded against
//! the registry's environment (the user's over the machine's, each value's own `%NAME%`s resolved the same way as
//! deep as they go, a cycle left as written), then this process's own, so a variable a version
//! manager added after this process started is still found. A `REG_SZ` value is taken as written. Expansion is own
//! code (`expand`): `ExpandEnvironmentStringsW`, and the crates that wrap it, only know this process's environment.

use super::LoginPath;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
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

/// The registry's environment (the user's variables over the machine's) and this process's, as one lookup.
struct Registry<'a> {
    user: &'a HashMap<String, Variable>,
    machine: &'a HashMap<String, Variable>,
    process: &'a dyn Fn(&str) -> Option<String>,
}

impl Registry<'_> {
    /// What the variable `name` stands for: the registry's, with the variables its own `REG_EXPAND_SZ` value names
    /// resolved the same way, as deep as they go; this process's when the registry has none. `visiting` are the
    /// names being resolved on the way here: one met again is a cycle, and is left as written.
    fn resolve(&self, name: &str, visiting: &[String]) -> Option<String> {
        let name = name.to_uppercase();
        if visiting.contains(&name) {
            return None;
        }
        match self.user.get(&name).or_else(|| self.machine.get(&name)) {
            Some(variable) => {
                let mut visiting = visiting.to_vec();
                visiting.push(name);
                Some(value(variable, |inner| self.resolve(inner, &visiting)))
            }
            None => (self.process)(&name),
        }
    }

    /// The machine's `Path` then the user's, each with the variables it names resolved ([`Registry::resolve`]).
    fn path(&self) -> String {
        let visiting = ["PATH".to_owned()];
        let path = |variables: &HashMap<String, Variable>| {
            variables
                .get("PATH")
                .map(|path| value(path, |name| self.resolve(name, &visiting)))
                .unwrap_or_default()
        };
        [path(self.machine), path(self.user)].join(";")
    }
}

impl super::Env for Os {
    const HOME: &'static str = "USERPROFILE";
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

    const CALL: &'static str = "& ";

    // PowerShell: inside single quotes only a quote is special, and PowerShell reads the typographic single quotes as
    // quotes too: each is doubled.
    fn quote(value: &str) -> String {
        let mut quoted = String::from("'");
        for c in value.chars() {
            quoted.push(c);
            if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
                quoted.push(c);
            }
        }
        quoted.push('\'');
        quoted
    }

    fn hand_safe(program: &str, args: &[&str]) -> bool {
        let direct = std::path::Path::new(program)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("exe") || extension.eq_ignore_ascii_case("com")
            });
        direct
            || !std::iter::once(program)
                .chain(args.iter().copied())
                .any(|word| word.contains(['&', '|', '<', '>', '^', '%', '!', '"']))
    }

    fn set(variable: &str, quoted_value: &str) -> String {
        format!("$env:{variable} = {quoted_value}; ")
    }

    // MSIX packages (a Store app) keep their files under `%LOCALAPPDATA%\Packages`.
    fn store_packages(local_app_data: &Path) -> Option<PathBuf> {
        Some(local_app_data.join("Packages"))
    }

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

    // A variable `Path` names is looked up in the registry's environment, the user's over the machine's (and the
    // variables its own `REG_EXPAND_SZ` value names, as deep as they go, a cycle left as written), then in this
    // process's.
    fn login_path() -> Option<LoginPath> {
        let machine = variables(HKEY_LOCAL_MACHINE, MACHINE);
        let user = variables(HKEY_CURRENT_USER, USER);
        let registry = Registry {
            user: &user,
            machine: &machine,
            process: &|name: &str| std::env::var(name).ok(),
        };
        LoginPath::clean(registry.path().as_ref()).map(LoginPath::Known)
    }

    fn in_sequence(commands: &[&str]) -> String {
        let mut line = String::new();
        for (index, command) in commands.iter().enumerate() {
            if index == 0 {
                line.push_str(command);
            } else {
                line.push_str(&format!("; if ($?) {{ {command} }}"));
            }
        }
        line
    }

    #[cfg(test)]
    fn hand_shell(line: &str) -> std::process::Command {
        // Windows PowerShell 5.1: the shell every Windows has (PowerShell 7 is installed apart), so what a person
        // most likely pastes into.
        let mut command = std::process::Command::new("powershell");
        command.args(["-NoProfile", "-NonInteractive", "-Command", line]);
        command
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

    fn table(entries: &[(&str, &str, bool)]) -> HashMap<String, Variable> {
        entries
            .iter()
            .map(|(name, text, expand)| {
                (
                    name.to_uppercase(),
                    Variable {
                        text: (*text).to_owned(),
                        expand: *expand,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn registry_variables_naming_registry_variables_resolve_as_deep_as_they_go() {
        let machine = table(&[
            ("TOOLS", r"C:\Tools", false),
            ("NVM_HOME", r"C:\Old", false),
            ("Path", r"%SystemRoot%\System32;%TOOLS%\bin", true),
        ]);
        let user = table(&[
            ("NVM_HOME", r"%TOOLS%\nvm", true),
            ("NVM_SYMLINK", r"%NVM_HOME%\current", true),
            ("Path", "%NVM_SYMLINK%;%Nvm_Home%;%PATH%", true),
        ]);
        let process = |name: &str| (name == "SYSTEMROOT").then(|| r"C:\Windows".to_owned());
        let registry = Registry {
            user: &user,
            machine: &machine,
            process: &process,
        };
        assert_eq!(
            registry.path(),
            r"C:\Windows\System32;C:\Tools\bin;C:\Tools\nvm\current;C:\Tools\nvm;%PATH%",
            "the user's over the machine's, then this process's; Path naming itself stays as written"
        );
    }

    #[test]
    fn a_cycle_between_registry_variables_is_left_as_written() {
        let machine = table(&[]);
        let user = table(&[
            ("A", r"%B%\a", true),
            ("B", r"%A%\b", true),
            ("Path", r"%A%;C:\ok", true),
        ]);
        let registry = Registry {
            user: &user,
            machine: &machine,
            process: &|_: &str| None,
        };
        assert_eq!(registry.path(), r";%A%\b\a;C:\ok");
    }
}
