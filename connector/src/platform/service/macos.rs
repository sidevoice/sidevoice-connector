//! macOS: launchd, through `/bin/launchctl`, in this user's GUI domain (`gui/<uid>`, the uid from `rustix`): one
//! LaunchAgent per job in `~/Library/LaunchAgents/<label>.plist`. RunAtLoad starts it at login, KeepAlive brings it
//! back (the core after a crash or a non-zero exit, the connector whenever it exits), ThrottleInterval 10 spaces the
//! starts; its output goes to the job's log. Whether it starts at login is launchd's override for its label
//! (`enable` / `disable`), which a plist's RunAtLoad obeys.

use super::{command, Job, JobId, JobState, Output, Places, Refusal, Restart, Service};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use tokio::time::{sleep, Duration, Instant};

/// launchd, for this user.
pub struct Os {
    places: Places,
}

impl Service for Os {
    const KIND: &'static str = "launchd";

    fn new(places: Places) -> Os {
        Os { places }
    }

    fn name(job: JobId) -> String {
        job.label.to_owned()
    }

    async fn file(&self, job: JobId) -> PathBuf {
        self.places
            .home
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", job.label))
    }

    fn definition(job: &Job) -> String {
        plist_text(job)
    }

    fn program_of(text: &str) -> Option<Vec<String>> {
        plist::from_bytes::<Program>(text.as_bytes())
            .ok()
            .map(|program| program.program_arguments)
    }

    // A changed plist is loaded again (launchd reads it only when it is bootstrapped); a running job is restarted
    // with `kickstart -k`.
    async fn start(
        &self,
        jobs: &[JobId],
        changed: &[JobId],
        restart: bool,
        login: bool,
    ) -> Result<(), Refusal> {
        let fail =
            |job: JobId, detail: String| Refusal::NotLoaded(format!("{}: {detail}", job.label));
        for &job in jobs {
            if login {
                // An override left by an earlier installation would keep the job from loading at the next login.
                let enabled = launchctl(&["enable", &target(job)]).await;
                if !enabled.ok {
                    return Err(Refusal::NotEnabled(format!(
                        "{}: enable: {}",
                        job.label,
                        enabled.trimmed()
                    )));
                }
            }
            let loaded = matches!(presence(job).await, Presence::Loaded);
            if changed.contains(&job) && loaded {
                boot_out(job).await.map_err(|detail| fail(job, detail))?;
            }
            if !matches!(presence(job).await, Presence::Loaded) {
                let now = if login {
                    Some(true)
                } else {
                    login_with(&run, job).await
                };
                let plan = boot_plan(login, now);
                if plan == Boot::Refused {
                    return Err(Refusal::LoginUnknown(job.label.to_owned()));
                }
                self.boot_in(job, plan == Boot::Reenabled)
                    .await
                    .map_err(|detail| fail(job, detail))?;
                continue;
            }
            let target = target(job);
            let mut args = vec!["kickstart"];
            if restart {
                args.push("-k");
            }
            args.push(&target);
            let kicked = launchctl(&args).await;
            if !kicked.ok {
                return Err(fail(job, kicked.trimmed()));
            }
        }
        Ok(())
    }

    // `bootout`, waited out, so KeepAlive does not bring it back until the next login.
    async fn stop(&self, job: JobId, _disable: bool) -> Result<(), String> {
        match presence(job).await {
            Presence::Unknown(detail) => Err(format!(
                "{}: the manager could not be asked ({detail})",
                job.label
            )),
            Presence::Absent => Ok(()),
            Presence::Loaded => boot_out(job)
                .await
                .map_err(|detail| format!("{}: {detail}", job.label)),
        }
    }

    async fn forget(&self, _jobs: &[JobId]) -> Result<(), String> {
        Ok(())
    }

    async fn status(&self, job: JobId) -> JobState {
        let printed = launchctl(&["print", &target(job)]).await;
        parse_launchd(printed.ok, &printed.output)
    }

    async fn login(&self, job: JobId) -> Option<bool> {
        login_with(&run, job).await
    }

    async fn set_login(&self, job: JobId, on: bool) -> Result<(), String> {
        set_login_with(&run, job, on).await
    }
}

impl Os {
    /// The job loaded from its plist; with `reenable` ([`Boot::Reenabled`]), enabled for the bootstrap and disabled
    /// again.
    async fn boot_in(&self, job: JobId, reenable: bool) -> Result<(), String> {
        if reenable {
            let enabled = launchctl(&["enable", &target(job)]).await;
            if !enabled.ok {
                return Err(format!("enable: {}", enabled.trimmed()));
            }
        }
        let booted = self.bootstrap(job).await;
        if reenable {
            let disabled = launchctl(&["disable", &target(job)]).await;
            if !disabled.ok {
                return Err(format!("disable: {}", disabled.trimmed()));
            }
        }
        booted
    }

    async fn bootstrap(&self, job: JobId) -> Result<(), String> {
        let file = self.file(job).await;
        let file = file.to_string_lossy().into_owned();
        let mut booted = launchctl(&["bootstrap", &domain(), &file]).await;
        for _ in 0..10 {
            if booted.ok || matches!(presence(job).await, Presence::Loaded) {
                return Ok(());
            }
            sleep(Duration::from_millis(500)).await;
            booted = launchctl(&["bootstrap", &domain(), &file]).await;
        }
        if booted.ok || matches!(presence(job).await, Presence::Loaded) {
            return Ok(());
        }
        Err(booted.trimmed())
    }
}

async fn launchctl(args: &[&str]) -> Output {
    command("/bin/launchctl", args).await
}

/// [`launchctl`] with owned arguments: the runner the steps below take, which tests fake.
async fn run(args: Vec<String>) -> Output {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    launchctl(&args).await
}

fn domain() -> String {
    format!("gui/{}", rustix::process::geteuid().as_raw())
}

fn target(job: JobId) -> String {
    format!("{}/{}", domain(), job.label)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Presence {
    Loaded,
    Absent,
    Unknown(String),
}

/// Whether launchd has a job, as it answers: absent only when it says so ("Could not find service", 113).
async fn presence(job: JobId) -> Presence {
    let printed = launchctl(&["print", &target(job)]).await;
    if printed.ok {
        return Presence::Loaded;
    }
    if printed.code == Some(113)
        || printed
            .output
            .to_ascii_lowercase()
            .contains("could not find service")
    {
        return Presence::Absent;
    }
    Presence::Unknown(if printed.trimmed().is_empty() {
        format!("launchctl exited {:?}", printed.code)
    } else {
        printed.trimmed()
    })
}

/// `launchctl bootout` returns before launchd has let the job go, and a `bootstrap` in that interval fails: the job
/// is waited out, until launchd says it is gone (which a manager that cannot be asked never says).
async fn boot_out(job: JobId) -> Result<(), String> {
    let out = launchctl(&["bootout", &target(job)]).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match presence(job).await {
            Presence::Absent => return Ok(()),
            now if Instant::now() >= deadline => {
                return Err(match now {
                    Presence::Unknown(detail) => format!("not confirmed: {detail}"),
                    _ if !out.trimmed().is_empty() => out.trimmed(),
                    _ => "still loaded".into(),
                })
            }
            _ => sleep(Duration::from_millis(100)).await,
        }
    }
}

/// How a job is bootstrapped, from whether this start enables it for login and, if not, whether it starts at login
/// now. launchd refuses to bootstrap a job disabled for login, so one the person turned off is enabled for the
/// bootstrap and disabled again right after: it runs now and still not at the next login. When that cannot be read,
/// enabling it could leave it on for good: refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Boot {
    Plain,
    Reenabled,
    Refused,
}

fn boot_plan(login: bool, now: Option<bool>) -> Boot {
    match (login, now) {
        (true, _) | (false, Some(true)) => Boot::Plain,
        (false, Some(false)) => Boot::Reenabled,
        (false, None) => Boot::Refused,
    }
}

/// `launchctl print` read. A job that is loaded, not running, and last ended badly is one launchd starts again
/// (its `KeepAlive`), throttled.
fn parse_launchd(ok: bool, printed: &str) -> JobState {
    if !ok {
        return JobState {
            reason: Some("not-loaded"),
            ..JobState::default()
        };
    }
    let field = |name: &str| {
        printed.lines().find_map(|line| {
            let (key, value) = line.trim().split_once(" = ")?;
            (key == name).then(|| value.trim().to_owned())
        })
    };
    let pid = field("pid")
        .and_then(|pid| pid.parse::<u32>().ok())
        .filter(|pid| *pid > 0);
    let running = field("state").as_deref() == Some("running") && pid.is_some();
    let exit = field("last exit code").and_then(|code| code.parse::<i64>().ok());
    let signal = field("last terminating signal").filter(|signal| !signal.is_empty());
    JobState {
        loaded: true,
        running,
        pid: pid.filter(|_| running),
        restarting: !running && (exit.is_some_and(|code| code != 0) || signal.is_some()),
        exit,
        signal,
        runs: field("runs")
            .and_then(|runs| runs.parse().ok())
            .filter(|runs| *runs > 0),
        reason: None,
        state: field("state"),
    }
}

/// Whether a job starts at the next login, read from `launchctl print-disabled <domain>`: its label overridden
/// `disabled` (`true` on older macOS) does not, `enabled` (`false`) does, and a label with no override loads with its
/// plist. `None`: the output is not that list.
fn parse_launchd_login(printed: &str, label: &str) -> Option<bool> {
    if !printed.contains("disabled services") {
        return None;
    }
    let quoted = format!("\"{label}\"");
    let value = printed.lines().find_map(|line| {
        line.trim()
            .strip_prefix(&quoted)?
            .trim_start()
            .strip_prefix("=>")
            .map(str::trim)
    });
    match value {
        None | Some("enabled" | "false") => Some(true),
        Some("disabled" | "true") => Some(false),
        Some(_) => None,
    }
}

async fn login_with<F, Fut>(run: &F, job: JobId) -> Option<bool>
where
    F: Fn(Vec<String>) -> Fut,
    Fut: Future<Output = Output>,
{
    let printed = run(vec!["print-disabled".into(), domain()]).await;
    printed
        .ok
        .then(|| parse_launchd_login(&printed.output, job.label))
        .flatten()
}

/// `enable` / `disable` of the job's label, which only says whether it loads at login: no `bootout`, `bootstrap` or
/// `kickstart`.
async fn set_login_with<F, Fut>(run: &F, job: JobId, on: bool) -> Result<(), String>
where
    F: Fn(Vec<String>) -> Fut,
    Fut: Future<Output = Output>,
{
    let verb = if on { "enable" } else { "disable" };
    let out = run(vec![verb.into(), target(job)]).await;
    if !out.ok {
        return Err(format!("{verb}: {}", out.trimmed()));
    }
    Ok(())
}

/// A LaunchAgent, as launchd reads it; written and read by `plist`.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Agent {
    label: String,
    program_arguments: Vec<String>,
    environment_variables: BTreeMap<String, String>,
    run_at_load: bool,
    keep_alive: KeepAlive,
    throttle_interval: u64,
    standard_out_path: PathBuf,
    standard_error_path: PathBuf,
}

/// `KeepAlive`: always (`true`), or only after a crash or a non-zero exit.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum KeepAlive {
    Always(bool),
    #[serde(rename_all = "PascalCase")]
    Unless {
        successful_exit: bool,
        crashed: bool,
    },
}

/// What is read back from a plist: its program.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Program {
    program_arguments: Vec<String>,
}

/// A LaunchAgent. [`Restart::OnFailure`]: back after a crash or a non-zero exit, never after exit 0 (the core);
/// [`Restart::Always`]: back whenever it exits (the connector). At most one start every 10 s. Output to the job's
/// log.
fn plist_text(job: &Job) -> String {
    let agent = Agent {
        label: job.id.label.to_owned(),
        program_arguments: job.program.clone(),
        environment_variables: job.environment.clone(),
        run_at_load: true,
        keep_alive: match job.restart {
            Restart::OnFailure => KeepAlive::Unless {
                successful_exit: false,
                crashed: true,
            },
            Restart::Always => KeepAlive::Always(true),
        },
        throttle_interval: 10,
        standard_out_path: job.log.clone(),
        standard_error_path: job.log.clone(),
    };
    let mut text = Vec::new();
    plist::to_writer_xml(&mut text, &agent).expect("a LaunchAgent serializes");
    String::from_utf8(text).expect("plist writes UTF-8")
}

// launchd's own answers and steps, which cannot be phrased for every manager: its text, its overrides.
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    const CORE: JobId = JobId {
        label: "dev.sidevoice.core",
        name: "sidevoice-core",
    };
    const CONNECTOR: JobId = JobId {
        label: "dev.sidevoice.connector",
        name: "sidevoice-connector",
    };

    /// A `launchctl` that answers each call from `script` (its arguments → whether it succeeds, and what it
    /// prints; anything else succeeds silently), and logs every call.
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

    const PRINT_DISABLED_OFF: &str = "disabled services = {\n\t\"com.apple.Siri.agent\" => enabled\n\t\"dev.sidevoice.core\" => disabled\n\t\"dev.sidevoice.connector\" => disabled\n}\n";
    const PRINT_DISABLED_ON: &str = "disabled services = {\n\t\"dev.sidevoice.core\" => enabled\n\t\"dev.sidevoice.connector\" => enabled\n}\n";

    fn job(restart: Restart, label: &'static str) -> Job {
        Job {
            id: JobId { label, name: "x" },
            description: "x",
            program: vec![
                "/r/current/core/bin/sidevoice-core".into(),
                "--data-dir".into(),
                "/d/core & <x>".into(),
            ],
            environment: BTreeMap::from([("HOME".to_owned(), "/home/u".to_owned())]),
            unset: vec![],
            restart,
            log: PathBuf::from("/d/core.stderr.log"),
            runner: vec![],
        }
    }

    #[test]
    fn plists_keep_the_two_restart_policies() {
        let read = |text: String| {
            plist::from_bytes::<plist::Dictionary>(text.as_bytes()).expect("a plist")
        };
        let core = read(plist_text(&job(Restart::OnFailure, "dev.sidevoice.core")));
        let connector = read(plist_text(&job(Restart::Always, "dev.sidevoice.connector")));
        let keep_alive = core["KeepAlive"].as_dictionary().unwrap();
        assert_eq!(keep_alive["SuccessfulExit"].as_boolean(), Some(false));
        assert_eq!(keep_alive["Crashed"].as_boolean(), Some(true));
        assert_eq!(connector["KeepAlive"].as_boolean(), Some(true));
        assert_eq!(core["Label"].as_string(), Some("dev.sidevoice.core"));
        assert_eq!(core["RunAtLoad"].as_boolean(), Some(true));
        assert_eq!(core["ThrottleInterval"].as_signed_integer(), Some(10));
        let arguments = core["ProgramArguments"].as_array().unwrap();
        assert_eq!(arguments[2].as_string(), Some("/d/core & <x>"));
        let environment = core["EnvironmentVariables"].as_dictionary().unwrap();
        assert_eq!(environment["HOME"].as_string(), Some("/home/u"));
        for key in ["StandardOutPath", "StandardErrorPath"] {
            assert_eq!(core[key].as_string(), Some("/d/core.stderr.log"));
        }
        let text = plist_text(&job(Restart::Always, "dev.sidevoice.connector"));
        assert!(text.contains("&amp; &lt;x&gt;"), "{text}");
        assert_eq!(
            Os::program_of(&text).unwrap()[2],
            "/d/core & <x>",
            "escaped by plist, read back by plist"
        );
    }

    #[tokio::test]
    async fn login_off_and_on_only_disable_and_enable_each_label() {
        let domain = domain();
        let printed = |output| vec![(format!("print-disabled {domain}"), true, output)];
        let log = Arc::new(Mutex::new(Vec::new()));
        let run = scripted(printed(PRINT_DISABLED_OFF), log.clone());
        for job in [CORE, CONNECTOR] {
            set_login_with(&run, job, false).await.unwrap();
            assert_eq!(login_with(&run, job).await, Some(false));
        }
        assert_eq!(
            *log.lock().unwrap(),
            vec![
                format!("disable {domain}/dev.sidevoice.core"),
                format!("print-disabled {domain}"),
                format!("disable {domain}/dev.sidevoice.connector"),
                format!("print-disabled {domain}"),
            ],
            "no bootout: the loaded jobs stay loaded and running"
        );

        let log = Arc::new(Mutex::new(Vec::new()));
        let run = scripted(printed(PRINT_DISABLED_ON), log.clone());
        set_login_with(&run, CORE, true).await.unwrap();
        assert_eq!(login_with(&run, CORE).await, Some(true));
        assert_eq!(
            *log.lock().unwrap(),
            vec![
                format!("enable {domain}/dev.sidevoice.core"),
                format!("print-disabled {domain}"),
            ],
            "no bootstrap or kickstart"
        );

        let failed = vec![(format!("print-disabled {domain}"), false, "")];
        let run = scripted(failed, log);
        assert_eq!(
            login_with(&run, CORE).await,
            None,
            "launchctl could not be asked"
        );
    }

    #[test]
    fn a_job_off_for_login_is_bootstrapped_reenabled_and_refused_when_it_cannot_tell() {
        assert_eq!(boot_plan(true, None), Boot::Plain);
        assert_eq!(boot_plan(false, Some(true)), Boot::Plain);
        assert_eq!(boot_plan(false, Some(false)), Boot::Reenabled);
        assert_eq!(boot_plan(false, None), Boot::Refused);
    }

    #[test]
    fn login_answers_read_as_on_off_or_unknown() {
        assert_eq!(
            parse_launchd_login(PRINT_DISABLED_OFF, "dev.sidevoice.core"),
            Some(false)
        );
        assert_eq!(
            parse_launchd_login(PRINT_DISABLED_ON, "dev.sidevoice.core"),
            Some(true)
        );
        let older = "disabled services = {\n\t\"dev.sidevoice.core\" => true\n\t\"dev.sidevoice.connector\" => false\n}\n";
        assert_eq!(
            parse_launchd_login(older, "dev.sidevoice.core"),
            Some(false)
        );
        assert_eq!(
            parse_launchd_login(older, "dev.sidevoice.connector"),
            Some(true)
        );
        assert_eq!(
            parse_launchd_login("disabled services = {\n}\n", "dev.sidevoice.core"),
            Some(true),
            "no override: it loads with its plist"
        );
        assert_eq!(parse_launchd_login("", "dev.sidevoice.core"), None);
        assert_eq!(
            parse_launchd_login(
                "disabled services = {\n\t\"dev.sidevoice.core\" => ???\n}",
                "dev.sidevoice.core"
            ),
            None
        );
    }

    #[test]
    fn reports_read_as_job_states() {
        let running = "gui/501/dev.sidevoice.core = {\n\tstate = running\n\truns = 3\n\tpid = 4242\n\tlast exit code = 0\n}";
        let state = parse_launchd(true, running);
        assert_eq!(
            (state.loaded, state.running, state.pid, state.runs),
            (true, true, Some(4242), Some(3))
        );
        assert!(!state.restarting);

        let crashed = "\tstate = not running\n\truns = 2\n\tlast exit code = 1\n\tlast terminating signal = Killed: 9\n";
        let state = parse_launchd(true, crashed);
        assert_eq!(
            (state.running, state.pid, state.exit),
            (false, None, Some(1))
        );
        assert_eq!(state.signal.as_deref(), Some("Killed: 9"));
        assert!(state.restarting);

        let gave_up = "\tstate = not running\n\tlast exit code = 0\n";
        assert!(!parse_launchd(true, gave_up).restarting);
        assert_eq!(
            parse_launchd(false, "Could not find service").reason,
            Some("not-loaded")
        );
    }

    #[test]
    fn jobs_are_named_by_their_labels() {
        assert_eq!(Os::name(CORE), "dev.sidevoice.core");
    }
}
