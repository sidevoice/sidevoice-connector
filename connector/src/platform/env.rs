//! The environment a program the connector starts is given, and where it looks for the programs it runs: the
//! variable a home is named by, what a child cannot run without, the `PATH` of one started with nothing else, and
//! the `PATH` of this user's login (where agents installed through a package manager or a version manager live).
//!
//! The interface is the [`Env`] trait; [`Environment`] is the one OS implementation compiled in, chosen below.
//! What differs per OS is data and the way the login `PATH` is asked for: a login shell on Unix, the user and
//! machine `Path` in the registry on Windows (a Windows session has no login shell). Everything else has one body,
//! the trait's default methods. Callers import both: `use crate::platform::env::{Env, Environment};` then
//! `Environment::find_program(name, &path)`.

#[cfg(unix)]
#[path = "env/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "env/windows.rs"]
mod imp;

pub use imp::Os as Environment;

use crate::platform::fs::Files;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// The longest `PATH` taken from a login.
const LONGEST_PATH: usize = 32_768;

/// How a login's `PATH` is asked for.
#[derive(Debug, PartialEq, Eq)]
pub enum LoginPath {
    /// Run `program` with `args`; what it prints between the markers is the `PATH` ([`LoginPath::parse`]). It is
    /// run as a person's own tool, with their profile's environment.
    Run { program: PathBuf, args: Vec<String> },
    /// The `PATH` itself, already cleaned ([`LoginPath::clean`]).
    Known(String),
}

impl LoginPath {
    /// The text printed before and after the `PATH` a [`LoginPath::Run`] program prints, so a banner or a prompt's
    /// noise around it is ignored.
    pub const START: &'static str = "\u{1e}SIDEVOICE_PATH_BEGIN\u{1e}";
    pub const END: &'static str = "\u{1e}SIDEVOICE_PATH_END\u{1e}";

    /// The `PATH` between the markers of `output`, cleaned; `None` when the markers are not there exactly once.
    pub fn parse(output: &str) -> Option<String> {
        let (_, framed) = output.split_once(Self::START)?;
        let (value, tail) = framed.split_once(Self::END)?;
        if tail.contains(Self::START) || tail.contains(Self::END) {
            return None;
        }
        Self::clean(OsStr::new(value))
    }

    /// `path` as a list that is safe to pass on: only the absolute entries are kept (a person's `PATH` may carry an
    /// empty or relative one, `::` or `.`; those are dropped, never the whole `PATH`), joined for this OS. `None`
    /// when it is empty, too long, has a control character, or leaves nothing.
    pub fn clean(path: &OsStr) -> Option<String> {
        let text = path.to_str()?;
        if text.is_empty() || text.len() > LONGEST_PATH || text.chars().any(char::is_control) {
            return None;
        }
        let kept: Vec<PathBuf> = std::env::split_paths(path)
            .filter(|part| part.is_absolute())
            .collect();
        if kept.is_empty() {
            return None;
        }
        std::env::join_paths(kept).ok()?.into_string().ok()
    }
}

/// What every OS implements ([`Environment`], in `env/unix.rs` and `env/windows.rs`): the constants and the
/// two methods that differ. The rest is one body for every OS; an OS overrides it only if it must.
pub trait Env {
    /// The variable that names this user's home directory: `HOME`, `USERPROFILE`.
    const HOME: &'static str;

    /// Variables a child cannot run without, passed on from this process when it has them (Windows: `SystemRoot`
    /// and its like; a child without them cannot use the network or the system's crypto; Unix: `XDG_RUNTIME_DIR`, where the
    /// sockets are, which the child and whoever talks to it must both resolve to the same directory).
    const SYSTEM: &'static [&'static str];

    /// What starts a command line that runs a quoted program name: nothing in a POSIX shell, the call operator `& `
    /// in PowerShell (a quoted name alone is only a string there).
    const CALL: &'static str;

    /// The `PATH` of a child started with nothing else: where this OS's programs and a package manager's are.
    fn base_path() -> OsString;

    /// `value` as one word of the command line a person types or pastes on this OS: single-quoted for a POSIX shell,
    /// single-quoted for PowerShell (nothing inside is special there but the quote itself, doubled).
    ///
    /// The contract on Windows: PowerShell reads the word exactly, but what reaches a native program is PowerShell's
    /// to pass. PowerShell 7.3 and later (`$PSNativeCommandArgumentPassing` `Windows`, the default, or `Standard`)
    /// passes an `.exe` every argument as given, an empty one and one with `"` in it included. Windows PowerShell
    /// 5.1, and 7.3+ for `.cmd`/`.bat` targets (their `Legacy` rules), drop an empty argument and mangle an embedded
    /// `"`. A `.cmd`/`.bat` is also re-parsed by `cmd.exe`, to which `& | < > ^ % !` and `"` are syntax whatever
    /// PowerShell quoted. So quoting is only half the contract: whoever builds a line to run by hand asks
    /// [`Env::hand_safe`] first, and passes no empty argument.
    fn quote(value: &str) -> String;

    /// Whether `program` with `args` can be written as one line to paste in this OS's shell and be read as the
    /// words given (see [`Env::quote`]). Always on Unix. On Windows a line is safe when the program is an `.exe` or
    /// `.com` (started directly), or when no word, the program included, has `&`, `|`, `<`, `>`, `^`, `%`, `!` or `"`:
    /// any other target (a `.cmd`/`.bat` shim, or a name with no extension that may resolve to one) is re-parsed by
    /// `cmd.exe`, which quoting for PowerShell does not protect against. When it is not safe, no line is made.
    fn hand_safe(_program: &str, _args: &[&str]) -> bool {
        true
    }

    /// The start of a command line that sets `variable` to `quoted_value` (already [`Env::quote`]d) for the command
    /// that follows: `NAME='v' ` in a POSIX shell, `$env:NAME = 'v'; ` in PowerShell.
    fn set(variable: &str, quoted_value: &str) -> String;

    /// `commands` (lines for this OS's shell) as one line that runs each once the one before succeeded, and stops at
    /// the first that fails: `a && b` in a POSIX shell; `a; if ($?) { b }` in PowerShell, which 5.1 (the one every
    /// Windows has) reads too, where `&&` does not exist and, in 7, may not be followed by an assignment (`$env:X = …`).
    fn in_sequence(commands: &[&str]) -> String;

    /// How this user's login `PATH` is asked for; `None` when this process has no way to know it.
    fn login_path() -> Option<LoginPath>;

    /// Tests: what runs `line`, written for this OS's shell as [`Env::quote`] and [`Env::set`] write one, the way a
    /// person who pastes it runs it: `sh -c` on Unix, Windows PowerShell 5.1 (`powershell -Command`, the one every
    /// Windows has) on Windows.
    #[cfg(test)]
    fn hand_shell(line: &str) -> std::process::Command;

    /// What a child started with its environment cleared is given: [`Env::base_path`] and [`Env::SYSTEM`].
    fn base() -> Vec<(OsString, OsString)> {
        let mut base = vec![(OsString::from("PATH"), Self::base_path())];
        base.extend(
            Self::SYSTEM
                .iter()
                .filter_map(|name| Some(((*name).into(), std::env::var_os(name)?))),
        );
        base
    }

    /// The program `name` as `path` (a list of directories) finds it: the first match, with the OS's rules for an
    /// executable (Windows: `PATHEXT`, so `tool` is found as `tool.cmd` or `tool.exe`).
    fn find_program(name: &str, path: &OsStr) -> Option<PathBuf> {
        // A rooted directory, so a relative entry of `path` or a relative `name` finds nothing of this process's.
        which::which_in(name, Some(path), Path::new(std::path::MAIN_SEPARATOR_STR)).ok()
    }

    /// Where the Microsoft Store packages of the user whose `%LOCALAPPDATA%` is `local_app_data` live (an app
    /// installed as MSIX keeps its files there); `None` where the OS has no such thing.
    fn store_packages(_local_app_data: &Path) -> Option<PathBuf> {
        None
    }

    /// A program the connector found outside `PATH` (somewhere it chose to look), judged before it is used: it and
    /// everything on the way to it from `root` (a directory of this user's, which the program must be under) belong
    /// to this user and cannot be changed by anybody else, and it is a regular file this OS can run. A link is
    /// resolved and what it leads to is judged, and must also be under `root`. Returns the program as `program`
    /// names it, or why it is refused (no path in it).
    fn trusted_program(program: &Path, root: &Path) -> Result<PathBuf, &'static str> {
        use crate::platform::fs::{Fs, Others};
        use crate::platform::user::{User, UserId};
        // The same form `target` has: `check_user_directory` gives the `\\?\` form on Windows.
        let root_found = Files::check_user_directory(root)
            .and_then(dunce::canonicalize)
            .map_err(|_| "its root is not safe")?;
        let target = dunce::canonicalize(program).map_err(|_| "it cannot be resolved")?;
        if !target.starts_with(&root_found) {
            return Err("it leads out of its root");
        }
        for path in [program, target.as_path()] {
            let base = if path == program { root } else { &root_found };
            for dir in path.ancestors().skip(1) {
                if dir == base {
                    break;
                }
                if !dir.starts_with(base) || Files::check_user_directory(dir).is_err() {
                    return Err("a directory on the way is not safe");
                }
            }
        }
        let file = Files::open_read(&target).map_err(|_| "it cannot be opened")?;
        let regular = file.metadata().is_ok_and(|meta| meta.is_file());
        let ours = UserId::current().owns_file(&file).unwrap_or(false);
        if !regular
            || !ours
            || Files::others_of(&file).map_or(true, |others| others == Others::Change)
        {
            return Err("it is not a regular file of this user's that nobody else can change");
        }
        which::which(&target).map_err(|_| "it cannot be run")?;
        Ok(program.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::fs::{Create, Files, Fs, TestFiles};

    /// Two absolute directories of this OS, as a list in its own form.
    fn absolute() -> (PathBuf, PathBuf, String) {
        let root = std::env::temp_dir();
        let (a, b) = (root.join("one"), root.join("two"));
        let list = std::env::join_paths([&a, &b])
            .unwrap()
            .into_string()
            .unwrap();
        (a, b, list)
    }

    fn framed(value: &str) -> String {
        format!("{}{value}{}", LoginPath::START, LoginPath::END)
    }

    #[test]
    fn the_login_path_ignores_banners_and_rejects_empty_or_incomplete_output() {
        let (_, _, list) = absolute();
        assert_eq!(
            LoginPath::parse(&format!("Welcome!\n{}\n", framed(&list))),
            Some(list.clone())
        );
        assert_eq!(LoginPath::parse("Welcome!\n"), None);
        assert_eq!(LoginPath::parse(&framed("")), None);
        assert_eq!(
            LoginPath::parse(&format!("{}{list}", LoginPath::START)),
            None
        );
        assert_eq!(LoginPath::parse(&framed("relative")), None);
        assert_eq!(
            LoginPath::parse(&format!("{}{}", framed(&list), framed(&list))),
            None,
            "twice is not one answer"
        );
        assert_eq!(LoginPath::parse(&framed(&format!("{list}\u{7}"))), None);
    }

    #[test]
    fn only_the_unusable_entries_of_a_path_are_dropped() {
        let (a, b, _) = absolute();
        let separator = if cfg!(windows) { ';' } else { ':' };
        let messy = format!(
            "{}{separator}{separator}.{separator}{}",
            a.display(),
            b.display()
        );
        assert_eq!(
            LoginPath::clean(OsStr::new(&messy)),
            Some(
                std::env::join_paths([&a, &b])
                    .unwrap()
                    .into_string()
                    .unwrap()
            )
        );
        assert_eq!(LoginPath::clean(OsStr::new("")), None);
        assert_eq!(LoginPath::clean(OsStr::new(".")), None);
        let long = format!("{}{}", a.display(), "x".repeat(LONGEST_PATH));
        assert_eq!(LoginPath::clean(OsStr::new(&long)), None);
    }

    #[test]
    fn a_child_started_with_nothing_else_has_a_path_and_what_the_system_needs() {
        let base = Environment::base();
        assert_eq!(base[0].0, "PATH");
        assert!(!base[0].1.is_empty());
        assert!(std::env::split_paths(&base[0].1).all(|dir| dir.is_absolute()));
        for (name, value) in &base[1..] {
            assert!(Environment::SYSTEM.iter().any(|system| name == system));
            assert_eq!(std::env::var_os(name).as_ref(), Some(value));
        }
    }

    #[test]
    fn a_value_is_quoted_as_one_word_and_set_before_a_command() {
        // A quote inside is the one thing each shell spells its own way: POSIX closes, escapes and reopens; PowerShell
        // doubles it.
        let (quote_inside, set) = if cfg!(windows) {
            ("'it''s'", "$env:NAME = 'a b'; ")
        } else {
            (r"'it'\''s'", "NAME='a b' ")
        };
        for (value, quoted) in [
            ("", "''"),
            ("plain", "'plain'"),
            ("two words", "'two words'"),
            ("it's", quote_inside),
            ("a\"b", "'a\"b'"),
            ("$HOME;rm", "'$HOME;rm'"),
        ] {
            assert_eq!(Environment::quote(value), quoted, "{value:?}");
        }
        assert_eq!(Environment::set("NAME", &Environment::quote("a b")), set);
    }

    /// Commands in sequence, run as a person pastes the line: each once the one before succeeded, none after a failure.
    #[test]
    fn commands_in_sequence_stop_at_the_first_that_fails() {
        use crate::platform::target::{Host, Target};
        use crate::test_support::stand_in::Script;
        let scratch = crate::test_support::temp_dir("env-sequence");
        let line = |name: &str| {
            format!(
                "{}{}",
                Environment::CALL,
                Environment::quote(&scratch.path().join(Host::exe_name(name)).to_string_lossy())
            )
        };
        let marker = scratch.path().join("ran");
        Script::default().make(&scratch.path().join(Host::exe_name("ok")));
        Script {
            exit: 1,
            ..Script::default()
        }
        .make(&scratch.path().join(Host::exe_name("fails")));
        Script {
            write: Some((marker.clone(), String::new())),
            ..Script::default()
        }
        .make(&scratch.path().join(Host::exe_name("marks")));
        assert_eq!(Environment::in_sequence(&["one"]), "one");

        let stopped = Environment::in_sequence(&[&line("fails"), &line("marks")]);
        Environment::hand_shell(&stopped).status().unwrap();
        assert!(!marker.exists(), "{stopped}");
        let middle = Environment::in_sequence(&[&line("ok"), &line("fails"), &line("marks")]);
        Environment::hand_shell(&middle).status().unwrap();
        assert!(
            !marker.exists(),
            "a failure in the middle stops the rest: {middle}"
        );

        let ran = Environment::in_sequence(&[&line("ok"), &line("ok"), &line("marks")]);
        assert!(
            Environment::hand_shell(&ran).status().unwrap().success(),
            "{ran}"
        );
        assert!(marker.exists(), "{ran}");
    }

    #[test]
    fn programs_are_found_by_the_os_rules_for_an_executable() {
        let scratch = crate::test_support::temp_dir("env-find");
        let name = format!("sv-tool{}", std::env::consts::EXE_SUFFIX);
        let program = scratch.path().join(&name);
        // A program as the platform makes one (executable where the OS marks that on the file).
        std::io::Write::write_all(
            &mut Files::open_private(&program, Create::NewProgram).unwrap(),
            b"#!/bin/sh\n",
        )
        .unwrap();
        let path = std::env::join_paths([scratch.path()]).unwrap();
        assert_eq!(Environment::find_program("sv-tool", &path), Some(program));
        assert_eq!(Environment::find_program("sv-absent", &path), None);
        assert_eq!(
            Environment::find_program("sv-tool", OsStr::new("")),
            None,
            "an empty path finds nothing"
        );
    }

    /// A program of this user's as the platform makes one, in a private directory under `root`.
    fn program_under(root: &Path, relative: &str) -> PathBuf {
        let program = root.join(relative);
        Files::ensure_private_dir(program.parent().unwrap()).unwrap();
        std::io::Write::write_all(
            &mut Files::open_private(&program, Create::NewProgram).unwrap(),
            b"#!/bin/sh\n",
        )
        .unwrap();
        program
    }

    #[test]
    fn a_program_outside_path_is_trusted_only_when_nobody_else_can_change_it_or_the_way_to_it() {
        let scratch = crate::test_support::temp_dir("env-trust");
        let root = scratch.path();
        let program = program_under(root, &format!("a/b/tool{}", std::env::consts::EXE_SUFFIX));
        assert_eq!(
            Environment::trusted_program(&program, root),
            Ok(program.clone())
        );
        // A link is judged by what it leads to.
        let link = root.join("link");
        Files::symlink(&program, &link);
        assert_eq!(Environment::trusted_program(&link, root), Ok(link.clone()));
        // Not under the root it is said to be under, or not there at all.
        let other = crate::test_support::temp_dir("env-trust-other");
        assert!(Environment::trusted_program(&program, other.path()).is_err());
        assert!(Environment::trusted_program(&root.join("none"), root).is_err());
        let outside = program_under(
            other.path(),
            &format!("x/tool{}", std::env::consts::EXE_SUFFIX),
        );
        let escape = root.join("escape");
        Files::symlink(&outside, &escape);
        assert!(
            Environment::trusted_program(&escape, root).is_err(),
            "a link out of the root"
        );
        // Somebody else can change the directory on the way, or the program.
        Files::let_others_change(program.parent().unwrap());
        assert!(Environment::trusted_program(&program, root).is_err());
        Files::shut_others_out(program.parent().unwrap());
        assert!(Environment::trusted_program(&program, root).is_ok());
        Files::let_others_change(&program);
        assert!(Environment::trusted_program(&program, root).is_err());
    }
}
