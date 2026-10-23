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

    // `$SHELL` as a login shell prints its `PATH`. `-l` reads only the login files (`.zprofile`); many people put
    // `brew shellenv`, nvm and their own `PATH` in `.zshrc` / `.bashrc`, which only an interactive shell reads. An
    // interactive shell would try to take the terminal (and stop under job control), so the rc file is sourced by
    // hand, its output and errors dropped.
    fn login_path() -> Option<LoginPath> {
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|value| Path::new(value).is_absolute())?;
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
}
