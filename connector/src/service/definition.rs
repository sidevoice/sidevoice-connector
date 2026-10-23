//! The two jobs, as data any manager turns into its own definition (`platform::service`): what each runs, with which
//! environment, where its output goes and when it comes back.
//!
//! | job | runs | restart | output |
//! |---|---|---|---|
//! | core `dev.sidevoice.core` / `sidevoice-core` | `R/current/core/bin/sidevoice-core-rust` with fixed arguments and `idle-exit 0` | on failure | `D/core.stderr.log` |
//! | connector `dev.sidevoice.connector` / `sidevoice-connector` | `install.json`'s `command` + `connector` | always | `D/connector.log` |
//!
//! Both name paths through `R/current`, so a definition is rewritten only when its text would change. Neither job
//! starts, signals or adopts the other: the connector links to the core when it answers, and the core's exit status
//! tells the manager whether to restart it (0 after a failed start: not again; 75 while another core holds its
//! directory: later; a crash after ready: again).

use super::jobs::Job;
use super::layout::Layout;
use super::{Failure, Result};
use crate::platform::service::{Job as Definition, Refusal, Restart, Service, Services};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;

/// The subcommand that runs a job from its definition file, for a manager that cannot set a job's environment and
/// output itself (`main.rs`, `platform::service::Service::run_job`).
pub const RUN_JOB: &str = "run-job";

/// What each job runs: the core through `R/current`, the connector as `install.json`'s `command` says.
pub struct Programs {
    pub core: Vec<String>,
    pub connector: Vec<String>,
}

/// The core's arguments: the job's (`idle_exit` 0, no launch id: the core makes one) or an on-demand launch's.
pub fn core_arguments(
    layout: &Layout,
    launch_id: Option<&str>,
    idle_exit: Option<u32>,
) -> Vec<String> {
    let path = |path: &Path| path.to_string_lossy().into_owned();
    let mut args = vec![
        "--data-dir".into(),
        path(&layout.core_data()),
        "--ready-file".into(),
        path(&layout.core_ready()),
        "--host".into(),
        layout
            .settings
            .get("SIDEVOICE_CORE_HOST")
            .filter(|host| !host.is_empty())
            .cloned()
            .unwrap_or_else(|| "0.0.0.0".into()),
        "--port".into(),
        layout.core_port(),
    ];
    if let Some(launch_id) = launch_id {
        args.extend(["--launch-id".into(), launch_id.to_owned()]);
    }
    args.extend(["--log-file".into(), path(&layout.core_log())]);
    if let Some(idle_exit) = idle_exit {
        args.extend(["--idle-exit".into(), idle_exit.to_string()]);
    }
    args
}

pub fn programs(layout: &Layout) -> Result<Programs> {
    let mut connector = layout
        .connector_command()
        .ok_or_else(|| Failure::keyed("service.no-installation", json!({})))?;
    connector.push("connector".into());
    let mut core = vec![layout.core_program().to_string_lossy().into_owned()];
    core.extend(core_arguments(layout, None, Some(0)));
    Ok(Programs { core, connector })
}

/// The environment both jobs start with: the installation's `SIDEVOICE_*` settings, where it lives, the agents'
/// directories the installing environment named, and which manager runs it.
pub fn environment(layout: &Layout) -> BTreeMap<String, String> {
    let path = |path: &Path| path.to_string_lossy().into_owned();
    let mut environment = layout.settings.clone();
    environment.insert("HOME".into(), path(&layout.home));
    environment.insert("SIDEVOICE_DATA_DIR".into(), path(&layout.data));
    environment.insert("XDG_DATA_HOME".into(), path(&layout.data_home));
    environment.insert("XDG_CONFIG_HOME".into(), path(&layout.config_home));
    for (name, dir) in &layout.agent_dirs {
        environment.insert(name.clone(), path(dir));
    }
    environment.insert("SIDEVOICE_SERVICE".into(), Services::KIND.into());
    environment
}

/// The core's environment: the jobs', with no loader variable, and its data directory.
pub fn core_environment(
    layout: &Layout,
    base: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut environment = crate::core_package::environment(base);
    environment.insert(
        "SIDEVOICE_CORE_DATA_DIR".into(),
        layout.core_data().to_string_lossy().into_owned(),
    );
    environment
}

/// Both jobs' definitions for the selected installation, every value checked.
pub fn definitions(layout: &Layout) -> Result<Vec<(Job, Definition)>> {
    let programs = programs(layout)?;
    let mut runner = layout
        .connector_command()
        .ok_or_else(|| Failure::keyed("service.no-installation", json!({})))?;
    runner.push(RUN_JOB.into());
    let environment = environment(layout);
    let definitions = vec![
        (
            Job::Core,
            Definition {
                id: Job::Core.id(),
                description: "Sidevoice core (this machine's conversations and voice)",
                program: programs.core,
                environment: core_environment(layout, &environment),
                unset: layout.agent_dirs_unset(),
                restart: Restart::OnFailure,
                log: layout.core_output(),
                runner: runner.clone(),
            },
        ),
        (
            Job::Connector,
            Definition {
                id: Job::Connector.id(),
                description: "Sidevoice connector (the harnesses' link to the core)",
                program: programs.connector,
                environment,
                unset: layout.agent_dirs_unset(),
                restart: Restart::Always,
                log: layout.connector_log(),
                runner,
            },
        ),
    ];
    for (_, definition) in &definitions {
        if let Err(Refusal::Unsafe(what)) = definition.check() {
            return Err(Failure::keyed(
                "service.unsafe-value",
                json!({"what": what}),
            ));
        }
    }
    Ok(definitions)
}

/// Why a defined job's program cannot run, if it cannot: `executable-missing` or `permission-denied` (not a file, or
/// not executable as `which` judges it on this OS).
pub fn program_problem(program: Option<&[String]>) -> Option<&'static str> {
    let file = Path::new(program.and_then(<[String]>::first)?);
    let metadata = match std::fs::metadata(file) {
        Ok(metadata) => metadata,
        Err(_) => return Some("executable-missing"),
    };
    if !metadata.is_file() || which::which(file).is_err() {
        return Some("permission-denied");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn the_core_environment_drops_loader_variables_and_names_its_data() {
        let layout = Layout::from_vars(
            |name| (name == "HOME").then(|| "/home/u".into()),
            Vec::new(),
            &[],
        )
        .unwrap();
        let base = BTreeMap::from([
            ("LD_PRELOAD".to_owned(), "/evil.so".to_owned()),
            ("HOME".to_owned(), "/home/u".to_owned()),
        ]);
        let core = core_environment(&layout, &base);
        assert!(!core.contains_key("LD_PRELOAD"));
        assert_eq!(core["SIDEVOICE_CORE_DATA_DIR"], "/home/u/.sidevoice/core");
        let args = core_arguments(&layout, None, Some(0));
        assert_eq!(&args[args.len() - 2..], ["--idle-exit", "0"]);
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--ready-file", "/home/u/.sidevoice/core/core.json"]));
        assert!(args.windows(2).any(|pair| pair == ["--host", "0.0.0.0"]));
        assert!(!args.contains(&"--launch-id".to_owned()));
    }

    #[test]
    fn explicitly_configured_core_host_reaches_service_and_on_demand_launches() {
        let layout = Layout::from_vars(
            |name| (name == "HOME").then(|| "/home/u".into()),
            [("SIDEVOICE_CORE_HOST".into(), "0.0.0.0".into())],
            &[],
        )
        .unwrap();
        for args in [
            core_arguments(&layout, None, Some(0)),
            core_arguments(&layout, Some("launch"), Some(600)),
        ] {
            assert!(args.windows(2).any(|pair| pair == ["--host", "0.0.0.0"]));
        }
        let empty = Layout::from_vars(
            |name| (name == "HOME").then(|| "/home/u".into()),
            [("SIDEVOICE_CORE_HOST".into(), "".into())],
            &[],
        )
        .unwrap();
        assert!(core_arguments(&empty, None, Some(0))
            .windows(2)
            .any(|pair| pair == ["--host", "0.0.0.0"]));
        let loopback = Layout::from_vars(
            |name| (name == "HOME").then(|| "/home/u".into()),
            [("SIDEVOICE_CORE_HOST".into(), "127.0.0.1".into())],
            &[],
        )
        .unwrap();
        assert!(core_arguments(&loopback, None, Some(0))
            .windows(2)
            .any(|pair| pair == ["--host", "127.0.0.1"]));
    }

    /// An agent home the installing environment leaves unset is unset in the job too, whatever the user's manager
    /// carries: the agent then uses its own default there, as it does for the person.
    #[test]
    fn a_job_does_not_inherit_an_agent_home_its_profile_leaves_unset() {
        let unset = Layout::from_vars(
            |name| (name == "HOME").then(|| "/home/u".into()),
            Vec::new(),
            &["ALPHA_HOME", "BETA_HOME"],
        )
        .unwrap();
        assert_eq!(unset.agent_dirs_unset(), ["ALPHA_HOME", "BETA_HOME"]);
        assert!(!environment(&unset).contains_key("ALPHA_HOME"));
        assert_eq!(environment(&unset)["SIDEVOICE_SERVICE"], Services::KIND);

        let named = Layout::from_vars(
            |name| match name {
                "HOME" => Some("/home/u".into()),
                "ALPHA_HOME" => Some("/home/u/.alpha".into()),
                _ => None,
            },
            Vec::new(),
            &["ALPHA_HOME", "BETA_HOME"],
        )
        .unwrap();
        assert_eq!(named.agent_dirs_unset(), ["BETA_HOME"]);
        assert_eq!(environment(&named)["ALPHA_HOME"], "/home/u/.alpha");
    }

    #[test]
    fn a_program_that_cannot_run_says_why() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            program_problem(Some(&words(&["/no/such/program"]))),
            Some("executable-missing")
        );
        assert_eq!(
            program_problem(Some(&words(&[dir.path().to_str().unwrap()]))),
            Some("permission-denied")
        );
        let exe = std::env::current_exe().unwrap();
        assert_eq!(
            program_problem(Some(&words(&[exe.to_str().unwrap()]))),
            None
        );
        assert_eq!(program_problem(None), None);
    }
}
