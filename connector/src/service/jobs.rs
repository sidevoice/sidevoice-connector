//! Which service manager Sidevoice uses here (`platform::service`, or none: Sidevoice on demand), and what is the
//! same over both jobs whatever the manager: which are defined, whether Sidevoice as a whole starts at login, and
//! the manager's refusals as the service's keyed failures.

use super::definition;
use super::layout::Layout;
use super::{Failure, Result};
use crate::platform::service::{JobId, Linger, Refusal, Service, Services};
use serde_json::{json, Value};

/// A manager runs the jobs, or none does (Sidevoice on demand).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Manager,
    None,
}

impl Kind {
    /// The manager's name (`launchd`, `systemd`, `task-scheduler`), or `none`.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Manager => Services::KIND,
            Kind::None => "none",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Job {
    Core,
    Connector,
}

impl Job {
    /// Both jobs, in the order they are started (stopped in reverse).
    pub const ALL: [Job; 2] = [Job::Core, Job::Connector];

    /// The names the app and older installs know.
    pub fn id(self) -> JobId {
        match self {
            Job::Core => JobId {
                label: "dev.sidevoice.core",
                name: "sidevoice-core",
            },
            Job::Connector => JobId {
                label: "dev.sidevoice.connector",
                name: "sidevoice-connector",
            },
        }
    }
}

fn ids(jobs: &[Job]) -> Vec<JobId> {
    jobs.iter().map(|job| job.id()).collect()
}

/// What the platform would offer, without asking it: none when `SIDEVOICE_SERVICE_MANAGER` says `none`, else this
/// OS's manager.
pub fn platform_kind() -> Kind {
    match std::env::var("SIDEVOICE_SERVICE_MANAGER").as_deref() {
        Ok("none") => Kind::None,
        _ => Kind::Manager,
    }
}

/// The manager this user has now: unless `SIDEVOICE_SERVICE_MANAGER` names one, only when it answers (Linux: a
/// systemd user manager).
pub async fn kind() -> Kind {
    let kind = platform_kind();
    if kind == Kind::Manager
        && std::env::var_os("SIDEVOICE_SERVICE_MANAGER").is_none()
        && !Services::answers().await
    {
        return Kind::None;
    }
    kind
}

/// This user's manager, keeping its definitions where this installation's environment says.
pub fn manager(layout: &Layout) -> Services {
    Services::new(layout.places())
}

/// The jobs defined on this machine, found by their definitions alone.
#[derive(Clone, Debug)]
pub struct Installed {
    pub core: bool,
    pub connector: bool,
}

impl Installed {
    pub fn has(&self, job: Job) -> bool {
        match job {
            Job::Core => self.core,
            Job::Connector => self.connector,
        }
    }
}

/// The jobs defined here; none where no manager is used.
pub async fn installed(layout: &Layout) -> Option<Installed> {
    if platform_kind() == Kind::None {
        return None;
    }
    let manager = manager(layout);
    let found = Installed {
        core: manager.defined(Job::Core.id()).await,
        connector: manager.defined(Job::Connector.id()).await,
    };
    (found.core || found.connector).then_some(found)
}

/// The jobs whose definitions are here.
fn defined(installed: &Installed) -> Vec<Job> {
    Job::ALL
        .into_iter()
        .filter(|&job| installed.has(job))
        .collect()
}

/// The jobs whose definitions are missing: an incomplete service, which `service install` repairs.
pub fn missing(installed: &Installed) -> Vec<Job> {
    Job::ALL
        .into_iter()
        .filter(|&job| !installed.has(job))
        .collect()
}

pub fn names(jobs: &[Job]) -> String {
    jobs.iter()
        .map(|job| Services::name(job.id()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A manager's refusal as the service says it, with its stable key. A start the manager refused is said by why, when
/// that can be told: the job's start limit, or a program that cannot run.
pub async fn failure(manager: &impl Service, refusal: Refusal) -> Failure {
    let (key, params) = match refusal {
        Refusal::Unsafe(what) => ("service.unsafe-value", json!({"what": what})),
        Refusal::Definition(detail) => ("service.definition-unsafe", json!({"detail": detail})),
        Refusal::Io(detail) => return Failure::plain(detail),
        Refusal::NotEnabled(detail) => ("service.not-enabled", json!({"detail": detail})),
        Refusal::NotLoaded(detail) => ("service.not-loaded", json!({"detail": detail})),
        Refusal::LoginUnknown(detail) => ("service.login-unknown", json!({"detail": detail})),
        Refusal::NotStarted(job, detail) => {
            let reason = manager.status(job).await.reason;
            let program = manager.program(job).await;
            let key = match reason.or_else(|| definition::program_problem(program.as_deref())) {
                Some("start-limit") => "service.start-limit",
                Some("executable-missing") => "service.executable-missing",
                Some("permission-denied") => "service.permission-denied",
                _ => "service.not-loaded",
            };
            (key, json!({"detail": detail}))
        }
    };
    Failure::keyed(key, params)
}

/// Each of `jobs` started from its definition as it is now ([`Service::start`]): a changed one loaded again, with
/// `restart` a running one restarted, with `login` each enabled for the next login.
pub async fn start(
    layout: &Layout,
    manager: &impl Service,
    changed: &[Job],
    restart: bool,
    login: bool,
    jobs: &[Job],
) -> Result<()> {
    if jobs.contains(&Job::Core) && (restart || changed.contains(&Job::Core)) {
        // A core about to start again: its last failure report is about a start that is over.
        let _ = std::fs::remove_file(layout.core_failure());
    }
    match manager
        .start(&ids(jobs), &ids(changed), restart, login)
        .await
    {
        Ok(()) => Ok(()),
        Err(refusal) => Err(failure(manager, refusal).await),
    }
}

/// The defined jobs stopped, the connector first ([`Service::stop`]); with `disable`, not at the next login either.
/// The first refusal, or none.
pub async fn unload(
    manager: &impl Service,
    installed: &Installed,
    disable: bool,
) -> Option<String> {
    for job in Job::ALL.iter().rev().copied() {
        if !installed.has(job) {
            continue;
        }
        if let Err(refused) = manager.stop(job.id(), disable).await {
            return Some(refused);
        }
    }
    None
}

/// After the definitions are deleted: the manager forgets both jobs.
pub async fn forget(manager: &impl Service) -> Result<()> {
    manager
        .forget(&ids(&Job::ALL))
        .await
        .map_err(|detail| Failure::keyed("service.unload-failed", json!({"detail": detail})))
}

/// Sidevoice's start at login from its jobs' answers: off when any job is off (Sidevoice would not come up whole), on
/// when every job is on, and otherwise (a job its manager cannot tell about, or no job) not known.
pub fn combine_login(jobs: impl IntoIterator<Item = Option<bool>>) -> Option<bool> {
    let jobs: Vec<Option<bool>> = jobs.into_iter().collect();
    if jobs.contains(&Some(false)) {
        Some(false)
    } else if jobs.is_empty() || jobs.contains(&None) {
        None
    } else {
        Some(true)
    }
}

async fn login_of(manager: &impl Service, jobs: &[Job]) -> Option<bool> {
    let mut each = Vec::new();
    for &job in jobs {
        each.push(manager.login(job.id()).await);
    }
    combine_login(each)
}

/// Whether both jobs start at the next login, as their manager says now; `None` when it cannot tell, or a job is not
/// defined (with a job missing, whether Sidevoice comes up at login is not that of either job).
pub async fn start_at_login(manager: &impl Service, installed: &Installed) -> Option<bool> {
    if !missing(installed).is_empty() {
        return None;
    }
    login_of(manager, &Job::ALL).await
}

/// Whether a start enables its jobs for the next login: on a new service (no job was defined), and otherwise only
/// when the jobs that were defined all start at login now. Off, or not known (launchd's override unreadable, systemd
/// `enabled-runtime`, …): left as it is, never turned on.
pub fn enable_on_start(new: bool, defined_now: Option<bool>) -> bool {
    new || defined_now == Some(true)
}

/// [`enable_on_start`] for the jobs defined here (`None`: none was).
pub async fn keeps_login(manager: &impl Service, installed: Option<&Installed>) -> bool {
    match installed {
        None => enable_on_start(true, None),
        Some(found) => enable_on_start(false, login_of(manager, &defined(found)).await),
    }
}

/// The jobs defined here set to start at the next login or not, and nothing else ([`Service::set_login`]): a
/// running job keeps running, a stopped one stays stopped. Each change is read back. The first job refused or not
/// confirmed, said with its name.
pub async fn set_start_at_login(
    manager: &impl Service,
    installed: &Installed,
    on: bool,
) -> std::result::Result<(), String> {
    let verb = if on { "enable" } else { "disable" };
    for job in defined(installed) {
        let name = Services::name(job.id());
        manager
            .set_login(job.id(), on)
            .await
            .map_err(|detail| format!("{name}: {detail}"))?;
        let now = manager.login(job.id()).await;
        if now != Some(on) {
            let now = now.map_or("unknown", |now| if now { "on" } else { "off" });
            return Err(format!("{name}: {verb}: not confirmed (now {now})"));
        }
    }
    Ok(())
}

/// Where the manager asks for it (Linux): whether this user's services run without a session, the command that
/// would make them (never run), and why.
pub async fn linger(manager: &impl Service) -> Option<Value> {
    let Linger { enabled, command } = manager.linger().await?;
    Some(json!({"enabled": enabled, "command": command,
        "reason": crate::messages::message("service.linger-reason", &Value::Null)}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::service::{Job as Definition, JobState, Places};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// A manager that answers whether each job starts at login from `login`, and changes it when asked unless the
    /// job is in `refuse` (refused) or `stuck` (accepted, not changed).
    #[derive(Default)]
    struct Fake {
        login: Mutex<BTreeMap<&'static str, Option<bool>>>,
        refuse: Vec<&'static str>,
        stuck: Vec<&'static str>,
        calls: Mutex<Vec<String>>,
    }

    impl Fake {
        fn with(login: Option<bool>) -> Fake {
            let fake = Fake::default();
            for job in Job::ALL {
                fake.login.lock().unwrap().insert(job.id().name, login);
            }
            fake
        }
    }

    impl Service for Fake {
        const KIND: &'static str = "fake";
        fn new(_: Places) -> Fake {
            Fake::default()
        }
        fn name(job: JobId) -> String {
            job.name.to_owned()
        }
        async fn file(&self, job: JobId) -> PathBuf {
            PathBuf::from(job.name)
        }
        fn definition(_: &Definition) -> String {
            String::new()
        }
        fn program_of(_: &str) -> Option<Vec<String>> {
            None
        }
        async fn start(
            &self,
            _: &[JobId],
            _: &[JobId],
            _: bool,
            _: bool,
        ) -> std::result::Result<(), Refusal> {
            Ok(())
        }
        async fn stop(&self, job: JobId, _: bool) -> std::result::Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("stop {}", job.name));
            Ok(())
        }
        async fn forget(&self, _: &[JobId]) -> std::result::Result<(), String> {
            Ok(())
        }
        async fn status(&self, _: JobId) -> JobState {
            JobState::default()
        }
        async fn login(&self, job: JobId) -> Option<bool> {
            self.login.lock().unwrap().get(job.name).copied().flatten()
        }
        async fn set_login(&self, job: JobId, on: bool) -> std::result::Result<(), String> {
            let verb = if on { "enable" } else { "disable" };
            self.calls
                .lock()
                .unwrap()
                .push(format!("{verb} {}", job.name));
            if self.refuse.contains(&job.name) {
                return Err(format!("{verb}: Access denied"));
            }
            if !self.stuck.contains(&job.name) {
                self.login.lock().unwrap().insert(job.name, Some(on));
            }
            Ok(())
        }
    }

    fn service(core: bool, connector: bool) -> Installed {
        Installed { core, connector }
    }

    #[tokio::test]
    async fn a_start_turns_login_on_only_for_a_new_service_or_one_that_has_it_on() {
        // install, upgrade (`define_and_start`), start and restart all decide through this.
        assert!(enable_on_start(true, None), "a new service starts at login");
        assert!(enable_on_start(false, Some(true)));
        assert!(
            !enable_on_start(false, Some(false)),
            "the person's off holds"
        );
        assert!(
            !enable_on_start(false, None),
            "not known is never turned on"
        );

        let both = service(true, true);
        assert!(keeps_login(&Fake::with(Some(true)), Some(&both)).await);
        assert!(keeps_login(&Fake::with(None), None).await);
        // Enabled for this boot only, or a manager that does not answer: left as it is.
        assert!(!keeps_login(&Fake::with(None), Some(&both)).await);
        assert!(!keeps_login(&Fake::with(Some(false)), Some(&both)).await);
    }

    #[tokio::test]
    async fn start_at_login_is_read_from_each_job_never_guessed() {
        let fake = Fake::with(Some(true));
        assert_eq!(login_of(&fake, &Job::ALL).await, Some(true));
        fake.login
            .lock()
            .unwrap()
            .insert(Job::Connector.id().name, Some(false));
        assert_eq!(
            login_of(&fake, &Job::ALL).await,
            Some(false),
            "Sidevoice would not come up whole"
        );
        assert_eq!(login_of(&Fake::with(None), &[Job::Core]).await, None);

        assert_eq!(combine_login([]), None, "no job");
        assert_eq!(combine_login([Some(true), None]), None);
        assert_eq!(combine_login([None, Some(false)]), Some(false));
    }

    #[tokio::test]
    async fn an_incomplete_service_never_reports_start_at_login() {
        let fake = Fake::with(Some(true));
        for (core, connector, expected) in [
            (true, true, Some(true)),
            (true, false, None),
            (false, true, None),
            (false, false, None),
        ] {
            let login = start_at_login(&fake, &service(core, connector)).await;
            assert_eq!(login, expected, "core {core}, connector {connector}");
        }
        assert_eq!(missing(&service(true, true)), vec![]);
        assert_eq!(missing(&service(true, false)), vec![Job::Connector]);
        assert_eq!(missing(&service(false, false)), Job::ALL.to_vec());
    }

    #[tokio::test]
    async fn a_login_change_only_changes_login_and_is_read_back() {
        let fake = Fake::with(Some(true));
        assert_eq!(
            set_start_at_login(&fake, &service(true, true), false).await,
            Ok(())
        );
        assert_eq!(
            *fake.calls.lock().unwrap(),
            vec!["disable sidevoice-core", "disable sidevoice-connector"],
            "nothing stopped"
        );
        assert_eq!(login_of(&fake, &Job::ALL).await, Some(false));
    }

    #[tokio::test]
    async fn a_login_change_refused_or_not_read_back_is_a_refusal() {
        let both = service(true, true);
        let refusing = Fake {
            refuse: vec!["sidevoice-core"],
            ..Fake::with(Some(true))
        };
        let name = Services::name(Job::Core.id());
        assert_eq!(
            set_start_at_login(&refusing, &both, false).await,
            Err(format!("{name}: disable: Access denied"))
        );

        // The manager accepted it and still says the job starts at login.
        let stuck = Fake {
            stuck: vec!["sidevoice-core"],
            ..Fake::with(Some(true))
        };
        assert_eq!(
            set_start_at_login(&stuck, &both, false).await,
            Err(format!("{name}: disable: not confirmed (now on)"))
        );

        // A manager that cannot be read back is not taken to have done it.
        let silent = Fake {
            stuck: vec!["sidevoice-core"],
            ..Fake::with(None)
        };
        assert_eq!(
            set_start_at_login(&silent, &both, true).await,
            Err(format!("{name}: enable: not confirmed (now unknown)"))
        );
    }

    #[tokio::test]
    async fn the_connector_is_stopped_before_the_core_and_only_what_is_defined() {
        let fake = Fake::default();
        assert_eq!(unload(&fake, &service(true, true), false).await, None);
        assert_eq!(
            *fake.calls.lock().unwrap(),
            vec!["stop sidevoice-connector", "stop sidevoice-core"]
        );
        let fake = Fake::default();
        unload(&fake, &service(true, false), true).await;
        assert_eq!(*fake.calls.lock().unwrap(), vec!["stop sidevoice-core"]);
    }

    #[tokio::test]
    async fn refusals_keep_their_keys() {
        let fake = Fake::default();
        for (refusal, key) in [
            (Refusal::Unsafe("HOME".into()), "service.unsafe-value"),
            (
                Refusal::Definition("/x".into()),
                "service.definition-unsafe",
            ),
            (Refusal::Io("disk full".into()), "service.failed"),
            (Refusal::NotEnabled("x".into()), "service.not-enabled"),
            (Refusal::NotLoaded("x".into()), "service.not-loaded"),
            (Refusal::LoginUnknown("x".into()), "service.login-unknown"),
            (
                Refusal::NotStarted(Job::Core.id(), "x".into()),
                "service.not-loaded",
            ),
        ] {
            assert_eq!(failure(&fake, refusal).await.key, key);
        }
    }

    #[test]
    fn job_names_are_the_ones_the_app_and_older_installs_know() {
        assert_eq!(Job::Core.id().label, "dev.sidevoice.core");
        assert_eq!(Job::Connector.id().label, "dev.sidevoice.connector");
        assert_eq!(Job::Core.id().name, "sidevoice-core");
        assert_eq!(Job::Connector.id().name, "sidevoice-connector");
    }
}
