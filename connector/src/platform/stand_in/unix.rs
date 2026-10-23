//! Unix: a `sh` script naming the part in `SV_STAND_IN` and `exec`ing this test binary with the arguments it got.

use crate::platform::env::{Env, Environment};
use crate::platform::fs::{Files, TestFiles};

/// The variable naming the part.
const PART: &str = "SV_STAND_IN";

/// This OS's stand-ins.
pub struct Os;

impl super::StandIns for Os {
    fn make(program: &std::path::Path, part: &str) {
        let exe = std::env::current_exe().unwrap();
        let script = format!(
            "#!/bin/sh\n{PART}={} exec {} \"$@\"\n",
            Environment::quote(part),
            Environment::quote(&exe.to_string_lossy())
        );
        std::fs::write(program, script).unwrap();
        Files::shut_others_out(program);
        Files::make_executable(program);
    }

    fn part() -> Option<String> {
        std::env::var(PART).ok()
    }
}
