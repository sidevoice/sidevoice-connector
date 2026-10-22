//! Start at login and supervision: this user's service manager, which runs each job, starts it at the next login or
//! not, and brings it back when it ends.
//!
//! The interface is the [`Manager`] trait; [`ServiceManager`] is the one manager compiled in, chosen by the OS below:
//! launchd on macOS (a LaunchAgent in this user's GUI domain), systemd's user manager on Linux (a user unit), Task
//! Scheduler on Windows (a task that runs at this user's logon, as this user, with no administrator: a Windows
//! service would need one, and runs outside the user's session). Where there is no manager to use (none answers, or
//! `SIDEVOICE_SERVICE_MANAGER=none`), the caller runs Sidevoice on demand instead; that is not a manager. Callers
//! import both: `use crate::platform::service::{Manager, ServiceManager};` then
//! `ServiceManager::new(places).start(..)`.
//!
//! What a job runs is host-agnostic data, a [`Job`], built by the caller; each manager turns it into its own
//! definition (a plist, a unit, a task) and reads the program back from it. Every manager command has an absolute
//! deadline (30 s: launchd holds a `kickstart` of a job started less than its ThrottleInterval ago until that
//! interval has passed, and systemd's `stop` waits TimeoutStopSec), and a manager that cannot be asked is never taken
//! to mean a job is absent.
//!
//! Stopping is the manager's: launchd and systemd send `SIGTERM`, then kill after their grace; Task Scheduler ends a
//! task with a hard stop. The graceful request on Windows (the core's in-band shutdown over the local channel) is the
//! caller's to make first, and arrives with the local channel.

#[cfg(target_os = "macos")]
#[path = "service/launchd.rs"]
mod imp;
#[cfg(target_os = "linux")]
#[path = "service/systemd.rs"]
mod imp;
#[cfg(windows)]
#[path = "service/task_scheduler.rs"]
mod imp;

pub use imp::Os as ServiceManager;

use super::fs::{Create, Files, Fs};
use super::process::{Process, Processes};
use super::user::{User, UserId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

const MANAGER_LIMIT: Duration = Duration::from_secs(30);
const OUTPUT_LIMIT: usize = 64 * 1024;
const DEFINITION_LIMIT: u64 = 128 * 1024;

/// Where a manager keeps what it defines: the user's home (launchd), their configuration directory as this process
/// sees it (systemd, when its user manager does not say), the data directory (Task Scheduler).
pub struct Places {
    pub home: PathBuf,
    pub config_home: PathBuf,
    pub data: PathBuf,
}

/// A job's names, fixed: `label` reverse-DNS (`dev.sidevoice.core`), `name` short (`sidevoice-core`). Each manager
/// derives what it calls the job from them ([`Manager::name`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct JobId {
    pub label: &'static str,
    pub name: &'static str,
}

/// When the manager brings a job back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restart {
    /// After a crash or a non-zero exit, never after exit 0.
    OnFailure,
    /// Whenever it exits.
    Always,
}

/// One job, as data: what it runs, with which environment, where its output goes and when it comes back.
pub struct Job {
    pub id: JobId,
    pub description: &'static str,
    /// The program (absolute) and its arguments.
    pub program: Vec<String>,
    pub environment: BTreeMap<String, String>,
    /// Variables the job must not inherit from the manager.
    pub unset: Vec<&'static str>,
    pub restart: Restart,
    /// Where its output goes, where the manager does not keep it itself (systemd keeps it in the journal).
    pub log: PathBuf,
    /// The command that runs a job from its definition file ([`Manager::run_job`], the file appended), for a
    /// manager that cannot set a job's environment and output itself (Task Scheduler).
    pub runner: Vec<String>,
}

impl Job {
    /// That every value can go into a definition: text with no control character (a newline in a path or a setting
    /// would start a directive of its own in a unit, an injected `ExecStartPre=`), variable names that are names.
    /// What is not, by what it is.
    pub fn check(&self) -> Result<(), Refusal> {
        let plain = |value: &str, what: &str| {
            if value.chars().any(char::is_control) {
                Err(Refusal::Unsafe(what.to_owned()))
            } else {
                Ok(())
            }
        };
        plain(self.description, "description")?;
        for word in self.program.iter().chain(&self.runner) {
            plain(word, "program")?;
        }
        plain(&self.log.to_string_lossy(), "log")?;
        for name in self
            .environment
            .keys()
            .map(String::as_str)
            .chain(self.unset.iter().copied())
        {
            let valid = name.chars().enumerate().all(|(index, ch)| {
                ch == '_' || ch.is_ascii_alphabetic() || (index > 0 && ch.is_ascii_digit())
            });
            if name.is_empty() || !valid {
                return Err(Refusal::Unsafe(name.to_owned()));
            }
        }
        for (name, value) in &self.environment {
            plain(value, name)?;
        }
        Ok(())
    }
}

/// A job as its manager reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JobState {
    pub loaded: bool,
    pub running: bool,
    pub pid: Option<u32>,
    pub exit: Option<i64>,
    pub signal: Option<String>,
    pub runs: Option<u64>,
    /// Not running, and the manager will start it again.
    pub restarting: bool,
    /// Why the manager does not run it: `not-loaded`, `start-limit`.
    pub reason: Option<&'static str>,
}

/// Linux: whether this user's services run without a session, and the command that would make them.
pub struct Linger {
    pub enabled: bool,
    pub command: String,
}

/// Why a manager did not do what it was asked, each with what it said.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A value a definition cannot carry ([`Job::check`]): which.
    Unsafe(String),
    /// A definition, or its directory, that is not safely this user's.
    Definition(String),
    /// Reading or writing a definition failed.
    Io(String),
    /// Enabling a job for the next login was refused or not confirmed.
    NotEnabled(String),
    /// Loading or starting a job was refused.
    NotLoaded(String),
    /// Whether the job starts at login cannot be read, and loading it could leave it on for good.
    LoginUnknown(String),
    /// The manager refused to start this defined job: its start limit, or a program that cannot run, may say why.
    NotStarted(JobId, String),
}

/// What every manager implements ([`ServiceManager`], in `service/launchd.rs`, `service/systemd.rs` and
/// `service/task_scheduler.rs`): the required methods. The rest is one body for every manager; one overrides it only
/// if it must.
pub trait Manager: Sized + Send + Sync {
    /// Its name, as `service status` and the jobs' `SIDEVOICE_SERVICE` say it.
    const KIND: &'static str;
    /// How many starts it allows before it gives up (systemd's start limit); none where it has none to report.
    const START_LIMIT: Option<u64> = None;

    /// This user's manager, its definitions kept under `places`. Nothing is asked of it yet.
    fn new(places: Places) -> Self;

    /// What the manager, and a person, call the job.
    fn name(job: JobId) -> String;

    /// Where the job's definition is.
    fn file(&self, job: JobId) -> impl Future<Output = PathBuf> + Send;

    /// The text of the job's definition. The values are checked ([`Job::check`]) before it is made.
    fn definition(job: &Job) -> String;

    /// The program a definition runs, read back from its text.
    fn program_of(text: &str) -> Option<Vec<String>>;

    /// Each of `jobs` started from its definition as it is now: one in `changed` loaded again, and with `restart` a
    /// running one restarted (a start limit cleared). With `login` each is also enabled for the next login; without,
    /// whether it starts at login is left as it is (a job the person turned off still runs now, and still not at the
    /// next login).
    fn start(
        &self,
        jobs: &[JobId],
        changed: &[JobId],
        restart: bool,
        login: bool,
    ) -> impl Future<Output = Result<(), Refusal>> + Send;

    /// The job stopped and kept from coming back until it is started or the next login; with `disable` (it is about
    /// to be removed), not at the next login either. Stopped is what the manager says afterwards. Why not.
    fn stop(&self, job: JobId, disable: bool) -> impl Future<Output = Result<(), String>> + Send;

    /// After the definitions are removed: the manager forgets the jobs.
    fn forget(&self, jobs: &[JobId]) -> impl Future<Output = Result<(), String>> + Send;

    /// The job as the manager reports it now.
    fn status(&self, job: JobId) -> impl Future<Output = JobState> + Send;

    /// Whether the job starts at the next login, as the manager answers now; `None` when it cannot tell.
    fn login(&self, job: JobId) -> impl Future<Output = Option<bool>> + Send;

    /// The job set to start at the next login (`on`) or not, and only that: nothing is loaded, started, stopped or
    /// unloaded. Not read back here. Why not.
    fn set_login(&self, job: JobId, on: bool) -> impl Future<Output = Result<(), String>> + Send;

    /// Whether this user's manager answers now.
    fn answers() -> impl Future<Output = bool> + Send {
        async { true }
    }

    /// Whether this user's jobs run without a session, where the manager asks for that (systemd's linger).
    fn linger(&self) -> impl Future<Output = Option<Linger>> + Send {
        async { None }
    }

    /// Whether the job is defined here, from its definition alone.
    fn defined(&self, job: JobId) -> impl Future<Output = bool> + Send {
        async move { std::fs::symlink_metadata(self.file(job).await).is_ok() }
    }

    /// The job's definition written when its text would change: a private file renamed into place, in a directory
    /// this user's and writable by nobody else (made private when missing). Whether it changed.
    fn define(&self, job: &Job) -> impl Future<Output = Result<bool, Refusal>> + Send {
        async move {
            job.check()?;
            write_definition(&self.file(job.id).await, &Self::definition(job))
        }
    }

    /// The job's definition deleted, when there is one.
    fn remove(&self, job: JobId) -> impl Future<Output = Result<(), Refusal>> + Send {
        async move {
            let file = self.file(job).await;
            if existing(&file)? {
                std::fs::remove_file(&file).map_err(|error| Refusal::Io(error.to_string()))?;
            }
            Ok(())
        }
    }

    /// The program the job's definition on disk runs.
    fn program(&self, job: JobId) -> impl Future<Output = Option<Vec<String>>> + Send {
        async move {
            let file = self.file(job).await;
            if !existing(&file).ok()? {
                return None;
            }
            Self::program_of(&std::fs::read_to_string(file).ok()?)
        }
    }

    /// Runs the job a definition file of [`Stored`] describes, as a manager that cannot set a job's environment and
    /// output itself starts it (Task Scheduler runs [`Job::runner`] with the file): the manager's environment with
    /// the job's set and its `unset` removed, output appended to its log, the whole tree ended with it. Its exit code.
    fn run_job(file: &Path) -> impl Future<Output = io::Result<i32>> + Send {
        async move {
            let bytes = Files::read_trusted(file, DEFINITION_LIMIT)
                .map_err(io::Error::other)?
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            let job: Stored = serde_json::from_slice(&bytes)?;
            let (program, args) = job
                .program
                .split_first()
                .ok_or_else(|| io::Error::other("no program"))?;
            let log = Files::open_private(&job.log, Create::Append)?;
            let mut command = Command::new(program);
            command
                .args(args)
                .envs(&job.environment)
                .stdin(Stdio::null())
                .stdout(log.try_clone()?)
                .stderr(log);
            for name in &job.unset {
                command.env_remove(name);
            }
            let mut child = Processes::spawn_tree(command).await?;
            Ok(child.wait().await?.code().unwrap_or(1))
        }
    }
}

/// A job as a file a runner reads ([`Manager::run_job`]): everything in [`Job`] but its names.
#[derive(Serialize, Deserialize)]
pub struct Stored {
    pub description: String,
    pub program: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub unset: Vec<String>,
    pub log: PathBuf,
    pub runner: Vec<String>,
}

impl From<&Job> for Stored {
    fn from(job: &Job) -> Stored {
        Stored {
            description: job.description.to_owned(),
            program: job.program.clone(),
            environment: job.environment.clone(),
            unset: job.unset.iter().map(|name| (*name).to_owned()).collect(),
            log: job.log.clone(),
            runner: job.runner.clone(),
        }
    }
}

/// What a manager's own command answered.
#[derive(Debug, Default)]
struct Output {
    ok: bool,
    code: Option<i32>,
    output: String,
}

impl Output {
    fn trimmed(&self) -> String {
        self.output
            .trim()
            .chars()
            .filter(|ch| !ch.is_control() || *ch == '\n')
            .take(400)
            .collect()
    }
}

async fn read_bounded<R: tokio::io::AsyncRead + Unpin>(mut reader: R) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut scratch = [0u8; 4096];
    while let Ok(count) = reader.read(&mut scratch).await {
        if count == 0 {
            break;
        }
        let room = OUTPUT_LIMIT.saturating_sub(kept.len());
        kept.extend_from_slice(&scratch[..count.min(room)]);
    }
    kept
}

/// Runs a manager's own command, with no shell; never fails, never outlives its deadline.
async fn command(program: &str, args: &[&str]) -> Output {
    let spawned = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            return Output {
                ok: false,
                code: None,
                output: format!("{program}: {error}"),
            }
        }
    };
    let stdout = tokio::spawn(read_bounded(child.stdout.take().expect("piped stdout")));
    let stderr = tokio::spawn(read_bounded(child.stderr.take().expect("piped stderr")));
    let status = match timeout(MANAGER_LIMIT, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            return Output {
                ok: false,
                code: None,
                output: format!("{program}: {error}"),
            }
        }
        Err(_) => {
            let _ = child.kill().await;
            return Output {
                ok: false,
                code: None,
                output: format!("{program} {} timed out", args.join(" ")),
            };
        }
    };
    let mut output = stdout.await.unwrap_or_default();
    output.extend(stderr.await.unwrap_or_default());
    Output {
        ok: status.success(),
        code: status.code(),
        output: String::from_utf8_lossy(&output).into_owned(),
    }
}

/// `value` as XML text (a plist, a task).
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn unsafe_definition(detail: impl Into<String>) -> Refusal {
    Refusal::Definition(detail.into())
}

/// A definition's directory: created private when missing, else this user's and writable by nobody else.
fn definition_directory(dir: &Path) -> Result<(), Refusal> {
    match std::fs::symlink_metadata(dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Files::ensure_private_dir(dir).map_err(|error| Refusal::Io(error.to_string()))
        }
        Err(error) => Err(Refusal::Io(error.to_string())),
        Ok(_) => {
            if Files::check_user_directory(dir).is_err() {
                return Err(unsafe_definition(dir.to_string_lossy()));
            }
            Ok(())
        }
    }
}

/// A definition on disk: this user's regular file (never a link), or none.
fn existing(file: &Path) -> Result<bool, Refusal> {
    match std::fs::symlink_metadata(file) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Refusal::Io(error.to_string())),
        Ok(metadata) => {
            if !metadata.is_file()
                || !UserId::current()
                    .owns_path(file)
                    .map_err(|error| Refusal::Io(error.to_string()))?
                || metadata.len() > DEFINITION_LIMIT
            {
                return Err(unsafe_definition(file.to_string_lossy()));
            }
            Ok(true)
        }
    }
}

/// `bytes` written to `file` when they differ from what is there: a private temporary file renamed into place.
/// Whether it changed.
fn write_file(file: &Path, bytes: &[u8]) -> Result<bool, Refusal> {
    let dir = file
        .parent()
        .ok_or_else(|| unsafe_definition("no parent directory"))?;
    definition_directory(dir)?;
    if existing(file)? && std::fs::read(file).ok().as_deref() == Some(bytes) {
        return Ok(false);
    }
    Files::replace(file, bytes).map_err(|error| Refusal::Io(error.to_string()))?;
    Ok(true)
}

fn write_definition(file: &Path, text: &str) -> Result<bool, Refusal> {
    write_file(file, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(program: &[&str], environment: &[(&str, &str)]) -> Job {
        Job {
            id: JobId {
                label: "dev.sidevoice.test",
                name: "sidevoice-test",
            },
            description: "Sidevoice test",
            program: program.iter().map(|word| (*word).to_owned()).collect(),
            environment: environment
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            unset: vec!["ALPHA_HOME"],
            restart: Restart::OnFailure,
            log: PathBuf::from("/d/test.log"),
            runner: vec!["/r/current/sidevoice-connector".into(), "run-job".into()],
        }
    }

    #[test]
    fn a_definition_reads_back_the_program_it_runs() {
        for program in [
            &[
                "/r/current/core/bin/sidevoice-core-rust",
                "--data-dir",
                "/d/core & <x>",
            ][..],
            &["/r/cur rent/bin", "100%", "$HOME", "a\"b\\c"],
        ] {
            let job = job(
                program,
                &[("HOME", "/home/u"), ("PRICE", "$5 at 50% \"off\"")],
            );
            job.check().unwrap();
            let text = ServiceManager::definition(&job);
            assert_eq!(
                ServiceManager::program_of(&text),
                Some(job.program.clone()),
                "{text}"
            );
        }
        assert_eq!(ServiceManager::program_of(""), None);
    }

    #[test]
    fn control_characters_and_bad_names_are_never_written() {
        let injected = job(&["/bin/true\nExecStartPre=/bin/evil"], &[]);
        assert_eq!(injected.check(), Err(Refusal::Unsafe("program".into())));
        for name in ["A=B", "1A", ""] {
            let bad = job(&["/bin/true"], &[(name, "x")]);
            assert_eq!(bad.check(), Err(Refusal::Unsafe(name.into())));
        }
        let value = job(&["/bin/true"], &[("HOME", "/home/u\n[Service]")]);
        assert_eq!(value.check(), Err(Refusal::Unsafe("HOME".into())));
    }

    #[test]
    fn a_definition_is_rewritten_only_when_its_text_changes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested/unit.service");
        assert!(write_definition(&file, "one").unwrap());
        assert!(!write_definition(&file, "one").unwrap());
        assert!(write_definition(&file, "two").unwrap());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "two");
        Files::check_private(&Files::open_read(&file).unwrap()).unwrap();
        assert!(existing(&file).unwrap());
        assert!(!existing(&dir.path().join("missing")).unwrap());
        assert!(
            existing(dir.path()).is_err(),
            "a directory is no definition"
        );
    }

    #[tokio::test]
    async fn a_job_runs_from_its_file_with_its_environment_and_its_exit() {
        let dir = tempfile::tempdir().unwrap();
        let exe = std::env::current_exe().unwrap();
        let mut stored = Stored::from(&job(&[], &[("SIDEVOICE_TEST_RUN_JOB", "set")]));
        stored.program = vec![
            exe.to_string_lossy().into_owned(),
            "--ignored".into(),
            "--exact".into(),
            "platform::service::tests::run_job_child".into(),
        ];
        stored.log = dir.path().join("job.log");
        let file = dir.path().join("job.json");
        write_definition(&file, &serde_json::to_string(&stored).unwrap()).unwrap();
        assert_eq!(ServiceManager::run_job(&file).await.unwrap(), 0);
        let log = std::fs::read_to_string(&stored.log).unwrap();
        assert!(log.contains("run_job_child"), "{log}");
        let missing = ServiceManager::run_job(&dir.path().join("none.json")).await;
        assert_eq!(missing.unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    /// What the job above runs: this test binary, which passes only with the job's environment.
    #[test]
    #[ignore = "a child of the test above, not a test"]
    fn run_job_child() {
        assert_eq!(
            std::env::var("SIDEVOICE_TEST_RUN_JOB").as_deref(),
            Ok("set")
        );
        assert!(std::env::var_os("ALPHA_HOME").is_none());
    }
}
