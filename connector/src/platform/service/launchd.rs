//! launchd's steps that need no macOS API, with `launchctl` (a runner of its arguments) and the files passed in, so
//! that they are tested on every Unix: whether launchd has a job, a job waited out after `bootout`, and the legacy jobs
//! removed.
//!
//! Legacy jobs: Sidevoice before 0.19 labelled its LaunchAgents `dev.sidevoice.core` and `dev.sidevoice.connector`;
//! 0.19 renamed them `ai.sidevoice.*` (sidevoice/sidevoice-connector#324) and removed nothing. A Mac updated from an
//! earlier release kept the old jobs loaded beside the new ones, and the old core held the core's port, so the new
//! one failed (`bind.port-in-use`). The labels are the service layer's (`service::jobs::Job::legacy_id`); this is how
//! launchd finds such a job and removes it, only when its definition is exactly what Sidevoice wrote; what it
//! runs is judged by the caller (`service::legacy`).

use super::{existing, Legacy, LegacyJob, LegacyRefusal, Output, Recorded, Refusal, Restart};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use tokio::time::{sleep, Duration, Instant};

/// Whether launchd has a job, as it answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Presence {
    Loaded,
    Absent,
    Unknown(String),
}

/// `launchctl print <target>` read: absent only when launchd says so ("Could not find service", 113).
pub fn presence_of(printed: &Output) -> Presence {
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

/// Whether launchd has the job `target` (`gui/<uid>/<label>`).
pub async fn presence_with<F, Fut>(run: &F, target: &str) -> Presence
where
    F: Fn(Vec<String>) -> Fut,
    Fut: Future<Output = Output>,
{
    presence_of(&run(vec!["print".into(), target.into()]).await)
}

/// `launchctl bootout` returns before launchd has let the job go, and a `bootstrap` in that interval fails: the job
/// is waited out, up to `limit`, until launchd says it is gone (which a manager that cannot be asked never says).
pub async fn boot_out_with<F, Fut>(run: &F, target: &str, limit: Duration) -> Result<(), String>
where
    F: Fn(Vec<String>) -> Fut,
    Fut: Future<Output = Output>,
{
    let out = run(vec!["bootout".into(), target.into()]).await;
    let deadline = Instant::now() + limit;
    loop {
        match presence_with(run, target).await {
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

/// The keys every LaunchAgent Sidevoice wrote has (0.7.0 through 0.18.x wrote these, and only these).
const WRITTEN_KEYS: [&str; 8] = [
    "Label",
    "ProgramArguments",
    "EnvironmentVariables",
    "RunAtLoad",
    "KeepAlive",
    "ThrottleInterval",
    "StandardOutPath",
    "StandardErrorPath",
];

/// A legacy definition read as a LaunchAgent Sidevoice wrote: exactly [`WRITTEN_KEYS`], labelled `label`, started at
/// load, at most every 10 s, kept alive as `restart` says (the core after a crash or a non-zero exit, the connector
/// always), its output and errors in one file, its program and environment plain strings. What it records, or why it
/// is not one.
fn written(bytes: &[u8], label: &str, restart: Restart) -> Result<Recorded, String> {
    let agent: plist::Dictionary =
        plist::from_bytes(bytes).map_err(|_| "its definition is not a property list".to_owned())?;
    let keys: BTreeSet<&str> = agent.keys().map(String::as_str).collect();
    if keys != BTreeSet::from(WRITTEN_KEYS) {
        let keys: Vec<&str> = keys.into_iter().collect();
        return Err(format!(
            "its definition has the keys {}, not those Sidevoice wrote",
            keys.join(", ")
        ));
    }
    if agent.get("Label").and_then(plist::Value::as_string) != Some(label) {
        return Err("its definition names another label".into());
    }
    if agent.get("RunAtLoad").and_then(plist::Value::as_boolean) != Some(true)
        || agent
            .get("ThrottleInterval")
            .and_then(plist::Value::as_signed_integer)
            != Some(10)
    {
        return Err("its definition is not started as Sidevoice started its jobs".into());
    }
    let keep_alive = agent.get("KeepAlive");
    let kept = match restart {
        Restart::OnFailure => keep_alive
            .and_then(plist::Value::as_dictionary)
            .is_some_and(|keep| {
                keep.len() == 2
                    && keep
                        .get("SuccessfulExit")
                        .and_then(plist::Value::as_boolean)
                        == Some(false)
                    && keep.get("Crashed").and_then(plist::Value::as_boolean) == Some(true)
            }),
        Restart::Always => keep_alive.and_then(plist::Value::as_boolean) == Some(true),
    };
    if !kept {
        return Err("its definition is not kept alive as Sidevoice kept this job".into());
    }
    let out = agent
        .get("StandardOutPath")
        .and_then(plist::Value::as_string);
    if out.is_none()
        || out
            != agent
                .get("StandardErrorPath")
                .and_then(plist::Value::as_string)
    {
        return Err("its definition does not send its output where Sidevoice did".into());
    }
    let strings = |value: &plist::Value| -> Option<Vec<String>> {
        value
            .as_array()?
            .iter()
            .map(|word| word.as_string().map(str::to_owned))
            .collect()
    };
    let program = agent
        .get("ProgramArguments")
        .and_then(strings)
        .filter(|program| !program.is_empty())
        .ok_or_else(|| "its definition's program is not a list of words".to_owned())?;
    let environment = agent
        .get("EnvironmentVariables")
        .and_then(plist::Value::as_dictionary)
        .and_then(|variables| {
            variables
                .iter()
                .map(|(name, value)| Some((name.clone(), value.as_string()?.to_owned())))
                .collect::<Option<BTreeMap<String, String>>>()
        })
        .ok_or_else(|| "its definition's environment is not plain text".to_owned())?;
    Ok(Recorded {
        program,
        environment,
        log: out.map(PathBuf::from),
    })
}

/// The lines of a block of `launchctl print` (`\t<name> = {` … `\t}`), trimmed.
fn printed_block<'a>(printed: &'a str, name: &str) -> Vec<&'a str> {
    let open = format!("{name} = {{");
    let mut lines = printed.lines();
    if !lines.any(|line| line.trim() == open) {
        return Vec::new();
    }
    lines
        .map(str::trim)
        .take_while(|line| *line != "}")
        .filter(|line| !line.is_empty())
        .collect()
}

/// What a loaded job runs, as `launchctl print` says: `arguments`, `environment` (`NAME => value`) and `stdout path`.
fn printed_job(printed: &str) -> Recorded {
    let mut program: Vec<String> = printed_block(printed, "arguments")
        .into_iter()
        .map(str::to_owned)
        .collect();
    if program.is_empty() {
        if let Some(only) = field(printed, "program") {
            program.push(only);
        }
    }
    Recorded {
        program,
        environment: printed_block(printed, "environment")
            .into_iter()
            .filter_map(|line| {
                let (name, value) = line.split_once(" => ")?;
                Some((name.trim().to_owned(), value.trim().to_owned()))
            })
            .collect(),
        log: field(printed, "stdout path").map(PathBuf::from),
    }
}

/// A `<name> = <value>` line of `launchctl print`.
fn field(printed: &str, name: &str) -> Option<String> {
    let prefix = format!("{name} = ");
    printed.lines().find_map(|line| {
        line.trim()
            .strip_prefix(&prefix)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty() && value != "{")
    })
}

/// `path` as one word of a POSIX shell.
fn quoted(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

/// Each `legacy` job in `domain` (`gui/<uid>`), whose definition would be in `agents` (`~/Library/LaunchAgents`),
/// removed when it is ours ([`super::Service::retire_legacy`]): with a definition, only when it is this user's plain
/// file that reads as one Sidevoice wrote ([`written`]) and runs what [`LegacyJob::ours`] recognises; then unloaded
/// (when loaded, or when launchd cannot say) and the definition deleted. With no definition, a loaded job is unloaded
/// when what launchd says it runs is recognised. Anything else under the label is refused, untouched. `limit` bounds
/// each wait for an unload.
pub async fn retire_with<F, Fut>(
    run: &F,
    domain: &str,
    agents: &Path,
    legacy: &[LegacyJob<'_>],
    limit: Duration,
) -> Result<Vec<Legacy>, LegacyRefusal>
where
    F: Fn(Vec<String>) -> Fut,
    Fut: Future<Output = Output>,
{
    let mut done = Vec::new();
    for job in legacy {
        let label = job.id.label;
        let target = format!("{domain}/{label}");
        let plist: PathBuf = agents.join(format!("{label}.plist"));
        let refused = |recognised: bool, detail: String| LegacyRefusal {
            label,
            plist: plist.clone(),
            recognised,
            detail,
            bootout: format!("launchctl bootout {target}"),
            remove: format!("rm -f {}", quoted(&plist)),
        };
        let printed = run(vec!["print".into(), target.clone()]).await;
        let presence = presence_of(&printed);
        let defined = match existing(&plist) {
            Ok(false) => None,
            Ok(true) => Some(
                std::fs::read(&plist)
                    .map_err(|error| format!("its definition cannot be read: {error}"))
                    .and_then(|bytes| written(&bytes, label, job.restart)),
            ),
            Err(Refusal::Definition(_)) => Some(Err(
                "its definition is not this user's plain file".to_owned(),
            )),
            Err(refusal) => Some(Err(format!("its definition cannot be read: {refusal:?}"))),
        };
        let recorded = match (defined, &presence) {
            (Some(recorded), _) => recorded,
            (None, Presence::Absent | Presence::Unknown(_)) => continue,
            (None, Presence::Loaded) => Ok(printed_job(&printed.output)),
        };
        if let Err(detail) = recorded.and_then(|recorded| (job.ours)(&recorded)) {
            return Err(refused(false, detail));
        }
        let unloaded = presence != Presence::Absent;
        if unloaded {
            boot_out_with(run, &target, limit)
                .await
                .map_err(|detail| refused(true, detail))?;
        }
        let deleted = match std::fs::remove_file(&plist) {
            Ok(()) => Some(plist.clone()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(refused(
                    true,
                    format!("deleting {}: {error}", plist.display()),
                ))
            }
        };
        done.push(Legacy {
            label,
            unloaded,
            deleted,
        });
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::super::JobId;
    use super::*;
    use std::sync::{Arc, Mutex};

    const DOMAIN: &str = "gui/501";
    const CORE: JobId = JobId {
        label: "dev.sidevoice.core",
        name: "sidevoice-core",
    };
    const CONNECTOR: JobId = JobId {
        label: "dev.sidevoice.connector",
        name: "sidevoice-connector",
    };
    const CORE_PROGRAM: &str = "/d/install/releases/0.18.0/core/bin/sidevoice-core";
    const CONNECTOR_PROGRAM: &str = "/d/install/releases/0.18.0/bin/sidevoice-connector";

    /// What the caller recognises in these tests: the program 0.18.0 wrote for the job.
    fn ours(expected: &'static str) -> impl Fn(&Recorded) -> Result<(), String> + Sync {
        move |recorded: &Recorded| {
            if recorded.program.first().map(String::as_str) == Some(expected) {
                Ok(())
            } else {
                Err(format!("it runs {:?}", recorded.program.first()))
            }
        }
    }

    /// A launchd that has the labels in `loaded`: `print` answers for them as launchd does (running `program`),
    /// `bootout` unloads them unless `stuck`, and every call is logged.
    #[derive(Clone, Default)]
    struct Launchd {
        loaded: Arc<Mutex<BTreeSet<String>>>,
        program: String,
        stuck: bool,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl Launchd {
        fn with(loaded: &[&str], program: &str) -> Launchd {
            Launchd {
                loaded: Arc::new(Mutex::new(
                    loaded
                        .iter()
                        .map(|label| format!("{DOMAIN}/{label}"))
                        .collect(),
                )),
                program: program.into(),
                ..Launchd::default()
            }
        }

        fn run(&self) -> impl Fn(Vec<String>) -> std::future::Ready<Output> + '_ {
            move |args: Vec<String>| {
                self.calls.lock().unwrap().push(args.join(" "));
                let mut loaded = self.loaded.lock().unwrap();
                let output = match (args[0].as_str(), args[1].as_str()) {
                    ("print", target) if loaded.contains(target) => Output {
                        ok: true,
                        code: Some(0),
                        output: format!(
                            "{target} = {{\n\tactive count = 1\n\tpath = (submitted by launchctl)\n\
                             \tstate = running\n\n\tprogram = {program}\n\targuments = {{\n\t\t{program}\n\
                             \t\t--data-dir\n\t\t/d/core\n\t}}\n\n\tstdout path = /d/core.stderr.log\n\
                             \tinherited environment = {{\n\t\tSSH_AUTH_SOCK => /x\n\t}}\n\n\tenvironment = {{\n\
                             \t\tSIDEVOICE_SERVICE => launchd\n\t\tSIDEVOICE_DATA_DIR => /d\n\t}}\n\n\tpid = 4242\n}}\n",
                            program = self.program
                        ),
                    },
                    ("print", target) => Output {
                        ok: false,
                        code: Some(113),
                        output: format!("Could not find service \"{target}\" in domain"),
                    },
                    ("bootout", _) if self.stuck => Output {
                        ok: false,
                        code: Some(5),
                        output: "Boot-out failed: 5: Input/output error".into(),
                    },
                    ("bootout", target) => {
                        loaded.remove(target);
                        Output {
                            ok: true,
                            code: Some(0),
                            output: String::new(),
                        }
                    }
                    _ => Output::default(),
                };
                std::future::ready(output)
            }
        }

        fn has(&self, label: &str) -> bool {
            self.loaded
                .lock()
                .unwrap()
                .contains(&format!("{DOMAIN}/{label}"))
        }

        fn booted_out(&self) -> bool {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call.starts_with("bootout"))
        }
    }

    /// A LaunchAgent as Sidevoice 0.16.0 through 0.18.x wrote it (the `plist` crate's layout), for `program`.
    fn written_by_0_18(label: &str, program: &[&str], keep_alive: &str) -> String {
        let arguments: String = program
            .iter()
            .map(|word| format!("\t\t<string>{word}</string>\n"))
            .collect();
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n\
             \t<key>Label</key>\n\t<string>{label}</string>\n\t<key>ProgramArguments</key>\n\t<array>\n{arguments}\t</array>\n\
             \t<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>HOME</key>\n\t\t<string>/Users/u</string>\n\
             \t\t<key>SIDEVOICE_DATA_DIR</key>\n\t\t<string>/d</string>\n\t\t<key>SIDEVOICE_SERVICE</key>\n\
             \t\t<string>launchd</string>\n\t</dict>\n\t<key>RunAtLoad</key>\n\t<true/>\n\t<key>KeepAlive</key>\n\
             \t{keep_alive}\n\t<key>ThrottleInterval</key>\n\t<integer>10</integer>\n\t<key>StandardOutPath</key>\n\
             \t<string>/d/core.stderr.log</string>\n\t<key>StandardErrorPath</key>\n\t<string>/d/core.stderr.log</string>\n\
             </dict>\n</plist>\n"
        )
    }

    const CORE_KEEP_ALIVE: &str =
        "<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t\t<key>Crashed</key>\n\t\t<true/>\n\t</dict>";

    /// A LaunchAgent as Sidevoice 0.7.0 through 0.15.x wrote it (its own two-space template).
    fn written_by_0_15(label: &str, program: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{program}</string>
    <string>--data-dir</string>
    <string>/Users/u/.sidevoice/core</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key>
    <string>/Users/u</string>
    <key>SIDEVOICE_DATA_DIR</key>
    <string>/Users/u/.sidevoice</string>
    <key>SIDEVOICE_SERVICE</key>
    <string>launchd</string>
    <key>XDG_DATA_HOME</key>
    <string>/Users/u/.local/share</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
    <key>Crashed</key>
    <true/>
  </dict>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>StandardOutPath</key>
  <string>/Users/u/.sidevoice/core.stderr.log</string>
  <key>StandardErrorPath</key>
  <string>/Users/u/.sidevoice/core.stderr.log</string>
</dict>
</plist>
"#
        )
    }

    struct Home {
        _scratch: tempfile::TempDir,
        agents: PathBuf,
    }

    impl Home {
        fn new() -> Home {
            let scratch = tempfile::tempdir().unwrap();
            let agents = scratch.path().join("Library/LaunchAgents");
            std::fs::create_dir_all(&agents).unwrap();
            Home {
                _scratch: scratch,
                agents,
            }
        }

        fn plist(&self, label: &str) -> PathBuf {
            self.agents.join(format!("{label}.plist"))
        }

        fn plant(&self, label: &str, text: &str) -> PathBuf {
            let file = self.plist(label);
            std::fs::write(&file, text).unwrap();
            file
        }

        fn plant_core(&self) -> PathBuf {
            self.plant(
                CORE.label,
                &written_by_0_18(
                    CORE.label,
                    &[CORE_PROGRAM, "--data-dir", "/d/core"],
                    CORE_KEEP_ALIVE,
                ),
            )
        }

        async fn retire(&self, launchd: &Launchd) -> Result<Vec<Legacy>, LegacyRefusal> {
            let (core, connector) = (ours(CORE_PROGRAM), ours(CONNECTOR_PROGRAM));
            retire_with(
                &launchd.run(),
                DOMAIN,
                &self.agents,
                &[
                    LegacyJob {
                        id: CORE,
                        restart: Restart::OnFailure,
                        ours: &core,
                    },
                    LegacyJob {
                        id: CONNECTOR,
                        restart: Restart::Always,
                        ours: &connector,
                    },
                ],
                Duration::ZERO,
            )
            .await
        }
    }

    #[tokio::test]
    async fn a_loaded_legacy_job_of_ours_is_unloaded_and_its_definition_deleted() {
        let home = Home::new();
        let core = home.plant_core();
        let connector = home.plant(
            CONNECTOR.label,
            &written_by_0_18(
                CONNECTOR.label,
                &[CONNECTOR_PROGRAM, "connector"],
                "<true/>",
            ),
        );
        let launchd = Launchd::with(&[CORE.label, CONNECTOR.label], CORE_PROGRAM);
        assert_eq!(
            home.retire(&launchd).await.unwrap(),
            vec![
                Legacy {
                    label: CORE.label,
                    unloaded: true,
                    deleted: Some(core.clone())
                },
                Legacy {
                    label: CONNECTOR.label,
                    unloaded: true,
                    deleted: Some(connector.clone())
                },
            ]
        );
        assert!(!launchd.has(CORE.label) && !launchd.has(CONNECTOR.label));
        assert!(!core.exists() && !connector.exists());

        // Done once, nothing is left to do: the second run finds nothing, and changes nothing.
        let calls = launchd.calls.lock().unwrap().len();
        assert_eq!(home.retire(&launchd).await.unwrap(), vec![]);
        let again = launchd.calls.lock().unwrap()[calls..].to_vec();
        assert!(
            again.iter().all(|call| call.starts_with("print ")),
            "{again:?}"
        );
    }

    #[tokio::test]
    async fn a_definition_of_ours_that_is_not_loaded_is_deleted_without_a_bootout() {
        let home = Home::new();
        let core = home.plant_core();
        let launchd = Launchd::with(&[], CORE_PROGRAM);
        assert_eq!(
            home.retire(&launchd).await.unwrap(),
            vec![Legacy {
                label: CORE.label,
                unloaded: false,
                deleted: Some(core.clone())
            }]
        );
        assert!(!core.exists());
        assert!(!launchd.booted_out());
    }

    #[tokio::test]
    async fn a_loaded_job_with_no_definition_is_unloaded_only_when_what_it_runs_is_recognised() {
        let home = Home::new();
        let launchd = Launchd::with(&[CORE.label], CORE_PROGRAM);
        assert_eq!(
            home.retire(&launchd).await.unwrap(),
            vec![Legacy {
                label: CORE.label,
                unloaded: true,
                deleted: None
            }]
        );
        assert!(!launchd.has(CORE.label));

        let elsewhere = Launchd::with(&[CORE.label], "/usr/local/bin/someone-elses");
        let refused = home.retire(&elsewhere).await.unwrap_err();
        assert!(!refused.recognised, "{refused:?}");
        assert!(refused.detail.contains("someone-elses"), "{refused:?}");
        assert!(elsewhere.has(CORE.label), "left loaded");
        assert!(!elsewhere.booted_out());
    }

    #[tokio::test]
    async fn nothing_there_is_nothing_to_do() {
        let home = Home::new();
        let launchd = Launchd::with(&[], CORE_PROGRAM);
        assert_eq!(home.retire(&launchd).await.unwrap(), vec![]);
        assert_eq!(
            *launchd.calls.lock().unwrap(),
            vec![
                format!("print {DOMAIN}/dev.sidevoice.core"),
                format!("print {DOMAIN}/dev.sidevoice.connector"),
            ]
        );
    }

    #[tokio::test]
    async fn a_definition_not_written_by_sidevoice_is_refused_and_left_with_its_job() {
        let home = Home::new();
        let genuine = written_by_0_18(
            CORE.label,
            &[CORE_PROGRAM, "--data-dir", "/d/core"],
            CORE_KEEP_ALIVE,
        );
        for (text, why) in [
            (
                genuine.replace(
                    "<key>RunAtLoad</key>",
                    "<key>WatchPaths</key>\n\t<array/>\n\t<key>RunAtLoad</key>",
                ),
                "a key Sidevoice never wrote",
            ),
            (
                genuine.replace(CORE_KEEP_ALIVE, "<true/>"),
                "kept alive as the connector was",
            ),
            (
                genuine.replace("<integer>10</integer>", "<integer>30</integer>"),
                "another throttle",
            ),
            (
                genuine.replace(
                    "<string>dev.sidevoice.core</string>",
                    "<string>com.example.x</string>",
                ),
                "another label",
            ),
            (
                genuine.replace(CORE_PROGRAM, "/d/my-script"),
                "a program the caller does not recognise",
            ),
            ("not a plist".to_owned(), "not a property list"),
        ] {
            let file = home.plant(CORE.label, &text);
            let launchd = Launchd::with(&[CORE.label], CORE_PROGRAM);
            let refused = home.retire(&launchd).await.unwrap_err();
            assert!(!refused.recognised, "{why}: {refused:?}");
            assert_eq!(refused.label, CORE.label);
            assert_eq!(refused.plist, file);
            assert_eq!(
                refused.bootout,
                "launchctl bootout gui/501/dev.sidevoice.core"
            );
            assert!(file.exists(), "{why}: the definition stays");
            assert!(launchd.has(CORE.label), "{why}: the job stays loaded");
            assert!(!launchd.booted_out(), "{why}");
        }

        // A link is not this user's plain file, wherever it points.
        let real = home.plant("real", &genuine);
        std::fs::remove_file(home.plist(CORE.label)).unwrap();
        std::os::unix::fs::symlink(&real, home.plist(CORE.label)).unwrap();
        let refused = home
            .retire(&Launchd::with(&[], CORE_PROGRAM))
            .await
            .unwrap_err();
        assert!(!refused.recognised);
        assert!(real.exists() && home.plist(CORE.label).exists());
    }

    #[tokio::test]
    async fn a_legacy_job_of_ours_that_will_not_unload_is_refused_with_the_commands_that_remove_it()
    {
        let home = Home::new();
        let core = home.plant_core();
        let launchd = Launchd {
            stuck: true,
            ..Launchd::with(&[CORE.label], CORE_PROGRAM)
        };
        let refused = home.retire(&launchd).await.unwrap_err();
        assert_eq!(
            refused,
            LegacyRefusal {
                label: CORE.label,
                plist: core.clone(),
                recognised: true,
                detail: "Boot-out failed: 5: Input/output error".into(),
                bootout: "launchctl bootout gui/501/dev.sidevoice.core".into(),
                remove: format!("rm -f '{}'", core.display()),
            }
        );
        assert!(core.exists(), "nothing deleted while the job is loaded");
        assert!(launchd.has(CORE.label));
    }

    #[test]
    fn both_historical_writers_read_as_what_they_recorded() {
        let era_a = written(
            written_by_0_15(
                CORE.label,
                "/Users/u/.local/share/sidevoice/current/core/bin/sidevoice-core-rust",
            )
            .as_bytes(),
            CORE.label,
            Restart::OnFailure,
        )
        .unwrap();
        assert_eq!(
            era_a.program,
            [
                "/Users/u/.local/share/sidevoice/current/core/bin/sidevoice-core-rust",
                "--data-dir",
                "/Users/u/.sidevoice/core"
            ]
        );
        assert_eq!(
            era_a.environment["SIDEVOICE_DATA_DIR"],
            "/Users/u/.sidevoice"
        );
        assert_eq!(
            era_a.log.as_deref(),
            Some(Path::new("/Users/u/.sidevoice/core.stderr.log"))
        );
        let era_b = written(
            written_by_0_18(
                CONNECTOR.label,
                &[CONNECTOR_PROGRAM, "connector"],
                "<true/>",
            )
            .as_bytes(),
            CONNECTOR.label,
            Restart::Always,
        )
        .unwrap();
        assert_eq!(era_b.program, [CONNECTOR_PROGRAM, "connector"]);
        assert_eq!(era_b.environment["SIDEVOICE_SERVICE"], "launchd");
    }

    #[test]
    fn launchd_answers_read_as_presence_and_what_a_job_runs() {
        let said = |ok, code, output: &str| {
            presence_of(&Output {
                ok,
                code,
                output: output.into(),
            })
        };
        assert_eq!(said(true, Some(0), ""), Presence::Loaded);
        assert_eq!(said(false, Some(113), ""), Presence::Absent);
        assert_eq!(
            said(
                false,
                Some(3),
                "Could not find service \"x\" in domain for user gui: 501"
            ),
            Presence::Absent
        );
        assert_eq!(
            said(false, Some(5), "Operation not permitted"),
            Presence::Unknown("Operation not permitted".into())
        );
        let printed = "x = {\n\tpath = /p.plist\n\tprogram = /a/b c\n\targuments = {\n\t\t/a/b c\n\t\t--port\n\t\t1\n\t}\n\
                       \tstdout path = /d/core.stderr.log\n\tdefault environment = {\n\t\tPATH => /usr/bin\n\t}\n\
                       \tenvironment = {\n\t\tSIDEVOICE_SERVICE => launchd\n\t}\n}";
        assert_eq!(
            printed_job(printed),
            Recorded {
                program: vec!["/a/b c".into(), "--port".into(), "1".into()],
                environment: BTreeMap::from([("SIDEVOICE_SERVICE".into(), "launchd".into())]),
                log: Some(PathBuf::from("/d/core.stderr.log")),
            }
        );
        assert_eq!(printed_job("x = {\n}"), Recorded::default());
        assert_eq!(quoted(Path::new("/a/it's")), r"'/a/it'\''s'");
    }
}
