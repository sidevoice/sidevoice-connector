//! `cargo xtask codex [DIR]`: the real Codex CLI the connector's tests register with, at a pinned version,
//! installed from npm into DIR (default `target/codex`), never onto the machine.

use std::path::Path;

use crate::util::*;
use crate::Result;

pub(crate) const DEFAULT_DIR: &str = "target/codex";
/// The Codex CLI release the registration test runs against. Bumped by hand, like any pin.
pub(crate) const VERSION: &str = "0.160.0";

pub(crate) fn codex(dir: &Path) -> Result<()> {
    mkdir(dir)?;
    output(
        "npm",
        &[
            "install",
            "--prefix",
            path_str(dir)?,
            "--no-audit",
            "--no-fund",
            "--no-save",
            &format!("@openai/codex@{VERSION}"),
        ],
        None,
    )?;
    // npm's shim: `codex` on Unix, `codex.cmd` on Windows (beside an extensionless one that cannot run there).
    let bin = dir.join("node_modules/.bin");
    let binary = which::which_in("codex", Some(&bin), dir)
        .map_err(|error| format!("no codex in {}: {error}", bin.display()))?;
    let version = output(path_str(&binary)?, &["--version"], None)?;
    if !version.contains(VERSION) {
        return Err(format!(
            "{} reports {version:?}, not {VERSION}",
            binary.display()
        ));
    }
    println!("{}", binary.display());
    Ok(())
}
