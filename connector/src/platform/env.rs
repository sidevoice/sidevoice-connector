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

/// What every OS implements ([`Environment`], in `env/unix.rs` and `env/windows.rs`): the three constants and the
/// two methods that differ. The rest is one body for every OS; an OS overrides it only if it must.
pub trait Env {
    /// The variable that names this user's home directory: `HOME`, `USERPROFILE`.
    const HOME: &'static str;

    /// The name of the connector's own directory inside the OS's standard place for a program's data, which is
    /// where its data directory is (`data_dir`): `Sidevoice` (macOS, Windows), `sidevoice` (Linux, by the XDG
    /// convention).
    const DATA_NAME: &'static str;

    /// Variables a child cannot run without, passed on from this process when it has them (Windows: `SystemRoot`
    /// and its like; a child without them cannot use the network or the system's crypto). None on Unix.
    const SYSTEM: &'static [&'static str];

    /// The `PATH` of a child started with nothing else: where this OS's programs and a package manager's are.
    fn base_path() -> OsString;

    /// How this user's login `PATH` is asked for; `None` when this process has no way to know it.
    fn login_path() -> Option<LoginPath>;

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::fs::{Create, Files, Fs};

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
}
