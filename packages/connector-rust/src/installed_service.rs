//! Installed service policy shared by the CLI, daemon and MCP launcher.
use crate::release::{self, Paths};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    process::Command,
    time::{sleep, timeout, Duration, Instant},
};

pub fn manager() -> &'static str {
    if cfg!(target_os = "macos") {
        "launchd"
    } else if cfg!(target_os = "linux") {
        "systemd"
    } else {
        "none"
    }
}
fn label(core: bool) -> &'static str {
    if core {
        "dev.sidevoice.core"
    } else {
        "dev.sidevoice.connector"
    }
}
fn unit(core: bool) -> &'static str {
    if core {
        "sidevoice-core.service"
    } else {
        "sidevoice-connector.service"
    }
}
fn target(core: bool) -> String {
    format!("gui/{}/{}", unsafe { libc::geteuid() }, label(core))
}
pub fn definition(p: &Paths, core: bool) -> Result<PathBuf> {
    let name = if manager() == "launchd" {
        format!("{}.plist", label(core))
    } else {
        unit(core).into()
    };
    let record = release::read_json(&p.data.join("install.json"))?;
    if let Some(paths) = record["definitions"].as_array() {
        for value in paths {
            if let Some(s) = value.as_str() {
                let path = PathBuf::from(s);
                if path.is_absolute() && path.file_name().is_some_and(|v| v == name.as_str()) {
                    return Ok(path);
                }
            }
        }
    }
    Ok(if manager() == "launchd" {
        p.home.join("Library/LaunchAgents").join(name)
    } else {
        p.config.join("systemd/user").join(name)
    })
}
pub fn defined(p: &Paths) -> Result<bool> {
    Ok(definition(p, true)?.exists() || definition(p, false)?.exists())
}
fn safe(s: &str) -> Result<String> {
    if s.chars().any(char::is_control) {
        return Err(crate::release::refusal(
            "control.unsafe-service-definition-value",
            json!({}),
        ));
    }
    Ok(s.into())
}
fn xml(s: &str) -> Result<String> {
    Ok(safe(s)?
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;"))
}
fn quote(s: &str, exec: bool) -> Result<String> {
    let v = safe(s)?
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    Ok(format!(
        "\"{}\"",
        if exec { v.replace('$', "$$") } else { v }
    ))
}
async fn manage(args: &[String]) -> Result<(bool, String, i32)> {
    let executable = if manager() == "launchd" {
        std::env::var("SIDEVOICE_LAUNCHCTL").unwrap_or("/bin/launchctl".into())
    } else {
        std::env::var("SIDEVOICE_SYSTEMCTL").unwrap_or("systemctl".into())
    };
    let mut cmd = Command::new(executable);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let out = child.stdout.take().context("manager stdout")?;
    let err = child.stderr.take().context("manager stderr")?;
    async fn drain<R: tokio::io::AsyncRead + Unpin>(mut r: R) -> Result<Vec<u8>> {
        let mut result = Vec::new();
        let mut b = [0u8; 4096];
        loop {
            let n = r.read(&mut b).await?;
            if n == 0 {
                break;
            }
            let keep = n.min(65536usize.saturating_sub(result.len()));
            result.extend_from_slice(&b[..keep]);
        }
        Ok(result)
    }
    let result = timeout(Duration::from_secs(30), async {
        let (o, e, s) = tokio::join!(drain(out), drain(err), child.wait());
        Ok::<_, anyhow::Error>((s?, o?, e?))
    })
    .await;
    match result {
        Ok(result) => {
            let (s, o, e) = result?;
            Ok((
                s.success(),
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&o),
                    String::from_utf8_lossy(&e)
                ),
                s.code().unwrap_or(-1),
            ))
        }
        Err(e) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(e.into())
        }
    }
}
async fn checked(args: Vec<String>) -> Result<()> {
    let (ok, out, _) = manage(&args).await?;
    if !ok {
        bail!(
            "{}",
            crate::agents::message("service.not-loaded", &json!({"detail":out}))
        );
    }
    Ok(())
}
#[derive(Default)]
struct Job {
    loaded: bool,
    active: bool,
    unknown: bool,
    pid: Option<u32>,
    reason: Option<&'static str>,
    restarting: bool,
}
async fn job(core: bool) -> Result<Job> {
    if manager() == "launchd" {
        let (ok, out, code) = manage(&["print".into(), target(core)]).await?;
        if !ok {
            return Ok(Job {
                unknown: code != 113 && !out.contains("Could not find service"),
                ..Default::default()
            });
        }
        let field = |name: &str| {
            out.lines()
                .find_map(|l| l.trim().strip_prefix(&format!("{name} = ")))
        };
        Ok(Job {
            loaded: true,
            active: field("state") == Some("running"),
            pid: field("pid").and_then(|s| s.parse().ok()),
            restarting: field("state") != Some("running") && field("last exit code") != Some("0"),
            ..Default::default()
        })
    } else {
        let (ok, out, _) = manage(&[
            "--user".into(),
            "show".into(),
            "-p".into(),
            "LoadState,ActiveState,SubState,MainPID,Result".into(),
            unit(core).into(),
        ])
        .await?;
        let field = |name: &str| {
            out.lines()
                .find_map(|l| l.strip_prefix(&format!("{name}=")))
        };
        Ok(Job {
            loaded: field("LoadState") == Some("loaded"),
            active: field("ActiveState") == Some("active")
                || field("ActiveState") == Some("activating"),
            unknown: !ok || field("LoadState").is_none(),
            pid: field("MainPID")
                .and_then(|s| s.parse().ok())
                .filter(|p| *p > 0),
            reason: if field("Result") == Some("start-limit-hit") {
                Some("start-limit")
            } else {
                None
            },
            restarting: field("SubState") == Some("auto-restart"),
        })
    }
}
pub fn stopped(p: &Paths) -> Result<bool> {
    let stop = release::read_json(&p.data.join("node-stopped.json"))?;
    if stop.is_null() {
        return Ok(false);
    }
    let switching = release::read_json(&p.data.join("runtime-switch.json"))?;
    let selected = p.selected("current")?.unwrap_or(Value::Null);
    Ok(!(switching["phase"] == "committed"
        && switching["to"] == selected["id"]
        && !selected["id"].is_null()
        && stop["to"] == selected["id"]
        && stop["runtime_switch_token"] == switching["token"]))
}
pub fn restore_gate(p: &Paths) -> Result<()> {
    let switching = release::read_json(&p.data.join("runtime-switch.json"))?;
    let selected = p.selected("current")?.unwrap_or(Value::Null);
    if switching["phase"] == "active" {
        return Ok(());
    }
    if switching["phase"] == "committed" && switching["to"] == selected["id"] {
        release::write_json(
            &p.data.join("node-stopped.json"),
            &json!({"runtime_switch_token":switching["token"],"to":selected["id"]}),
        )
    } else {
        release::remove_file(&p.data.join("node-stopped.json"))
    }
}
pub fn runtime_switching(p: &Paths) -> Result<bool> {
    use fs2::FileExt;
    use std::os::unix::fs::OpenOptionsExt;
    let marker = release::read_json(&p.data.join("runtime-switch.json"))?;
    if marker["phase"] != "active" {
        return Ok(false);
    }
    let path = p.data.join("install.lock");
    let f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    crate::proof::private_file(&path)?;
    match f.try_lock_exclusive() {
        Ok(()) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(true),
        Err(e) => Err(e.into()),
    }
}
pub async fn ipc(p: &Paths, method: &str, params: Value) -> Result<Option<Value>> {
    let socket = p.data.join("connector.sock");
    if !socket.exists() {
        return Ok(None);
    }
    crate::proof::verify_socket(&socket)?;
    let result = timeout(Duration::from_secs(2), async {
        let mut s = UnixStream::connect(&socket).await?;
        s.write_all(format!("{}\n", json!({"id":1,"method":method,"params":params})).as_bytes())
            .await?;
        let mut bytes = Vec::new();
        let mut b = [0u8; 1024];
        loop {
            let n = s.read(&mut b).await?;
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&b[..n]);
            if bytes.len() > 65536 {
                return Err(crate::release::refusal(
                    "control.ipc-response-too-large",
                    json!({}),
                ));
            }
            if bytes.contains(&b'\n') {
                break;
            }
        }
        let line = bytes
            .split(|v| *v == b'\n')
            .next()
            .context("IPC response")?;
        let value: Value = serde_json::from_slice(line)?;
        if value["ok"] != true {
            return Err(crate::release::refusal("control.ipc-refused", json!({})));
        }
        Ok::<_, anyhow::Error>(value["result"].clone())
    })
    .await;
    match result {
        Ok(Ok(v)) => Ok(Some(v)),
        _ => Ok(None),
    }
}
pub async fn health(p: &Paths) -> Result<Option<Value>> {
    let ready = release::read_json(&p.data.join("core/core.json"))?;
    let socket = p.data.join("core/local.sock");
    if ready["socket"].as_str() != socket.to_str() {
        return Ok(None);
    }
    if !socket.exists() {
        return Ok(None);
    }
    crate::proof::verify_socket(&socket)?;
    let result = timeout(Duration::from_secs(2), async {
        let mut s = UnixStream::connect(&socket).await?;
        s.write_all(
            b"GET /api/local/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await?;
        let mut b = Vec::new();
        s.take(65537).read_to_end(&mut b).await?;
        if b.len() > 65536 {
            return Err(crate::release::refusal(
                "control.health-too-large",
                json!({}),
            ));
        }
        let text = String::from_utf8(b)?;
        if !text.starts_with("HTTP/1.1 200 ") && !text.starts_with("HTTP/1.0 200 ") {
            return Err(crate::release::refusal("control.health-refused", json!({})));
        }
        let (_, body) = text.split_once("\r\n\r\n").context("health body")?;
        let v: Value = serde_json::from_str(body)?;
        if v["pid"] != ready["pid"] || v["launch_id"] != ready["launch_id"] {
            return Err(crate::release::refusal(
                "control.health-identity-differs",
                json!({}),
            ));
        }
        Ok::<_, anyhow::Error>(v)
    })
    .await;
    Ok(result.ok().and_then(Result::ok))
}
pub async fn status(p: &Paths, connector_self: bool) -> Result<Value> {
    let installed = p.selected("current")?.is_some();
    let core_defined = definition(p, true)?.exists();
    let connector_defined = definition(p, false)?.exists();
    let health = health(p).await?;
    let j = if core_defined {
        job(true).await?
    } else {
        Job::default()
    };
    let reachable = health.is_some();
    let body = health.unwrap_or(Value::Null);
    let mut failure = Value::Null;
    let state = if !installed && !core_defined && !connector_defined {
        "absent"
    } else if !core_defined {
        "not-installed"
    } else if stopped(p)? {
        "stopped-by-person"
    } else if j.unknown || !j.loaded || j.reason.is_some() {
        failure = json!({"key":j.reason.unwrap_or("not-loaded")});
        "service-failed"
    } else if reachable {
        "running"
    } else if j.active {
        "starting"
    } else if j.restarting {
        "backoff"
    } else {
        failure = release::read_json(&p.data.join("core/core-failure.json"))?;
        if failure.is_null() {
            failure = json!({"key":"launch.exited","step":"run"});
        }
        "failed"
    };
    Ok(
        json!({"ok":true,"service":manager(),"installed":installed,"state":state,"core":if reachable{json!({"pid":body["pid"],"version":body["version"],"api":body["api"],"launch_id":body["launch_id"]})}else{Value::Null},"calls":body["calls"],"failure":failure,"attempts":null,"limit":null,"since":null,"window_started":null,"next_retry_at":null,"reachable":reachable,"connector":{"running":connector_self||ipc(p,"status",json!({})).await?.is_some()}}),
    )
}
pub fn core_program(p: &Paths) -> Result<PathBuf> {
    let selected = p.selected("current")?.context("no selected release")?;
    let root = p.root.join("current/core");
    Ok(match selected["core_kind"].as_str() {
        Some("rust-native-v1") => root.join("bin/sidevoice-core-rust"),
        Some("python-bundle") => root.join("python/bin/python3"),
        _ => root.join("bin/sidevoice-core"),
    })
}
pub fn core_args(p: &Paths, managed: bool) -> Result<Vec<String>> {
    let native = core_program(p)?
        .file_name()
        .is_some_and(|v| v == "sidevoice-core-rust");
    let mut args = Vec::new();
    if p.selected("current")?
        .is_some_and(|v| v["core_kind"] == "python-bundle")
    {
        args.extend(["-I".into(), "-m".into(), "sidevoice_core.server".into()]);
    }
    args.extend([
        "--data-dir".into(),
        p.data.join("core").display().to_string(),
        "--socket".into(),
        p.data.join("core/local.sock").display().to_string(),
        "--port".into(),
        std::env::var("SIDEVOICE_CORE_PORT").unwrap_or("8768".into()),
        "--idle-exit".into(),
        if managed { "0".into() } else { "600".into() },
        "--room-credential".into(),
        p.data.join("credentials.json").display().to_string(),
    ]);
    if native {
        args.extend([
            "--ready-file".into(),
            p.data.join("core/core.json").display().to_string(),
            "--host".into(),
            "127.0.0.1".into(),
            "--log-file".into(),
            p.data.join("core.log").display().to_string(),
        ]);
    }
    Ok(args)
}
fn environment(p: &Paths, core: bool) -> BTreeMap<String, String> {
    let excluded = [
        "SIDEVOICE_URL",
        "SIDEVOICE_CONNECTOR_ID",
        "SIDEVOICE_CONNECTOR_TOKEN",
        "SIDEVOICE_TEST_HOOKS",
        "SIDEVOICE_CORE_BIN",
        "SIDEVOICE_CORE_SPEC",
        "SIDEVOICE_CORE_WHEEL_DIR",
        "SIDEVOICE_UV",
        "SIDEVOICE_INSTALL_FROM_SOURCE",
        "SIDEVOICE_INSTALLED_BY",
    ];
    let mut env: BTreeMap<_, _> = std::env::vars()
        .filter(|(k, _)| k.starts_with("SIDEVOICE_") && !excluded.contains(&k.as_str()))
        .collect();
    for (key, path) in [
        ("HOME", p.home.clone()),
        ("SIDEVOICE_DATA_DIR", p.data.clone()),
        ("XDG_CONFIG_HOME", p.config.clone()),
        ("XDG_DATA_HOME", p.root.parent().unwrap().to_path_buf()),
    ] {
        env.insert(key.into(), path.display().to_string());
    }
    env.insert("SIDEVOICE_SERVICE".into(), manager().into());
    if core {
        env.insert(
            "RUSTVANI_CACHE_DIR".into(),
            p.root.join("current/core/models").display().to_string(),
        );
    }
    env
}
pub fn core_environment(p: &Paths, cmd: &mut Command) {
    for key in [
        "LD_PRELOAD",
        "LD_AUDIT",
        "LD_DEBUG",
        "LD_LIBRARY_PATH",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
        "DYLD_FALLBACK_LIBRARY_PATH",
        "DYLD_FRAMEWORK_PATH",
        "DYLD_VERSIONED_LIBRARY_PATH",
        "DYLD_ROOT_PATH",
    ] {
        cmd.env_remove(key);
    }
    cmd.env("RUSTVANI_CACHE_DIR", p.root.join("current/core/models"));
}
fn program(p: &Paths, core: bool) -> Result<Vec<String>> {
    if core {
        let mut v = vec![core_program(p)?.display().to_string()];
        v.extend(core_args(p, true)?);
        Ok(v)
    } else {
        let selected = p.selected("current")?.context("no selected release")?;
        if selected["runtime_kind"] == "rust-native-v1" {
            Ok(vec![
                p.root
                    .join("current/dist/sidevoice-rust")
                    .display()
                    .to_string(),
                "--installed".into(),
                "connector".into(),
            ])
        } else {
            let mut v = stable_command(p)?;
            v.extend(["connector".into(), "--service".into()]);
            Ok(v)
        }
    }
}
pub fn stable_command(p: &Paths) -> Result<Vec<String>> {
    let r = p.selected("current")?.context("no selected release")?;
    if r["format"] == "sea" || r["format"] == "rust-native" {
        return Ok(vec![p
            .root
            .join("current/dist/sidevoice")
            .display()
            .to_string()]);
    }
    let installed = release::read_json(&p.data.join("install.json"))?;
    let node = installed["nodeExecutable"]
        .as_str()
        .or_else(|| installed["command"][0].as_str())
        .context("legacy Node executable missing")?;
    Ok(vec![
        node.into(),
        p.root.join("current/dist/cli.mjs").display().to_string(),
    ])
}
pub fn write_definitions(p: &Paths) -> Result<()> {
    for core in [true, false] {
        let args = program(p, core)?;
        let env = environment(p, core);
        let text = if manager() == "launchd" {
            let args = args
                .iter()
                .map(|a| Ok(format!("<string>{}</string>", xml(a)?)))
                .collect::<Result<Vec<_>>>()?
                .join("");
            let vars = env
                .iter()
                .map(|(k, v)| {
                    Ok(format!(
                        "<key>{}</key><string>{}</string>",
                        xml(k)?,
                        xml(v)?
                    ))
                })
                .collect::<Result<Vec<_>>>()?
                .join("");
            let log = p.data.join(if core {
                "core.stderr.log"
            } else {
                "connector.log"
            });
            format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Label</key><string>{}</string><key>ProgramArguments</key><array>{args}</array><key>EnvironmentVariables</key><dict>{vars}</dict><key>RunAtLoad</key><true/><key>KeepAlive</key>{}<key>ThrottleInterval</key><integer>10</integer><key>StandardOutPath</key><string>{}</string><key>StandardErrorPath</key><string>{}</string></dict></plist>\n",label(core),if core{"<dict><key>SuccessfulExit</key><false/><key>Crashed</key><true/></dict>"}else{"<true/>"},xml(&log.display().to_string())?,xml(&log.display().to_string())?)
        } else {
            let args = args
                .iter()
                .map(|a| quote(a, true))
                .collect::<Result<Vec<_>>>()?
                .join(" ");
            let vars = env
                .iter()
                .map(|(k, v)| {
                    Ok(format!(
                        "Environment={}",
                        quote(&format!("{k}={v}"), false)?
                    ))
                })
                .collect::<Result<Vec<_>>>()?
                .join("\n");
            format!("[Unit]\nDescription=Sidevoice {}\nStartLimitIntervalSec=600\nStartLimitBurst=5\n\n[Service]\nType=simple\nExecStart={args}\n{vars}\nRestart={}\nRestartSec={}\nKillMode=control-group\nTimeoutStopSec=20\n\n[Install]\nWantedBy=default.target\n",if core{"core"}else{"connector"},if core{"on-failure"}else{"always"},if core{10}else{2})
        };
        let file = definition(p, core)?;
        if file.exists() {
            crate::proof::private_file(&file)?;
        }
        release::write(&file, text.as_bytes(), 0o600)?;
    }
    record(p, true)?;
    Ok(())
}
pub fn record(p: &Paths, services: bool) -> Result<()> {
    let mut v = release::read_json(&p.data.join("install.json"))?;
    if !v.is_object() {
        v = json!({});
    }
    v["command"] = json!(stable_command(p)?);
    v["releases"] = json!(p.root);
    if services {
        v["definitions"] = json!([definition(p, true)?, definition(p, false)?]);
    } else if v["definitions"].is_null() {
        v["definitions"] = json!([]);
    }
    release::write_json(&p.data.join("install.json"), &v)
}
async fn bootout(core: bool) -> Result<()> {
    let (_, _, _) = manage(&["bootout".into(), target(core)]).await?;
    let end = Instant::now() + Duration::from_secs(15);
    while Instant::now() < end {
        let state = job(core).await?;
        if !state.unknown && !state.loaded {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    Err(release::refusal(
        "control.service-unload-not-confirmed",
        json!({}),
    ))
}
pub async fn start_jobs(p: &Paths, restart: bool, core_only: bool) -> Result<()> {
    if manager() == "none" {
        bail!(
            "{}",
            crate::agents::message("service.no-manager", &json!({}))
        );
    }
    if manager() == "systemd" {
        checked(vec!["--user".into(), "daemon-reload".into()]).await?;
    }
    for core in [true, false] {
        if core_only && !core {
            continue;
        }
        if !definition(p, core)?.exists() {
            continue;
        }
        if manager() == "launchd" {
            let j = job(core).await?;
            if j.unknown {
                return Err(crate::release::refusal(
                    "control.service-manager-unavailable",
                    json!({}),
                ));
            }
            if j.loaded && restart {
                bootout(core).await?;
            }
            if !j.loaded || restart {
                checked(vec![
                    "bootstrap".into(),
                    format!("gui/{}", unsafe { libc::geteuid() }),
                    definition(p, core)?.display().to_string(),
                ])
                .await?;
            } else {
                checked(vec!["kickstart".into(), target(core)]).await?;
            }
        } else {
            checked(vec!["--user".into(), "enable".into(), unit(core).into()]).await?;
            checked(vec![
                "--user".into(),
                "reset-failed".into(),
                unit(core).into(),
            ])
            .await?;
            checked(vec![
                "--user".into(),
                if restart {
                    "restart".into()
                } else {
                    "start".into()
                },
                unit(core).into(),
            ])
            .await?;
        }
    }
    Ok(())
}
pub async fn unload(p: &Paths, disable: bool) -> Result<()> {
    for core in [false, true] {
        if !definition(p, core)?.exists() {
            continue;
        }
        let before = job(core).await?;
        if before.unknown {
            return Err(crate::release::refusal(
                "control.service-manager-unavailable",
                json!({}),
            ));
        }
        if manager() == "launchd" {
            if before.loaded {
                bootout(core).await?;
            }
        } else {
            let mut args = vec![
                "--user".into(),
                if disable {
                    "disable".into()
                } else {
                    "stop".into()
                },
            ];
            if disable {
                args.push("--now".into());
            }
            args.push(unit(core).into());
            let result = manage(&args).await?;
            let after = job(core).await?;
            if after.unknown || after.active || (!result.0 && after.loaded) {
                return Err(crate::release::refusal(
                    "control.service-unload-not-confirmed",
                    json!({}),
                ));
            }
        }
    }
    Ok(())
}
pub async fn ensure_core(p: &Paths) -> Result<Value> {
    if let Some(v) = health(p).await? {
        return Ok(v);
    }
    if stopped(p)? || runtime_switching(p)? {
        bail!(
            "{}",
            crate::agents::message("service.node-stopped", &json!({}))
        );
    }
    if defined(p)? {
        bail!(
            "{}",
            crate::agents::message("service.not-loaded", &json!({"detail":"core"}))
        );
    }
    p.prepare()?;
    let mut cmd = Command::new(core_program(p)?);
    cmd.args(core_args(p, false)?)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    core_environment(p, &mut cmd);
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    let end = Instant::now() + Duration::from_secs(60);
    while Instant::now() < end {
        if let Some(v) = health(p).await? {
            return Ok(v);
        }
        sleep(Duration::from_millis(100)).await;
    }
    bail!("{}", crate::agents::message("ready.timeout", &json!({})))
}
#[derive(Clone, PartialEq)]
struct ProcessIdentity {
    start: String,
    command: String,
}
async fn process_identity(pid: u32) -> Result<Option<ProcessIdentity>> {
    if pid <= 1 {
        return Ok(None);
    }
    if cfg!(target_os = "linux") {
        let path = PathBuf::from(format!("/proc/{pid}"));
        let stat = match fs::read_to_string(path.join("stat")) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let fields: Vec<_> = stat
            .rsplit_once(')')
            .context("process stat")?
            .1
            .split_whitespace()
            .collect();
        if fields.len() < 20 {
            return Err(release::refusal(
                "control.process-identity-unavailable",
                json!({}),
            ));
        }
        if matches!(fields[0], "Z" | "X" | "x") {
            return Ok(None);
        }
        let owner = fs::metadata(&path)?.uid();
        if owner != unsafe { libc::geteuid() } {
            return Err(release::refusal("control.foreign-process-owner", json!({})));
        }
        return Ok(Some(ProcessIdentity {
            start: fields[19].into(),
            command: String::from_utf8(fs::read(path.join("cmdline"))?)?
                .split('\0')
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" "),
        }));
    }
    let output = timeout(
        Duration::from_secs(3),
        Command::new("/bin/ps")
            .args([
                "-p",
                &pid.to_string(),
                "-o",
                "stat=",
                "-o",
                "uid=",
                "-o",
                "lstart=",
                "-o",
                "command=",
            ])
            .output(),
    )
    .await??;
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    let line = String::from_utf8(output.stdout)?;
    let fields: Vec<_> = line.split_whitespace().collect();
    if fields.len() < 8 {
        return Err(release::refusal(
            "control.process-identity-unavailable",
            json!({}),
        ));
    }
    if fields[0].contains('Z') {
        return Ok(None);
    }
    if fields[1].parse::<u32>()? != unsafe { libc::geteuid() } {
        return Err(release::refusal("control.foreign-process-owner", json!({})));
    }
    Ok(Some(ProcessIdentity {
        start: fields[2..7].join(" "),
        command: fields[7..].join(" "),
    }))
}
async fn terminate_verified(pid: u32, expected: &ProcessIdentity) -> Result<()> {
    if process_identity(pid).await?.as_ref() != Some(expected) {
        return Ok(());
    }
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    let end = Instant::now() + Duration::from_secs(15);
    while Instant::now() < end {
        if process_identity(pid).await?.as_ref() != Some(expected) {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    if process_identity(pid).await?.as_ref() == Some(expected) {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        if process_identity(pid).await?.as_ref() != Some(expected) {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    Err(release::refusal("control.process-did-not-exit", json!({})))
}
async fn stop_core(p: &Paths) -> Result<()> {
    let ready = release::read_json(&p.data.join("core/core.json"))?;
    let Some(pid) = ready["pid"]
        .as_u64()
        .filter(|v| *v > 1 && *v <= i32::MAX as u64)
    else {
        return Ok(());
    };
    let Some(identity) = process_identity(pid as u32).await? else {
        return Ok(());
    };
    let binary = core_program(p)?;
    let canonical = binary.canonicalize()?;
    // Both the immutable binary and the explicit data-directory argument bind this process to this installation.
    if !(identity.command.contains(&canonical.display().to_string())
        || identity.command.contains(&binary.display().to_string()))
        || !identity
            .command
            .contains(&format!("--data-dir {}", p.data.join("core").display()))
    {
        return Err(release::refusal(
            "control.core-process-does-not-belong-to-selected-release",
            json!({}),
        ));
    }
    terminate_verified(pid as u32, &identity).await?;
    if health(p).await?.is_some() {
        return Err(release::refusal(
            "control.core-socket-remained-active",
            json!({}),
        ));
    }
    Ok(())
}

pub async fn stop_on_demand(p: &Paths) -> Result<()> {
    use fs2::FileExt;
    use std::os::unix::fs::OpenOptionsExt;
    let selected = p.selected("current")?.unwrap_or(Value::Null);
    if selected["runtime_kind"] != "rust-native-v1" {
        let path = p.data.join("connector.lock");
        if path.exists() {
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)?;
            crate::proof::private_file(&path)?;
            match file.try_lock_exclusive() {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    let owner = release::read_json(&path)?;
                    let pid = owner["pid"]
                        .as_u64()
                        .filter(|v| *v > 1 && *v < i32::MAX as u64)
                        .context("legacy connector lock owner")?
                        as u32;
                    let identity = process_identity(pid)
                        .await?
                        .context("legacy connector process identity")?;
                    if owner["start"].as_str() != Some(identity.start.as_str())
                        || !identity.command.contains("connector")
                    {
                        return Err(release::refusal(
                            "control.connector-owner-differs-from-selected-release",
                            json!({}),
                        ));
                    }
                    terminate_verified(pid, &identity).await?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        if ipc(p, "status", json!({})).await?.is_some() {
            return Err(release::refusal(
                "control.connector-socket-remained-active",
                json!({}),
            ));
        }
        return stop_core(p).await;
    }

    if let Some(identity) = ipc(p, "identity", json!({})).await? {
        let selected = p
            .selected("current")?
            .context("connector selected release")?;
        let expected = p
            .root
            .join("releases")
            .join(selected["id"].as_str().context("release id")?)
            .join("dist/sidevoice-rust");
        if identity["managed"] != false
            || identity["release_id"] != selected["id"]
            || identity["runtime_sha256"] != selected["runtime_sha256"]
            || identity["executable"].as_str() != expected.to_str()
        {
            return Err(crate::release::refusal(
                "control.connector-owner-differs-from-selected-release",
                json!({}),
            ));
        }
        if ipc(
            p,
            "shutdown",
            json!({"expected_pid":identity["pid"],"expected_executable":identity["executable"]}),
        )
        .await?
        .is_none()
        {
            return Err(crate::release::refusal(
                "control.connector-shutdown-refused",
                json!({}),
            ));
        }
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end {
            if ipc(p, "status", json!({})).await?.is_none() {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
    }
    if ipc(p, "status", json!({})).await?.is_some() {
        return Err(crate::release::refusal(
            "control.connector-socket-remained-active",
            json!({}),
        ));
    }
    stop_core(p).await
}
pub async fn run(p: &Paths, action: crate::service::Action) -> Result<Value> {
    use crate::service::Action;
    if action == Action::Status {
        return status(p, false).await;
    }
    p.prepare()?;
    let _lock = p.lock().await?;
    match action {
        Action::Install => {
            p.selected("current")?.context("no selected release")?;
            release::remove_file(&p.data.join("node-stopped.json"))?;
            if !defined(p)? {
                stop_on_demand(p).await?;
            }
            write_definitions(p)?;
            start_jobs(p, true, false).await?;
        }
        Action::Start => {
            restore_gate(p)?;
            if defined(p)? {
                start_jobs(p, false, false).await?;
            }
        }
        Action::Restart => {
            if stopped(p)? {
                restore_gate(p)?;
                if defined(p)? {
                    start_jobs(p, false, false).await?;
                }
            } else if defined(p)? {
                start_jobs(p, true, true).await?;
            } else {
                stop_core(p).await?;
            }
        }
        Action::Stop | Action::Uninstall => {
            release::write_json(
                &p.data.join("node-stopped.json"),
                &json!({"at":crate::service::chrono_free_iso()}),
            )?;
            unload(p, action == Action::Uninstall).await?;
            stop_on_demand(p).await?;
            if action == Action::Uninstall {
                for core in [true, false] {
                    release::remove_file(&definition(p, core)?)?;
                }
                let mut record = release::read_json(&p.data.join("install.json"))?;
                if record.is_object() {
                    record["definitions"] = json!([]);
                    release::write_json(&p.data.join("install.json"), &record)?;
                }
                if manager() == "systemd" {
                    checked(vec!["--user".into(), "daemon-reload".into()]).await?;
                }
                restore_gate(p)?;
            }
        }
        Action::Status => {}
    }
    let now = status(p, false).await?;
    let mut result = json!({"ok":true,"state":now["state"],"service":manager()});
    if action == Action::Install && manager() == "systemd" {
        let user = std::env::var("USER").unwrap_or_default();
        result["linger"] = json!({"enabled":false,"command":format!("loginctl enable-linger {user}"),"reason":crate::agents::message("service.linger-reason",&json!({}))});
    }
    Ok(result)
}
