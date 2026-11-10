//! A step of putting a release's files in place that failed, said by key with what it was doing and where:
//! `install.step-failed`, or `install.files-busy` when a rename or a removal was still refused after being tried
//! again. A bare OS error ("Access is denied. (os error 5)") names neither the step nor the path, so one failure could
//! not be told from another.
//!
//! On Windows a file just written is often held open for a moment by another program, most often an antivirus
//! scanning it, and while it is held the directory it is in cannot be renamed or removed (`ERROR_ACCESS_DENIED`,
//! `ERROR_SHARING_VIOLATION`). Those renames and removals ([`retried`]) are tried again with a bounded backoff, as
//! npm's graceful-fs, Go and rustup do; elsewhere they are tried once.
//!
//! What a client reads is the key and its params: `step` (one of [`Step::ALL`]'s ids; a step it does not know reads
//! as a failure to install), `action` (the step in English words, `install.step.<step>` in the catalogue), `path`,
//! `os_error` (the OS's error number, or `null`), `kind` (Rust's `io::ErrorKind`, or `null`), `detail` (the error's
//! text), and, when it was tried again, `attempts`, `waited_ms` and `seconds`.

use crate::messages::Keyed;
use anyhow::Result;
use serde_json::{json, Value};
use std::io;
use std::path::Path;
use std::time::Duration;

/// What the installer was doing when it failed. The ids are a closed set that only grows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Making (or checking) the installation's root, `D/install`.
    InstallDir,
    /// Reading the package being installed: the connector's binary, its inventory, the core archive.
    ReadPackage,
    /// Removing a release of the same name that is incomplete or another build, before staging this one.
    RemoveOldRelease,
    /// Making `D/install/releases`.
    ReleasesDir,
    /// Making the directory the release is staged in.
    StageDir,
    /// Copying the connector into the staged release.
    CopyConnector,
    /// Checking the copied connector: private, runnable, the same bytes, its version.
    CheckConnector,
    /// Copying the windowless background program (Windows).
    CopyBackground,
    /// Unpacking the core into the staged release.
    UnpackCore,
    /// Renaming the unpacked core into place.
    PublishCore,
    /// Renaming the staged release into place.
    PublishRelease,
    /// Writing the records that select the release.
    Select,
    /// Removing a release that did not start, once the one before it is back.
    RemoveFailedRelease,
}

impl Step {
    pub const ALL: [Step; 13] = [
        Step::InstallDir,
        Step::ReadPackage,
        Step::RemoveOldRelease,
        Step::ReleasesDir,
        Step::StageDir,
        Step::CopyConnector,
        Step::CheckConnector,
        Step::CopyBackground,
        Step::UnpackCore,
        Step::PublishCore,
        Step::PublishRelease,
        Step::Select,
        Step::RemoveFailedRelease,
    ];

    /// The stable id a client reads (`params.step`).
    pub fn id(self) -> &'static str {
        match self {
            Step::InstallDir => "install-dir",
            Step::ReadPackage => "read-package",
            Step::RemoveOldRelease => "remove-old-release",
            Step::ReleasesDir => "releases-dir",
            Step::StageDir => "stage-dir",
            Step::CopyConnector => "copy-connector",
            Step::CheckConnector => "check-connector",
            Step::CopyBackground => "copy-background",
            Step::UnpackCore => "unpack-core",
            Step::PublishCore => "publish-core",
            Step::PublishRelease => "publish-release",
            Step::Select => "select",
            Step::RemoveFailedRelease => "remove-failed-release",
        }
    }

    /// The step in English words, from the catalogue (`install.step.<id>`).
    pub fn action(self) -> String {
        crate::messages::message(&format!("install.step.{}", self.id()), &json!({}))
    }
}

/// How an operation another program may be holding up is tried again: the pause before each new attempt, and which
/// errors are worth one.
#[derive(Clone, Copy)]
pub struct Retry {
    pub pauses: &'static [Duration],
    pub transient: fn(&io::Error) -> bool,
}

/// About 19 s in all: an antivirus scan of a new program takes from a few milliseconds to a few seconds.
const WINDOWS_PAUSES: &[Duration] = &[
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
    Duration::from_millis(1600),
    Duration::from_secs(2),
    Duration::from_secs(2),
    Duration::from_secs(2),
    Duration::from_secs(2),
    Duration::from_secs(2),
    Duration::from_secs(2),
    Duration::from_secs(2),
    Duration::from_secs(2),
];

/// Windows' `ERROR_ACCESS_DENIED` (5) and `ERROR_SHARING_VIOLATION` (32): what renaming or removing a directory says
/// while another program has a file in it open. Only meaningful for an error Windows gave.
pub fn held_open(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(5 | 32))
}

impl Retry {
    /// Tried once.
    pub const ONCE: Retry = Retry {
        pauses: &[],
        transient: |_| false,
    };

    /// Windows' policy: [`held_open`] is tried again for about 19 s.
    pub fn windows() -> Retry {
        Retry {
            pauses: WINDOWS_PAUSES,
            transient: held_open,
        }
    }

    /// This OS's policy: Windows', else [`Retry::ONCE`] (a Unix rename or removal is not refused for an open file).
    pub fn platform() -> Retry {
        if cfg!(windows) {
            Retry::windows()
        } else {
            Retry::ONCE
        }
    }

    /// `op` until it succeeds, fails with an error that is not transient, or the pauses run out; `pause` waits.
    pub fn run<T>(
        &self,
        mut op: impl FnMut() -> io::Result<T>,
        mut pause: impl FnMut(Duration),
    ) -> std::result::Result<T, Refused> {
        let mut waited = Duration::ZERO;
        let mut attempts = 0;
        loop {
            attempts += 1;
            let error = match op() {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };
            let transient = (self.transient)(&error);
            match self.pauses.get(attempts - 1) {
                Some(&delay) if transient => {
                    pause(delay);
                    waited += delay;
                }
                _ => {
                    return Err(Refused {
                        error,
                        attempts: attempts as u32,
                        waited,
                        busy: transient && attempts > 1,
                    })
                }
            }
        }
    }
}

/// How a retried operation ended without succeeding.
#[derive(Debug)]
pub struct Refused {
    /// The last error.
    pub error: io::Error,
    pub attempts: u32,
    pub waited: Duration,
    /// Still refused as held open when the pauses ran out.
    pub busy: bool,
}

/// What a failure said, kept to one line of bounded length.
fn printable(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control())
        .take(240)
        .collect()
}

fn params(step: Step, path: &Path, error: &anyhow::Error) -> Value {
    let io = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<io::Error>());
    json!({
        "step": step.id(),
        "action": step.action(),
        "path": path.display().to_string(),
        "os_error": io.and_then(io::Error::raw_os_error),
        "kind": io.map(|io| format!("{:?}", io.kind())),
        "detail": printable(&format!("{error:#}")),
    })
}

/// `error`, met at `step` on `path`, as the keyed failure that says both. A failure that is keyed already (a package
/// that is not what it says, a staged connector that says another version) keeps its own key.
pub fn failure(step: Step, path: &Path, error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<Keyed>().is_some() {
        return error;
    }
    Keyed::new("install.step-failed", params(step, path, &error)).into()
}

/// A retried operation that did not succeed: `install.files-busy` when it was still refused as held open, else the
/// step's failure, with how many attempts it took and how long it waited.
fn refused(step: Step, path: &Path, refused: Refused) -> anyhow::Error {
    let (busy, attempts, waited) = (refused.busy, refused.attempts, refused.waited);
    let mut params = params(step, path, &refused.error.into());
    if attempts > 1 {
        params["attempts"] = json!(attempts);
        params["waited_ms"] = json!(waited.as_millis() as u64);
        params["seconds"] = json!(waited.as_secs_f64().round() as u64);
    }
    let key = if busy {
        "install.files-busy"
    } else {
        "install.step-failed"
    };
    Keyed::new(key, params).into()
}

/// Says a failure as the step's: `result.at_step(Step::…, path)?`. Where the operation is a closure, [`attempt`] is
/// preferred: a test can make it fail there instead of running it.
pub trait AtStep<T> {
    fn at_step(self, step: Step, path: &Path) -> Result<T>;
}

impl<T, E: Into<anyhow::Error>> AtStep<T> for std::result::Result<T, E> {
    fn at_step(self, step: Step, path: &Path) -> Result<T> {
        self.map_err(|error| failure(step, path, error.into()))
    }
}

/// The failure a unit test asked for at `step` on `path`, instead of the operation; never one outside the tests.
pub fn injected(step: Step, path: &Path) -> Option<io::Error> {
    #[cfg(test)]
    return faults::take(step, path);
    #[cfg(not(test))]
    {
        let _ = (step, path);
        None
    }
}

/// `op` at `step` on `path`, its failure keyed: `step::attempt(Step::…, path, || …)?`.
pub fn attempt<T, E: Into<anyhow::Error>>(
    step: Step,
    path: &Path,
    op: impl FnOnce() -> std::result::Result<T, E>,
) -> Result<T> {
    match injected(step, path) {
        Some(error) => Err(failure(step, path, error.into())),
        None => op().at_step(step, path),
    }
}

/// `op` at `step` on `path`, tried again as this OS's [`Retry`] says; its failure keyed.
pub fn retried<T>(step: Step, path: &Path, op: impl FnMut() -> io::Result<T>) -> Result<T> {
    retried_with(&Retry::platform(), step, path, op, std::thread::sleep)
}

/// [`retried`], with the policy and the wait given. A failure a test asks for is met at an attempt, in place of the
/// operation, and goes through the same policy.
pub fn retried_with<T>(
    retry: &Retry,
    step: Step,
    path: &Path,
    mut op: impl FnMut() -> io::Result<T>,
    pause: impl FnMut(Duration),
) -> Result<T> {
    retry
        .run(
            || match injected(step, path) {
                Some(error) => Err(error),
                None => op(),
            },
            pause,
        )
        .map_err(|refusal| refused(step, path, refusal))
}

/// Failures the unit tests make happen at a step, in place of its operation, for paths below a root of their own
/// (each test has its scratch directory, so tests running at once never meet another's).
#[cfg(test)]
pub(crate) mod faults {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    static FAULTS: Mutex<Vec<(PathBuf, Step, i32)>> = Mutex::new(Vec::new());

    /// The next time `step` is attempted on `root` or a path below it, it fails with OS error `code` instead.
    pub fn fail_next(root: &Path, step: Step, code: i32) {
        FAULTS
            .lock()
            .unwrap()
            .push((root.to_path_buf(), step, code));
    }

    pub(super) fn take(step: Step, path: &Path) -> Option<io::Error> {
        let mut faults = FAULTS.lock().unwrap();
        let at = faults
            .iter()
            .position(|(root, at, _)| *at == step && path.starts_with(root))?;
        let (_, _, code) = faults.remove(at);
        Some(io::Error::from_raw_os_error(code))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const SHORT: &[Duration] = &[
        Duration::from_millis(1),
        Duration::from_millis(2),
        Duration::from_millis(3),
    ];

    fn short() -> Retry {
        Retry {
            pauses: SHORT,
            transient: held_open,
        }
    }

    fn said(error: &anyhow::Error) -> Keyed {
        crate::messages::keyed(error)
    }

    #[test]
    fn every_step_has_an_id_and_english_words() {
        let mut ids = std::collections::BTreeSet::new();
        for step in Step::ALL {
            assert!(ids.insert(step.id()), "{step:?}");
            let action = step.action();
            assert!(!action.starts_with("install.step."), "{step:?}: {action}");
        }
    }

    #[test]
    fn a_failure_names_its_step_its_path_and_the_os_error_and_keeps_a_key_it_has() {
        let path = Path::new("/some/where/.1.0.0.partial-x");
        let error = Err::<(), _>(io::Error::from_raw_os_error(5))
            .at_step(Step::PublishRelease, path)
            .unwrap_err();
        let keyed = said(&error);
        assert_eq!(keyed.key, "install.step-failed");
        assert_eq!(keyed.params["step"], "publish-release");
        assert_eq!(keyed.params["path"], path.display().to_string());
        assert_eq!(keyed.params["os_error"], 5);
        assert!(keyed.params["kind"].is_string(), "{}", keyed.params);
        assert!(keyed.params.get("attempts").is_none());
        let text = keyed.message();
        assert!(
            text.starts_with(
                "Sidevoice could not be installed: moving the staged release into place failed at /some/where/"
            ),
            "{text}"
        );
        assert!(text.contains("(os error 5)"), "{text}");
        let value = keyed.value();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["key"], "install.step-failed");
        assert_eq!(value["error"]["message"], text.as_str());

        // Context is kept in the detail; an error with no OS error says null.
        let error = Err::<(), _>(anyhow::anyhow!("inner").context("outer"))
            .at_step(Step::Select, path)
            .unwrap_err();
        let keyed = said(&error);
        assert_eq!(keyed.params["detail"], "outer: inner");
        assert!(keyed.params["os_error"].is_null());

        let mismatch = anyhow::Error::new(Keyed::new("core.package-mismatch", json!({})));
        let error = Err::<(), _>(mismatch)
            .at_step(Step::UnpackCore, path)
            .unwrap_err();
        assert_eq!(said(&error).key, "core.package-mismatch");
    }

    #[test]
    fn a_held_operation_succeeds_once_it_is_let_go() {
        let calls = Cell::new(0);
        let paused = Cell::new(Duration::ZERO);
        let value = retried_with(
            &short(),
            Step::PublishCore,
            Path::new("/x"),
            || {
                calls.set(calls.get() + 1);
                if calls.get() <= 2 {
                    Err(io::Error::from_raw_os_error(if calls.get() == 1 {
                        5
                    } else {
                        32
                    }))
                } else {
                    Ok("moved")
                }
            },
            |delay| paused.set(paused.get() + delay),
        )
        .unwrap();
        assert_eq!(value, "moved");
        assert_eq!(calls.get(), 3);
        assert_eq!(paused.get(), Duration::from_millis(3));
    }

    #[test]
    fn a_held_operation_gives_up_saying_files_busy_with_attempts_and_time_waited() {
        let calls = Cell::new(0);
        let error = retried_with(
            &short(),
            Step::PublishRelease,
            Path::new("/x/.1.0.0.partial-y"),
            || -> io::Result<()> {
                calls.set(calls.get() + 1);
                Err(io::Error::from_raw_os_error(5))
            },
            |_| {},
        )
        .unwrap_err();
        assert_eq!(calls.get(), 4);
        let keyed = said(&error);
        assert_eq!(keyed.key, "install.files-busy");
        assert_eq!(keyed.params["step"], "publish-release");
        assert_eq!(keyed.params["attempts"], 4);
        assert_eq!(keyed.params["waited_ms"], 6);
        assert_eq!(keyed.params["os_error"], 5);
        let text = keyed.message();
        assert!(text.contains("after 4 attempts"), "{text}");
        assert!(text.contains("antivirus"), "{text}");
        assert!(text.contains("`npx sidevoice install`"), "{text}");
    }

    #[test]
    fn an_error_that_is_not_transient_is_not_tried_again() {
        let calls = Cell::new(0);
        let error = retried_with(
            &short(),
            Step::RemoveOldRelease,
            Path::new("/x/1.0.0"),
            || -> io::Result<()> {
                calls.set(calls.get() + 1);
                Err(io::Error::new(io::ErrorKind::NotFound, "gone"))
            },
            |_| panic!("no pause for an error that is not transient"),
        )
        .unwrap_err();
        assert_eq!(calls.get(), 1);
        let keyed = said(&error);
        assert_eq!(keyed.key, "install.step-failed");
        assert_eq!(keyed.params["kind"], "NotFound");
        assert!(keyed.params.get("attempts").is_none());

        // Transient at first, then something else: the step's failure, with the attempts it took.
        let calls = Cell::new(0);
        let error = retried_with(
            &short(),
            Step::RemoveOldRelease,
            Path::new("/x/1.0.0"),
            || -> io::Result<()> {
                calls.set(calls.get() + 1);
                Err(if calls.get() == 1 {
                    io::Error::from_raw_os_error(32)
                } else {
                    io::Error::new(io::ErrorKind::InvalidData, "other")
                })
            },
            |_| {},
        )
        .unwrap_err();
        let keyed = said(&error);
        assert_eq!(keyed.key, "install.step-failed");
        assert_eq!(keyed.params["attempts"], 2);
    }

    #[test]
    fn only_windows_tries_again_and_for_a_bounded_time() {
        let total: Duration = Retry::windows().pauses.iter().sum();
        assert!(
            total >= Duration::from_secs(10) && total <= Duration::from_secs(30),
            "{total:?}"
        );
        assert!(held_open(&io::Error::from_raw_os_error(5)));
        assert!(held_open(&io::Error::from_raw_os_error(32)));
        assert!(!held_open(&io::Error::from_raw_os_error(2)));
        assert!(!held_open(&io::Error::other("no code")));
        assert_eq!(Retry::platform().pauses.is_empty(), !cfg!(windows));
    }

    /// A directory with a file another handle holds open (no sharing, as a scanner may) is refused by Windows while
    /// it is held: the refusal is keyed, and a rename tried again succeeds once the handle is let go.
    #[cfg(windows)]
    #[test]
    fn a_directory_with_a_file_held_open_is_renamed_once_it_is_let_go() {
        use std::os::windows::fs::OpenOptionsExt;
        let scratch = crate::test_support::temp_dir("held-open");
        let from = scratch.path().join(".1.0.0.partial-held");
        let to = scratch.path().join("1.0.0");
        std::fs::create_dir(&from).unwrap();
        std::fs::write(from.join("program.exe"), b"bytes").unwrap();
        let open = || {
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(from.join("program.exe"))
                .unwrap()
        };

        let held = open();
        let error = retried_with(
            &short(),
            Step::PublishRelease,
            &from,
            || std::fs::rename(&from, &to),
            std::thread::sleep,
        )
        .unwrap_err();
        let keyed = said(&error);
        assert_eq!(keyed.key, "install.files-busy", "{}", keyed.params);
        assert!(
            matches!(keyed.params["os_error"].as_i64(), Some(5 | 32)),
            "{}",
            keyed.params
        );
        drop(held);

        let held = open();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        let calls = Cell::new(0);
        retried_with(
            &Retry::windows(),
            Step::PublishRelease,
            &from,
            || {
                calls.set(calls.get() + 1);
                std::fs::rename(&from, &to)
            },
            std::thread::sleep,
        )
        .unwrap();
        release.join().unwrap();
        assert!(calls.get() > 1, "{}", calls.get());
        assert!(to.join("program.exe").is_file());
    }
}
