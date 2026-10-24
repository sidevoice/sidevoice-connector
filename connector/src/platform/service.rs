//! Start at login and supervision: this user's service manager, which runs each job, starts it at the next login or
//! not, and brings it back when it ends.
//!
//! The interface is the [`Service`] trait; [`Services`] is the one manager compiled in, chosen by the OS below:
//! launchd on macOS (a LaunchAgent in this user's GUI domain), systemd's user manager on Linux (a user unit), Task
//! Scheduler on Windows (a task that runs at this user's logon, as this user, with no administrator: a Windows
//! service would need one, and runs outside the user's session). Where there is no manager to use (none answers, or
//! `SIDEVOICE_SERVICE_MANAGER=none`), the caller runs Sidevoice on demand instead; that is not a manager. Callers
//! import both: `use crate::platform::service::{Service, Services};` then
//! `Services::new(places).start(..)`.
//!
//! What a job runs is host-agnostic data, a [`Job`], built by the caller; each manager turns it into its own
//! definition (a plist, a unit, a task) and reads the program back from it. Every manager command has an absolute
//! deadline (30 s: launchd holds a `kickstart` of a job started less than its ThrottleInterval ago until that
//! interval has passed, and systemd's `stop` waits TimeoutStopSec), and a manager that cannot be asked is never taken
//! to mean a job is absent.
//!
//! Stopping is the manager's: launchd and systemd send `SIGTERM`, then kill after their grace; Task Scheduler ends a
//! task with a hard stop, so the Windows implementation first asks the job's process to leave the way
//! [`Process::stop`](super::process::Process::stop) does (its stop event) and gives it a grace time.

#[cfg(target_os = "macos")]
#[path = "service/macos.rs"]
mod imp;
#[cfg(target_os = "linux")]
#[path = "service/linux.rs"]
mod imp;
#[cfg(windows)]
#[path = "service/windows.rs"]
mod imp;

pub use imp::Os as Services;

use super::fs::{Create, Files, Fs};
use super::process::{Process, Processes};
use super::target;
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
/// derives what it calls the job from them ([`Service::name`]).
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
    /// The command that runs a job from its definition file ([`Service::run_job`], the file appended), for a
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
    /// What the manager itself calls the job's state, in its own words and fields (systemd's `ActiveState`, launchd's
    /// `state`, Task Scheduler's `Status`), for a person to read; never parsed.
    pub state: Option<String>,
}

/// A job as its manager reports it now, named: what a person needs to see why it does not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnosis {
    /// The manager ([`Service::KIND`]).
    pub manager: &'static str,
    /// What the manager calls the job ([`Service::name`]).
    pub name: String,
    pub job: JobState,
}

impl std::fmt::Display for Diagnosis {
    /// `systemd job sidevoice-core: loaded, not running, state ActiveState=failed, last exit 2`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let job = &self.job;
        write!(f, "{} job {}: ", self.manager, self.name)?;
        if !job.loaded {
            f.write_str("not loaded")?;
        } else if let (true, Some(pid)) = (job.running, job.pid) {
            write!(f, "loaded, running (pid {pid})")?;
        } else if job.running {
            f.write_str("loaded, running")?;
        } else if job.restarting {
            f.write_str("loaded, not running, the manager will start it again")?;
        } else {
            f.write_str("loaded, not running")?;
        }
        if let Some(state) = &job.state {
            write!(f, ", state {state}")?;
        }
        if let Some(signal) = &job.signal {
            write!(f, ", last signal {signal}")?;
        }
        if let Some(exit) = job.exit {
            write!(f, ", last exit {exit}")?;
        }
        if let Some(runs) = job.runs {
            write!(f, ", {runs} restarts")?;
        }
        if let Some(reason) = job.reason {
            write!(f, ", {reason}")?;
        }
        Ok(())
    }
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

/// What every manager implements ([`Services`], in `service/macos.rs` (launchd), `service/linux.rs` (systemd) and
/// `service/windows.rs` (Task Scheduler)): the required methods. The rest is one body for every manager; one
/// overrides it only if it must.
pub trait Service: Sized + Send + Sync {
    /// Its name, as `service status` and the jobs' `SIDEVOICE_SERVICE` say it.
    const KIND: &'static str;
    /// How many starts it allows before it gives up (systemd's start limit); none where it has none to report.
    const START_LIMIT: Option<u64> = None;
    /// The program, beside the connector, that runs the jobs' runner without a window (a stem: [`target::exe_name`] names it),
    /// where the manager starts a console program in a window of its own (Task Scheduler). It travels in the package,
    /// is staged in each release beside the connector, and replaces the connector in [`Service::runner`].
    const BACKGROUND: Option<&'static str> = None;

    /// This user's manager, its definitions kept under `places`. Nothing is asked of it yet.
    fn new(places: Places) -> Self;

    /// What the manager, and a person, call the job.
    fn name(job: JobId) -> String;

    /// Where the job's definition is.
    fn file(&self, job: JobId) -> impl Future<Output = PathBuf> + Send;

    /// The text of the job's definition. The values are checked ([`Job::check`]) before it is made.
    fn definition(job: &Job) -> String;

    /// The command a manager runs to run a job (then [`Job::runner`]'s `run-job` and the definition file): the
    /// installation's `command`, its program replaced by [`Service::BACKGROUND`] beside it where there is one and the
    /// release has it: a release from before the launcher (a rollback to it) keeps the plain command.
    fn runner(command: &[String]) -> Vec<String> {
        let mut runner = command.to_vec();
        if let (Some(stem), Some(program)) = (Self::BACKGROUND, runner.first_mut()) {
            let launcher = Path::new(program.as_str()).with_file_name(target::exe_name(stem));
            if launcher.is_file() {
                *program = launcher.to_string_lossy().into_owned();
            }
        }
        runner
    }

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
    /// to be removed), not at the next login either. Stopped is what the manager says afterwards. Why not. Its process
    /// is first asked to leave and given a grace time, as [`Process::stop`](crate::platform::process::Process::stop)
    /// does: launchd and systemd do that themselves (`SIGTERM`, then wait); Task Scheduler's end is a hard stop, so
    /// its implementation asks first. The same for a running job that `start` restarts.
    fn stop(&self, job: JobId, disable: bool) -> impl Future<Output = Result<(), String>> + Send;

    /// After the definitions are removed: the manager forgets the jobs.
    fn forget(&self, jobs: &[JobId]) -> impl Future<Output = Result<(), String>> + Send;

    /// The job as the manager reports it now.
    fn status(&self, job: JobId) -> impl Future<Output = JobState> + Send;

    /// The job as the manager reports it now, named as a person reads it ([`Diagnosis`]'s `Display`): the one account
    /// of a job that does not run, for `status`, `voice_diagnose` and an install that did not start.
    fn diagnose(&self, job: JobId) -> impl Future<Output = Diagnosis> + Send {
        async move {
            Diagnosis {
                manager: Self::KIND,
                name: Self::name(job),
                job: self.status(job).await,
            }
        }
    }

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

/// A job as a file a runner reads ([`Service::run_job`]): everything in [`Job`] but its names.
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

/// How long the output of a manager's command is awaited once the command itself has exited: a descendant that
/// keeps the pipes open does not hold the caller.
const PIPE_GRACE: Duration = Duration::from_secs(2);

/// Aborts its tasks when dropped, so a cancelled call leaves no reader behind.
struct Readers(Vec<tokio::task::AbortHandle>);

impl Drop for Readers {
    fn drop(&mut self) {
        for reader in &self.0 {
            reader.abort();
        }
    }
}

/// Runs a manager's own command, with no shell; never fails, never outlives its deadline (the command, its output and
/// what it started in its group or job: the tree is killed with it, also when the call is dropped).
async fn command(program: &str, args: &[&str]) -> Output {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match Processes::spawn_tree(command).await {
        Ok(child) => child,
        Err(error) => {
            return Output {
                ok: false,
                code: None,
                output: format!("{program}: {error}"),
            }
        }
    };
    let stdout = tokio::spawn(read_bounded(child.stdout().expect("piped stdout")));
    let stderr = tokio::spawn(read_bounded(child.stderr().expect("piped stderr")));
    let _readers = Readers(vec![stdout.abort_handle(), stderr.abort_handle()]);
    let status = match timeout(MANAGER_LIMIT, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            child.kill_tree().await;
            return Output {
                ok: false,
                code: None,
                output: format!("{program}: {error}"),
            };
        }
        Err(_) => {
            child.kill_tree().await;
            return Output {
                ok: false,
                code: None,
                output: format!("{program} {} timed out", args.join(" ")),
            };
        }
    };
    let collected = timeout(PIPE_GRACE, async {
        let mut output = stdout.await.unwrap_or_default();
        output.extend(stderr.await.unwrap_or_default());
        output
    })
    .await;
    // Whatever the command left running goes with it.
    child.kill_tree().await;
    let Ok(output) = collected else {
        return Output {
            ok: status.success(),
            code: status.code(),
            output: format!("{program}: its output did not end"),
        };
    };
    Output {
        ok: status.success(),
        code: status.code(),
        output: String::from_utf8_lossy(&output).into_owned(),
    }
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
    /// A manager command that exits while a descendant keeps its pipes open is answered within the grace, not when
    /// the descendant ends. (Unix only: it needs a shell to leave a descendant behind.)
    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_whose_descendant_holds_its_pipes_is_not_waited_for() {
        let started = tokio::time::Instant::now();
        let out = command("sh", &["-c", "sleep 30 & exit 0"]).await;
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(out.ok);
    }

    #[test]
    fn a_diagnosis_reads_as_the_job_its_manager_and_how_it_last_ended() {
        let said = |job: JobState| {
            Diagnosis {
                manager: "task-scheduler",
                name: "sidevoice-core-a".into(),
                job,
            }
            .to_string()
        };
        assert_eq!(
            said(JobState::default()),
            "task-scheduler job sidevoice-core-a: not loaded"
        );
        assert_eq!(
            said(JobState {
                loaded: true,
                state: Some("Ready".into()),
                exit: Some(2),
                ..JobState::default()
            }),
            "task-scheduler job sidevoice-core-a: loaded, not running, state Ready, last exit 2"
        );
        assert_eq!(
            said(JobState {
                loaded: true,
                running: true,
                pid: Some(7),
                ..JobState::default()
            }),
            "task-scheduler job sidevoice-core-a: loaded, running (pid 7)"
        );
    }

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
    fn the_runner_is_the_installations_command_with_the_background_program_where_the_manager_has_one(
    ) {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path();
        let connector = bin.join(target::exe_name("sidevoice-connector"));
        // A release from before the launcher has none beside its connector: its command is kept.
        let command = vec![connector.to_string_lossy().into_owned(), "x".into()];
        assert_eq!(Services::runner(&command), command);
        if let Some(stem) = Services::BACKGROUND {
            std::fs::write(bin.join(target::exe_name(stem)), b"launcher").unwrap();
        }
        let command = vec![connector.to_string_lossy().into_owned(), "x".into()];
        let runner = Services::runner(&command);
        let expected = match Services::BACKGROUND {
            Some(stem) => bin.join(target::exe_name(stem)),
            None => connector,
        };
        assert_eq!(Path::new(&runner[0]), expected);
        assert_eq!(runner[1..], command[1..]);
        assert!(Services::runner(&[]).is_empty());
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
            let text = Services::definition(&job);
            assert_eq!(
                Services::program_of(&text),
                Some(job.program.clone()),
                "{text}"
            );
        }
        assert_eq!(Services::program_of(""), None);
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
        assert_eq!(Services::run_job(&file).await.unwrap(), 0);
        let log = std::fs::read_to_string(&stored.log).unwrap();
        assert!(log.contains("run_job_child"), "{log}");
        let missing = Services::run_job(&dir.path().join("none.json")).await;
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
