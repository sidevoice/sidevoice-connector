//! Stand-ins for the programs the code under test runs, played by this test binary (`platform::stand_in` makes the
//! file): the behaviour is Rust, once, so the same tests run on every OS (as `tests/health.rs` does with its fake MCP
//! servers). A part is a JSON object whose `play` names who plays it. [`Script`] is the common one, played here; a
//! module with a part of its own (an agent's CLI) plays it from its own [`ctor`] with [`serve`].
//!
//! The parts are played before libtest's `main` (`ctor`): a stand-in is started with the arguments the code under test
//! gives it, which libtest would refuse.

use crate::platform::stand_in::{StandIn, StandIns};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Makes `program` a stand-in playing `part` (an object with `play`).
pub(crate) fn make(program: &Path, part: &Value) {
    StandIn::make(program, &part.to_string());
}

/// When this process is a stand-in whose part `play` knows, plays it and exits with the code it returns. `play` is
/// given the part's `play`, the part, and the arguments; it answers `None` for a part that is not its own.
pub(crate) fn serve(play: impl FnOnce(&str, &Value, &[String]) -> Option<i32>) {
    let Some(part) = StandIn::part() else {
        return;
    };
    let Ok(part) = serde_json::from_str::<Value>(&part) else {
        return;
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let name = part["play"].as_str().unwrap_or_default().to_owned();
    if let Some(code) = play(&name, &part, &args) {
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        std::process::exit(code);
    }
}

/// The common part: what a small shell script of a test used to do, in this order.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Script {
    /// Said, then exit 0, when the only argument is `--version`.
    pub version: Option<String>,
    /// A file the arguments are written to, one per line (after `PATH` when `record_path`).
    pub record: Option<PathBuf>,
    /// Whether `PATH` is the first line of `record`.
    pub record_path: bool,
    /// Whether `record` is appended to instead of replaced.
    pub append: bool,
    /// A file written with `text`: `(file, text)`.
    pub write: Option<(PathBuf, String)>,
    /// What it prints on stdout.
    pub print: String,
    /// A child program started before this script reports its pid or sleeps.
    pub spawn: Option<PathBuf>,
    /// A file its process id is written to, before sleeping or producing output.
    pub pid: Option<PathBuf>,
    /// How long it stays up (milliseconds) before it does the rest.
    pub sleep_ms: u64,
    /// Its exit code.
    pub exit: i32,
}

impl Script {
    /// Makes `program` play this script.
    pub(crate) fn make(&self, program: &Path) {
        let mut part = serde_json::to_value(self).unwrap();
        part["play"] = "script".into();
        make(program, &part);
    }

    fn play(&self, args: &[String]) -> i32 {
        // Dropping std::process::Child does not kill it: the delivery's owned tree must clean it up.
        let _child = self
            .spawn
            .as_ref()
            .map(|program| std::process::Command::new(program).spawn().unwrap());
        if let Some(file) = &self.pid {
            std::fs::write(file, std::process::id().to_string()).unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(self.sleep_ms));
        if let Some(version) = self.version.as_ref().filter(|_| args == ["--version"]) {
            println!("{version}");
            return 0;
        }
        if let Some(file) = &self.record {
            let mut lines = String::new();
            if self.record_path {
                lines.push_str(&std::env::var("PATH").unwrap_or_default());
                lines.push('\n');
            }
            for arg in args {
                lines.push_str(arg);
                lines.push('\n');
            }
            let mut out = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .append(self.append)
                .truncate(!self.append)
                .open(file)
                .unwrap();
            out.write_all(lines.as_bytes()).unwrap();
        }
        if let Some((file, text)) = &self.write {
            std::fs::write(file, text).unwrap();
        }
        print!("{}", self.print);
        self.exit
    }
}

#[ctor::ctor(unsafe)]
fn play_script() {
    serve(|name, part, args| {
        (name == "script").then(|| {
            serde_json::from_value::<Script>(part.clone())
                .unwrap()
                .play(args)
        })
    });
}
