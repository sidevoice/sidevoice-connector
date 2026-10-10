//! The two jobs' definitions: what each runs, with which environment, and the text the manager reads.
//!
//! | job | launchd (`~/Library/LaunchAgents/<label>.plist`) | systemd (`<config>/systemd/user/<unit>`) |
//! |---|---|---|
//! | core `dev.sidevoice.core` / `sidevoice-core.service` | RunAtLoad; KeepAlive {SuccessfulExit false, Crashed true}; ThrottleInterval 10; output → `D/core.stderr.log` | Restart=on-failure, RestartSec=10, at most 5 starts in 10 min |
//! | connector `dev.sidevoice.connector` / `sidevoice-connector.service` | RunAtLoad; KeepAlive; ThrottleInterval 10; output → `D/connector.log` | Restart=always, RestartSec=2, the same start limit |
//!
//! The core runs `R/current/core/bin/sidevoice-core-rust` with fixed arguments and `idle-exit 0`; the connector runs
//! `install.json`'s `command` + `connector`. Both name paths through `R/current`, so a definition is rewritten only
//! when its text would change. Neither job starts, signals or adopts the other: the connector links to the core when
//! it answers, and the core's exit status tells the manager whether to restart it (0 after a failed start: not
//! again; 75 while another core holds its directory: later; a crash after ready: again).

use super::layout::Layout;
use super::manager::{Job, Kind};
use super::{Failure, Result};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

/// systemd's start limit, the same for both jobs; launchd has none.
pub const START_LIMIT: u64 = 5;

/// A value that goes into a definition: text with no control character. A newline in a path or a setting would
/// start a directive of its own in a unit (an injected `ExecStartPre=`), and has no place in a plist either.
fn safe<'a>(value: &'a str, what: &str) -> Result<&'a str> {
    if value.chars().any(char::is_control) {
        return Err(Failure::keyed(
            "service.unsafe-value",
            json!({"what": what}),
        ));
    }
    Ok(value)
}

fn xml(value: &str, what: &str) -> Result<String> {
    Ok(safe(value, what)?
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;"))
}

fn unxml(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

/// An `ExecStart=` word: quoted; `\` and `"` escaped, `%` a specifier (`%%`), `$` variable expansion (`$$`).
fn exec_word(value: &str) -> Result<String> {
    Ok(format!(
        "\"{}\"",
        safe(value, "ExecStart")?
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    ))
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
fn environment_assignment(name: &str, value: &str) -> Result<String> {
    let valid = name.chars().enumerate().all(|(index, ch)| {
        ch == '_' || ch.is_ascii_alphabetic() || (index > 0 && ch.is_ascii_digit())
    });
    if name.is_empty() || !valid {
        return Err(Failure::keyed(
            "service.unsafe-value",
            json!({"what": name}),
        ));
    }
    Ok(format!(
        "\"{name}={}\"",
        safe(value, name)?
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    ))
}

/// A LaunchAgent. `crashed`: back after a crash or a non-zero exit, never after exit 0 (the core); otherwise back
/// whenever it exits (the connector). At most one start every 10 s.
pub fn plist_text(
    label: &str,
    program: &[String],
    log: &Path,
    environment: &BTreeMap<String, String>,
    crashed: bool,
) -> Result<String> {
    let arguments = program
        .iter()
        .map(|word| {
            Ok(format!(
                "    <string>{}</string>",
                xml(word, "ProgramArguments")?
            ))
        })
        .collect::<Result<Vec<_>>>()?
        .join("\n");
    let variables = environment
        .iter()
        .map(|(name, value)| {
            Ok(format!(
                "    <key>{}</key>\n    <string>{}</string>",
                xml(name, name)?,
                xml(value, name)?
            ))
        })
        .collect::<Result<Vec<_>>>()?
        .join("\n");
    let keep_alive = if crashed {
        "<dict>\n    <key>SuccessfulExit</key>\n    <false/>\n    <key>Crashed</key>\n    <true/>\n  </dict>"
    } else {
        "<true/>"
    };
    let log = xml(&log.to_string_lossy(), "StandardOutPath")?;
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{arguments}
  </array>
  <key>EnvironmentVariables</key>
  <dict>
{variables}
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  {keep_alive}
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        label = xml(label, "Label")?,
    ))
}

/// A user unit: restarted `restart` (`on-failure`: the core; `always`: the connector) `restart_sec` later, at most
/// [`START_LIMIT`] starts in 10 minutes (then `start-limit-hit`, which `service restart` clears); started with the
/// user's manager. Output goes to the journal.
pub fn unit_text(
    description: &str,
    program: &[String],
    environment: &BTreeMap<String, String>,
    restart: &str,
    restart_sec: u32,
) -> Result<String> {
    let exec = program
        .iter()
        .map(|word| exec_word(word))
        .collect::<Result<Vec<_>>>()?
        .join(" ");
    let variables = environment
        .iter()
        .map(|(name, value)| {
            Ok(format!(
                "Environment={}",
                environment_assignment(name, value)?
            ))
        })
        .collect::<Result<Vec<_>>>()?
        .join("\n");
    Ok(format!(
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
        description = safe(description, "Description")?,
    ))
}

/// The program a definition runs, read back as its serializer wrote it.
pub fn definition_program(kind: Kind, text: &str) -> Option<Vec<String>> {
    match kind {
        Kind::Launchd => {
            let start = text.find("<key>ProgramArguments</key>")?;
            let rest = &text[start..];
            let open = rest.find("<array>")? + "<array>".len();
            let close = rest.find("</array>")?;
            let block = rest.get(open..close)?;
            Some(
                block
                    .split("<string>")
                    .skip(1)
                    .filter_map(|part| part.split_once("</string>").map(|(word, _)| unxml(word)))
                    .collect(),
            )
        }
        Kind::Systemd => {
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
        Kind::None => None,
    }
}

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
        "--socket".into(),
        path(&layout.core_socket()),
        "--ready-file".into(),
        path(&layout.core_ready()),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        layout.core_port(),
    ];
    if let Some(launch_id) = launch_id {
        args.extend(["--launch-id".into(), launch_id.to_owned()]);
    }
    args.extend([
        "--log-file".into(),
        path(&layout.core_log()),
        "--room-credential".into(),
        path(&layout.room_credential()),
    ]);
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
pub fn environment(layout: &Layout, kind: Kind) -> BTreeMap<String, String> {
    let path = |path: &Path| path.to_string_lossy().into_owned();
    let mut environment = layout.settings.clone();
    environment.insert("HOME".into(), path(&layout.home));
    environment.insert("SIDEVOICE_DATA_DIR".into(), path(&layout.data));
    environment.insert("XDG_DATA_HOME".into(), path(&layout.data_home));
    environment.insert("XDG_CONFIG_HOME".into(), path(&layout.config_home));
    for (name, dir) in &layout.agent_dirs {
        environment.insert(name.clone(), path(dir));
    }
    environment.insert("SIDEVOICE_SERVICE".into(), kind.as_str().into());
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

/// The text of both definitions for this manager.
pub fn texts(layout: &Layout, kind: Kind) -> Result<BTreeMap<Job, String>> {
    let programs = programs(layout)?;
    let environment = environment(layout, kind);
    let core_environment = core_environment(layout, &environment);
    let mut texts = BTreeMap::new();
    match kind {
        Kind::Launchd => {
            texts.insert(
                Job::Core,
                plist_text(
                    Job::Core.label(),
                    &programs.core,
                    &layout.core_output(),
                    &core_environment,
                    true,
                )?,
            );
            texts.insert(
                Job::Connector,
                plist_text(
                    Job::Connector.label(),
                    &programs.connector,
                    &layout.connector_log(),
                    &environment,
                    false,
                )?,
            );
        }
        Kind::Systemd => {
            texts.insert(
                Job::Core,
                unit_text(
                    "Sidevoice core (this machine's conversations and voice)",
                    &programs.core,
                    &core_environment,
                    "on-failure",
                    10,
                )?,
            );
            texts.insert(
                Job::Connector,
                unit_text(
                    "Sidevoice connector (the harnesses' link to the core)",
                    &programs.connector,
                    &environment,
                    "always",
                    2,
                )?,
            );
        }
        Kind::None => return Err(Failure::keyed("service.no-manager", json!({}))),
    }
    Ok(texts)
}

fn unsafe_definition(detail: impl Into<String>) -> Failure {
    Failure::keyed(
        "service.definition-unsafe",
        json!({"detail": detail.into()}),
    )
}

/// A definition's directory: created (`0700`) when missing, else this user's and writable by nobody else.
fn definition_directory(dir: &Path) -> Result<()> {
    match fs::symlink_metadata(dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(Failure::plain),
        Err(error) => Err(Failure::plain(error)),
        Ok(metadata) => {
            if !metadata.is_dir()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o022 != 0
            {
                return Err(unsafe_definition(dir.to_string_lossy()));
            }
            Ok(())
        }
    }
}

/// A definition on disk: this user's regular file (never a link), or none.
pub fn existing(file: &Path) -> Result<bool> {
    match fs::symlink_metadata(file) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Failure::plain(error)),
        Ok(metadata) => {
            if !metadata.is_file()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.len() > 128 * 1024
            {
                return Err(unsafe_definition(file.to_string_lossy()));
            }
            Ok(true)
        }
    }
}

/// Write `text` to `file` when it differs: a private temporary file renamed into place. Whether it changed.
pub fn write(file: &Path, text: &str) -> Result<bool> {
    let dir = file
        .parent()
        .ok_or_else(|| unsafe_definition("no parent directory"))?;
    definition_directory(dir)?;
    if existing(file)? && fs::read(file).ok().as_deref() == Some(text.as_bytes()) {
        return Ok(false);
    }
    let temporary = dir.join(format!(
        ".{}.{}.tmp",
        file.file_name().unwrap_or_default().to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let written = (|| -> io::Result<()> {
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        output.write_all(text.as_bytes())?;
        output.sync_all()?;
        fs::rename(&temporary, file)?;
        File::open(dir)?.sync_all()
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(Failure::plain(error));
    }
    Ok(true)
}

/// Why a defined job's program cannot run, if it cannot: `executable-missing` or `permission-denied`.
pub fn program_problem(program: Option<&[String]>) -> Option<&'static str> {
    let file = program.and_then(<[String]>::first)?;
    let metadata = match fs::metadata(file) {
        Ok(metadata) => metadata,
        Err(_) => return Some("executable-missing"),
    };
    let executable = std::ffi::CString::new(file.as_bytes())
        .map(|path| unsafe { libc::access(path.as_ptr(), libc::X_OK) } == 0)
        .unwrap_or(false);
    if !metadata.is_file() || !executable {
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
    fn plists_keep_the_two_restart_policies_and_read_back_their_program() {
        let program = words(&[
            "/r/current/core/bin/sidevoice-core-rust",
            "--data-dir",
            "/d/core & <x>",
        ]);
        let env = BTreeMap::from([("HOME".to_owned(), "/home/u".to_owned())]);
        let core = plist_text(
            "dev.sidevoice.core",
            &program,
            Path::new("/d/core.stderr.log"),
            &env,
            true,
        )
        .unwrap();
        let connector = plist_text(
            "dev.sidevoice.connector",
            &program,
            Path::new("/d/c.log"),
            &env,
            false,
        )
        .unwrap();
        assert!(core.contains(
            "<key>SuccessfulExit</key>\n    <false/>\n    <key>Crashed</key>\n    <true/>"
        ));
        assert!(connector.contains("<key>KeepAlive</key>\n  <true/>"));
        assert!(core.contains("<integer>10</integer>"));
        assert!(core.contains("<string>/d/core &amp; &lt;x&gt;</string>"));
        assert!(core.contains("<key>HOME</key>\n    <string>/home/u</string>"));
        assert_eq!(definition_program(Kind::Launchd, &core), Some(program));
    }

    #[test]
    fn units_quote_every_word_and_read_back_their_program() {
        let program = words(&["/r/cur rent/bin", "100%", "$HOME", "a\"b\\c"]);
        let env = BTreeMap::from([("PRICE".to_owned(), "$5 at 50% \"off\"".to_owned())]);
        let unit = unit_text("Sidevoice core", &program, &env, "on-failure", 10).unwrap();
        assert!(
            unit.contains(r#"ExecStart="/r/cur rent/bin" "100%%" "$$HOME" "a\"b\\c""#),
            "{unit}"
        );
        assert!(
            unit.contains(r#"Environment="PRICE=$5 at 50%% \"off\"""#),
            "{unit}"
        );
        assert!(unit.contains("Restart=on-failure\nRestartSec=10\n"));
        assert!(unit.contains("StartLimitIntervalSec=600\nStartLimitBurst=5\n"));
        assert!(unit.contains("WantedBy=default.target"));
        assert_eq!(definition_program(Kind::Systemd, &unit), Some(program));
    }

    #[test]
    fn control_characters_and_bad_names_are_never_written() {
        let env = BTreeMap::new();
        let injected = words(&["/bin/true\nExecStartPre=/bin/evil"]);
        assert_eq!(
            unit_text("x", &injected, &env, "always", 2)
                .unwrap_err()
                .key,
            "service.unsafe-value"
        );
        assert_eq!(
            plist_text("l", &injected, Path::new("/l"), &env, false)
                .unwrap_err()
                .key,
            "service.unsafe-value"
        );
        let bad = BTreeMap::from([("A=B".to_owned(), "x".to_owned())]);
        assert!(unit_text("x", &words(&["/bin/true"]), &bad, "always", 2).is_err());
        let digit = BTreeMap::from([("1A".to_owned(), "x".to_owned())]);
        assert!(unit_text("x", &words(&["/bin/true"]), &digit, "always", 2).is_err());
    }

    #[test]
    fn the_core_environment_drops_loader_variables_and_names_its_data() {
        let layout = Layout::from_vars(
            |name| (name == "HOME").then(|| "/home/u".into()),
            Vec::new(),
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
        assert!(!args.contains(&"--launch-id".to_owned()));
    }

    #[test]
    fn a_definition_is_rewritten_only_when_its_text_changes() {
        let dir = std::env::temp_dir().join(format!("sv-def-{}", uuid::Uuid::new_v4()));
        let file = dir.join("nested/unit.service");
        assert!(write(&file, "one").unwrap());
        assert!(!write(&file, "one").unwrap());
        assert!(write(&file, "two").unwrap());
        assert_eq!(fs::read_to_string(&file).unwrap(), "two");
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            program_problem(Some(&words(&["/no/such/program"]))),
            Some("executable-missing")
        );
        assert_eq!(
            program_problem(Some(&words(&[file.to_str().unwrap()]))),
            Some("permission-denied")
        );
        assert_eq!(program_problem(Some(&words(&["/bin/sh"]))), None);
        fs::remove_dir_all(&dir).unwrap();
    }
}
