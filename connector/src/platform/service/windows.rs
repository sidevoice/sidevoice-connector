//! Windows: Task Scheduler, through `schtasks.exe` (the platform's own tool; no maintained crate covers a per-user
//! task with restart on failure: `planif` only registers, `auto-launch` is a login item, `service-manager` drives
//! Windows services, which need an administrator). One task per job, `<name>-<user>` (task names are machine-wide),
//! run as this user, only while they are logged on, with no administrator and no stored password: a logon trigger
//! for this user starts it at login, enabled or disabled for `service login on|off`, and `RestartOnFailure` brings it
//! back (once a minute, the shortest Task Scheduler allows). Every call names absolute paths and goes through no
//! shell; the task is defined from XML (`/Create /XML`), started (`/Run`), ended (`/End`), read (`/Query /XML`) and
//! deleted (`/Delete`).
//!
//! A task cannot set its program's environment or send its output to a file, so its action runs the job's runner
//! ([`Job::runner`], the connector's `run-job`) on the job's definition, `<data>/service/<name>.json`
//! ([`Stored`]), which does both ([`Service::run_job`](super::Service::run_job)). Task Scheduler cannot tell a job's
//! pid or count its restarts: a job runs when this user has a process running its program, and restarts are not
//! reported. It also brings a task back only after a failure, so a connector that exits cleanly stays down until
//! the next login or start. `/End` is a hard stop; the graceful request is the caller's, in band, first.

use super::{command, write_file, Job, JobId, JobState, Output, Places, Refusal, Service, Stored};
use crate::platform::fs::{Files, Fs};
use crate::platform::process::{Process, Processes};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::time::{sleep, Duration, Instant};

/// Task Scheduler, for this user.
pub struct Os {
    places: Places,
}

impl Service for Os {
    const KIND: &'static str = "task-scheduler";

    fn new(places: Places) -> Os {
        Os { places }
    }

    fn name(job: JobId) -> String {
        format!("{}-{}", job.name, whoami::username().unwrap_or_default())
    }

    async fn file(&self, job: JobId) -> PathBuf {
        self.dir().join(format!("{}.json", job.name))
    }

    fn definition(job: &Job) -> String {
        let mut text = serde_json::to_string_pretty(&Stored::from(job)).unwrap_or_default();
        text.push('\n');
        text
    }

    fn program_of(text: &str) -> Option<Vec<String>> {
        Some(serde_json::from_str::<Stored>(text).ok()?.program)
    }

    // A changed definition, or a task not there, is registered again; a running task is ended first on `restart`,
    // and `/Run` starts it (a running task is left as it is: one instance at a time).
    async fn start(
        &self,
        jobs: &[JobId],
        changed: &[JobId],
        restart: bool,
        login: bool,
    ) -> Result<(), Refusal> {
        for &job in jobs {
            let task = Self::name(job);
            let registered = registered(&task).await;
            let now = if registered {
                self.login(job).await
            } else {
                Some(false)
            };
            if !registered || changed.contains(&job) || (login && now != Some(true)) {
                let on = match (login, now) {
                    (true, _) => true,
                    (false, Some(now)) => now,
                    (false, None) => return Err(Refusal::LoginUnknown(task)),
                };
                self.register(job, on)
                    .await
                    .map_err(|detail| Refusal::NotLoaded(format!("{task}: {detail}")))?;
            }
            if restart {
                schtasks(&["/End", "/TN", &task]).await;
            }
            let ran = schtasks(&["/Run", "/TN", &task]).await;
            if !ran.ok {
                return Err(Refusal::NotLoaded(format!("{task}: {}", said(&ran))));
            }
        }
        Ok(())
    }

    async fn stop(&self, job: JobId, _disable: bool) -> Result<(), String> {
        let task = Self::name(job);
        if !registered(&task).await {
            return Ok(());
        }
        let ended = schtasks(&["/End", "/TN", &task]).await;
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.status(job).await.running {
            if Instant::now() >= deadline {
                return Err(format!("{task}: still running ({})", said(&ended)));
            }
            sleep(Duration::from_millis(100)).await;
        }
        Ok(())
    }

    async fn forget(&self, jobs: &[JobId]) -> Result<(), String> {
        for &job in jobs {
            let task = Self::name(job);
            if registered(&task).await {
                let deleted = schtasks(&["/Delete", "/TN", &task, "/F"]).await;
                if !deleted.ok {
                    return Err(format!("{task}: {}", said(&deleted)));
                }
            }
            let _ = std::fs::remove_file(self.xml_file(job));
        }
        Ok(())
    }

    async fn status(&self, job: JobId) -> JobState {
        if !registered(&Self::name(job)).await {
            return JobState {
                reason: Some("not-loaded"),
                ..JobState::default()
            };
        }
        let pid = match self.program(job).await {
            Some(program) => {
                let command = program.join(" ");
                Processes::find(|found| found == command).first().copied()
            }
            None => None,
        };
        JobState {
            loaded: true,
            running: pid.is_some(),
            pid,
            ..JobState::default()
        }
    }

    async fn login(&self, job: JobId) -> Option<bool> {
        let queried = schtasks(&["/Query", "/TN", &Self::name(job), "/XML"]).await;
        queried.ok.then(|| parse_login(&queried.output)).flatten()
    }

    async fn set_login(&self, job: JobId, on: bool) -> Result<(), String> {
        self.register(job, on).await
    }
}

impl Os {
    fn dir(&self) -> PathBuf {
        self.places.data.join("service")
    }

    fn xml_file(&self, job: JobId) -> PathBuf {
        self.dir().join(format!("{}.xml", job.name))
    }

    /// The task (re)defined from the job's definition, its logon trigger enabled (`on`) or not: the XML written
    /// beside the definition, then `/Create /XML /F`, which replaces the task and leaves a running instance running.
    async fn register(&self, job: JobId, on: bool) -> Result<(), String> {
        let file = self.file(job).await;
        let stored: Stored = Files::read_trusted(&file, super::DEFINITION_LIMIT)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("{} is missing", file.display()))
            .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|error| error.to_string()))?;
        let user = whoami::account().map_err(|error| error.to_string())?;
        let xml_file = self.xml_file(job);
        write_file(&xml_file, &utf16(&task_xml(&stored, &user, on)?))
            .map_err(|refusal| format!("{refusal:?}"))?;
        let created = schtasks(&[
            "/Create",
            "/TN",
            &Self::name(job),
            "/XML",
            &xml_file.to_string_lossy(),
            "/F",
        ])
        .await;
        if !created.ok {
            return Err(said(&created));
        }
        Ok(())
    }
}

// `schtasks` from the system directory, which Rust's process search looks in before any `PATH` entry.
async fn schtasks(args: &[&str]) -> Output {
    command("schtasks", args).await
}

/// What `schtasks` said, or how it exited when it said nothing.
fn said(out: &Output) -> String {
    match out.trimmed() {
        said if said.is_empty() => format!("schtasks exited {:?}", out.code),
        said => said,
    }
}

/// Whether Task Scheduler has the task. A query that fails for any reason is no task: nothing distinguishes "not
/// found" from another failure across languages but the exit.
async fn registered(task: &str) -> bool {
    schtasks(&["/Query", "/TN", task]).await.ok
}

/// What is read back from a task's XML: whether its logon trigger and the task itself are enabled (Task Scheduler
/// leaves out an `Enabled` that is `true`).
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Queried {
    triggers: Option<QueriedTriggers>,
    settings: Option<Switch>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct QueriedTriggers {
    logon_trigger: Option<Switch>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Switch {
    enabled: Option<bool>,
}

/// Whether the task starts at the next logon, from its XML (`/Query /XML`, UTF-16 or not): its logon trigger and
/// the task itself both enabled. `None`: the output is not a task.
fn parse_login(text: &str) -> Option<bool> {
    let text: String = text
        .chars()
        .filter(|ch| !matches!(ch, '\0' | '\u{feff}'))
        .collect();
    let task: Queried = quick_xml::de::from_str(text.trim()).ok()?;
    let on = |switch: Option<&Switch>| switch.is_some_and(|switch| switch.enabled != Some(false));
    let trigger = task
        .triggers
        .as_ref()
        .and_then(|triggers| triggers.logon_trigger.as_ref());
    Some(
        on(trigger)
            && task
                .settings
                .as_ref()
                .is_none_or(|settings| settings.enabled != Some(false)),
    )
}

/// One argument as the program's C runtime reads it back (`CommandLineToArgvW` rules): quoted when it has a space,
/// a tab or a quote, or is empty; a quote escaped, and the backslashes before a quote or the closing one doubled.
fn quote(argument: &str) -> String {
    if !argument.is_empty() && !argument.contains([' ', '\t', '"']) {
        return argument.to_owned();
    }
    let mut out = String::from('"');
    let mut backslashes = 0;
    for ch in argument.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            other => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(other);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// A task as Task Scheduler reads it (`/Create /XML`); written by `quick-xml`.
#[derive(Serialize)]
#[serde(rename = "Task", rename_all = "PascalCase")]
struct Task<'a> {
    #[serde(rename = "@version")]
    version: &'a str,
    #[serde(rename = "@xmlns")]
    xmlns: &'a str,
    registration_info: RegistrationInfo<'a>,
    triggers: Triggers<'a>,
    principals: Principals<'a>,
    settings: Settings,
    actions: Actions<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct RegistrationInfo<'a> {
    description: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Triggers<'a> {
    logon_trigger: LogonTrigger<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct LogonTrigger<'a> {
    enabled: bool,
    user_id: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Principals<'a> {
    principal: Principal<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Principal<'a> {
    #[serde(rename = "@id")]
    id: &'a str,
    user_id: &'a str,
    logon_type: &'a str,
    run_level: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Settings {
    multiple_instances_policy: &'static str,
    disallow_start_if_on_batteries: bool,
    stop_if_going_on_batteries: bool,
    allow_hard_terminate: bool,
    start_when_available: bool,
    run_only_if_network_available: bool,
    idle_settings: IdleSettings,
    allow_start_on_demand: bool,
    enabled: bool,
    hidden: bool,
    run_only_if_idle: bool,
    wake_to_run: bool,
    execution_time_limit: &'static str,
    priority: u8,
    restart_on_failure: RestartOnFailure,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct IdleSettings {
    stop_on_idle_end: bool,
    restart_on_idle: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct RestartOnFailure {
    interval: &'static str,
    count: u16,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Actions<'a> {
    #[serde(rename = "@Context")]
    context: &'a str,
    exec: Exec<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Exec<'a> {
    command: &'a str,
    arguments: String,
}

/// The task: at this user's logon (`login`: its trigger enabled), as this user with their interactive token and
/// least privilege, one instance at a time, no time limit, not stopped on battery, restarted once a minute after a
/// failure; its action runs the job's runner on the job's definition. Task Scheduler expands `%NAME%` in an action,
/// so a word with `%` is refused.
fn task_xml(job: &Stored, user: &str, login: bool) -> Result<String, String> {
    let (command, runner_args) = job.runner.split_first().ok_or("no runner")?;
    if !Path::new(command).is_absolute() {
        return Err(format!("{command} is not an absolute path"));
    }
    if job.runner.iter().any(|word| word.contains('%')) {
        return Err("a runner word contains %, which Task Scheduler would expand".into());
    }
    let task = Task {
        version: "1.2",
        xmlns: "http://schemas.microsoft.com/windows/2004/02/mit/task",
        registration_info: RegistrationInfo {
            description: &job.description,
        },
        triggers: Triggers {
            logon_trigger: LogonTrigger {
                enabled: login,
                user_id: user,
            },
        },
        principals: Principals {
            principal: Principal {
                id: "Author",
                user_id: user,
                logon_type: "InteractiveToken",
                run_level: "LeastPrivilege",
            },
        },
        settings: Settings {
            multiple_instances_policy: "IgnoreNew",
            disallow_start_if_on_batteries: false,
            stop_if_going_on_batteries: false,
            allow_hard_terminate: true,
            start_when_available: false,
            run_only_if_network_available: false,
            idle_settings: IdleSettings {
                stop_on_idle_end: false,
                restart_on_idle: false,
            },
            allow_start_on_demand: true,
            enabled: true,
            hidden: false,
            run_only_if_idle: false,
            wake_to_run: false,
            execution_time_limit: "PT0S",
            priority: 7,
            restart_on_failure: RestartOnFailure {
                interval: "PT1M",
                count: 999,
            },
        },
        actions: Actions {
            context: "Author",
            exec: Exec {
                command,
                arguments: runner_args
                    .iter()
                    .map(|word| quote(word))
                    .collect::<Vec<_>>()
                    .join(" "),
            },
        },
    };
    let mut text = String::from("<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n");
    let mut serializer = quick_xml::se::Serializer::new(&mut text);
    serializer.indent(' ', 2);
    task.serialize(serializer)
        .map_err(|error| error.to_string())?;
    text.push('\n');
    Ok(text)
}

/// UTF-16 little-endian with its byte-order mark: how Task Scheduler writes task XML, and reads it surely.
fn utf16(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    bytes
}

// Task Scheduler's own text, which cannot be phrased for every manager: its XML, its quoting.
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn stored(runner: &[&str]) -> Stored {
        Stored {
            description: "Sidevoice core & <voice>".into(),
            program: vec!["C:\\r\\core.exe".into()],
            environment: BTreeMap::new(),
            unset: vec![],
            log: PathBuf::from("C:\\d\\core.log"),
            runner: runner.iter().map(|word| (*word).to_owned()).collect(),
        }
    }

    #[test]
    fn a_task_runs_the_runner_at_this_users_logon_and_restarts_after_a_failure() {
        let job = stored(&[
            "C:\\Program Files\\Sidevoice\\sidevoice-connector.exe",
            "run-job",
            "C:\\Users\\A B\\data\\service\\sidevoice-core.json",
        ]);
        let text = task_xml(&job, "alice@example.com", true).unwrap();
        assert!(text.starts_with("<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">"), "{text}");
        assert!(text
            .contains("<Command>C:\\Program Files\\Sidevoice\\sidevoice-connector.exe</Command>"));
        assert!(
            text.replace("&quot;", "\"").contains(
                "<Arguments>run-job \"C:\\Users\\A B\\data\\service\\sidevoice-core.json\"</Arguments>"
            ),
            "{text}"
        );
        for element in [
            "<UserId>alice@example.com</UserId>",
            "<Principal id=\"Author\">",
            "<LogonType>InteractiveToken</LogonType>",
            "<RunLevel>LeastPrivilege</RunLevel>",
            "<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
            "<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
            "<Interval>PT1M</Interval>",
            "<Count>999</Count>",
            "<Actions Context=\"Author\">",
            "<Description>Sidevoice core &amp; &lt;voice&gt;</Description>",
        ] {
            assert!(text.contains(element), "{element}: {text}");
        }
        assert_eq!(parse_login(&text), Some(true));
        let off = task_xml(&job, "alice", false).unwrap();
        assert_eq!(parse_login(&off), Some(false));
        let utf16_text: String = String::from_utf16_lossy(
            &utf16(&off)[2..]
                .chunks(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>(),
        );
        assert_eq!(utf16_text, off);
    }

    #[test]
    fn a_relative_runner_or_one_task_scheduler_would_expand_is_refused() {
        assert!(task_xml(&stored(&["sidevoice-connector.exe"]), "a", true).is_err());
        assert!(task_xml(&stored(&["C:\\x.exe", "%PATH%"]), "a", true).is_err());
        assert!(task_xml(&stored(&[]), "a", true).is_err());
    }

    #[test]
    fn arguments_are_quoted_as_the_c_runtime_reads_them() {
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote(""), "\"\"");
        assert_eq!(quote("a b"), "\"a b\"");
        assert_eq!(quote("C:\\a b\\"), "\"C:\\a b\\\\\"");
        assert_eq!(quote("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(quote("a\\\"b"), "\"a\\\\\\\"b\"");
    }

    #[test]
    fn login_is_read_from_the_trigger_and_the_task() {
        let task = |trigger: &str, task: &str| {
            format!("<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n<RegistrationInfo><URI>\\\\sidevoice-core-alice</URI></RegistrationInfo>\n<Triggers><LogonTrigger><Enabled>{trigger}</Enabled><UserId>alice</UserId></LogonTrigger></Triggers>\n<Settings><IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd></IdleSettings><Enabled>{task}</Enabled></Settings></Task>")
        };
        assert_eq!(parse_login(&task("true", "true")), Some(true));
        assert_eq!(parse_login(&task("false", "true")), Some(false));
        assert_eq!(parse_login(&task("true", "false")), Some(false));
        let utf16ish: String = task("true", "true")
            .chars()
            .flat_map(|ch| [ch, '\0'])
            .collect();
        assert_eq!(parse_login(&utf16ish), Some(true));
        assert_eq!(
            parse_login("<Task><Triggers/></Task>"),
            Some(false),
            "no logon trigger"
        );
        assert_eq!(
            parse_login(
                "<Task><Triggers><LogonTrigger><UserId>a</UserId></LogonTrigger></Triggers></Task>"
            ),
            Some(true),
            "an Enabled left out is true"
        );
        assert_eq!(parse_login("ERROR: access denied"), None);
    }
}
