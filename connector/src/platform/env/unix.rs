//! Unix: `HOME`, nothing a child cannot run without, a fixed `PATH` for one started with nothing else, and the login
//! `PATH` from the person's own shell.

use super::LoginPath;
use std::ffi::OsString;
use std::path::Path;

/// What a child started with nothing else has for a `PATH`.
const BASE_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// This OS's environment.
pub struct Os;

impl super::Env for Os {
    const HOME: &'static str = "HOME";
    // The sockets are in the runtime directory (`sidevoice-platform`'s `ipc`): a child must look in the same one.
    const SYSTEM: &'static [&'static str] = &["XDG_RUNTIME_DIR"];
    const CALL: &'static str = "";

    fn base_path() -> OsString {
        BASE_PATH.into()
    }

    fn quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    fn set(variable: &str, quoted_value: &str) -> String {
        format!("{variable}={quoted_value} ")
    }

    // The person's shell (the user database, then `$SHELL`) as a login shell prints its `PATH`. `-l` reads only the login files (`.zprofile`); many people put
    // `brew shellenv`, nvm and their own `PATH` in `.zshrc` / `.bashrc`, which only an interactive shell reads. An
    // interactive shell would try to take the terminal (and stop under job control), so the rc file is sourced by
    // hand, its output and errors dropped.
    fn login_path() -> Option<LoginPath> {
        login_path_of(database_shell(), std::env::var("SHELL").ok())
    }

    fn in_sequence(commands: &[&str]) -> String {
        commands.join(" && ")
    }

    #[cfg(test)]
    fn hand_shell(line: &str) -> std::process::Command {
        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-c").arg(line);
        command
    }
}

/// The shell to ask: the user database's entry for this uid (a service started by `systemd --user` or `launchd` may have
/// no `$SHELL`), else `$SHELL`; either must be an absolute path.
fn login_path_of(database: Option<String>, environment: Option<String>) -> Option<LoginPath> {
    let shell = [database, environment]
        .into_iter()
        .flatten()
        .find(|value| Path::new(value).is_absolute())?;
    let name = Path::new(&shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let print = r#"printf '\036SIDEVOICE_PATH_BEGIN\036%s\036SIDEVOICE_PATH_END\036' "$PATH""#;
    let command = match name {
        "fish" => "printf '\\036SIDEVOICE_PATH_BEGIN\\036%s\\036SIDEVOICE_PATH_END\\036' (string join : $PATH)".to_owned(),
        "zsh" => format!(r#"[ -r "${{ZDOTDIR:-$HOME}}/.zshrc" ] && . "${{ZDOTDIR:-$HOME}}/.zshrc" >/dev/null 2>&1 </dev/null; {print}"#),
        "bash" => format!(r#"[ -r "$HOME/.bashrc" ] && . "$HOME/.bashrc" >/dev/null 2>&1 </dev/null; {print}"#),
        _ => print.to_owned(),
    };
    Some(LoginPath::Run {
        program: shell.into(),
        args: vec!["-lc".into(), command],
    })
}

/// `pw_shell` of the current uid; none when the database has no entry or an empty shell. The tests (and `test-support` builds, which the integration tests start) have `$SHELL` stand
/// for the person's shell, so they never consult the machine's database.
#[cfg(not(any(test, feature = "test-support")))]
fn database_shell() -> Option<String> {
    let user = nix::unistd::User::from_uid(nix::unistd::Uid::current()).ok()??;
    user.shell
        .into_os_string()
        .into_string()
        .ok()
        .filter(|shell| !shell.is_empty())
}

#[cfg(any(test, feature = "test-support"))]
fn database_shell() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(login: Option<LoginPath>) -> Option<String> {
        match login? {
            LoginPath::Run { program, .. } => Some(program.to_string_lossy().into_owned()),
            _ => None,
        }
    }

    #[test]
    fn the_database_shell_is_used_when_shell_is_unset() {
        assert_eq!(
            program(login_path_of(Some("/bin/zsh".into()), None)).as_deref(),
            Some("/bin/zsh")
        );
    }

    #[test]
    fn the_database_wins_over_shell_and_shell_is_the_fallback() {
        let both = login_path_of(Some("/bin/zsh".into()), Some("/bin/bash".into()));
        assert_eq!(program(both).as_deref(), Some("/bin/zsh"));
        let only_env = login_path_of(None, Some("/bin/bash".into()));
        assert_eq!(program(only_env).as_deref(), Some("/bin/bash"));
        assert!(login_path_of(None, None).is_none());
        assert!(login_path_of(Some("zsh".into()), Some("bash".into())).is_none());
    }
}
