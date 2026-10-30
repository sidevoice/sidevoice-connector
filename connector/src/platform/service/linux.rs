//! Linux: systemd's user manager, through `systemctl --user`: one user unit per job (`<name>.service`) in the
//! directory the manager reads them from. `Restart=` brings it back (the core on failure after 10 s, the connector
//! always after 2 s), at most [`START_LIMIT`] starts in 10 minutes; `enable` / `disable` say whether it starts at
//! login (`WantedBy=default.target`). Its output goes to the journal. A user's units run only while the user has a
//! session, unless the machine's owner enables linger (`loginctl enable-linger`): printed, never run.

use super::{command, Job, JobId, JobState, Linger, Output, Places, Refusal, Restart, Service};
use crate::platform::user::{User, UserId};
use std::future::Future;
use std::path::PathBuf;
use std::sync::OnceLock;

/// systemd's start limit, the same for both jobs.
const START_LIMIT: u64 = 5;
/// Sidevoice prints the linger command and never runs it. The one place to change that decision.
const RUN_LINGER: bool = false;

/// systemd's user manager, for this user.
pub struct Os {
    places: Places,
}

impl Service for Os {
    const KIND: &'static str = "systemd";
    const START_LIMIT: Option<u64> = Some(START_LIMIT);

    fn new(places: Places) -> Os {
        Os { places }
    }

    fn name(job: JobId) -> String {
        format!("{}.service", job.name)
    }

    // `$XDG_CONFIG_HOME/systemd/user` as the manager sees it (its own environment, which a shell's may differ from),
    // else as this process sees it.
    async fn file(&self, job: JobId) -> PathBuf {
        static MANAGER_CONFIG: OnceLock<Option<PathBuf>> = OnceLock::new();
        if MANAGER_CONFIG.get().is_none() {
            let shown = systemctl(&["show-environment"]).await;
            let value = |name: &str| {
                shown
                    .output
                    .lines()
                    .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute())
            };
            let config = shown
                .ok
                .then(|| {
                    value("XDG_CONFIG_HOME")
                        .or_else(|| value("HOME").map(|home| home.join(".config")))
                })
                .flatten();
            let _ = MANAGER_CONFIG.set(config);
        }
        MANAGER_CONFIG
            .get()
            .cloned()
            .flatten()
            .unwrap_or_else(|| self.places.config_home.clone())
            .join("systemd/user")
            .join(Self::name(job))
    }

    fn definition(job: &Job) -> String {
        unit_text(job)
    }

    fn program_of(text: &str) -> Option<Vec<String>> {
        let line = text
            .lines()
            .find_map(|line| line.strip_prefix("ExecStart="))?;
        let mut words = Vec::new();
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '"' {
                continue;
            }
            let mut word = String::new();
            while let Some(ch) = chars.next() {
                match ch {
                    '\\' => {
                        word.push('\\');
                        if let Some(next) = chars.next() {
                            word.push(next);
                        }
                    }
                    '"' => break,
                    other => word.push(other),
                }
            }
            words.push(unexec_word(&word));
        }
        Some(words)
    }

    // A changed unit is read again (`daemon-reload`); each job `reset-failed` (which clears an earlier start limit)
    // and `start`ed, or `restart`ed when asked or changed.
    async fn start(
        &self,
        jobs: &[JobId],
        changed: &[JobId],
        restart: bool,
        login: bool,
    ) -> Result<(), Refusal> {
        if let Err((job, detail)) = prepare(run, !changed.is_empty(), login, jobs).await {
            return Err(Refusal::NotEnabled(match job {
                Some(job) => format!("{}: {detail}", Self::name(job)),
                None => detail,
            }));
        }
        for &job in jobs {
            // A job that never failed has no start limit to clear, so the answer does not matter.
            systemctl(&["reset-failed", &Self::name(job)]).await;
            let verb = if restart || changed.contains(&job) {
                "restart"
            } else {
                "start"
            };
            let out = systemctl(&[verb, &Self::name(job)]).await;
            if !out.ok {
                return Err(Refusal::NotStarted(job, out.trimmed()));
            }
        }
        Ok(())
    }

    // `stop`, or `disable --now` (stopped and off at login): stopped is what the manager says afterwards, not what
    // the command returned.
    async fn stop(&self, job: JobId, disable: bool) -> Result<(), String> {
        let unit = Self::name(job);
        let out = if disable {
            systemctl(&["disable", "--now", &unit]).await
        } else {
            systemctl(&["stop", &unit]).await
        };
        match presence(&unit).await {
            Presence::Unknown(detail) => Err(format!("{unit}: not confirmed ({detail})")),
            Presence::Loaded { active: true } => Err(format!("{unit}: still active")),
            Presence::Loaded { .. } if !out.ok => Err(format!("{unit}: {}", out.trimmed())),
            _ => Ok(()),
        }
    }

    async fn forget(&self, jobs: &[JobId]) -> Result<(), String> {
        let reloaded = systemctl(&["daemon-reload"]).await;
        if !reloaded.ok {
            return Err(reloaded.trimmed());
        }
        for &job in jobs {
            systemctl(&["reset-failed", &Self::name(job)]).await;
        }
        Ok(())
    }

    async fn status(&self, job: JobId) -> JobState {
        let shown = systemctl(&[
            "show",
            "-p",
            "LoadState,ActiveState,SubState,Result,ExecMainStatus,ExecMainPID,NRestarts",
            &Self::name(job),
        ])
        .await;
        parse_systemd(if shown.ok { &shown.output } else { "" })
    }

    async fn login(&self, job: JobId) -> Option<bool> {
        login_with(&run, job).await
    }

    async fn set_login(&self, job: JobId, on: bool) -> Result<(), String> {
        set_login_with(&run, job, on).await
    }

    async fn answers() -> bool {
        systemctl(&["show-environment"]).await.ok
    }

    async fn linger(&self) -> Option<Linger> {
        let user = std::env::var("USER")
            .ok()
            .filter(|user| !user.is_empty())
            .or_else(UserId::name)
            .unwrap_or_default();
        let shown = command("loginctl", &["show-user", &user, "-p", "Linger"]).await;
        let enabled = shown.output.lines().any(|line| line.trim() == "Linger=yes");
        if !enabled && RUN_LINGER {
            command("loginctl", &["enable-linger", &user]).await;
        }
        Some(Linger {
            enabled,
            command: format!("loginctl enable-linger {user}"),
        })
    }
}

async fn systemctl(args: &[&str]) -> Output {
    let mut all = vec!["--user"];
    all.extend_from_slice(args);
    command("systemctl", &all).await
}

/// `name=value` in its `key=value` answer.
fn field(text: &str, name: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.trim_start().strip_prefix(name)?.strip_prefix('='))
        .map(|value| value.trim().to_owned())
}

/// [`systemctl`] with owned arguments (after `--user`): the runner the steps below take, which tests fake.
async fn run(args: Vec<String>) -> Output {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    systemctl(&args).await
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Presence {
    Loaded { active: bool },
    Absent,
    Unknown(String),
}

/// Whether the manager has a unit, as it answers: absent only when it says so (`LoadState=not-found`).
async fn presence(unit: &str) -> Presence {
    let shown = systemctl(&["show", "-p", "LoadState,ActiveState", unit]).await;
    let field = |name: &str| field(&shown.output, name);
    match (shown.ok, field("LoadState")) {
        (true, Some(load)) if load == "not-found" => Presence::Absent,
        (true, Some(_)) => Presence::Loaded {
            active: matches!(
                field("ActiveState").as_deref(),
                Some("active" | "activating" | "deactivating" | "reloading")
            ),
        },
        _ => Presence::Unknown(if shown.trimmed().is_empty() {
            format!("systemctl exited {:?}", shown.code)
        } else {
            shown.trimmed()
        }),
    }
}

/// `systemctl --user show` read.
fn parse_systemd(shown: &str) -> JobState {
    let field = |name: &str| field(shown, name);
    let load = field("LoadState");
    let sub = field("SubState");
    let pid = field("ExecMainPID")
        .and_then(|pid| pid.parse::<u32>().ok())
        .filter(|pid| *pid > 0);
    let running = matches!(
        field("ActiveState").as_deref(),
        Some("active" | "activating" | "reloading")
    ) && sub.as_deref() != Some("auto-restart")
        && pid.is_some();
    let loaded = load.as_deref().is_some_and(|load| load != "not-found");
    JobState {
        loaded,
        running,
        pid: pid.filter(|_| running),
        exit: field("ExecMainStatus").and_then(|code| code.parse().ok()),
        signal: None,
        runs: field("NRestarts").and_then(|runs| runs.parse().ok()),
        restarting: sub.as_deref() == Some("auto-restart"),
        reason: if !loaded {
            Some("not-loaded")
        } else if field("Result").as_deref() == Some("start-limit-hit") {
            Some("start-limit")
        } else {
            None
        },
        state: Some(
            ["ActiveState", "SubState", "Result"]
                .iter()
                .filter_map(|name| field(name).map(|value| format!("{name}={value}")))
                .collect::<Vec<_>>()
                .join(" "),
        )
        .filter(|state| !state.is_empty()),
    }
}

/// What systemd does before `jobs` start, through `systemctl` (its arguments after `--user`): with `reload`, it reads
/// the definitions again, and with `login` it enables each job for the next login, then checks that it is (without,
/// a job the person turned off for login stays off). A refusal of any of it is the start failing, even when the job
/// would run now: it would not start at the next login, or would run the definition from before. The job a refusal
/// is about, if it is about one, and what the manager said.
async fn prepare<F, Fut>(
    systemctl: F,
    reload: bool,
    login: bool,
    jobs: &[JobId],
) -> Result<(), (Option<JobId>, String)>
where
    F: Fn(Vec<String>) -> Fut,
    Fut: Future<Output = Output>,
{
    if reload {
        let out = systemctl(vec!["daemon-reload".into()]).await;
        if !out.ok {
            return Err((None, format!("daemon-reload: {}", out.trimmed())));
        }
    }
    if !login {
        return Ok(());
    }
    for &job in jobs {
        let unit = Os::name(job);
        let out = systemctl(vec!["enable".into(), unit.clone()]).await;
        if !out.ok {
            return Err((Some(job), format!("enable: {}", out.trimmed())));
        }
        let state = systemctl(vec!["is-enabled".into(), unit]).await;
        if !state.ok || state.trimmed() != "enabled" {
            return Err((Some(job), format!("is-enabled: {}", state.trimmed())));
        }
    }
    Ok(())
}

/// Whether a unit starts at the next login, from `systemctl --user is-enabled`: `enabled` or `disabled`; anything
/// else (`masked`, `static`, `enabled-runtime`, no answer) is `None`.
fn parse_systemd_login(shown: &str) -> Option<bool> {
    match shown.lines().next().map(str::trim) {
        Some("enabled") => Some(true),
        Some("disabled") => Some(false),
        _ => None,
    }
}

async fn login_with<F, Fut>(run: &F, job: JobId) -> Option<bool>
where
    F: Fn(Vec<String>) -> Fut,
    Fut: Future<Output = Output>,
{
    // `is-enabled` exits non-zero for `disabled`: its answer is the text.
    let shown = run(vec!["is-enabled".into(), Os::name(job)]).await;
    parse_systemd_login(&shown.output)
}

/// `enable` / `disable` of the unit, never with `--now`: nothing started or stopped.
async fn set_login_with<F, Fut>(run: &F, job: JobId, on: bool) -> Result<(), String>
where
    F: Fn(Vec<String>) -> Fut,
    Fut: Future<Output = Output>,
{
    let verb = if on { "enable" } else { "disable" };
    let out = run(vec![verb.into(), Os::name(job)]).await;
    if !out.ok {
        return Err(format!("{verb}: {}", out.trimmed()));
    }
    Ok(())
}

/// An `ExecStart=` word: quoted; `\` and `"` escaped, `%` a specifier (`%%`), `$` variable expansion (`$$`).
fn exec_word(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    )
}

fn unexec_word(word: &str) -> String {
    let mut out = String::new();
    let mut chars = word.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(ch);
        }
    }
    out.replace("%%", "%").replace("$$", "$")
}

/// One quoted `Environment=` assignment: `\` and `"` escaped, `%` a specifier; `$` is nothing special there.
fn environment_assignment(name: &str, value: &str) -> String {
    format!(
        "\"{name}={}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    )
}

/// A user unit: restarted on failure 10 s later ([`Restart::OnFailure`], the core) or always 2 s later
/// ([`Restart::Always`], the connector), at most [`START_LIMIT`] starts in 10 minutes (then `start-limit-hit`, which
/// a restart clears); started with the user's manager. `unset` names variables the job must not inherit from it.
fn unit_text(job: &Job) -> String {
    let exec = job
        .program
        .iter()
        .map(|word| exec_word(word))
        .collect::<Vec<_>>()
        .join(" ");
    let variables = job
        .environment
        .iter()
        .map(|(name, value)| format!("Environment={}", environment_assignment(name, value)))
        .collect::<Vec<_>>()
        .join("\n");
    let variables = if job.unset.is_empty() {
        variables
    } else {
        format!("{variables}\nUnsetEnvironment={}", job.unset.join(" "))
    };
    let (restart, restart_sec) = match job.restart {
        Restart::OnFailure => ("on-failure", 10),
        Restart::Always => ("always", 2),
    };
    format!(
        "[Unit]
Description={description}
StartLimitIntervalSec=600
StartLimitBurst={START_LIMIT}

[Service]
Type=simple
ExecStart={exec}
{variables}
Restart={restart}
RestartSec={restart_sec}
KillMode=control-group
TimeoutStopSec=20

[Install]
WantedBy=default.target
",
        description = job.description,
    )
}

// systemd's own answers and steps, which cannot be phrased for every manager: its unit text, its call sequences.
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    const CORE: JobId = JobId {
        label: "ai.sidevoice.core",
        name: "sidevoice-core",
    };
    const CONNECTOR: JobId = JobId {
        label: "ai.sidevoice.connector",
        name: "sidevoice-connector",
    };
    const BOTH: [JobId; 2] = [CORE, CONNECTOR];

    /// A `systemctl` that answers each call from `script` (its arguments → whether it succeeds, and what it prints;
    /// anything else succeeds silently), and logs every call.
    fn scripted(
        script: Vec<(String, bool, &'static str)>,
        log: Arc<Mutex<Vec<String>>>,
    ) -> impl Fn(Vec<String>) -> std::future::Ready<Output> {
        move |args: Vec<String>| {
            let call = args.join(" ");
            log.lock().unwrap().push(call.clone());
            let (ok, output) = script
                .iter()
                .find(|(line, _, _)| *line == call)
                .map_or((true, ""), |(_, ok, output)| (*ok, *output));
            std::future::ready(Output {
                ok,
                code: Some(i32::from(!ok)),
                output: output.into(),
            })
        }
    }

    fn enabled() -> Vec<(String, bool, &'static str)> {
        BOTH.iter()
            .map(|job| {
                (
                    format!("is-enabled {}.service", job.name),
                    true,
                    "enabled\n",
                )
            })
            .collect()
    }

    fn unit(program: &[&str], environment: &[(&str, &str)], unset: Vec<&'static str>) -> String {
        unit_text(&Job {
            id: CORE,
            description: "Sidevoice core",
            program: program.iter().map(|word| (*word).to_owned()).collect(),
            environment: environment
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect::<BTreeMap<_, _>>(),
            unset,
            restart: Restart::OnFailure,
            log: PathBuf::from("/d/core.log"),
            runner: vec![],
        })
    }

    #[test]
    fn units_quote_every_word_and_keep_the_restart_policy() {
        let text = unit(
            &["/r/cur rent/bin", "100%", "$HOME", "a\"b\\c"],
            &[("PRICE", "$5 at 50% \"off\"")],
            vec![],
        );
        assert!(
            text.contains(r#"ExecStart="/r/cur rent/bin" "100%%" "$$HOME" "a\"b\\c""#),
            "{text}"
        );
        assert!(
            text.contains(r#"Environment="PRICE=$5 at 50%% \"off\"""#),
            "{text}"
        );
        assert!(text.contains("Restart=on-failure\nRestartSec=10\n"));
        assert!(text.contains("StartLimitIntervalSec=600\nStartLimitBurst=5\n"));
        assert!(text.contains("WantedBy=default.target"));
        assert!(!text.contains("UnsetEnvironment"));
        let unset = unit(&["/bin/true"], &[], vec!["ALPHA_HOME", "BETA_HOME"]);
        assert!(
            unset.contains("\nUnsetEnvironment=ALPHA_HOME BETA_HOME\n"),
            "{unset}"
        );
    }

    #[tokio::test]
    async fn a_start_reloads_enables_and_checks_each_job() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let prepared = prepare(scripted(enabled(), log.clone()), true, true, &BOTH).await;
        assert_eq!(prepared, Ok(()));
        let calls = log.lock().unwrap().clone();
        assert_eq!(calls[0], "daemon-reload");
        for job in BOTH {
            assert!(
                calls.contains(&format!("enable {}.service", job.name)),
                "{calls:?}"
            );
            assert!(
                calls.contains(&format!("is-enabled {}.service", job.name)),
                "{calls:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_start_that_keeps_login_as_it_is_enables_nothing() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let prepared = prepare(scripted(vec![], log.clone()), true, false, &BOTH).await;
        assert_eq!(prepared, Ok(()));
        assert_eq!(*log.lock().unwrap(), vec!["daemon-reload".to_owned()]);
        log.lock().unwrap().clear();
        let prepared = prepare(scripted(vec![], log.clone()), false, false, &BOTH).await;
        assert_eq!(prepared, Ok(()));
        assert!(log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_refused_reload_or_enable_fails_the_start_even_when_the_job_would_run() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut script = enabled();
        script.push(("daemon-reload".into(), false, "Access denied"));
        let reload = prepare(scripted(script, log.clone()), true, true, &BOTH).await;
        assert_eq!(reload, Err((None, "daemon-reload: Access denied".into())));

        let mut script = enabled();
        script.push((
            "enable sidevoice-core.service".into(),
            false,
            "Failed to create symlink: Permission denied",
        ));
        let refused = prepare(scripted(script, log.clone()), false, true, &BOTH).await;
        assert_eq!(
            refused,
            Err((
                Some(CORE),
                "enable: Failed to create symlink: Permission denied".into()
            ))
        );

        // An enable the manager accepted while the job is still not enabled for the next login.
        let mut script = enabled();
        script.retain(|(line, _, _)| line != "is-enabled sidevoice-core.service");
        script.push((
            "is-enabled sidevoice-core.service".into(),
            false,
            "disabled",
        ));
        let checked = prepare(scripted(script, log), false, true, &BOTH).await;
        assert_eq!(checked, Err((Some(CORE), "is-enabled: disabled".into())));
    }

    #[tokio::test]
    async fn login_off_and_on_only_disable_and_enable_each_unit() {
        let off = BOTH
            .iter()
            .map(|job| {
                (
                    format!("is-enabled {}.service", job.name),
                    false,
                    "disabled\n",
                )
            })
            .collect();
        let log = Arc::new(Mutex::new(Vec::new()));
        let run = scripted(off, log.clone());
        for job in BOTH {
            set_login_with(&run, job, false).await.unwrap();
            assert_eq!(login_with(&run, job).await, Some(false));
        }
        assert_eq!(
            *log.lock().unwrap(),
            vec![
                "disable sidevoice-core.service",
                "is-enabled sidevoice-core.service",
                "disable sidevoice-connector.service",
                "is-enabled sidevoice-connector.service",
            ],
            "no --now, no stop: the running service is left running"
        );

        let log = Arc::new(Mutex::new(Vec::new()));
        let run = scripted(enabled(), log.clone());
        for job in BOTH {
            set_login_with(&run, job, true).await.unwrap();
        }
        assert_eq!(
            *log.lock().unwrap(),
            vec![
                "enable sidevoice-core.service",
                "enable sidevoice-connector.service"
            ],
            "no --now, no start: a stopped service stays stopped"
        );

        let refused = vec![(
            "disable sidevoice-core.service".to_owned(),
            false,
            "Access denied",
        )];
        let run = scripted(refused, log);
        assert_eq!(
            set_login_with(&run, CORE, false).await,
            Err("disable: Access denied".into())
        );
    }

    #[test]
    fn login_answers_read_as_on_off_or_unknown() {
        assert_eq!(parse_systemd_login("enabled\n"), Some(true));
        assert_eq!(parse_systemd_login("disabled\n"), Some(false));
        for other in ["masked\n", "static\n", "enabled-runtime\n", ""] {
            assert_eq!(parse_systemd_login(other), None, "{other:?}");
        }
    }

    #[test]
    fn reports_read_as_job_states() {
        let running = "LoadState=loaded\nActiveState=active\nSubState=running\nResult=success\nExecMainStatus=0\nExecMainPID=77\nNRestarts=0\n";
        let state = parse_systemd(running);
        assert_eq!(
            (state.loaded, state.running, state.pid, state.reason),
            (true, true, Some(77), None)
        );

        let waiting = "LoadState=loaded\nActiveState=activating\nSubState=auto-restart\nResult=exit-code\nExecMainStatus=1\nExecMainPID=0\nNRestarts=2\n";
        let state = parse_systemd(waiting);
        assert_eq!(
            (state.running, state.restarting, state.runs, state.exit),
            (false, true, Some(2), Some(1))
        );
        assert_eq!(
            state.state.as_deref(),
            Some("ActiveState=activating SubState=auto-restart Result=exit-code")
        );

        let limited = "LoadState=loaded\nActiveState=failed\nSubState=failed\nResult=start-limit-hit\nExecMainPID=0\n";
        assert_eq!(parse_systemd(limited).reason, Some("start-limit"));
        assert_eq!(
            parse_systemd("LoadState=not-found\n").reason,
            Some("not-loaded")
        );
        assert_eq!(
            parse_systemd("").reason,
            Some("not-loaded"),
            "a manager that did not answer"
        );
    }

    #[test]
    fn units_are_named_from_their_jobs_with_a_start_limit() {
        assert_eq!(Os::name(CORE), "sidevoice-core.service");
        assert_eq!(Os::name(CONNECTOR), "sidevoice-connector.service");
        assert_eq!(Os::START_LIMIT, Some(5));
    }
}
