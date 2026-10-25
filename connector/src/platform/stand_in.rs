//! Tests only: programs that stand in for the ones the code under test runs (an agent's CLI, an MCP server, a staged
//! connector). A stand-in is this test binary, started to play a part (`crate::test_support::stand_in`, which reads
//! the part and plays it before libtest's `main`), so its behaviour is written once, in Rust, and the same tests run
//! on every OS. What differs per OS is only how a file at a given path becomes "this test binary, playing that part".
//!
//! The interface is the [`StandIns`] trait; [`StandIn`] is the one OS implementation compiled in. Unix: a `sh`
//! script that names the part in a variable and `exec`s this test binary (no copy of it). Windows: a copy of this
//! test binary with the part appended after its image (Windows has no `#!`, and a program a test names `x.exe` must
//! be one), which the loader ignores.

#[cfg(unix)]
#[path = "stand_in/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "stand_in/windows.rs"]
mod imp;

pub use imp::Os as StandIn;

use std::path::Path;

/// What every OS implements ([`StandIn`]).
pub trait StandIns {
    /// Makes `program` (named as this OS names a program: `target::exe_name`) this test binary playing `part`, a
    /// line of JSON: executable, and private as `Files::shut_others_out` leaves a file. Two calls with another
    /// `part` give two different files.
    fn make(program: &Path, part: &str);

    /// The part this process was started to play, if it was started as a stand-in.
    fn part() -> Option<String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in made here is found, and plays its part, whatever the OS: run, it says the part back.
    #[test]
    fn a_stand_in_runs_and_plays_its_part() {
        use crate::platform::target;
        let scratch = crate::test_support::temp_dir("stand-in");
        let program = scratch.path().join(target::exe_name("echo-part"));
        StandIn::make(
            &program,
            r#"{"play":"script","print":"it's \"me\"\n","exit":3}"#,
        );
        let output = std::process::Command::new(&program).output().unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(String::from_utf8_lossy(&output.stdout), "it's \"me\"\n");
        assert_eq!(StandIn::part(), None, "this process is no stand-in");
    }
}
