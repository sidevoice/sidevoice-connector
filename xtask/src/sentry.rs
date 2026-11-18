//! `cargo xtask sentry-upload`: the release `sidevoice-connector@<version>` and the debug files `dist` left
//! (`debug_files.rs`: the unstripped Linux binary, a `.dSYM`, a PDB), to Sentry, so that a panic's addresses become function
//! names and lines (connector/src/crash.rs). `sentry-cli` does the upload; this only says what and where.
//!
//! Reads `SENTRY_AUTH_TOKEN` (the organisation's secret, in the environment of the workflow step that runs this and no
//! other; never printed, never on a command line). The organisation, the project and the EU region are fixed here: org
//! `twentynine-tech-sl`, project `sidevoice`, `https://de.sentry.io`. Only the release and nightly workflows run it.

use std::process::Command;

use crate::debug_files;
use crate::util::connector_version;
use crate::{Result, PACKAGE};

const URL: &str = "https://de.sentry.io";
const ORG: &str = "twentynine-tech-sl";
const PROJECT: &str = "sidevoice";

/// The release a build reports itself as: `<service.name>@<version>`, the connector's `service.name` being its package's
/// name (connector/src/crash/report.rs).
fn release_name(version: &str) -> String {
    format!("{PACKAGE}@{version}")
}

fn sentry(args: &[&str]) -> Result<()> {
    if std::env::var_os("SENTRY_AUTH_TOKEN").is_none_or(|token| token.is_empty()) {
        return Err("SENTRY_AUTH_TOKEN is not set".into());
    }
    // npm's global shim on Windows is a batch file.
    let program = if cfg!(windows) {
        "sentry-cli.cmd"
    } else {
        "sentry-cli"
    };
    let status = Command::new(program)
        .args(args)
        .env("SENTRY_URL", URL)
        .env("SENTRY_ORG", ORG)
        .env("SENTRY_PROJECT", PROJECT)
        .status()
        .map_err(|error| format!("sentry-cli: {error}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("sentry-cli {} failed", args.join(" ")))
}

pub(crate) fn upload() -> Result<()> {
    let release = release_name(&connector_version()?);
    let files: Vec<String> = debug_files::files()?
        .iter()
        .map(|file| file.to_string_lossy().into_owned())
        .collect();
    sentry(&["releases", "new", &release])?;
    let mut upload = vec!["debug-files", "upload", "--wait"];
    upload.extend(files.iter().map(String::as_str));
    sentry(&upload)?;
    sentry(&["releases", "finalize", &release])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_is_named_after_its_service() {
        assert_eq!(release_name("0.20.0"), "sidevoice-connector@0.20.0");
    }
}
