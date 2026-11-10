//! launchd's steps that need no macOS API, with `launchctl` (a runner of its arguments) and the files passed in, so
//! that they are tested on every Unix: whether launchd has a job, a job waited out after `bootout`, and the legacy jobs
//! removed.
//!
//! Legacy jobs: Sidevoice before 0.19 labelled its LaunchAgents `dev.sidevoice.core` and `dev.sidevoice.connector`;
//! 0.19 renamed them `ai.sidevoice.*` (sidevoice/sidevoice-connector#324) and removed nothing. A Mac updated from an
//! earlier release kept the old jobs loaded beside the new ones, and the old core held the core's port, so the new
//! one failed (`bind.port-in-use`). The labels are the service layer's (`service::jobs::Job::legacy_id`); this is how
//! launchd finds such a job and removes it, only when it is Sidevoice's.

use super::{existing, JobId, Legacy, LegacyOutcome, LegacyRefusal, Output, Refusal};
use serde::Deserialize;
use std::future::Future;
use std::io;
use std::path::{Component, Path, PathBuf};
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

/// What is read of a legacy definition: its label and its program. Anything else in it is ignored.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Definition {
    label: String,
    program_arguments: Vec<String>,
}

/// A legacy label's definition on disk.
#[derive(Debug, PartialEq, Eq)]
enum Found {
    None,
    /// Sidevoice's: this user's plain file, naming the label and a program inside one of the roots.
    Ours,
    /// Something else: why it is not ours.
    Foreign(String),
}

/// Whether `program` is an absolute path, with no `..`, inside one of `roots` (as written, or both resolved when they
/// exist). A root that is a file-system root proves nothing.
fn inside(program: &str, roots: &[&Path]) -> bool {
    let program = Path::new(program);
    if !program.is_absolute()
        || program
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return false;
    }
    let resolved = std::fs::canonicalize(program).ok();
    roots
        .iter()
        .filter(|root| root.parent().is_some())
        .any(|root| {
            program.starts_with(root)
                || std::fs::canonicalize(root).is_ok_and(|root| {
                    program.starts_with(&root)
                        || resolved
                            .as_ref()
                            .is_some_and(|path| path.starts_with(&root))
                })
        })
}

fn definition(plist: &Path, label: &str, roots: &[&Path]) -> Found {
    match existing(plist) {
        Ok(false) => return Found::None,
        Ok(true) => {}
        Err(Refusal::Definition(_)) => {
            return Found::Foreign("its definition is not this user's plain file".into())
        }
        Err(refusal) => {
            return Found::Foreign(format!("its definition cannot be read: {refusal:?}"))
        }
    }
    let Ok(bytes) = std::fs::read(plist) else {
        return Found::Foreign("its definition cannot be read".into());
    };
    let Ok(found) = plist::from_bytes::<Definition>(&bytes) else {
        return Found::Foreign("its definition is not a LaunchAgent Sidevoice wrote".into());
    };
    if found.label != label {
        return Found::Foreign(format!("its definition is labelled {}", found.label));
    }
    match found.program_arguments.first() {
        Some(program) if inside(program, roots) => Found::Ours,
        Some(program) => Found::Foreign(format!("it runs {program}, outside Sidevoice's folders")),
        None => Found::Foreign("its definition runs no program".into()),
    }
}

/// The program a loaded job runs, from `launchctl print` (`program = <path>`).
fn loaded_program(printed: &str) -> Option<String> {
    printed.lines().find_map(|line| {
        line.trim()
            .strip_prefix("program = ")
            .map(|program| program.trim().to_owned())
            .filter(|program| !program.is_empty())
    })
}

/// `path` as one word of a POSIX shell.
fn quoted(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

/// Each `legacy` job in `domain` (`gui/<uid>`) whose definition would be in `agents` (`~/Library/LaunchAgents`)
/// removed when it is ours, and left when it is not ([`super::Service::retire_legacy`]): with its definition ours,
/// unloaded (when loaded, or when launchd cannot say) and the definition deleted; with no definition, unloaded when
/// launchd runs a program of ours for it; nothing there, nothing done. `limit` bounds each wait for an unload.
pub async fn retire_with<F, Fut>(
    run: &F,
    domain: &str,
    agents: &Path,
    legacy: &[JobId],
    roots: &[&Path],
    limit: Duration,
) -> Result<Vec<Legacy>, LegacyRefusal>
where
    F: Fn(Vec<String>) -> Fut,
    Fut: Future<Output = Output>,
{
    let mut done = Vec::new();
    for job in legacy {
        let target = format!("{domain}/{}", job.label);
        let plist: PathBuf = agents.join(format!("{}.plist", job.label));
        let refused = |detail: String| LegacyRefusal {
            label: job.label,
            detail,
            bootout: format!("launchctl bootout {target}"),
            remove: format!("rm -f {}", quoted(&plist)),
        };
        let printed = run(vec!["print".into(), target.clone()]).await;
        let presence = presence_of(&printed);
        let outcome = match (definition(&plist, job.label, roots), presence) {
            (Found::Foreign(reason), _) => LegacyOutcome::Left { reason },
            (Found::None, Presence::Absent | Presence::Unknown(_)) => continue,
            (Found::None, Presence::Loaded) => match loaded_program(&printed.output) {
                Some(program) if inside(&program, roots) => {
                    boot_out_with(run, &target, limit).await.map_err(refused)?;
                    LegacyOutcome::Removed {
                        unloaded: true,
                        deleted: None,
                    }
                }
                Some(program) => LegacyOutcome::Left {
                    reason: format!("it runs {program}, outside Sidevoice's folders"),
                },
                None => LegacyOutcome::Left {
                    reason: "launchd does not say what it runs".into(),
                },
            },
            (Found::Ours, presence) => {
                let unloaded = presence != Presence::Absent;
                if unloaded {
                    boot_out_with(run, &target, limit).await.map_err(refused)?;
                }
                match std::fs::remove_file(&plist) {
                    Err(error) if error.kind() != io::ErrorKind::NotFound => {
                        return Err(refused(format!("deleting {}: {error}", plist.display())))
                    }
                    _ => {}
                }
                LegacyOutcome::Removed {
                    unloaded,
                    deleted: Some(plist),
                }
            }
        };
        done.push(Legacy {
            label: job.label,
            outcome,
        });
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
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

    /// A launchd that has the labels in `loaded`: `print` answers for them (running `program`), `bootout` unloads
    /// them unless `stuck` (then it answers `refusal`), and every call is logged.
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
                            "{target} = {{\n\tstate = running\n\tprogram = {}\n\tpid = 4242\n}}\n",
                            self.program
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
    }

    struct Home {
        _scratch: tempfile::TempDir,
        agents: PathBuf,
        data: PathBuf,
    }

    impl Home {
        fn new() -> Home {
            let scratch = tempfile::tempdir().unwrap();
            let agents = scratch.path().join("Library/LaunchAgents");
            let data = scratch.path().join("Library/Application Support/Sidevoice");
            std::fs::create_dir_all(&agents).unwrap();
            std::fs::create_dir_all(&data).unwrap();
            Home {
                _scratch: scratch,
                agents,
                data,
            }
        }

        fn program(&self) -> String {
            self.data
                .join("install/releases/0.18.0/core/bin/sidevoice-core")
                .to_string_lossy()
                .into_owned()
        }

        fn plist(&self, label: &str) -> PathBuf {
            self.agents.join(format!("{label}.plist"))
        }

        /// A definition as Sidevoice 0.18 wrote it, everything it carried besides the label and the program included.
        fn plant(&self, label: &str, program: &str) -> PathBuf {
            let file = self.plist(label);
            let text = format!(
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
		<string>/d/core</string>
	</array>
	<key>EnvironmentVariables</key>
	<dict>
		<key>HOME</key>
		<string>/Users/u</string>
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
	<string>/d/core.stderr.log</string>
	<key>SomethingNewer</key>
	<string>ignored</string>
</dict>
</plist>
"#
            );
            std::fs::write(&file, text).unwrap();
            file
        }

        async fn retire(&self, launchd: &Launchd) -> Result<Vec<Legacy>, LegacyRefusal> {
            retire_with(
                &launchd.run(),
                DOMAIN,
                &self.agents,
                &[CORE, CONNECTOR],
                &[self.data.as_path(), self.data.join("install").as_path()],
                Duration::ZERO,
            )
            .await
        }
    }

    #[tokio::test]
    async fn a_loaded_legacy_job_of_ours_is_unloaded_and_its_definition_deleted() {
        let home = Home::new();
        let core = home.plant(CORE.label, &home.program());
        let connector = home.plant(CONNECTOR.label, &home.program());
        let launchd = Launchd::with(&[CORE.label, CONNECTOR.label], &home.program());
        let done = home.retire(&launchd).await.unwrap();
        assert_eq!(
            done,
            vec![
                Legacy {
                    label: CORE.label,
                    outcome: LegacyOutcome::Removed {
                        unloaded: true,
                        deleted: Some(core.clone())
                    }
                },
                Legacy {
                    label: CONNECTOR.label,
                    outcome: LegacyOutcome::Removed {
                        unloaded: true,
                        deleted: Some(connector.clone())
                    }
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
        let core = home.plant(CORE.label, &home.program());
        let launchd = Launchd::with(&[], &home.program());
        let done = home.retire(&launchd).await.unwrap();
        assert_eq!(
            done,
            vec![Legacy {
                label: CORE.label,
                outcome: LegacyOutcome::Removed {
                    unloaded: false,
                    deleted: Some(core.clone())
                }
            }]
        );
        assert!(!core.exists());
        assert!(!launchd
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call.starts_with("bootout")));
    }

    #[tokio::test]
    async fn a_loaded_job_with_no_definition_is_unloaded_only_when_it_runs_a_program_of_ours() {
        let home = Home::new();
        let launchd = Launchd::with(&[CORE.label], &home.program());
        assert_eq!(
            home.retire(&launchd).await.unwrap(),
            vec![Legacy {
                label: CORE.label,
                outcome: LegacyOutcome::Removed {
                    unloaded: true,
                    deleted: None
                }
            }]
        );
        assert!(!launchd.has(CORE.label));

        let elsewhere = Launchd::with(&[CORE.label], "/usr/local/bin/someone-elses");
        let done = home.retire(&elsewhere).await.unwrap();
        assert!(
            matches!(&done[0].outcome, LegacyOutcome::Left { reason } if reason.contains("/usr/local/bin/someone-elses")),
            "{done:?}"
        );
        assert!(elsewhere.has(CORE.label), "left loaded");
    }

    #[tokio::test]
    async fn nothing_there_is_nothing_to_do() {
        let home = Home::new();
        let launchd = Launchd::with(&[], &home.program());
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
    async fn a_definition_that_is_not_ours_is_left_with_its_job() {
        let home = Home::new();
        // Another program, outside Sidevoice's folders; a path that climbs out of them; another label in the file.
        let climbing = format!("{}/../../../../bin/sh", home.data.display());
        let ours = home.program();
        for (program, label) in [
            ("/usr/local/bin/other", CORE.label),
            (climbing.as_str(), CORE.label),
            (ours.as_str(), "com.example.other"),
        ] {
            let file = home.plist(CORE.label);
            let planted = home.plant(label, program);
            if planted != file {
                std::fs::rename(&planted, &file).unwrap();
            }
            let launchd = Launchd::with(&[CORE.label], program);
            let done = home.retire(&launchd).await.unwrap();
            assert!(
                matches!(
                    &done[..],
                    [Legacy {
                        label: "dev.sidevoice.core",
                        outcome: LegacyOutcome::Left { .. }
                    }]
                ),
                "{program} {label}: {done:?}"
            );
            assert!(file.exists(), "the definition stays");
            assert!(launchd.has(CORE.label), "the job stays loaded");
            assert!(!launchd
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call.starts_with("bootout")));
        }

        // Not a LaunchAgent at all, and a link: left.
        std::fs::write(home.plist(CORE.label), b"not a plist").unwrap();
        let launchd = Launchd::with(&[], &home.program());
        assert!(matches!(
            home.retire(&launchd).await.unwrap()[0].outcome,
            LegacyOutcome::Left { .. }
        ));
        let real = home.plant("real", &home.program());
        std::fs::remove_file(home.plist(CORE.label)).unwrap();
        std::os::unix::fs::symlink(&real, home.plist(CORE.label)).unwrap();
        assert!(matches!(
            home.retire(&launchd).await.unwrap()[0].outcome,
            LegacyOutcome::Left { .. }
        ));
        assert!(real.exists() && home.plist(CORE.label).exists());
    }

    #[tokio::test]
    async fn a_legacy_job_of_ours_that_will_not_unload_is_refused_with_the_commands_that_remove_it()
    {
        let home = Home::new();
        let core = home.plant(CORE.label, &home.program());
        let launchd = Launchd {
            stuck: true,
            ..Launchd::with(&[CORE.label], &home.program())
        };
        let refused = home.retire(&launchd).await.unwrap_err();
        assert_eq!(refused.label, CORE.label);
        assert_eq!(refused.detail, "Boot-out failed: 5: Input/output error");
        assert_eq!(
            refused.bootout,
            "launchctl bootout gui/501/dev.sidevoice.core"
        );
        assert_eq!(refused.remove, format!("rm -f '{}'", core.display()));
        assert!(core.exists(), "nothing deleted while the job is loaded");
        assert!(launchd.has(CORE.label));
    }

    #[test]
    fn launchd_answers_read_as_presence_and_program() {
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
        assert_eq!(
            loaded_program(
                "x = {\n\tpath = /p.plist\n\tprogram = /a/b c\n\targuments = {\n\t\t/a/b c\n\t}\n}"
            ),
            Some("/a/b c".into())
        );
        assert_eq!(loaded_program("x = {\n}"), None);
        assert_eq!(quoted(Path::new("/a/it's")), r"'/a/it'\''s'");
    }

    #[test]
    fn a_program_is_ours_only_inside_a_root_and_never_by_climbing_out() {
        let roots = [Path::new("/Users/u/Library/Application Support/Sidevoice")];
        assert!(inside(
            "/Users/u/Library/Application Support/Sidevoice/install/releases/1/core/bin/sidevoice-core",
            &roots
        ));
        assert!(!inside(
            "/Users/u/Library/Application Support/Sidevoice2/x",
            &roots
        ));
        assert!(!inside(
            "/Users/u/Library/Application Support/Sidevoice/../../../bin/sh",
            &roots
        ));
        assert!(!inside("relative/Sidevoice/x", &roots));
        assert!(
            !inside("/bin/sh", &[Path::new("/")]),
            "a root proves nothing"
        );
    }
}
