//! What Sidevoice before 0.19 ran under its launchd jobs (`dev.sidevoice.*`, [`Job::legacy_id`]), recognised before
//! such a job is removed ([`Service::retire_legacy`](crate::platform::service::Service::retire_legacy)): a job is
//! Sidevoice's only when what it runs is exactly what one of those releases wrote for it ([`WRITERS`]). The manager
//! checks its own part of the definition (`platform/service/launchd.rs`).
//!
//! Every release had the job carry `SIDEVOICE_SERVICE=launchd` and `SIDEVOICE_DATA_DIR=D`, send its output to
//! `D/core.stderr.log` or `D/connector.log`, and run the connector as `<program> connector`. `D` was `~/.sidevoice` up
//! to 0.15.x and `~/Library/Application Support/Sidevoice` from 0.16.0, unless `SIDEVOICE_DATA_DIR` named another, which
//! is recognised when it is this installation's. The core's arguments were always the full list of its release, in
//! order, every value derived from `D` and the job's `SIDEVOICE_CORE_HOST` and `SIDEVOICE_CORE_PORT` (whose default was
//! 8768 up to 0.8.1, 10829 from 0.9.0). Paths are compared with `.` and `..` worked out: those releases wrote
//! `SIDEVOICE_DATA_DIR`, `XDG_DATA_HOME` and `install.json`'s `releases` as given, `..` included.

use super::jobs::Job;
use super::layout::Layout;
use crate::platform::fs::{Files, Fs};
use crate::platform::service::{Recorded, Restart};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// Where a release put the programs its jobs ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Release {
    /// `R/current/…`, `R` being `install.json`'s `releases`, else `~/.local/share/sidevoice` (0.7.0–0.15.x).
    Current,
    /// `D/install/releases/<id>/…` (0.16.0 on).
    Selected,
}

/// How a release chose the core's `--host`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Host {
    /// Always `127.0.0.1`.
    Loopback,
    /// `SIDEVOICE_CORE_HOST` when set and not empty, else `0.0.0.0`.
    Setting,
}

/// One historical writer of the jobs: the releases it covers, where their programs were, the core's program name and
/// the arguments it gave the core.
struct Writer {
    releases: &'static str,
    release: Release,
    core: &'static str,
    socket: bool,
    host: Host,
    credential: bool,
    /// The core's port when the job's `SIDEVOICE_CORE_PORT` names none.
    port: &'static str,
}

/// Every writer before 0.19, from the history of `service/definition.rs` and `service/layout.rs` (v0.7.0 to v0.18.0).
/// The core ran `--data-dir D/core [--socket D/core/local.sock] --ready-file D/core/core.json --host H --port P
/// --log-file D/core.log [--room-credential D/credentials.json] --idle-exit 0`.
const WRITERS: [Writer; 6] = [
    Writer {
        releases: "0.7.0-0.8.1",
        release: Release::Current,
        core: "sidevoice-core-rust",
        socket: true,
        host: Host::Loopback,
        credential: true,
        port: "8768",
    },
    Writer {
        releases: "0.9.0-0.10.2",
        release: Release::Current,
        core: "sidevoice-core-rust",
        socket: true,
        host: Host::Loopback,
        credential: true,
        port: "10829",
    },
    Writer {
        releases: "0.10.3-0.13.x",
        release: Release::Current,
        core: "sidevoice-core-rust",
        socket: true,
        host: Host::Setting,
        credential: true,
        port: "10829",
    },
    Writer {
        releases: "0.14.0-0.15.x",
        release: Release::Current,
        core: "sidevoice-core-rust",
        socket: true,
        host: Host::Setting,
        credential: false,
        port: "10829",
    },
    Writer {
        releases: "0.16.0-0.17.x",
        release: Release::Selected,
        core: "sidevoice-core-rust",
        socket: false,
        host: Host::Setting,
        credential: false,
        port: "10829",
    },
    Writer {
        releases: "0.18.x",
        release: Release::Selected,
        core: "sidevoice-core",
        socket: false,
        host: Host::Setting,
        credential: false,
        port: "10829",
    },
];

/// How launchd kept the job alive, in every release: the core after a crash or a non-zero exit, the connector always.
pub fn restart(job: Job) -> Restart {
    match job {
        Job::Core => Restart::OnFailure,
        Job::Connector => Restart::Always,
    }
}

/// Whether `recorded`, what launchd has under `job`'s legacy label, is exactly what one of the [`WRITERS`] wrote for
/// this installation; why not, in a few English words.
pub fn recognise(job: Job, layout: &Layout, recorded: &Recorded) -> Result<(), String> {
    let environment = &recorded.environment;
    if environment.get("SIDEVOICE_SERVICE").map(String::as_str) != Some("launchd") {
        return Err("it does not carry SIDEVOICE_SERVICE=launchd, as Sidevoice's jobs did".into());
    }
    let data = environment
        .get("SIDEVOICE_DATA_DIR")
        .map(PathBuf::from)
        .filter(|data| data.is_absolute())
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
    let log = data.join(match job {
        Job::Core => "core.stderr.log",
        Job::Connector => "connector.log",
    });
    if let Some(recorded) = &recorded.log {
        if !same(recorded, &log) {
            return Err(format!(
                "its output goes to {}, not where Sidevoice sent it",
                recorded.display()
            ));
        }
    }
    let Some((program, arguments)) = recorded.program.split_first() else {
        return Err("it runs nothing".into());
    };
    let program = Path::new(program);
    for writer in &WRITERS {
        let expected = match job {
            Job::Core => core_arguments(writer, &data, environment),
            Job::Connector => vec!["connector".to_owned()],
        };
        if arguments != expected.as_slice() {
            continue;
        }
        if let Some(found) = released_program(job, writer, layout, &data, program, environment) {
            return found;
        }
    }
    Err(format!(
        "it runs {} {arguments:?}, which no Sidevoice release wrote",
        program.display()
    ))
}

/// The core job's arguments exactly as `writer` wrote them for the data directory `data` and the job's environment.
fn core_arguments(
    writer: &Writer,
    data: &Path,
    environment: &BTreeMap<String, String>,
) -> Vec<String> {
    let path = |path: PathBuf| path.to_string_lossy().into_owned();
    let core = data.join("core");
    let mut arguments = vec!["--data-dir".to_owned(), path(core.clone())];
    if writer.socket {
        arguments.extend(["--socket".to_owned(), path(core.join("local.sock"))]);
    }
    let host = match writer.host {
        Host::Loopback => "127.0.0.1".to_owned(),
        Host::Setting => environment
            .get("SIDEVOICE_CORE_HOST")
            .filter(|host| !host.is_empty())
            .cloned()
            .unwrap_or_else(|| "0.0.0.0".to_owned()),
    };
    let port = environment
        .get("SIDEVOICE_CORE_PORT")
        .filter(|port| port.parse::<u16>().is_ok())
        .cloned()
        .unwrap_or_else(|| writer.port.to_owned());
    arguments.extend([
        "--ready-file".to_owned(),
        path(core.join("core.json")),
        "--host".to_owned(),
        host,
        "--port".to_owned(),
        port,
        "--log-file".to_owned(),
        path(data.join("core.log")),
    ]);
    if writer.credential {
        arguments.extend([
            "--room-credential".to_owned(),
            path(data.join("credentials.json")),
        ]);
    }
    arguments.extend(["--idle-exit".to_owned(), "0".to_owned()]);
    arguments
}

/// Where `writer`'s releases were, for the data directory `data`: `D/install` (0.16.0 on); up to 0.15.x exactly one,
/// as they chose it: `install.json`'s `releases` when it is an absolute path ending in `sidevoice`, else
/// `$XDG_DATA_HOME/sidevoice` as the job recorded it, else `~/.local/share/sidevoice`.
fn release_roots(
    release: Release,
    layout: &Layout,
    data: &Path,
    environment: &BTreeMap<String, String>,
) -> Vec<PathBuf> {
    match release {
        Release::Selected => vec![data.join("install")],
        Release::Current => {
            let recorded = Files::read_trusted(&data.join("install.json"), 64 * 1024)
                .ok()
                .flatten()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .and_then(|record| record.get("releases")?.as_str().map(PathBuf::from))
                .filter(|releases| {
                    releases.is_absolute()
                        && releases.file_name().is_some_and(|name| name == "sidevoice")
                });
            // Exactly one, in the order those releases chose it: the record, else `$XDG_DATA_HOME` (an empty one is
            // unset; a relative one they refused, so no job of theirs has it), else the home's default.
            let chosen = match (
                recorded,
                environment
                    .get("XDG_DATA_HOME")
                    .filter(|home| !home.is_empty()),
            ) {
                (Some(recorded), _) => Some(recorded),
                (None, Some(home)) => {
                    Some(Path::new(home).join("sidevoice")).filter(|root| root.is_absolute())
                }
                (None, None) => Some(layout.home.join(".local/share/sidevoice")),
            };
            chosen.into_iter().collect()
        }
    }
}

/// Whether `program` is the job's program in one of `writer`'s releases: `<R>/current/<entry>` or
/// `<R>/releases/<id>/<entry>`, as that writer laid them out, with the core named as it named it. None when it is not;
/// otherwise whether, its links resolved, it stays inside that release root.
fn released_program(
    job: Job,
    writer: &Writer,
    layout: &Layout,
    data: &Path,
    program: &Path,
    environment: &BTreeMap<String, String>,
) -> Option<Result<(), String>> {
    // Compared as written, `.` and `..` worked out (old releases wrote such paths verbatim).
    let written = normal(program)?;
    for root in release_roots(writer.release, layout, data, environment) {
        let Some(rest) =
            normal(&root).and_then(|root| written.strip_prefix(root).ok().map(Path::to_path_buf))
        else {
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
        let entry = match (writer.release, parts.as_slice()) {
            (Release::Current, ["current", entry @ ..]) => entry,
            (Release::Selected, ["releases", id, entry @ ..]) if !id.starts_with('.') => entry,
            _ => continue,
        };
        let shaped = match (job, entry) {
            (Job::Core, ["core", "bin", name]) => *name == writer.core,
            (Job::Connector, ["bin", "sidevoice-connector"]) => true,
            _ => false,
        };
        if !shaped {
            continue;
        }
        return Some(match (resolved(program), resolved(&root)) {
            (Some(program), Some(root)) if program.starts_with(&root) => Ok(()),
            _ => Err(format!(
                "it runs {}, which leads outside {} (the releases of Sidevoice {})",
                program.display(),
                root.display(),
                writer.releases
            )),
        });
    }
    None
}

/// `path` with `.` and `..` worked out as written, without the file system; none when it is not absolute.
fn normal(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    Some(out)
}

/// `path` ([`normal`]) with every link resolved: its longest prefix the file system resolves made canonical, the rest
/// appended. None when it is not absolute or nothing of it can be resolved.
fn resolved(path: &Path) -> Option<PathBuf> {
    let path = normal(path)?;
    let parts: Vec<Component> = path.components().collect();
    for keep in (1..=parts.len()).rev() {
        let prefix: PathBuf = parts[..keep].iter().collect();
        if let Ok(mut found) = std::fs::canonicalize(&prefix) {
            for part in &parts[keep..] {
                if let Component::Normal(name) = part {
                    found.push(name);
                }
            }
            return Some(found);
        }
    }
    None
}

/// Whether two absolute paths are the same place: as written with `.` and `..` worked out, or with their links
/// resolved.
fn same(one: &Path, other: &Path) -> bool {
    let (Some(written), Some(other_written)) = (normal(one), normal(other)) else {
        return false;
    };
    written == other_written || resolved(one).is_some_and(|one| Some(one) == resolved(other))
}

// Links and the paths of a Mac: Unix only.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

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

        /// Release `id` under `root`, with the core program `core`, its programs there (or pruned).
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

        /// The data directory of 0.7.0–0.15.x, `~/.sidevoice`, and its default releases with `current` linked to
        /// release `id`. The core and connector programs through `current`.
        fn era_a(&self, id: &str) -> (PathBuf, PathBuf, PathBuf) {
            let data = self.home.join(".sidevoice");
            Files::ensure_private_dir(&data).unwrap();
            let releases = self.home.join(".local/share/sidevoice");
            self.release(&releases, id, "sidevoice-core-rust", true);
            let current = releases.join("current");
            let _ = std::fs::remove_file(&current);
            std::os::unix::fs::symlink(releases.join("releases").join(id), &current).unwrap();
            (
                data,
                current.join("core/bin/sidevoice-core-rust"),
                current.join("bin/sidevoice-connector"),
            )
        }
    }

    fn path(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    fn environment(data: &Path, settings: &[(&str, &str)]) -> BTreeMap<String, String> {
        let mut environment = BTreeMap::from([
            ("HOME".into(), "/Users/u".into()),
            ("SIDEVOICE_DATA_DIR".into(), path(data)),
            ("SIDEVOICE_SERVICE".into(), "launchd".into()),
        ]);
        for (name, value) in settings {
            environment.insert((*name).into(), (*value).into());
        }
        environment
    }

    /// The core job written out by hand, word for word, as a release of each era wrote it.
    fn core_job(
        program: &Path,
        data: &Path,
        arguments: &[String],
        settings: &[(&str, &str)],
    ) -> Recorded {
        Recorded {
            program: std::iter::once(path(program))
                .chain(arguments.iter().cloned())
                .collect(),
            environment: environment(data, settings),
            log: Some(data.join("core.stderr.log")),
        }
    }

    fn connector_job(program: &Path, data: &Path) -> Recorded {
        Recorded {
            program: vec![path(program), "connector".into()],
            environment: environment(data, &[]),
            log: Some(data.join("connector.log")),
        }
    }

    fn words(text: &str, data: &Path) -> Vec<String> {
        text.split(' ')
            .map(|word| word.replace("{D}", &path(data)))
            .collect()
    }

    const ARGS_0_8: &str = "--data-dir {D}/core --socket {D}/core/local.sock --ready-file {D}/core/core.json \
        --host 127.0.0.1 --port 8768 --log-file {D}/core.log --room-credential {D}/credentials.json --idle-exit 0";
    const ARGS_0_10_2: &str = "--data-dir {D}/core --socket {D}/core/local.sock --ready-file {D}/core/core.json \
        --host 127.0.0.1 --port 10829 --log-file {D}/core.log --room-credential {D}/credentials.json --idle-exit 0";
    const ARGS_0_13: &str = "--data-dir {D}/core --socket {D}/core/local.sock --ready-file {D}/core/core.json \
        --host 0.0.0.0 --port 10829 --log-file {D}/core.log --room-credential {D}/credentials.json --idle-exit 0";
    const ARGS_0_15: &str =
        "--data-dir {D}/core --socket {D}/core/local.sock --ready-file {D}/core/core.json \
        --host 192.168.1.7 --port 4000 --log-file {D}/core.log --idle-exit 0";
    const ARGS_0_18: &str =
        "--data-dir {D}/core --ready-file {D}/core/core.json --host 0.0.0.0 --port 10829 \
        --log-file {D}/core.log --idle-exit 0";

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
            let job = core_job(&core, &data, &words(ARGS_0_18, &data), &[]);
            assert_eq!(recognise(Job::Core, &mac.layout, &job), Ok(()), "{id}");
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
        let mut printed = core_job(&core, &data, &words(ARGS_0_18, &data), &[]);
        printed.log = None;
        assert_eq!(recognise(Job::Core, &mac.layout, &printed), Ok(()));
    }

    #[test]
    fn the_jobs_of_0_7_to_0_15_are_recognised_each_with_its_own_arguments() {
        let mac = Mac::new();
        let (data, core, connector) = mac.era_a("0.13.1");
        for (arguments, settings) in [
            (ARGS_0_8, &[][..]),
            (ARGS_0_10_2, &[][..]),
            (ARGS_0_13, &[][..]),
            (
                ARGS_0_15,
                &[
                    ("SIDEVOICE_CORE_HOST", "192.168.1.7"),
                    ("SIDEVOICE_CORE_PORT", "4000"),
                ][..],
            ),
        ] {
            let job = core_job(&core, &data, &words(arguments, &data), settings);
            assert_eq!(
                recognise(Job::Core, &mac.layout, &job),
                Ok(()),
                "{arguments}"
            );
        }
        assert_eq!(
            recognise(
                Job::Connector,
                &mac.layout,
                &connector_job(&connector, &data)
            ),
            Ok(())
        );
    }

    #[test]
    fn releases_put_elsewhere_are_recognised_where_install_json_recorded_them() {
        let mac = Mac::new();
        let (data, _, connector) = mac.era_a("0.15.0");
        let elsewhere = mac.home.join("opt/sidevoice");
        mac.release(&elsewhere, "0.15.0", "sidevoice-core-rust", true);
        std::os::unix::fs::symlink(elsewhere.join("releases/0.15.0"), elsewhere.join("current"))
            .unwrap();
        let core = elsewhere.join("current/core/bin/sidevoice-core-rust");
        let job = core_job(
            &core,
            &data,
            &words(ARGS_0_15, &data),
            &[
                ("SIDEVOICE_CORE_HOST", "192.168.1.7"),
                ("SIDEVOICE_CORE_PORT", "4000"),
            ],
        );
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
    fn data_and_release_directories_written_with_dots_are_recognised_as_this_installation() {
        let mac = Mac::new();
        // `SIDEVOICE_DATA_DIR=~/state/../.sidevoice` and a non-default `XDG_DATA_HOME=~/./x/../xdg-data`, written as
        // given; the releases in `$XDG_DATA_HOME/sidevoice`, `current` linked to one.
        let data = mac.home.join("state/../.sidevoice");
        Files::ensure_private_dir(&mac.home.join(".sidevoice")).unwrap();
        let xdg = mac.home.join("./x/../xdg-data");
        let real = mac.home.join("xdg-data/sidevoice");
        mac.release(&real, "0.13.1", "sidevoice-core-rust", true);
        std::os::unix::fs::symlink(real.join("releases/0.13.1"), real.join("current")).unwrap();
        let releases = xdg.join("sidevoice");
        let core = releases.join("current/core/bin/sidevoice-core-rust");
        let connector = releases.join("current/bin/sidevoice-connector");
        let xdg_text = path(&xdg);
        let settings = [("XDG_DATA_HOME", xdg_text.as_str())];
        let job = core_job(&core, &data, &words(ARGS_0_13, &data), &settings);
        assert_eq!(recognise(Job::Core, &mac.layout, &job), Ok(()));
        let mut connector_job = connector_job(&connector, &data);
        connector_job.environment.extend(job.environment.clone());
        assert_eq!(
            recognise(Job::Connector, &mac.layout, &connector_job),
            Ok(())
        );

        // The home's default is not where those releases looked once `XDG_DATA_HOME` named another place.
        let (_, default_core, _) = mac.era_a("0.13.1");
        let elsewhere_job = core_job(&default_core, &data, &words(ARGS_0_13, &data), &settings);
        assert!(recognise(Job::Core, &mac.layout, &elsewhere_job).is_err());

        // Worked out, the data directory must still be this installation's.
        let other = mac.home.join(".sidevoice/../other");
        let job = core_job(&core, &other, &words(ARGS_0_13, &other), &settings);
        assert!(recognise(Job::Core, &mac.layout, &job).is_err());
    }

    #[test]
    fn xdg_data_home_is_not_where_releases_were_when_install_json_named_another_place() {
        let mac = Mac::new();
        let data = mac.home.join(".sidevoice");
        Files::ensure_private_dir(&data).unwrap();
        let xdg = mac.home.join("xdg-data");
        let releases = xdg.join("sidevoice");
        mac.release(&releases, "0.15.0", "sidevoice-core-rust", true);
        std::os::unix::fs::symlink(releases.join("releases/0.15.0"), releases.join("current"))
            .unwrap();
        let core = releases.join("current/core/bin/sidevoice-core-rust");
        let xdg_text = path(&xdg);
        let settings = [("XDG_DATA_HOME", xdg_text.as_str())];
        let job = core_job(&core, &data, &words(ARGS_0_13, &data), &settings);
        assert_eq!(
            recognise(Job::Core, &mac.layout, &job),
            Ok(()),
            "no record: XDG"
        );
        let record = |releases: &Path| {
            Files::write_private(
                &data.join("install.json"),
                serde_json::json!({"releases": path(releases)})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        };
        record(&mac.home.join("opt/sidevoice"));
        assert!(
            recognise(Job::Core, &mac.layout, &job).is_err(),
            "the record names another root: only that one"
        );
        // A record those releases ignored (not ending in `sidevoice`) leaves XDG chosen.
        record(&mac.home.join("opt/other"));
        assert_eq!(recognise(Job::Core, &mac.layout, &job), Ok(()));
    }
    #[test]
    fn arguments_no_release_wrote_are_not_recognised() {
        let mac = Mac::new();
        let data = mac.layout.data.clone();
        let (core, _) = mac.release(&mac.layout.releases, "0.18.0", "sidevoice-core", true);
        let genuine = words(ARGS_0_18, &data);
        let with = |from: &str, to: &str| -> Vec<String> {
            genuine
                .iter()
                .map(|word| {
                    if word == from {
                        to.to_owned()
                    } else {
                        word.clone()
                    }
                })
                .collect()
        };
        let ready = path(&data.join("core/core.json"));
        let log = path(&data.join("core.log"));
        let without_host: Vec<String> = genuine
            .iter()
            .enumerate()
            .filter(|(index, _)| ![4, 5].contains(index))
            .map(|(_, word)| word.clone())
            .collect();
        let mut reordered = genuine.clone();
        reordered.swap(2, 6);
        reordered.swap(3, 7);
        let mut extra = genuine.clone();
        extra.splice(10..10, ["--verbose".to_owned(), "1".to_owned()]);
        for (arguments, settings, why) in [
            (with(&ready, "/tmp/x"), &[][..], "a foreign --ready-file"),
            (with(&log, "/tmp/x"), &[][..], "a foreign --log-file"),
            (without_host, &[][..], "no --host"),
            (
                with("0.0.0.0", "127.0.0.1"),
                &[][..],
                "the loopback host 0.18 never wrote",
            ),
            (
                with("10829", "4000"),
                &[][..],
                "a port its environment does not name",
            ),
            (
                genuine.clone(),
                &[("SIDEVOICE_CORE_PORT", "4000")][..],
                "the port its environment names, not used",
            ),
            (reordered, &[][..], "another order"),
            (extra, &[][..], "an extra flag"),
            (
                words(ARGS_0_15, &data),
                &[][..],
                "the arguments of another era",
            ),
        ] {
            let job = core_job(&core, &data, &arguments, settings);
            assert!(
                recognise(Job::Core, &mac.layout, &job).is_err(),
                "{why}: {arguments:?}"
            );
        }
    }

    #[test]
    fn a_layout_and_program_no_release_paired_are_not_recognised() {
        let mac = Mac::new();
        let data = mac.layout.data.clone();
        // `sidevoice-core` was only ever run from `releases/<id>`, `sidevoice-core-rust` through `current` only with
        // the 0.7–0.15 arguments, and a 0.18 release never ran `sidevoice-core-rust` with 0.15's.
        let releases = mac.home.join(".local/share/sidevoice");
        mac.release(&releases, "x", "sidevoice-core", true);
        std::os::unix::fs::symlink(releases.join("releases/x"), releases.join("current")).unwrap();
        let current_new = releases.join("current/core/bin/sidevoice-core");
        let (selected_old, _) =
            mac.release(&mac.layout.releases, "0.17.0", "sidevoice-core-rust", true);
        let era_a = mac.home.join(".sidevoice");
        Files::ensure_private_dir(&era_a).unwrap();
        for (program, data, arguments, why) in [
            (
                current_new,
                era_a.clone(),
                words(ARGS_0_13, &era_a),
                "`current` with sidevoice-core",
            ),
            (
                selected_old.clone(),
                data.clone(),
                words(ARGS_0_13, &data),
                "a 0.17 release with 0.13's arguments",
            ),
            (
                data.join("install/current/core/bin/sidevoice-core-rust"),
                data.clone(),
                words(ARGS_0_13, &data),
                "`current` under D/install",
            ),
        ] {
            let job = core_job(&program, &data, &arguments, &[]);
            assert!(recognise(Job::Core, &mac.layout, &job).is_err(), "{why}");
        }
    }

    #[test]
    fn a_script_of_the_persons_own_under_the_data_directory_is_not_recognised() {
        let mac = Mac::new();
        let data = mac.layout.data.clone();
        let script = data.join("my-script");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        let (core, connector) = mac.release(&mac.layout.releases, "0.18.0", "sidevoice-core", true);
        let other = mac.home.join("Library/Application Support/Other");
        let args = words(ARGS_0_18, &data);
        for (job, recorded) in [
            (Job::Core, core_job(&script, &data, &args, &[])),
            (Job::Connector, connector_job(&script, &data)),
            (
                Job::Connector,
                Recorded {
                    program: vec![path(&connector), "mcp".into()],
                    ..connector_job(&connector, &data)
                },
            ),
            (Job::Core, connector_job(&connector, &data)),
            (
                Job::Core,
                Recorded {
                    environment: BTreeMap::new(),
                    ..core_job(&core, &data, &args, &[])
                },
            ),
            (
                Job::Core,
                core_job(
                    &other.join("install/releases/0.18.0/core/bin/sidevoice-core"),
                    &other,
                    &words(ARGS_0_18, &other),
                    &[],
                ),
            ),
            (
                Job::Core,
                Recorded {
                    log: Some(data.join("elsewhere.log")),
                    ..core_job(&core, &data, &args, &[])
                },
            ),
            (
                Job::Core,
                core_job(
                    &data.join("install/releases/0.18.0/../../../../../../bin/sh"),
                    &data,
                    &args,
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
        let refused = recognise(
            Job::Core,
            &mac.layout,
            &core_job(&core, &data, &words(ARGS_0_18, &data), &[]),
        )
        .unwrap_err();
        assert!(refused.contains("leads outside"), "{refused}");
    }
}
