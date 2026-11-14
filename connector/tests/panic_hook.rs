//! The panic hook in a real process: this test binary runs itself as a child that installs the connector's logging and
//! panics on two threads, once with a literal message and once with a formatted one that carries a canary path. The
//! child's log file and stderr are scanned (the shared logging guideline, sections 9 and 11). Its own test binary and
//! one test, because the hook and the subscriber are the process's.
#![allow(clippy::disallowed_macros)] // test output is not logging

use sidevoice_connector::logfile;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

const CHILD: &str = "SIDEVOICE_PANIC_HOOK_CHILD";
const CANARY_HOME: &str = "/home/canary-user-7f3a";
const LITERAL: &str = "literal words that may be logged";

/// What the child does: log to `dir`, then panic twice, each on a thread of its own.
fn child(dir: &Path, service: bool) {
    logfile::install("info");
    logfile::init(dir.join("connector.log"), service, Path::new(CANARY_HOME));
    let literal = std::thread::Builder::new()
        .name("literal-panic".into())
        .spawn(|| panic!("literal words that may be logged"))
        .unwrap();
    assert!(literal.join().is_err());
    let formatted = std::thread::Builder::new()
        .name("formatted-panic".into())
        .spawn(|| {
            let secret = format!("{CANARY_HOME}/canary-secret-7f3a");
            let result: Result<(), String> = std::hint::black_box(Err(secret));
            result.unwrap();
        })
        .unwrap();
    assert!(formatted.join().is_err());
}

#[test]
fn a_panic_is_two_redacted_events_and_the_backtrace_reaches_the_file_only() {
    if let Ok(dir) = std::env::var(CHILD) {
        return child(Path::new(&dir), false);
    }
    let scratch = tempfile::tempdir().unwrap();
    let stderr = scratch.path().join("stderr.txt");
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_panic_is_two_redacted_events_and_the_backtrace_reaches_the_file_only",
            "--nocapture",
        ])
        .env(CHILD, scratch.path())
        .env("SIDEVOICE_LOG", "trace")
        .env("HOME", CANARY_HOME)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(fs::File::create(&stderr).unwrap())
        .status()
        .unwrap();
    assert!(status.success(), "the child ran to its end");
    let file = fs::read_to_string(scratch.path().join("connector.log")).unwrap();
    let stderr = fs::read_to_string(&stderr).unwrap();

    let panics: Vec<&str> = file
        .lines()
        .filter(|line| line.contains(" panic: "))
        .collect();
    let backtraces: Vec<&str> = file
        .lines()
        .filter(|line| line.contains(" crash_backtrace: "))
        .collect();
    assert_eq!((panics.len(), backtraces.len()), (2, 2), "{file}");
    for line in &panics {
        for field in [
            "exception.type=\"panic\"",
            "code.file.path=",
            "code.line.number=",
            "thread.name=",
            "service.version=",
        ] {
            assert!(line.contains(field), "{field} missing: {line}");
        }
        assert!(!line.contains("exception.stacktrace"), "{line}");
    }
    let literal = panics
        .iter()
        .find(|line| line.contains("literal-panic"))
        .unwrap();
    assert!(
        literal.contains(&format!("exception.message=\"{LITERAL}\"")),
        "{literal}"
    );
    assert!(!literal.contains("len="), "{literal}");
    let formatted = panics
        .iter()
        .find(|line| line.contains("formatted-panic"))
        .unwrap();
    assert!(
        formatted.contains("exception.message=\"<formatted panic message omitted>\""),
        "{formatted}"
    );
    assert!(formatted.contains("len="), "{formatted}");
    for line in &backtraces {
        assert!(line.contains("exception.stacktrace=Backtrace ["), "{line}");
    }

    // The backtrace is for the file: stderr has the panic record and no backtrace.
    assert!(stderr.contains(" panic: "), "{stderr}");
    assert!(!stderr.contains("crash_backtrace"), "{stderr}");
    assert!(!stderr.contains("exception.stacktrace"), "{stderr}");
    assert!(
        !stderr.contains("thread '"),
        "the default hook is not chained: {stderr}"
    );

    for text in [&file, &stderr] {
        for forbidden in ["canary-secret-7f3a", "canary-user-7f3a"] {
            assert!(!text.contains(forbidden), "{forbidden} leaked:\n{text}");
        }
    }
}

const SERVICE_TEST: &str = "a_service_panic_reaches_the_file_and_not_stderr";
const PRE_INIT_TEST: &str = "a_panic_before_the_file_sink_is_stderr_only_with_no_backtrace";

/// Runs this binary again as the child of `test` and gives back the child's log file (if any) and stderr.
fn run_child(test: &str, scratch: &Path) -> (String, String) {
    let stderr = scratch.join("stderr.txt");
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(CHILD, scratch)
        .env("SIDEVOICE_LOG", "trace")
        .env("HOME", CANARY_HOME)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(fs::File::create(&stderr).unwrap())
        .status()
        .unwrap();
    assert!(status.success(), "the child ran to its end");
    (
        fs::read_to_string(scratch.join("connector.log")).unwrap_or_default(),
        fs::read_to_string(&stderr).unwrap(),
    )
}

#[test]
fn a_service_panic_reaches_the_file_and_not_stderr() {
    if let Ok(dir) = std::env::var(CHILD) {
        return child(Path::new(&dir), true);
    }
    let scratch = tempfile::tempdir().unwrap();
    let (file, stderr) = run_child(SERVICE_TEST, scratch.path());
    // A service's stderr is the same file under its manager: writing both would say everything twice.
    assert_eq!(file.matches(" panic: ").count(), 2, "{file}");
    assert_eq!(file.matches(" crash_backtrace: ").count(), 2, "{file}");
    assert!(!stderr.contains(" panic: "), "{stderr}");
    assert!(!stderr.contains("crash_backtrace"), "{stderr}");
    assert!(!stderr.contains("exception.stacktrace"), "{stderr}");
    assert!(!file.contains("canary-secret-7f3a"), "{file}");
}

#[test]
fn a_panic_before_the_file_sink_is_stderr_only_with_no_backtrace() {
    if let Ok(dir) = std::env::var(CHILD) {
        // No `init`: the CLI and the MCP server have no file.
        logfile::install("info");
        let panicked = std::thread::Builder::new()
            .name("early-panic".into())
            .spawn(|| panic!("literal words that may be logged"))
            .unwrap();
        assert!(panicked.join().is_err());
        assert!(!Path::new(&dir).join("connector.log").exists());
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let (file, stderr) = run_child(PRE_INIT_TEST, scratch.path());
    assert!(file.is_empty(), "{file}");
    assert!(stderr.contains(" panic: "), "{stderr}");
    assert!(stderr.contains(LITERAL), "{stderr}");
    assert!(!stderr.contains("crash_backtrace"), "{stderr}");
    assert!(!stderr.contains("exception.stacktrace"), "{stderr}");
}
