//! Private-profile launchd jobs and status. Linux mutation remains deliberately disabled until its real
//! user-manager path has been reviewed against an isolated runner.

use crate::agents::message;
#[cfg(target_os = "macos")]
use crate::proof::atomic_json;
use crate::proof::{private_dir, private_file, Profile, Ready};
#[cfg(target_os = "macos")]
use anyhow::Context;
#[cfg(target_os = "macos")]
use fs2::FileExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs;
#[cfg(target_os = "macos")]
use std::fs::File;
#[cfg(target_os = "macos")]
use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::MetadataExt;
#[cfg(target_os = "macos")]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
#[cfg(target_os = "macos")]
use tokio::process::Child;
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::time::{sleep, timeout, Duration, Instant};

#[cfg(target_os = "macos")]
const MANAGER_LIMIT: Duration = Duration::from_secs(30);
#[cfg(target_os = "macos")]
const START_LIMIT: Duration = Duration::from_secs(60);
#[cfg(target_os = "macos")]
const STOP_LIMIT: Duration = Duration::from_secs(20);
const STARTING_LIMIT: u64 = 60;
#[cfg(target_os = "macos")]
const OUTPUT_LIMIT: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Install,
    Start,
    Stop,
    Restart,
    Status,
    Uninstall,
}

#[derive(Debug)]
pub struct Failure {
    key: &'static str,
    params: Value,
}

impl Failure {
    fn keyed(key: &'static str, params: Value) -> Self {
        Self { key, params }
    }

    fn plain(error: impl Display) -> Self {
        let detail = clean(&error.to_string(), 240);
        Self::keyed("service.failed", json!({"detail": detail}))
    }

    pub fn value(&self) -> Value {
        json!({"ok":false,"error":{"key":self.key,"params":self.params,
            "message":message(self.key,&self.params)}})
    }
}

pub fn proof_profile_required() -> Value {
    Failure::keyed("service.proof-profile-required", json!({})).value()
}

impl Display for Failure {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&message(self.key, &self.params))
    }
}

impl Error for Failure {}

impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        Self::plain(error)
    }
}

type Result<T> = std::result::Result<T, Failure>;

#[derive(Clone, Debug)]
pub struct ServiceSpec {
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    pub data_dir: PathBuf,
    pub release_root: PathBuf,
    pub core_argv: Vec<String>,
    pub connector_argv: Vec<String>,
    pub core_definition: PathBuf,
    pub connector_definition: PathBuf,
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    pub core_job: String,
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    pub connector_job: String,
    pub core_environment: BTreeMap<String, String>,
    pub connector_environment: BTreeMap<String, String>,
}

impl ServiceSpec {
    pub fn for_private_fixture(profile: &Profile) -> Result<Self> {
        if profile.data.join("install.json").exists() {
            return Err(Failure::keyed("service.no-installation", json!({})));
        }
        let root = &profile.root;
        let current = root.join("releases/current");
        let release_root = root.join("releases");
        let mut digest = Sha256::new();
        digest.update(root.as_os_str().as_encoded_bytes());
        let suffix = hex::encode(digest.finalize());
        let core_job = format!("dev.sidevoice.rustproof.{}.core", &suffix[..16]);
        let connector_job = format!("dev.sidevoice.rustproof.{}.connector", &suffix[..16]);
        validate_label(&core_job)?;
        validate_label(&connector_job)?;
        let service_dir = profile.data.join("service");
        let room_credential = profile.data.join("credentials.json");
        let core_program = current.join("core/bin/sidevoice-core");
        let connector_program = current.join("dist/sidevoice-rust-proof");
        let core_argv = vec![
            core_program.to_string_lossy().into_owned(),
            "--data-dir".into(),
            profile.data.join("core").to_string_lossy().into_owned(),
            "--socket".into(),
            profile.core_socket.to_string_lossy().into_owned(),
            "--room-credential".into(),
            room_credential.to_string_lossy().into_owned(),
            "--port".into(),
            "0".into(),
            "--idle-exit".into(),
            "0".into(),
        ];
        let connector_argv = vec![
            connector_program.to_string_lossy().into_owned(),
            "connector".into(),
            "--service".into(),
            "--profile-root".into(),
            root.to_string_lossy().into_owned(),
        ];
        let core_environment = BTreeMap::from([
            ("HOME".into(), profile.home.to_string_lossy().into_owned()),
            (
                "XDG_CONFIG_HOME".into(),
                profile.xdg_config.to_string_lossy().into_owned(),
            ),
            (
                "XDG_DATA_HOME".into(),
                profile.xdg_data.to_string_lossy().into_owned(),
            ),
            (
                "SIDEVOICE_DATA_DIR".into(),
                profile.data.to_string_lossy().into_owned(),
            ),
        ]);
        let connector_environment = BTreeMap::from([
            ("HOME".into(), profile.home.to_string_lossy().into_owned()),
            (
                "CLAUDE_CONFIG_DIR".into(),
                profile.claude.to_string_lossy().into_owned(),
            ),
            (
                "CODEX_HOME".into(),
                profile.codex.to_string_lossy().into_owned(),
            ),
            (
                "CURSOR_CONFIG_DIR".into(),
                profile.cursor.to_string_lossy().into_owned(),
            ),
            (
                "CURSOR_DATA_DIR".into(),
                profile.cursor_data.to_string_lossy().into_owned(),
            ),
            (
                "XDG_CONFIG_HOME".into(),
                profile.xdg_config.to_string_lossy().into_owned(),
            ),
            (
                "XDG_DATA_HOME".into(),
                profile.xdg_data.to_string_lossy().into_owned(),
            ),
            (
                "SIDEVOICE_DATA_DIR".into(),
                profile.data.to_string_lossy().into_owned(),
            ),
            ("SIDEVOICE_SERVICE".into(), "launchd".into()),
        ]);
        Ok(Self {
            data_dir: profile.data.clone(),
            release_root,
            core_argv,
            connector_argv,
            core_definition: service_dir.join(format!("{core_job}.plist")),
            connector_definition: service_dir.join(format!("{connector_job}.plist")),
            core_job,
            connector_job,
            core_environment,
            connector_environment,
        })
    }

    fn validate_programs(&self, profile: &Profile) -> Result<()> {
        profile.validate_private().map_err(Failure::plain)?;
        if profile.data.join("install.json").exists() {
            return Err(Failure::keyed("service.no-installation", json!({})));
        }
        let selected =
            checked_current(&profile.root, &profile.root.join("releases/current"), true)?;
        if !selected.starts_with(&self.release_root) {
            return Err(Failure::keyed("service.no-installation", json!({})));
        }
        let release = profile.root.join("releases/current");
        let core = release.join("core/bin/sidevoice-core");
        let connector = release.join("dist/sidevoice-rust-proof");
        let credential = profile.data.join("credentials.json");
        validate_executable(&profile.root, &core)?;
        validate_executable(&profile.root, &connector)?;
        match fs::symlink_metadata(&credential) {
            Ok(_) => check_file_under(&profile.root, &credential)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(Failure::plain(error)),
        }
        if self.core_argv.first().map(String::as_str) != Some(core.to_string_lossy().as_ref())
            || self.connector_argv.first().map(String::as_str)
                != Some(connector.to_string_lossy().as_ref())
        {
            return Err(Failure::keyed("service.no-installation", json!({})));
        }
        for value in self
            .core_argv
            .iter()
            .chain(&self.connector_argv)
            .chain(self.core_environment.values())
            .chain(self.connector_environment.values())
        {
            safe_value(value, "ProgramArguments")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
struct Job {
    defined: bool,
    loaded: bool,
    running: bool,
    unknown: bool,
    pid: Option<u32>,
    exit: Option<i64>,
    signal: Option<String>,
    runs: Option<u64>,
    restarting: bool,
    reason: Option<String>,
}

#[derive(Clone, Debug)]
struct Observation {
    service: &'static str,
    installed: bool,
    core: Job,
    connector: Job,
    stopped: bool,
    health: Option<Value>,
    ready: Option<Ready>,
    failure: Option<Value>,
    core_age: Option<u64>,
    connector_running: bool,
    definition_error: Option<String>,
    manager_error: Option<String>,
    program_error: Option<&'static str>,
}

#[derive(Clone, Debug)]
struct ConnectorIdentity {
    pid: u32,
    executable: PathBuf,
    managed: bool,
}

fn clean(value: &str, limit: usize) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .take(limit)
        .collect()
}

fn safe_value(value: &str, what: &str) -> Result<()> {
    if value.chars().any(char::is_control) {
        return Err(Failure::keyed(
            "service.unsafe-value",
            json!({"what":clean(what,80)}),
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn xml(value: &str, what: &str) -> Result<String> {
    safe_value(value, what)?;
    Ok(value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;"))
}

fn validate_label(label: &str) -> Result<()> {
    if !label.starts_with("dev.sidevoice.rustproof.")
        || !label
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '-')
    {
        return Err(Failure::keyed(
            "service.unsafe-value",
            json!({"what":"Label"}),
        ));
    }
    Ok(())
}

fn checked_current(root: &Path, current: &Path, required: bool) -> Result<PathBuf> {
    match fs::symlink_metadata(current) {
        Err(error) if error.kind() == io::ErrorKind::NotFound && !required => {
            return Ok(root.join("releases/current"));
        }
        Err(_) => return Err(Failure::keyed("service.no-installation", json!({}))),
        Ok(metadata)
            if !metadata.file_type().is_symlink()
                || metadata.uid() != unsafe { libc::geteuid() } =>
        {
            return Err(Failure::keyed("service.no-installation", json!({})));
        }
        Ok(_) => {}
    }
    let target = fs::read_link(current).map_err(Failure::plain)?;
    if target.is_absolute() || target.components().count() != 1 {
        return Err(Failure::keyed("service.no-installation", json!({})));
    }
    let releases = root.join("releases");
    private_dir(&releases).map_err(Failure::plain)?;
    let selected = releases.join(target);
    private_dir(&selected).map_err(Failure::plain)?;
    let releases = releases.canonicalize().map_err(Failure::plain)?;
    let canonical = selected.canonicalize().map_err(Failure::plain)?;
    if !canonical.starts_with(&releases) || canonical.parent() != Some(releases.as_path()) {
        return Err(Failure::keyed("service.no-installation", json!({})));
    }
    Ok(canonical)
}

fn validate_executable(root: &Path, path: &Path) -> Result<()> {
    let canonical_root = root.canonicalize().map_err(Failure::plain)?;
    let canonical = path
        .canonicalize()
        .map_err(|_| Failure::keyed("service.no-installation", json!({})))?;
    if !canonical.starts_with(&canonical_root) {
        return Err(Failure::keyed("service.no-installation", json!({})));
    }
    let metadata = fs::symlink_metadata(path).map_err(Failure::plain)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.mode() & 0o100 == 0
    {
        return Err(Failure::keyed("service.no-installation", json!({})));
    }
    Ok(())
}

fn check_file_under(root: &Path, path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(Failure::plain)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || !path
            .canonicalize()
            .map_err(Failure::plain)?
            .starts_with(root)
    {
        return Err(Failure::keyed("service.no-installation", json!({})));
    }
    private_file(path).map_err(Failure::plain)
}

#[cfg(any(target_os = "macos", test))]
fn plist(spec: &ServiceSpec, core: bool) -> Result<String> {
    let (label, argv, env, log, keep_alive) = if core {
        (&spec.core_job, &spec.core_argv, &spec.core_environment,
         spec.data_dir.join("service/core.log"), "<dict>\n    <key>SuccessfulExit</key>\n    <false/>\n    <key>Crashed</key>\n    <true/>\n  </dict>")
    } else {
        (
            &spec.connector_job,
            &spec.connector_argv,
            &spec.connector_environment,
            spec.data_dir.join("service/connector.log"),
            "<true/>",
        )
    };
    let args = argv
        .iter()
        .map(|arg| {
            Ok(format!(
                "    <string>{}</string>",
                xml(arg, "ProgramArguments")?
            ))
        })
        .collect::<Result<Vec<_>>>()?
        .join("\n");
    let variables = env
        .iter()
        .map(|(key, value)| {
            Ok(format!(
                "    <key>{}</key>\n    <string>{}</string>",
                xml(key, key)?,
                xml(value, key)?
            ))
        })
        .collect::<Result<Vec<_>>>()?
        .join("\n");
    Ok(format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{}</string>\n  <key>ProgramArguments</key>\n  <array>\n{}\n  </array>\n  <key>EnvironmentVariables</key>\n  <dict>\n{}\n  </dict>\n  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  {}\n  <key>ThrottleInterval</key>\n  <integer>10</integer>\n  <key>StandardOutPath</key>\n  <string>{}</string>\n  <key>StandardErrorPath</key>\n  <string>{}</string>\n</dict>\n</plist>\n",
        xml(label,"Label")?,args,variables,keep_alive,xml(&log.to_string_lossy(),"StandardOutPath")?,xml(&log.to_string_lossy(),"StandardErrorPath")?))
}

#[cfg(target_os = "macos")]
fn preflight_definition(file: &Path) -> Result<()> {
    let parent = file
        .parent()
        .context("definition parent")
        .map_err(Failure::plain)?;
    match fs::symlink_metadata(parent) {
        Ok(_) => private_dir(parent).map_err(|_| {
            Failure::keyed(
                "service.definition-unsafe",
                json!({"detail":"service directory"}),
            )
        })?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(Failure::plain(error)),
    }
    match fs::symlink_metadata(file) {
        Ok(_) => private_file(file).map_err(|_| {
            Failure::keyed(
                "service.definition-unsafe",
                json!({"detail":file.file_name().unwrap_or_default().to_string_lossy()}),
            )
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Failure::plain(error)),
    }
}

#[cfg(target_os = "macos")]
fn write_definition(file: &Path, contents: &str) -> Result<bool> {
    let parent = file
        .parent()
        .context("definition parent")
        .map_err(Failure::plain)?;
    preflight_definition(file)?;
    private_dir(parent).map_err(Failure::plain)?;
    if fs::read(file).ok().as_deref() == Some(contents.as_bytes()) {
        return Ok(false);
    }
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        file.file_name().unwrap().to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| -> Result<()> {
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp)
            .map_err(Failure::plain)?;
        use std::io::Write;
        output
            .write_all(contents.as_bytes())
            .map_err(Failure::plain)?;
        output.sync_all().map_err(Failure::plain)?;
        fs::rename(&temp, file).map_err(Failure::plain)?;
        File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(Failure::plain)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result?;
    Ok(true)
}

fn existing_definition(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Failure::plain(error)),
        Ok(_) => {
            private_file(path).map_err(|_| {
                Failure::keyed(
                    "service.definition-unsafe",
                    json!({
                "detail":path.file_name().unwrap_or_default().to_string_lossy()}),
                )
            })?;
            let metadata = fs::metadata(path).map_err(Failure::plain)?;
            if metadata.len() > 128 * 1024 {
                return Err(Failure::keyed(
                    "service.definition-unsafe",
                    json!({"detail":"oversized definition"}),
                ));
            }
            Ok(true)
        }
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct ManagerOutput {
    success: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

#[cfg(target_os = "macos")]
async fn read_bounded<R: tokio::io::AsyncRead + Unpin>(mut reader: R) -> io::Result<String> {
    let mut kept = Vec::with_capacity(OUTPUT_LIMIT.min(4096));
    let mut scratch = [0u8; 4096];
    loop {
        let count = reader.read(&mut scratch).await?;
        if count == 0 {
            break;
        }
        let remaining = OUTPUT_LIMIT.saturating_sub(kept.len());
        kept.extend_from_slice(&scratch[..count.min(remaining)]);
    }
    Ok(String::from_utf8_lossy(&kept).into_owned())
}

#[cfg(target_os = "macos")]
async fn reap(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(target_os = "macos")]
async fn launchctl(args: &[String]) -> Result<ManagerOutput> {
    let mut child = Command::new("/bin/launchctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(Failure::plain)?;
    let stdout = child
        .stdout
        .take()
        .context("launchctl stdout")
        .map_err(Failure::plain)?;
    let stderr = child
        .stderr
        .take()
        .context("launchctl stderr")
        .map_err(Failure::plain)?;
    let out_task = tokio::spawn(read_bounded(stdout));
    let err_task = tokio::spawn(read_bounded(stderr));
    let status = tokio::select! {
        result=timeout(MANAGER_LIMIT,child.wait())=>match result {
            Ok(Ok(status))=>status,
            Ok(Err(error))=>return Err(Failure::plain(error)),
            Err(_)=>{reap(&mut child).await;return Err(Failure::keyed("service.manager-unavailable",json!({"detail":"launchctl timed out"})));}
        },
        signal=tokio::signal::ctrl_c()=>{
            reap(&mut child).await;
            let _=signal;
            return Err(Failure::keyed("service.manager-unavailable",json!({"detail":"launchctl command cancelled"})));
        }
    };
    let stdout = out_task
        .await
        .map_err(Failure::plain)?
        .map_err(Failure::plain)?;
    let stderr = err_task
        .await
        .map_err(Failure::plain)?
        .map_err(Failure::plain)?;
    Ok(ManagerOutput {
        success: status.success(),
        code: status.code(),
        stdout,
        stderr,
    })
}

#[cfg(target_os = "macos")]
fn launchd_domain() -> String {
    format!("gui/{}", unsafe { libc::geteuid() })
}
#[cfg(target_os = "macos")]
fn target(label: &str) -> String {
    format!("{}/{}", launchd_domain(), label)
}

#[cfg(target_os = "macos")]
fn parse_job(output: &ManagerOutput, defined: bool) -> Job {
    let combined = format!("{}\n{}", output.stdout, output.stderr);
    if !output.success {
        if output.code == Some(113)
            || combined
                .to_ascii_lowercase()
                .contains("could not find service")
        {
            return Job {
                defined,
                ..Job::default()
            };
        }
        return Job {
            defined,
            unknown: true,
            reason: Some("manager-unavailable".into()),
            ..Job::default()
        };
    }
    let field = |name: &str| -> Option<String> {
        combined.lines().find_map(|line| {
            let line = line.trim();
            let (key, value) = line.split_once(" = ")?;
            (key == name).then(|| value.trim().to_owned())
        })
    };
    let pid = field("pid").and_then(|value| value.parse().ok());
    let running = field("state").as_deref() == Some("running") && pid.is_some();
    let exit = field("last exit code").and_then(|value| value.parse().ok());
    let signal = field("last terminating signal").filter(|value| !value.is_empty());
    let runs = field("runs").and_then(|value| value.parse().ok());
    let restarting = !running && (exit.is_some_and(|value| value != 0) || signal.is_some());
    Job {
        defined,
        loaded: true,
        running,
        unknown: false,
        pid: if running { pid } else { None },
        exit,
        signal,
        runs,
        restarting,
        reason: None,
    }
}

#[cfg(target_os = "macos")]
async fn manager_job(label: &str, defined: bool) -> Result<Job> {
    let output = launchctl(&["print".into(), target(label)]).await?;
    Ok(parse_job(&output, defined))
}

fn stop_marker(profile: &Profile) -> Result<bool> {
    let path = profile.data.join("node-stopped.json");
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Failure::plain(error)),
        Ok(_) => {
            private_file(&path).map_err(|_| {
                Failure::keyed(
                    "service.definition-unsafe",
                    json!({"detail":"node-stopped.json"}),
                )
            })?;
            Ok(true)
        }
    }
}

#[cfg(target_os = "macos")]
async fn install_lock(profile: &Profile) -> Result<File> {
    let path = profile.data.join("install.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(Failure::plain)?;
    private_file(&path).map_err(Failure::plain)?;
    timeout(Duration::from_secs(30), async {
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    sleep(Duration::from_millis(20)).await
                }
                Err(error) => return Err(Failure::plain(error)),
            }
        }
    })
    .await
    .map_err(|_| {
        Failure::keyed(
            "service.busy",
            json!({"detail":"another install or service command"}),
        )
    })??;
    Ok(file)
}

#[cfg(target_os = "macos")]
async fn agents_lock(profile: &Profile) -> Result<File> {
    let path = profile.data.join("agents.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(Failure::plain)?;
    private_file(&path).map_err(Failure::plain)?;
    timeout(Duration::from_secs(18), async {
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    sleep(Duration::from_millis(10)).await
                }
                Err(error) => return Err(Failure::plain(error)),
            }
        }
    })
    .await
    .map_err(|_| Failure::keyed("service.busy", json!({"detail":"host agent scan"})))??;
    Ok(file)
}

#[cfg(target_os = "macos")]
async fn set_stopped(profile: &Profile, stopped: bool) -> Result<()> {
    let _agents = agents_lock(profile).await?;
    let marker = profile.data.join("node-stopped.json");
    match fs::symlink_metadata(&marker) {
        Ok(_) => private_file(&marker).map_err(|_| {
            Failure::keyed(
                "service.definition-unsafe",
                json!({"detail":"node-stopped.json"}),
            )
        })?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(Failure::plain(error)),
    }
    if stopped {
        atomic_json(&marker, &json!({"at":chrono_free_iso()})).map_err(Failure::plain)?;
    } else {
        match fs::symlink_metadata(&marker) {
            Ok(_) => {
                private_file(&marker).map_err(|_| {
                    Failure::keyed(
                        "service.definition-unsafe",
                        json!({"detail":"node-stopped.json"}),
                    )
                })?;
                fs::remove_file(&marker).map_err(Failure::plain)?;
                File::open(&profile.data)
                    .and_then(|dir| dir.sync_all())
                    .map_err(Failure::plain)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(Failure::plain(error)),
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn chrono_free_iso() -> String {
    // A stable RFC3339 UTC marker; no date crate is needed for this private stop-intent record.
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (seconds / 86_400) as i64;
    let day_seconds = seconds % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        day_seconds / 3600,
        (day_seconds % 3600) / 60,
        day_seconds % 60
    )
}

fn read_failure(profile: &Profile) -> Option<Value> {
    let path = profile.data.join("core/core-failure.json");
    let metadata = fs::symlink_metadata(&path).ok()?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() > 64 * 1024
    {
        return None;
    }
    let value: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let key = value
        .get("key")
        .and_then(Value::as_str)
        .map(|s| clean(s, 80))?;
    let step = value
        .get("step")
        .and_then(Value::as_str)
        .map(|s| clean(s, 40));
    let at = value
        .get("at")
        .and_then(Value::as_str)
        .map(|s| clean(s, 40));
    let params = if matches!(
        key.as_str(),
        "import.missing-module"
            | "launch.exited"
            | "start.failed"
            | "launch.permission"
            | "launch.missing-executable"
    ) {
        json!({"detail":"the Core process reported a startup failure"})
    } else {
        Value::Null
    };
    let mut result = json!({"key":key});
    if let Some(step) = step {
        result["step"] = json!(step);
    }
    if let Some(at) = at {
        result["at"] = json!(at);
    }
    result["message"] = json!(message(&key, &params));
    Some(result)
}

async fn connector_request(
    profile: &Profile,
    method: &str,
    params: Value,
) -> anyhow::Result<Option<Value>> {
    profile.validate_existing_private()?;
    if !profile.socket.exists() {
        return Ok(None);
    }
    crate::proof::verify_socket(&profile.socket)?;
    let mut stream = match timeout(
        Duration::from_millis(800),
        UnixStream::connect(&profile.socket),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) if matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        ) => return Ok(None),
        Ok(Err(error)) => return Err(error.into()),
        Err(_) => return Ok(None),
    };
    stream
        .write_all(json!({"id":1,"method":method,"params":params}).to_string().as_bytes())
        .await?;
    stream.write_all(b"\n").await?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    timeout(Duration::from_secs(2), reader.read_line(&mut line)).await??;
    if line.is_empty() {
        return Ok(None);
    }
    if line.len() > 65536 {
        anyhow::bail!("connector IPC reply exceeded its limit");
    }
    let response: Value = serde_json::from_str(&line)?;
    if response.get("ok") != Some(&json!(true)) {
        anyhow::bail!("connector IPC request was refused");
    }
    Ok(response.get("result").cloned())
}

async fn connector_identity(profile: &Profile) -> anyhow::Result<Option<ConnectorIdentity>> {
    let Some(value) = connector_request(profile, "identity", json!({})).await? else {
        return Ok(None);
    };
    let pid = value
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid > 0)
        .context("connector PID missing")?;
    let executable = value
        .get("executable")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .context("connector executable missing")?;
    let managed = value
        .get("managed")
        .and_then(Value::as_bool)
        .context("connector manager identity missing")?;
    Ok(Some(ConnectorIdentity {
        pid,
        executable,
        managed,
    }))
}

fn verified_profile_executable(profile: &Profile, path: &Path) -> bool {
    validate_executable(&profile.root, path).is_ok()
        && path
            .canonicalize()
            .ok()
            .and_then(|canonical| canonical.strip_prefix(&profile.root).ok().map(Path::to_path_buf))
            .is_some_and(|relative| {
                let parts = relative.components().collect::<Vec<_>>();
                parts.len() == 4
                    && parts[0].as_os_str() == "releases"
                    && parts[2].as_os_str() == "dist"
                    && parts[3].as_os_str() == "sidevoice-rust-proof"
            })
}

#[cfg(target_os = "macos")]
async fn wait_connector_gone(profile: &Profile, pid: Option<u32>) -> Result<()> {
    let deadline = Instant::now() + STOP_LIMIT;
    while Instant::now() < deadline {
        let lock_free = connector_stopped(profile);
        if lock_free && profile.socket.exists() {
            remove_stale_socket(&profile.socket).map_err(Failure::plain)?;
        }
        let process_gone = pid.map_or(true, |pid| !process_alive(pid));
        if lock_free && process_gone && !profile.socket.exists() {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    Err(Failure::keyed(
        "service.unload-failed",
        json!({"detail":"verified on-demand connector did not stop"}),
    ))
}

#[cfg(target_os = "macos")]
async fn stop_on_demand_connector(profile: &Profile, spec: &ServiceSpec) -> Result<()> {
    let socket_exists = profile.socket.exists();
    let lock_held = !connector_stopped(profile);
    if !socket_exists && !lock_held {
        return Ok(());
    }

    let identity = connector_identity(profile).await;
    let identity = match identity {
        Ok(Some(identity)) => identity,
        Ok(None) if !lock_held => return wait_connector_gone(profile, None).await,
        Ok(None) => {
            return Err(Failure::keyed(
                "service.unload-failed",
                json!({"detail":"connector socket owner could not be verified"}),
            ));
        }
        Err(_) if !lock_held => return wait_connector_gone(profile, None).await,
        Err(error) => {
            return Err(Failure::keyed(
                "service.unload-failed",
                json!({"detail":clean(&error.to_string(),240)}),
            ));
        }
    };

    if identity.managed {
        let state = manager_job(&spec.connector_job, true).await?;
        if state.running && state.pid == Some(identity.pid) {
            return Ok(());
        }
        if state.running {
            return Err(Failure::keyed(
                "service.unload-failed",
                json!({"detail":"connector socket PID does not match its launchd job"}),
            ));
        }
        return wait_connector_gone(profile, Some(identity.pid)).await;
    }

    if !verified_profile_executable(profile, &identity.executable) {
        return Err(Failure::keyed(
            "service.unload-failed",
            json!({"detail":"on-demand connector executable is outside the private release"}),
        ));
    }
    let shutdown = connector_request(
        profile,
        "shutdown",
        json!({"expected_pid":identity.pid,"expected_executable":identity.executable}),
    )
    .await;
    if let Ok(Some(value)) = shutdown {
        if value.get("stopping") == Some(&json!(true))
            && value.get("pid").and_then(Value::as_u64) == Some(identity.pid as u64)
        {
            return wait_connector_gone(profile, Some(identity.pid)).await;
        }
    }
    wait_connector_gone(profile, Some(identity.pid)).await
}

fn parse_elapsed(value: &str) -> Option<u64> {
    let value = value.trim();
    let (days, clock) = if let Some((days, clock)) = value.split_once('-') {
        (days.parse::<u64>().ok()?, clock)
    } else {
        (0, value)
    };
    let parts = clock
        .split(':')
        .map(str::parse::<u64>)
        .collect::<std::result::Result<Vec<_>, _>>()
        .ok()?;
    let seconds = match parts.as_slice() {
        [minutes, seconds] => *minutes * 60 + *seconds,
        [hours, minutes, seconds] => *hours * 3600 + *minutes * 60 + *seconds,
        _ => return None,
    };
    Some(days * 86400 + seconds)
}

async fn process_age(pid: u32) -> Option<u64> {
    let pid = pid.to_string();
    let mut child = Command::new("/bin/ps")
        .args(["-o", "etime=", "-p", pid.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut output = Vec::new();
    let mut stdout = child.stdout.take()?;
    let read = timeout(Duration::from_secs(2), stdout.read_to_end(&mut output));
    let waited = timeout(Duration::from_secs(2), child.wait());
    let (read, waited) = tokio::join!(read, waited);
    if read.ok()?.ok().is_none() || !waited.ok()?.ok()?.success() {
        return None;
    }
    parse_elapsed(&String::from_utf8_lossy(&output))
}

async fn verify_connector_socket(profile: &Profile) -> anyhow::Result<()> {
    if connector_identity(profile).await?.is_none() {
        anyhow::bail!("connector status refused");
    }
    Ok(())
}

pub async fn status(profile: &Profile, connector_self: bool) -> Value {
    match observe(profile, connector_self).await {
        Ok(observation) => derive_status(&observation),
        Err(error) => {
            let detail = clean(&error.to_string(), 240);
            json!({"ok":true,"service":if cfg!(target_os="macos"){"launchd"}else{"systemd"},
                "installed":profile.root.join("releases/current").exists(),"state":"service-failed","core":Value::Null,
                "calls":Value::Null,"failure":{"key":"service.manager-unavailable","params":{"detail":detail},
                    "message":message("service.manager-unavailable",&json!({"detail":detail}))},
                "attempts":Value::Null,"limit":Value::Null,"since":Value::Null,"window_started":Value::Null,
                "next_retry_at":Value::Null,"reachable":false,"connector":{"running":connector_self}})
        }
    }
}

pub async fn ensure_connector(profile: &Profile) -> anyhow::Result<()> {
    // The established Rust daemon wins the race first, including after a refused service bootout.
    if verify_connector_socket(profile).await.is_ok() {
        return Ok(());
    }
    if profile.service_stopped()? {
        return Err(anyhow::Error::new(Failure::keyed(
            "service.node-stopped",
            json!({}),
        )));
    }
    let spec = ServiceSpec::for_private_fixture(profile).map_err(anyhow::Error::new)?;
    let managed = existing_definition(&spec.core_definition).map_err(anyhow::Error::new)?
        || existing_definition(&spec.connector_definition).map_err(anyhow::Error::new)?;
    if managed {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if verify_connector_socket(profile).await.is_ok() {
                return Ok(());
            }
            sleep(Duration::from_millis(100)).await;
        }
        let now = status(profile, false).await;
        let state = now
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("service-failed");
        return Err(anyhow::Error::new(Failure::keyed(
            "service.not-loaded",
            json!({"detail":state}),
        )));
    }
    spec.validate_programs(profile)
        .map_err(anyhow::Error::new)?;
    let executable = std::env::current_exe()?.canonicalize()?;
    let staged = profile
        .root
        .join("releases/current/dist/sidevoice-rust-proof");
    let selected = staged.canonicalize()?;
    if executable != selected || !executable.starts_with(&profile.root) {
        return Err(anyhow::Error::new(Failure::keyed(
            "service.no-installation",
            json!({}),
        )));
    }
    let mut command = Command::new(executable);
    command
        .args(["connector", "--profile-root"])
        .arg(&profile.root)
        .env_clear()
        .env(
            "PATH",
            "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    profile.command_env(&mut command);
    let mut child = command.spawn()?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if verify_connector_socket(profile).await.is_ok() {
            return Ok(());
        }
        if profile.service_stopped()? {
            return Err(anyhow::Error::new(Failure::keyed(
                "service.node-stopped",
                json!({}),
            )));
        }
        sleep(Duration::from_millis(100)).await;
    }
    Err(anyhow::Error::new(Failure::keyed(
        "service.not-loaded",
        json!({"detail":"connector socket did not appear"}),
    )))
}

async fn observe(profile: &Profile, connector_self: bool) -> anyhow::Result<Observation> {
    let spec = ServiceSpec::for_private_fixture(profile).map_err(anyhow::Error::new)?;
    let current_valid =
        checked_current(&profile.root, &profile.root.join("releases/current"), false).is_ok()
            && profile.root.join("releases/current").exists();
    let core_result = existing_definition(&spec.core_definition);
    let connector_result = existing_definition(&spec.connector_definition);
    let core_defined = core_result.as_ref().is_ok_and(|defined| *defined)
        || fs::symlink_metadata(&spec.core_definition).is_ok();
    let connector_defined = connector_result.as_ref().is_ok_and(|defined| *defined)
        || fs::symlink_metadata(&spec.connector_definition).is_ok();
    #[cfg(target_os = "macos")]
    let (mut core, mut connector, manager_error) = {
        let core = manager_job(&spec.core_job, core_defined)
            .await
            .unwrap_or_else(|_| Job {
                defined: core_defined,
                unknown: true,
                reason: Some("manager-unavailable".into()),
                ..Job::default()
            });
        let connector = manager_job(&spec.connector_job, connector_defined)
            .await
            .unwrap_or_else(|_| Job {
                defined: connector_defined,
                unknown: true,
                reason: Some("manager-unavailable".into()),
                ..Job::default()
            });
        let error = if core.unknown || connector.unknown {
            Some("launchctl could not confirm a private service job".to_owned())
        } else {
            None
        };
        (core, connector, error)
    };
    #[cfg(not(target_os = "macos"))]
    let (mut core, mut connector, manager_error) = (
        Job {
            defined: core_defined,
            unknown: true,
            reason: Some("manager-unavailable".into()),
            ..Job::default()
        },
        Job {
            defined: connector_defined,
            unknown: true,
            reason: Some("manager-unavailable".into()),
            ..Job::default()
        },
        Some("Linux service support is not enabled in the private Rust proof".to_owned()),
    );
    let mut definition_error = None;
    if let Err(error) = core_result {
        definition_error = Some(error.to_string());
        core.unknown = true;
    }
    if let Err(error) = connector_result {
        definition_error = Some(error.to_string());
        connector.unknown = true;
    }
    let ready = profile.read_ready().ok();
    let health = match ready.as_ref() {
        Some(ready) => profile.health_ready(ready).await.ok(),
        None => None,
    };
    let stopped = stop_marker(profile).unwrap_or(true);
    let failure = read_failure(profile);
    let core_age = match core.pid {
        Some(pid) => process_age(pid).await,
        None => None,
    };
    let identity = if connector_self {
        None
    } else {
        connector_identity(profile).await.ok().flatten()
    };
    let connector_pid = if connector_self {
        Some(std::process::id())
    } else {
        identity.as_ref().map(|identity| identity.pid)
    };
    let connector_is_running = match connector_pid {
        Some(pid) if connector.defined => {
            connector.running
                && connector.pid == Some(pid)
                && (connector_self || identity.as_ref().is_some_and(|identity| identity.managed))
        }
        Some(_) if connector_self => true,
        Some(_) => identity.as_ref().is_some_and(|identity| {
            !identity.managed && verified_profile_executable(profile, &identity.executable)
        }),
        None => false,
    };
    let program_error = if core_defined
        && !profile
            .root
            .join("releases/current/core/bin/sidevoice-core")
            .exists()
    {
        Some("service.executable-missing")
    } else {
        None
    };
    Ok(Observation {
        service: if cfg!(target_os = "macos") {
            "launchd"
        } else {
            "systemd"
        },
        installed: current_valid,
        core,
        connector,
        stopped,
        health,
        ready,
        failure,
        core_age,
        connector_running: connector_is_running,
        definition_error,
        manager_error,
        program_error,
    })
}

fn derive_status(observation: &Observation) -> Value {
    let health = observation.health.as_ref();
    let health_pid = health
        .and_then(|value| value.get("pid"))
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok());
    let ready_pid = observation.ready.as_ref().map(|ready| ready.pid);
    let core_identity_matches = if observation.core.defined {
        observation.core.running
            && observation.core.pid == ready_pid
            && observation.core.pid == health_pid
    } else {
        ready_pid.is_some() && ready_pid == health_pid
    };
    let body = health.filter(|_| health_pid.is_some() && core_identity_matches);
    let reachable = body.is_some();
    let core=body.map(|body|json!({"pid":body.get("pid").cloned().unwrap_or(Value::Null),
        "version":body.get("version").cloned().unwrap_or(Value::Null),"api":body.get("api").cloned().unwrap_or(Value::Null),
        "launch_id":body.get("launch_id").cloned().unwrap_or(Value::Null)}));
    let calls = body
        .and_then(|value| value.get("calls"))
        .filter(|value| value.is_number())
        .cloned()
        .unwrap_or(Value::Null);
    let base = json!({"ok":true,"service":observation.service,"installed":observation.installed,"core":core,
        "calls":calls,"failure":Value::Null,"attempts":Value::Null,"limit":Value::Null,"since":Value::Null,
        "window_started":Value::Null,"next_retry_at":Value::Null,"reachable":reachable,
        "connector":{"running":observation.connector_running}});
    let mut result = base;
    let state;
    let defined_core = observation.core.defined;
    let defined_any = defined_core || observation.connector.defined;
    if !observation.installed && !defined_any {
        state = "absent";
    } else if !defined_core {
        state = "not-installed";
    } else if observation.stopped {
        state = "stopped-by-person";
    } else if let Some(detail) = observation
        .definition_error
        .as_deref()
        .or(observation.manager_error.as_deref())
    {
        state = "service-failed";
        let key = if observation.definition_error.is_some() {
            "service.definition-unsafe"
        } else {
            "service.manager-unavailable"
        };
        result["failure"] = json!({"key":key,"params":{"detail":clean(detail,240)},"message":message(key,&json!({"detail":clean(detail,240)}))});
    } else if let Some(key) = observation.program_error {
        state = "service-failed";
        let params = json!({"detail":"the staged Core executable is missing"});
        result["failure"] = json!({"key":key,"params":params,"message":message(key,&params)});
    } else if observation.core.unknown
        || !observation.core.loaded
        || observation.core.reason.is_some()
    {
        state = "service-failed";
        let key = if observation.core.unknown {
            "service.manager-unavailable"
        } else {
            "service.not-loaded"
        };
        result["failure"] = json!({"key":key,"message":message(key,&json!({"detail":"launchd"}))});
    } else if reachable {
        state = "running";
    } else if observation.core.running {
        if observation
            .ready
            .as_ref()
            .is_some_and(|ready| Some(ready.pid) == observation.core.pid)
        {
            state = "failed";
            result["failure"] =
                json!({"key":"hang","step":"health","message":message("hang",&Value::Null)});
        } else if observation.core_age.is_some_and(|age| age > STARTING_LIMIT) {
            state = "failed";
            result["failure"] = json!({"key":"ready.timeout","step":"ready","message":message("ready.timeout",&Value::Null)});
        } else {
            state = "starting";
        }
    } else if let Some(failure) = &observation.failure {
        state = "failed";
        result["failure"] = failure.clone();
    } else if observation.core.restarting {
        state = "backoff";
        let detail = observation
            .core
            .signal
            .clone()
            .or_else(|| observation.core.exit.map(|code| code.to_string()))
            .unwrap_or_else(|| "?".into());
        result["attempts"] = observation
            .core
            .runs
            .map(Value::from)
            .unwrap_or(Value::Null);
        result["failure"] = json!({"key":"launch.exited","step":"run","detail":detail,"message":message("launch.exited",&json!({"detail":detail}))});
    } else {
        state = "failed";
        let detail = observation
            .core
            .signal
            .clone()
            .or_else(|| observation.core.exit.map(|code| code.to_string()))
            .unwrap_or_else(|| "?".into());
        result["failure"] = json!({"key":"launch.exited","step":"run","detail":detail,"message":message("launch.exited",&json!({"detail":detail}))});
    }
    result["state"] = json!(state);
    result
}

pub async fn run(profile: Profile, action: Action) -> Value {
    if action == Action::Status {
        return status(&profile, false).await;
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = profile;
        Failure::keyed("service.unsupported", json!({})).value()
    }
    #[cfg(target_os = "macos")]
    {
        match mutate(&profile, action).await {
            Ok(value) => value,
            Err(error) => error.value(),
        }
    }
}

#[cfg(target_os = "macos")]
async fn mutate(profile: &Profile, action: Action) -> Result<Value> {
    profile.validate_private().map_err(Failure::plain)?;
    let spec = ServiceSpec::for_private_fixture(profile)?;
    let lock = install_lock(profile).await?;
    let result = match action {
        Action::Install => install(profile, &spec).await,
        Action::Start => start(profile, &spec).await,
        Action::Stop => stop(profile, &spec, false).await,
        Action::Restart => restart(profile, &spec).await,
        Action::Uninstall => stop(profile, &spec, true).await,
        Action::Status => unreachable!(),
    };
    drop(lock);
    result
}

#[cfg(target_os = "macos")]
async fn install(profile: &Profile, spec: &ServiceSpec) -> Result<Value> {
    spec.validate_programs(profile)?;
    let core_text = plist(spec, true)?;
    let connector_text = plist(spec, false)?;
    let service_dir = profile.data.join("service");
    preflight_definition(&spec.core_definition)?;
    preflight_definition(&spec.connector_definition)?;
    set_stopped(profile, false).await?;
    match fs::symlink_metadata(&service_dir) {
        Ok(_) => private_dir(&service_dir).map_err(|_| {
            Failure::keyed(
                "service.definition-unsafe",
                json!({"detail":"service directory"}),
            )
        })?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(&service_dir).map_err(Failure::plain)?;
            fs::set_permissions(&service_dir, fs::Permissions::from_mode(0o700))
                .map_err(Failure::plain)?;
            private_dir(&service_dir).map_err(Failure::plain)?;
        }
        Err(error) => return Err(Failure::plain(error)),
    }
    let core_changed = write_definition(&spec.core_definition, &core_text)?;
    let connector_changed = write_definition(&spec.connector_definition, &connector_text)?;
    if core_changed && manager_job(&spec.core_job, true).await?.loaded {
        bootout(&spec.core_job).await?;
    }
    if connector_changed && manager_job(&spec.connector_job, true).await?.loaded {
        bootout(&spec.connector_job).await?;
    }
    stop_on_demand_connector(profile, spec).await?;
    start_job(&spec.core_job, &spec.core_definition).await?;
    start_job(&spec.connector_job, &spec.connector_definition).await?;
    wait_settled(profile, true).await
}

#[cfg(target_os = "macos")]
async fn start(profile: &Profile, spec: &ServiceSpec) -> Result<Value> {
    let core = existing_definition(&spec.core_definition)?;
    let connector = existing_definition(&spec.connector_definition)?;
    if core != connector {
        return Err(Failure::keyed(
            "service.definition-unsafe",
            json!({"detail":"both launchd definitions are required"}),
        ));
    }
    if core {
        spec.validate_programs(profile)?;
    }
    set_stopped(profile, false).await?;
    if core && connector {
        stop_on_demand_connector(profile, spec).await?;
        start_job(&spec.core_job, &spec.core_definition).await?;
        start_job(&spec.connector_job, &spec.connector_definition).await?;
        wait_settled(profile, false).await
    } else {
        Ok(status(profile, false).await)
    }
}

#[cfg(target_os = "macos")]
async fn restart(profile: &Profile, spec: &ServiceSpec) -> Result<Value> {
    if stop_marker(profile)? {
        return start(profile, spec).await;
    }
    if !existing_definition(&spec.core_definition)? {
        return Ok(status(profile, false).await);
    }
    spec.validate_programs(profile)?;
    let result = launchctl(&["kickstart".into(), "-k".into(), target(&spec.core_job)]).await?;
    if !result.success {
        return Err(Failure::keyed(
            "service.not-loaded",
            json!({"detail":clean(&result.stderr,240)}),
        ));
    }
    wait_settled(profile, false).await
}

#[cfg(target_os = "macos")]
async fn stop(profile: &Profile, spec: &ServiceSpec, uninstall: bool) -> Result<Value> {
    set_stopped(profile, true).await?;
    let mut errors = Vec::new();
    let mut core_pids = Vec::new();
    for (label, definition) in [
        (&spec.connector_job, &spec.connector_definition),
        (&spec.core_job, &spec.core_definition),
    ] {
        let defined = existing_definition(definition)?;
        let state = manager_job(label, defined).await?;
        if label == &spec.core_job {
            if let Some(pid) = state.pid {
                core_pids.push(pid);
            }
        }
        if state.unknown {
            errors.push(format!("{label}: manager state unknown"));
            continue;
        }
        if state.loaded {
            if let Err(error) = bootout(label).await {
                errors.push(error.to_string());
            }
        }
    }
    if !errors.is_empty() {
        return Err(Failure::keyed(
            "service.unload-failed",
            json!({"detail":clean(&errors.join("; "),400)}),
        ));
    }
    stop_on_demand_connector(profile, spec).await?;
    let deadline = Instant::now() + STOP_LIMIT;
    while Instant::now() < deadline {
        let connector_lock_gone = connector_stopped(profile);
        let core_gone = core_stopped(profile, &mut core_pids);
        if connector_lock_gone && profile.socket.exists() {
            let _ = remove_stale_socket(&profile.socket);
        }
        if core_gone && profile.core_socket.exists() {
            let _ = remove_stale_socket(&profile.core_socket);
        }
        let socket_gone = !profile.socket.exists();
        let core_socket_gone = !profile.core_socket.exists();
        if connector_lock_gone && core_gone && socket_gone && core_socket_gone {
            let _ = remove_stale_ready(&profile.core_ready);
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    if !connector_stopped(profile)
        || !core_stopped(profile, &mut core_pids)
        || profile.socket.exists()
        || profile.core_socket.exists()
    {
        return Err(Failure::keyed(
            "service.unload-failed",
            json!({"detail":"private processes or sockets remain"}),
        ));
    }
    if uninstall {
        for definition in [&spec.connector_definition, &spec.core_definition] {
            if existing_definition(definition)? {
                fs::remove_file(definition).map_err(Failure::plain)?;
            }
        }
        if spec.data_dir.join("service").exists() {
            File::open(spec.data_dir.join("service"))
                .and_then(|dir| dir.sync_all())
                .map_err(Failure::plain)?;
        }
    }
    Ok(status(profile, false).await)
}

#[cfg(target_os = "macos")]
async fn start_job(label: &str, definition: &Path) -> Result<()> {
    let state = manager_job(label, existing_definition(definition)?).await?;
    if state.unknown {
        return Err(Failure::keyed(
            "service.manager-unavailable",
            json!({"detail":"launchctl print"}),
        ));
    }
    if state.loaded && state.running {
        return Ok(());
    }
    if state.loaded {
        let output = launchctl(&["kickstart".into(), target(label)]).await?;
        if !output.success {
            return Err(Failure::keyed(
                "service.not-loaded",
                json!({"detail":clean(&output.stderr,240)}),
            ));
        }
    } else {
        let output = launchctl(&[
            "bootstrap".into(),
            launchd_domain(),
            definition.to_string_lossy().into_owned(),
        ])
        .await?;
        if !output.success {
            let confirmed = manager_job(label, true).await?;
            if !confirmed.loaded {
                return Err(Failure::keyed(
                    "service.not-loaded",
                    json!({"detail":clean(&output.stderr,240)}),
                ));
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let now = manager_job(label, true).await?;
        if now.unknown {
            return Err(Failure::keyed(
                "service.manager-unavailable",
                json!({"detail":"launchctl print"}),
            ));
        }
        if now.loaded {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Failure::keyed(
                "service.not-loaded",
                json!({"detail":label}),
            ));
        }
        sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(target_os = "macos")]
async fn bootout(label: &str) -> Result<()> {
    let before = manager_job(label, true).await?;
    if before.unknown {
        return Err(Failure::keyed(
            "service.manager-unavailable",
            json!({"detail":"launchctl print"}),
        ));
    }
    if !before.loaded {
        return Ok(());
    }
    let output = launchctl(&["bootout".into(), target(label)]).await?;
    let deadline = Instant::now() + STOP_LIMIT;
    loop {
        let now = manager_job(label, true).await?;
        if !now.unknown && !now.loaded {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Failure::keyed(
                "service.unload-failed",
                json!({"detail":if output.success {label.to_owned()}else{clean(&output.stderr,240)}}),
            ));
        }
        sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(target_os = "macos")]
fn connector_stopped(profile: &Profile) -> bool {
    let path = profile.data.join("connector.lock");
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return true,
        Err(_) => return false,
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => return false,
        Ok(_) => {}
    }
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
    else {
        return false;
    };
    if private_file(&path).is_err() {
        return false;
    }
    match file.try_lock_exclusive() {
        Ok(()) => true,
        Err(_) => false,
    }
}

#[cfg(target_os = "macos")]
fn core_stopped(profile: &Profile, pids: &mut Vec<u32>) -> bool {
    if let Ok(bytes) = fs::read(&profile.core_ready) {
        if let Ok(ready) = serde_json::from_slice::<Ready>(&bytes) {
            pids.push(ready.pid);
        }
    }
    pids.sort_unstable();
    pids.dedup();
    !pids.iter().copied().any(process_alive)
}

#[cfg(target_os = "macos")]
fn remove_stale_socket(path: &Path) -> io::Result<()> {
    crate::proof::verify_socket(path)
        .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error))?;
    fs::remove_file(path)?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn remove_stale_ready(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(_) => {
            private_file(path)
                .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error))?;
            fs::remove_file(path)?;
            File::open(path.parent().unwrap())?.sync_all()
        }
    }
}

#[cfg(target_os = "macos")]
fn process_alive(pid: u32) -> bool {
    unsafe {
        libc::kill(pid as i32, 0) == 0
            || io::Error::last_os_error().kind() == io::ErrorKind::PermissionDenied
    }
}

#[cfg(target_os = "macos")]
async fn wait_settled(profile: &Profile, sixty_seconds: bool) -> Result<Value> {
    let deadline = Instant::now()
        + if sixty_seconds {
            START_LIMIT
        } else {
            Duration::from_secs(10)
        };
    loop {
        let now = status(profile, false).await;
        if now.get("state").and_then(Value::as_str) == Some("running")
            && now.pointer("/connector/running").and_then(Value::as_bool) == Some(true)
        {
            return Ok(now);
        }
        if now
            .get("state")
            .and_then(Value::as_str)
            .is_some_and(|state| matches!(state, "failed" | "service-failed" | "stopped-by-person"))
            && !manager_restart_transient(&now)
        {
            return Err(Failure::keyed(
                "service.not-loaded",
                json!({"detail":now.pointer("/failure/key").and_then(Value::as_str).unwrap_or("core health not confirmed")}),
            ));
        }
        if Instant::now() >= deadline {
            return Err(Failure::keyed(
                "service.not-loaded",
                json!({"detail":"Core and connector health did not settle"}),
            ));
        }
        sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(any(target_os = "macos", test))]
fn manager_restart_transient(status: &Value) -> bool {
    matches!(
        status.get("state").and_then(Value::as_str),
        Some("failed" | "service-failed")
    ) && status.pointer("/failure/key").and_then(Value::as_str) == Some("launch.exited")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation() -> Observation {
        Observation {
            service: "launchd",
            installed: true,
            core: Job {
                defined: true,
                loaded: true,
                running: true,
                pid: Some(7),
                ..Job::default()
            },
            connector: Job {
                defined: true,
                loaded: true,
                running: true,
                pid: Some(8),
                ..Job::default()
            },
            stopped: false,
            health: Some(json!({"pid":7,"version":"0.1.0","api":1,"launch_id":"launch","calls":3})),
            ready: Some(Ready {
                pid: 7,
                launch_id: "launch".into(),
                socket: "/tmp/socket".into(),
                connector_id: "id".into(),
                token: "secret".into(),
                connector_protocols: Some(vec![3]),
            }),
            failure: None,
            core_age: Some(4),
            connector_running: true,
            definition_error: None,
            manager_error: None,
            program_error: None,
        }
    }

    #[test]
    fn launchd_exit_observation_is_retryable_until_health_settles() {
        assert!(manager_restart_transient(
            &json!({"state":"failed","failure":{"key":"launch.exited"}})
        ));
        assert!(manager_restart_transient(
            &json!({"state":"service-failed","failure":{"key":"launch.exited"}})
        ));
        assert!(!manager_restart_transient(
            &json!({"state":"service-failed","failure":{"key":"service.not-loaded"}})
        ));
        assert!(!manager_restart_transient(
            &json!({"state":"stopped-by-person","failure":{"key":"launch.exited"}})
        ));
    }

    #[test]
    fn plist_keeps_private_arguments_and_two_restart_policies() {
        let spec = ServiceSpec {
            data_dir: "/tmp/p/sidevoice".into(),
            release_root: "/tmp/p/releases/r1".into(),
            core_argv: vec![
                "/tmp/p/releases/current/core/bin/sidevoice-core".into(),
                "--idle-exit".into(),
                "0".into(),
            ],
            connector_argv: vec![
                "/tmp/p/releases/current/dist/sidevoice-rust-proof".into(),
                "connector".into(),
                "--service".into(),
                "--profile-root".into(),
                "/tmp/p".into(),
            ],
            core_definition: "/tmp/p/sidevoice/service/core.plist".into(),
            connector_definition: "/tmp/p/sidevoice/service/connector.plist".into(),
            core_job: "dev.sidevoice.rustproof.deadbeef.core".into(),
            connector_job: "dev.sidevoice.rustproof.deadbeef.connector".into(),
            core_environment: BTreeMap::new(),
            connector_environment: BTreeMap::new(),
        };
        let core = plist(&spec, true).unwrap();
        let connector = plist(&spec, false).unwrap();
        assert!(core.contains("<key>SuccessfulExit</key>\n    <false/>"));
        assert!(core.contains("<key>Crashed</key>\n    <true/>"));
        assert!(connector.contains("<key>KeepAlive</key>\n  <true/>"));
        assert!(connector.contains("--profile-root"));
        assert!(connector.contains("<integer>10</integer>"));
    }

    #[test]
    fn plist_rejects_control_characters() {
        assert!(xml("/tmp/p\n<key>Injected</key>", "ProgramArguments").is_err());
    }

    #[test]
    fn status_derivation_matches_the_eight_public_states_and_keeps_precedence() {
        let mut current = observation();
        assert_eq!(derive_status(&current)["state"], "running");
        assert_eq!(derive_status(&current)["calls"], 3);
        current.stopped = true;
        assert_eq!(derive_status(&current)["state"], "stopped-by-person");
        assert_eq!(derive_status(&current)["reachable"], true);
        current.stopped = false;
        current.manager_error = Some("launchctl unavailable".into());
        assert_eq!(derive_status(&current)["state"], "service-failed");
        current.manager_error = None;
        current.core.running = false;
        current.core.pid = None;
        current.health = None;
        current.failure = Some(
            json!({"key":"identity.unreadable","message":"The core could not read this machine's identity."}),
        );
        assert_eq!(derive_status(&current)["state"], "failed");
        current.failure = None;
        current.core.restarting = true;
        current.core.runs = Some(3);
        assert_eq!(derive_status(&current)["state"], "backoff");
        current.core.restarting = false;
        current.core.running = true;
        current.core.pid = Some(7);
        current.ready = Some(Ready {
            pid: 7,
            launch_id: "stale".into(),
            socket: "/tmp/socket".into(),
            connector_id: "id".into(),
            token: "secret".into(),
            connector_protocols: Some(vec![3]),
        });
        assert_eq!(derive_status(&current)["state"], "failed");
        current.ready = Some(Ready {
            pid: 9,
            launch_id: "stale".into(),
            socket: "/tmp/socket".into(),
            connector_id: "id".into(),
            token: "secret".into(),
            connector_protocols: Some(vec![3]),
        });
        assert_eq!(derive_status(&current)["state"], "starting");
        current.core_age = Some(61);
        assert_eq!(derive_status(&current)["failure"]["key"], "ready.timeout");
        current = observation();
        current.ready = Some(Ready {
            pid: 9,
            launch_id: "other-launch".into(),
            socket: "/tmp/socket".into(),
            connector_id: "id".into(),
            token: "secret".into(),
            connector_protocols: Some(vec![3]),
        });
        current.health = Some(json!({"pid":9,"version":"0.1.0","api":1,"launch_id":"other-launch","calls":3}));
        assert_eq!(derive_status(&current)["reachable"], false);
        assert_eq!(derive_status(&current)["state"], "starting");
        current.core_age = Some(61);
        assert_eq!(derive_status(&current)["failure"]["key"], "ready.timeout");
        current.core.loaded = false;
        current.core.running = false;
        assert_eq!(derive_status(&current)["state"], "service-failed");
        current.core.defined = false;
        current.connector.defined = false;
        current.installed = false;
        assert_eq!(derive_status(&current)["state"], "absent");
        current.installed = true;
        assert_eq!(derive_status(&current)["state"], "not-installed");
    }

    #[test]
    fn ready_core_with_failed_health_is_reported_as_a_hang() {
        let mut current = observation();
        current.health = None;
        let status = derive_status(&current);
        assert_eq!(status["state"], "failed");
        assert_eq!(status["failure"]["key"], "hang");
        assert_eq!(status["reachable"], false);
    }
}
