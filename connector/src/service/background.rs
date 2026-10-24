//! `sidevoice-background`: the program a Windows service job's task runs, instead of the console program.
//!
//! Task Scheduler gives a console program a console window of its own, and closing it ends the job. This program is a
//! Windows GUI program with no window (`src/background.rs`), the way `pythonw.exe` is: it starts the connector that
//! sits next to it with the arguments it was given, without a window, as the leader of a tree
//! ([`Processes::spawn_tree`]: the Job Object that ends everything in it when this program does, however it ends), waits
//! for it and exits with its code, so that the task's restart on failure, and a clean exit staying down, are those of the
//! connector's `run-job`. Stopping the job is not this program's: the job's own process is asked to leave
//! (`platform::service`), the connector's `run-job` then exits, and so does this program.

use crate::platform::process::{ChildProcess, Processes};
use crate::platform::target::{Host, Target as _};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

/// The program this one is named after, as the installation names it (`platform::service::Service::BACKGROUND`).
pub const PROGRAM: &str = "sidevoice-background";

/// The connector beside the running program.
pub fn connector() -> io::Result<PathBuf> {
    Ok(std::env::current_exe()?.with_file_name(Host::exe_name("sidevoice-connector")))
}

/// `program` run with `args` and no window, waited for: its exit code (1 when it has none).
pub async fn run(program: &Path, args: Vec<OsString>) -> io::Result<i32> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = Processes::spawn_tree(command).await?;
    Ok(child.wait().await?.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the child below runs: this test binary, exiting with the code its argument says.
    #[test]
    #[ignore = "a child of the test below, not a test"]
    fn exits_with_the_code_it_is_told() {
        let code = std::env::var("SIDEVOICE_TEST_EXIT")
            .ok()
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        std::process::exit(code);
    }

    #[tokio::test]
    async fn the_program_gets_the_arguments_and_its_exit_code_is_returned() {
        let me = std::env::current_exe().unwrap();
        let args = [
            "--ignored",
            "--exact",
            "service::background::tests::exits_with_the_code_it_is_told",
        ];
        for code in [0, 3] {
            std::env::set_var("SIDEVOICE_TEST_EXIT", code.to_string());
            let got = run(&me, args.iter().map(OsString::from).collect())
                .await
                .unwrap();
            assert_eq!(got, code);
        }
        std::env::remove_var("SIDEVOICE_TEST_EXIT");
        assert!(run(Path::new("no-such-program-here"), vec![])
            .await
            .is_err());
    }
}
