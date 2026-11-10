//! What Sidevoice before 0.19 ran under its launchd jobs (`dev.sidevoice.*`, [`Job::legacy_id`]), recognised before
//! such a job is removed ([`Service::retire_legacy`](crate::platform::service::Service::retire_legacy)): a job is
//! Sidevoice's only when everything it records is what one of those releases wrote. The manager checks its own part
//! of the definition (`platform/service/launchd.rs`); this checks what the job runs, from the releases' history:
//!
//! | releases | `D` (`SIDEVOICE_DATA_DIR` in the job) | releases `R` | core program | connector program |
//! |---|---|---|---|---|
//! | 0.7.0–0.15.x | `~/.sidevoice` | `install.json`'s `releases`, else `~/.local/share/sidevoice` | `R/current/core/bin/sidevoice-core-rust` | `R/current/bin/sidevoice-connector` |
//! | 0.16.0–0.17.x | `~/Library/Application Support/Sidevoice` | `D/install` | `R/releases/<id>/core/bin/sidevoice-core-rust` | `R/releases/<id>/bin/sidevoice-connector` |
//! | 0.18.x | the same | `D/install` | `R/releases/<id>/core/bin/sidevoice-core` | the same |
//!
//! Each with `SIDEVOICE_DATA_DIR` set to this installation's `D` instead, when it was. The core ran with `--data-dir
//! D/core` first, then `--socket` (up to 0.15), `--ready-file`, `--host`, `--port`, `--log-file`,
//! `--room-credential` (up to 0.13) and `--idle-exit 0` last; the connector with `connector` alone. Both had
//! `SIDEVOICE_SERVICE=launchd` and wrote to `D/core.stderr.log` and `D/connector.log`. Either release layout, `current`
//! or `releases/<id>`, is accepted for each, and the program, links resolved, must stay inside its releases.

use super::jobs::Job;
use super::layout::Layout;
use crate::platform::fs::{Files, Fs};
use crate::platform::service::{Recorded, Restart};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

/// The core's program: `sidevoice-core-rust` up to 0.17.x, `sidevoice-core` in 0.18.x.
const CORE_PROGRAMS: [&str; 2] = ["sidevoice-core-rust", "sidevoice-core"];
/// The flags the core job ran with, each once with its value.
const CORE_FLAGS: [&str; 8] = [
    "--data-dir",
    "--socket",
    "--ready-file",
    "--host",
    "--port",
    "--log-file",
    "--room-credential",
    "--idle-exit",
];
/// The flags every release gave the core job.
const CORE_REQUIRED: [&str; 5] = [
    "--data-dir",
    "--ready-file",
    "--port",
    "--log-file",
    "--idle-exit",
];

/// How launchd kept the job alive, in every release: the core after a crash or a non-zero exit, the connector always.
pub fn restart(job: Job) -> Restart {
    match job {
        Job::Core => Restart::OnFailure,
        Job::Connector => Restart::Always,
    }
}

/// Whether `recorded`, what launchd has under `job`'s legacy label, is what a Sidevoice before 0.19 wrote for this
/// installation (the module's table); why not, in a few English words.
pub fn recognise(job: Job, layout: &Layout, recorded: &Recorded) -> Result<(), String> {
    let environment = &recorded.environment;
    if environment.get("SIDEVOICE_SERVICE").map(String::as_str) != Some("launchd") {
        return Err("it does not carry SIDEVOICE_SERVICE=launchd, as Sidevoice's jobs did".into());
    }
    let data = environment
        .get("SIDEVOICE_DATA_DIR")
        .map(PathBuf::from)
        .filter(|data| plain(data))
        .ok_or_else(|| "it names no data directory".to_owned())?;
    if ![layout.data.clone(), layout.home.join(".sidevoice")]
        .iter()
        .any(|known| same(&data, known))
    {
        return Err(format!(
            "its data directory, {}, is not Sidevoice's",
            data.display()
        ));
    }
    let Some((program, arguments)) = recorded.program.split_first() else {
        return Err("it runs nothing".into());
    };
    let log = match job {
        Job::Core => {
            core_arguments(arguments, &data)?;
            "core.stderr.log"
        }
        Job::Connector => {
            if arguments != ["connector"] {
                return Err(format!(
                    "it runs {} with {arguments:?}, not `connector`",
                    program
                ));
            }
            "connector.log"
        }
    };
    if let Some(recorded) = &recorded.log {
        if !same(recorded, &data.join(log)) {
            return Err(format!(
                "its output goes to {}, not where Sidevoice sent it",
                recorded.display()
            ));
        }
    }
    released_program(job, layout, &data, Path::new(program))
}

/// The core job's arguments as every release wrote them: flag and value pairs from [`CORE_FLAGS`], each once, all of
/// [`CORE_REQUIRED`], `--data-dir D/core` first and `--idle-exit 0` last.
fn core_arguments(arguments: &[String], data: &Path) -> Result<(), String> {
    let refused = || {
        Err(format!(
            "it runs the core with {arguments:?}, not as Sidevoice did"
        ))
    };
    if !arguments.len().is_multiple_of(2) {
        return refused();
    }
    let mut seen = BTreeSet::new();
    for pair in arguments.chunks(2) {
        if !CORE_FLAGS.contains(&pair[0].as_str()) || !seen.insert(pair[0].as_str()) {
            return refused();
        }
    }
    let first = arguments.first().map(String::as_str) == Some("--data-dir")
        && same(Path::new(&arguments[1]), &data.join("core"));
    let last = arguments[arguments.len().saturating_sub(2)..] == ["--idle-exit", "0"];
    if !first || !last || CORE_REQUIRED.iter().any(|flag| !seen.contains(flag)) {
        return refused();
    }
    Ok(())
}

/// Where a release before 0.19 could have put its releases, for the data directory `data`: this installation's,
/// `data/install` (0.16.0 on), and `install.json`'s `releases`, else `~/.local/share/sidevoice` (up to 0.15.x).
fn release_roots(layout: &Layout, data: &Path) -> Vec<PathBuf> {
    let mut roots = vec![
        layout.releases.clone(),
        data.join("install"),
        layout.home.join(".local/share/sidevoice"),
    ];
    let recorded = Files::read_trusted(&data.join("install.json"), 64 * 1024)
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|record| record.get("releases")?.as_str().map(PathBuf::from))
        .filter(|releases| plain(releases));
    roots.extend(recorded);
    roots
}

/// Whether `program` is the job's program in a release: `<root>/current/<entry>` or `<root>/releases/<id>/<entry>`
/// below one of the [`release_roots`], and, its links resolved, still inside that root.
fn released_program(job: Job, layout: &Layout, data: &Path, program: &Path) -> Result<(), String> {
    if !plain(program) {
        return Err(format!(
            "it runs {}, not an absolute plain path",
            program.display()
        ));
    }
    for root in release_roots(layout, data) {
        let Ok(rest) = program.strip_prefix(&root) else {
            continue;
        };
        let Some(parts) = rest
            .components()
            .map(|part| match part {
                Component::Normal(name) => name.to_str(),
                _ => None,
            })
            .collect::<Option<Vec<&str>>>()
        else {
            continue;
        };
        let entry = match parts.as_slice() {
            ["current", entry @ ..] => entry,
            ["releases", id, entry @ ..] if !id.starts_with('.') => entry,
            _ => continue,
        };
        let shaped = match (job, entry) {
            (Job::Core, ["core", "bin", name]) => CORE_PROGRAMS.contains(name),
            (Job::Connector, ["bin", "sidevoice-connector"]) => true,
            _ => false,
        };
        if !shaped {
            continue;
        }
        return match (resolved(program), resolved(&root)) {
            (Some(program), Some(root)) if program.starts_with(&root) => Ok(()),
            _ => Err(format!(
                "it runs {}, which leads outside {}",
                program.display(),
                root.display()
            )),
        };
    }
    Err(format!(
        "it runs {}, not the program of a Sidevoice release",
        program.display()
    ))
}

/// An absolute path with no `..`.
fn plain(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
}

/// `path` with every link resolved: its longest existing ancestor made canonical, the rest appended. None when it
/// is not [`plain`] or nothing of it can be resolved.
fn resolved(path: &Path) -> Option<PathBuf> {
    if !plain(path) {
        return None;
    }
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(mut found) = std::fs::canonicalize(existing) {
            found.extend(rest.iter().rev());
            return Some(found);
        }
        rest.push(existing.file_name()?);
        existing = existing.parent()?;
    }
}

/// Whether two paths are the same place: as written, or with their links resolved.
fn same(one: &Path, other: &Path) -> bool {
    plain(one)
        && plain(other)
        && (one == other || resolved(one).is_some_and(|one| Some(one) == resolved(other)))
}

// Links and the paths of a Mac: Unix only.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A home with this installation's standard data directory (as 0.16.0 on), and a layout naming it.
    struct Mac {
        _scratch: tempfile::TempDir,
        home: PathBuf,
        layout: Layout,
    }

    impl Mac {
        fn new() -> Mac {
            let scratch = tempfile::tempdir().unwrap();
            let home = scratch.path().canonicalize().unwrap().join("Users/u");
            let data = home.join("Library/Application Support/Sidevoice");
            std::fs::create_dir_all(data.join("install/releases")).unwrap();
            let layout = Layout::from_vars(
                |name| {
                    use crate::platform::env::{Env, Environment};
                    if name == Environment::HOME {
                        Some(home.clone().into())
                    } else if name == "SIDEVOICE_DATA_DIR" {
                        Some(data.clone().into())
                    } else {
                        None
                    }
                },
                Vec::new(),
                &[],
            )
            .unwrap();
            Mac {
                _scratch: scratch,
                home,
                layout,
            }
        }

        /// What release `id` with the core program `core` recorded: its program existing (or pruned) under `root`.
        fn release(&self, root: &Path, id: &str, core: &str, keep: bool) -> (PathBuf, PathBuf) {
            let release = root.join("releases").join(id);
            let (core, connector) = (
                release.join("core/bin").join(core),
                release.join("bin/sidevoice-connector"),
            );
            if keep {
                for program in [&core, &connector] {
                    std::fs::create_dir_all(program.parent().unwrap()).unwrap();
                    std::fs::write(program, "").unwrap();
                }
            }
            (core, connector)
        }
    }

    fn path(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    /// The core job as `data`'s release wrote it, `extra` flags (with values) in its era's place.
    fn core_job(program: &Path, data: &Path, extra: &[&str]) -> Recorded {
        let mut words = vec![path(program), "--data-dir".into(), path(&data.join("core"))];
        words.extend(extra.iter().map(|word| (*word).to_owned()));
        words.extend(
            [
                "--ready-file",
                &path(&data.join("core/core.json")),
                "--host",
                "127.0.0.1",
                "--port",
                "10829",
                "--log-file",
                &path(&data.join("core.log")),
                "--idle-exit",
                "0",
            ]
            .map(str::to_owned),
        );
        Recorded {
            program: words,
            environment: environment(data),
            log: Some(data.join("core.stderr.log")),
        }
    }

    fn connector_job(program: &Path, data: &Path) -> Recorded {
        Recorded {
            program: vec![path(program), "connector".into()],
            environment: environment(data),
            log: Some(data.join("connector.log")),
        }
    }

    fn environment(data: &Path) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("HOME".into(), "/Users/u".into()),
            ("SIDEVOICE_DATA_DIR".into(), path(data)),
            ("SIDEVOICE_SERVICE".into(), "launchd".into()),
        ])
    }

    #[test]
    fn the_jobs_of_0_16_to_0_18_are_recognised_kept_or_pruned() {
        let mac = Mac::new();
        let (data, releases) = (mac.layout.data.clone(), mac.layout.releases.clone());
        for (id, core, keep) in [
            ("0.16.0", "sidevoice-core-rust", true),
            ("0.17.2", "sidevoice-core-rust", false),
            ("0.18.0", "sidevoice-core", true),
        ] {
            let (core, connector) = mac.release(&releases, id, core, keep);
            assert_eq!(
                recognise(Job::Core, &mac.layout, &core_job(&core, &data, &[])),
                Ok(()),
                "{id}"
            );
            assert_eq!(
                recognise(
                    Job::Connector,
                    &mac.layout,
                    &connector_job(&connector, &data)
                ),
                Ok(()),
                "{id}"
            );
        }
        // What `launchctl print` says has no output path when it cannot be read: the rest still decides.
        let (core, _) = mac.release(&releases, "0.18.0", "sidevoice-core", true);
        let mut printed = core_job(&core, &data, &[]);
        printed.log = None;
        assert_eq!(recognise(Job::Core, &mac.layout, &printed), Ok(()));
    }

    #[test]
    fn the_jobs_of_0_7_to_0_15_are_recognised_through_current_and_a_recorded_releases_dir() {
        let mac = Mac::new();
        let data = mac.home.join(".sidevoice");
        Files::ensure_private_dir(&data).unwrap();
        // The default releases, `R/current` a link to the selected release, the core with `--socket` and (to 0.13)
        // `--room-credential`.
        let releases = mac.home.join(".local/share/sidevoice");
        mac.release(&releases, "0.13.1", "sidevoice-core-rust", true);
        std::os::unix::fs::symlink(releases.join("releases/0.13.1"), releases.join("current"))
            .unwrap();
        let extra = [
            "--socket",
            "/Users/u/.sidevoice/core/local.sock",
            "--room-credential",
            "/Users/u/.sidevoice/room.json",
        ];
        let core = releases.join("current/core/bin/sidevoice-core-rust");
        let connector = releases.join("current/bin/sidevoice-connector");
        assert_eq!(
            recognise(Job::Core, &mac.layout, &core_job(&core, &data, &extra)),
            Ok(())
        );
        assert_eq!(
            recognise(
                Job::Connector,
                &mac.layout,
                &connector_job(&connector, &data)
            ),
            Ok(())
        );
        // Releases put elsewhere, as `install.json` recorded (0.14 and 0.15: `--socket`, no credential).
        let elsewhere = mac.home.join("opt/sidevoice");
        let (core, _) = mac.release(&elsewhere, "0.15.0", "sidevoice-core-rust", true);
        let job = core_job(&core, &data, &extra[..2]);
        assert!(
            recognise(Job::Core, &mac.layout, &job).is_err(),
            "not recorded yet"
        );
        Files::write_private(
            &data.join("install.json"),
            serde_json::json!({"command": [path(&connector)], "releases": path(&elsewhere)})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(recognise(Job::Core, &mac.layout, &job), Ok(()));
    }

    #[test]
    fn a_script_of_the_persons_own_under_the_data_directory_is_not_recognised() {
        let mac = Mac::new();
        let data = mac.layout.data.clone();
        let script = data.join("my-script");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        let other = mac.home.join("Library/Application Support/Other");
        let (core, connector) = mac.release(&mac.layout.releases, "0.18.0", "sidevoice-core", true);
        for (job, recorded) in [
            (Job::Core, core_job(&script, &data, &[])),
            (Job::Connector, connector_job(&script, &data)),
            // Our program, run otherwise.
            (
                Job::Connector,
                Recorded {
                    program: vec![path(&connector), "mcp".into()],
                    ..connector_job(&connector, &data)
                },
            ),
            (
                Job::Core,
                Recorded {
                    program: vec![path(&core), "--data-dir".into(), path(&data.join("core"))],
                    ..core_job(&core, &data, &[])
                },
            ),
            (Job::Core, core_job(&core, &data, &["--verbose", "1"])),
            // Our program, under another job's label.
            (Job::Core, connector_job(&connector, &data)),
            // Not written for launchd, another data directory, its output elsewhere.
            (
                Job::Core,
                Recorded {
                    environment: BTreeMap::new(),
                    ..core_job(&core, &data, &[])
                },
            ),
            (
                Job::Core,
                core_job(
                    &other.join("install/releases/0.18.0/core/bin/sidevoice-core"),
                    &other,
                    &[],
                ),
            ),
            (
                Job::Core,
                Recorded {
                    log: Some(data.join("elsewhere.log")),
                    ..core_job(&core, &data, &[])
                },
            ),
            // A path that climbs out.
            (
                Job::Core,
                core_job(
                    &data.join("install/releases/0.18.0/../../../../../../bin/sh"),
                    &data,
                    &[],
                ),
            ),
        ] {
            assert!(
                recognise(job, &mac.layout, &recorded).is_err(),
                "{job:?} {recorded:?}"
            );
        }
    }

    #[test]
    fn a_release_that_links_outside_the_releases_is_not_recognised() {
        let mac = Mac::new();
        let data = mac.layout.data.clone();
        let outside = mac.home.join("elsewhere");
        mac.release(&outside, "0.18.0", "sidevoice-core", true);
        std::os::unix::fs::symlink(
            outside.join("releases/0.18.0"),
            mac.layout.releases.join("releases/0.18.0"),
        )
        .unwrap();
        let core = mac
            .layout
            .releases
            .join("releases/0.18.0/core/bin/sidevoice-core");
        let refused = recognise(Job::Core, &mac.layout, &core_job(&core, &data, &[])).unwrap_err();
        assert!(refused.contains("leads outside"), "{refused}");
    }
}
