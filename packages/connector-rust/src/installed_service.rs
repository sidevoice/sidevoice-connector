//! Installed service policy shared by the CLI, daemon and MCP launcher.
use crate::release::{self, Paths};
use crate::service::{Job, Observation};
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
    match std::env::var("SIDEVOICE_SERVICE_MANAGER").ok().as_deref() {
        Some("launchd") => return "launchd",
        Some("systemd") => return "systemd",
        Some("none") => return "none",
        _ => {}
    }
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
            runs: field("runs").and_then(|s| s.parse().ok()),
            exit: field("last exit code").and_then(|s| s.parse().ok()),
            running: field("state") == Some("running")
                && field("pid")
                    .and_then(|value| value.parse::<u32>().ok())
                    .is_some_and(|pid| pid > 0),
            pid: field("pid").and_then(|s| s.parse().ok()),
            restarting: field("state") != Some("running") && field("last exit code") != Some("0"),
            ..Default::default()
        })
    } else {
        let (ok, out, _) = manage(&[
            "--user".into(),
            "show".into(),
            "-p".into(),
            "LoadState,ActiveState,SubState,MainPID,Result,NRestarts,ExecMainStatus".into(),
            unit(core).into(),
        ])
        .await?;
        let field = |name: &str| {
            out.lines()
                .find_map(|l| l.strip_prefix(&format!("{name}=")))
        };
        Ok(Job {
            runs: field("NRestarts").and_then(|s| s.parse().ok()),
            exit: field("ExecMainStatus").and_then(|s| s.parse().ok()),
            loaded: field("LoadState") == Some("loaded"),
            running: field("ActiveState") == Some("active")
                && field("MainPID")
                    .and_then(|value| value.parse::<u32>().ok())
                    .is_some_and(|pid| pid > 0),
            unknown: !ok || field("LoadState").is_none(),
            pid: field("MainPID")
                .and_then(|s| s.parse().ok())
                .filter(|p| *p > 0),
            reason: if field("Result") == Some("start-limit-hit") {
                Some("service.start-limit".into())
            } else {
                None
            },
            restarting: field("SubState") == Some("auto-restart"),
            ..Default::default()
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
    let mut core = if core_defined {
        job(true).await?
    } else {
        Job::default()
    };
    core.defined = core_defined;
    let connector = Job {
        defined: connector_defined,
        ..Default::default()
    };
    let health = health(p).await?;
    let ready = serde_json::from_value(release::read_json(&p.data.join("core/core.json"))?).ok();
    let mut failure = release::read_json(&p.data.join("core/core-failure.json"))?;
    if let Some(key) = failure["key"].as_str().map(str::to_owned) {
        failure["message"] = json!(crate::agents::message(
            &key,
            &json!({"detail":failure["detail"]})
        ));
    }
    let core_age = if let Some(pid) = core.pid {
        crate::service::process_age(pid).await
    } else {
        None
    };
    let program_error = if core_defined && core_program(p).is_ok_and(|path| !path.exists()) {
        Some("service.executable-missing")
    } else {
        None
    };
    let observation = Observation {
        service: manager(),
        installed,
        core,
        connector,
        stopped: stopped(p)?,
        health,
        ready,
        failure: if failure.is_null() {
            None
        } else {
            Some(failure)
        },
        core_age,
        connector_running: connector_self || ipc(p, "status", json!({})).await?.is_some(),
        definition_error: None,
        manager_error: None,
        program_error,
    };
    Ok(crate::service::derive_status(&observation))
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
            if after.unknown || after.running || (!result.0 && after.loaded) {
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
    argv: Vec<String>,
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
        let argv = process_arguments(pid)?;
        return Ok(Some(ProcessIdentity {
            start: fields[19].into(),
            command: argv.join(" "),
            argv,
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
    let argv = process_arguments(pid)?;
    Ok(Some(ProcessIdentity {
        start: fields[2..7].join(" "),
        command: argv.join(" "),
        argv,
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
        // Exiting processes can lose argv before releasing their open files and locks.
        // After signalling, wait for this process lifetime to end, not merely argv to change.
        if process_identity(pid)
            .await?
            .is_none_or(|identity| identity.start != expected.start)
        {
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
        // Exiting processes can lose argv before releasing their open files and locks.
        // After signalling, wait for this process lifetime to end, not merely argv to change.
        if process_identity(pid)
            .await?
            .is_none_or(|identity| identity.start != expected.start)
        {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    Err(release::refusal("control.process-did-not-exit", json!({})))
}
#[cfg(target_os = "linux")]
fn process_arguments(pid: u32) -> Result<Vec<String>> {
    let raw = fs::read(format!("/proc/{pid}/cmdline"))?;
    raw.split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8(arg.to_vec()).map_err(Into::into))
        .collect()
}
#[cfg(target_os = "macos")]
fn process_arguments(pid: u32) -> Result<Vec<String>> {
    // KERN_PROCARGS2 preserves argv boundaries, unlike `ps command=` when paths contain spaces.
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut size = 0usize;
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    if !(4..=1024 * 1024).contains(&size) {
        return Err(release::refusal(
            "control.process-identity-unavailable",
            json!({}),
        ));
    }
    let mut raw = vec![0u8; size];
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            raw.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    raw.truncate(size);
    let argc = i32::from_ne_bytes(raw.get(..4).context("process argc")?.try_into()?);
    if !(1..=4096).contains(&argc) {
        return Err(release::refusal(
            "control.process-identity-unavailable",
            json!({}),
        ));
    }
    let mut offset = 4
        + raw[4..]
            .iter()
            .position(|b| *b == 0)
            .context("process executable")?
        + 1;
    while raw.get(offset) == Some(&0) {
        offset += 1;
    }
    let mut argv = Vec::new();
    for _ in 0..argc {
        let end = offset
            + raw
                .get(offset..)
                .context("process argv")?
                .iter()
                .position(|b| *b == 0)
                .context("process argument terminator")?;
        argv.push(String::from_utf8(raw[offset..end].to_vec())?);
        offset = end + 1;
    }
    Ok(argv)
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_arguments(_pid: u32) -> Result<Vec<String>> {
    Err(release::refusal(
        "control.process-identity-unavailable",
        json!({}),
    ))
}

fn same_path(value: &str, expected: &Path) -> bool {
    let candidate = Path::new(value);
    candidate == expected
        || candidate.is_absolute()
            && candidate
                .canonicalize()
                .ok()
                .zip(expected.canonicalize().ok())
                .is_some_and(|(left, right)| left == right)
}
fn core_data_argument(argv: &[String], data: &Path) -> bool {
    argv.iter().enumerate().any(|(index, arg)| {
        if arg == "--data-dir" {
            argv.get(index + 1)
                .is_some_and(|value| same_path(value, data))
        } else {
            arg.strip_prefix("--data-dir=")
                .is_some_and(|value| same_path(value, data))
        }
    })
}
fn core_program_argument(argv: &[String], program: &Path) -> bool {
    // Native binary argv[0], or a legacy Python console script argv[1]. Never match arbitrary trailing text.
    argv.first().is_some_and(|arg| same_path(arg, program))
        || argv.get(1).is_some_and(|arg| same_path(arg, program))
}
async fn user_pids() -> Result<Vec<u32>> {
    let user = unsafe { libc::geteuid() };
    if cfg!(target_os = "linux") {
        let mut pids = Vec::new();
        for entry in fs::read_dir("/proc")? {
            let entry = entry?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            match fs::metadata(entry.path()) {
                Ok(metadata) if metadata.uid() == user => pids.push(pid),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        return Ok(pids);
    }
    let output = timeout(
        Duration::from_secs(3),
        Command::new("/bin/ps")
            .args(["-U", &user.to_string(), "-o", "pid="])
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    if !output.status.success() || output.stdout.len() > 1024 * 1024 {
        return Err(release::refusal(
            "control.process-identity-unavailable",
            json!({}),
        ));
    }
    String::from_utf8(output.stdout)?
        .split_whitespace()
        .map(|value| value.parse().map_err(Into::into))
        .collect()
}
async fn core_processes(p: &Paths) -> Result<Vec<(u32, ProcessIdentity)>> {
    let data = p.data.join("core");
    let program = core_program(p).ok();
    let mut owned = Vec::new();
    for pid in user_pids().await? {
        if pid == std::process::id() {
            continue;
        }
        let identity = match process_identity(pid).await {
            Ok(Some(identity)) => identity,
            Ok(None) => continue,
            // macOS ps -U selects real UID; setuid processes may have another effective
            // owner. Discovery must skip them; targeted signalling still refuses them.
            Err(error)
                if error
                    .downcast_ref::<release::ControlError>()
                    .is_some_and(|error| error.key == "control.foreign-process-owner") =>
            {
                continue
            }
            Err(error)
                if error
                    .chain()
                    .filter_map(|e| e.downcast_ref::<std::io::Error>())
                    .any(|e| matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ESRCH))) =>
            {
                continue
            }
            Err(error) => return Err(error),
        };
        if !core_data_argument(&identity.argv, &data) {
            continue;
        }
        if !program
            .as_ref()
            .is_some_and(|program| core_program_argument(&identity.argv, program))
        {
            return Err(release::refusal(
                "control.core-process-does-not-belong-to-selected-release",
                json!({}),
            ));
        }
        owned.push((pid, identity));
    }
    Ok(owned)
}
fn core_lock_probe(p: &Paths) -> Result<fs::File> {
    use fs2::FileExt;
    use std::os::unix::fs::OpenOptionsExt;
    let path = p.data.join("core/core.lock");
    crate::proof::private_dir(path.parent().context("Core lock parent")?)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    crate::proof::private_file(&path)?;
    file.try_lock_exclusive()
        .map_err(|_| release::refusal("control.core-lock-owner-unverified", json!({})))?;
    let opened = file.metadata()?;
    let current = fs::symlink_metadata(&path)?;
    if opened.ino() != current.ino() || opened.dev() != current.dev() {
        return Err(release::refusal(
            "control.core-lock-owner-unverified",
            json!({}),
        ));
    }
    Ok(file)
}
async fn raw_core_socket_silent(p: &Paths) -> Result<()> {
    let socket = p.data.join("core/local.sock");
    match fs::symlink_metadata(&socket) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    crate::proof::verify_socket(&socket)?;
    match timeout(Duration::from_millis(800), UnixStream::connect(&socket)).await {
        Ok(Err(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            Ok(())
        }
        _ => Err(release::refusal(
            "control.core-socket-remained-active",
            json!({}),
        )),
    }
}
async fn stop_core(p: &Paths) -> Result<()> {
    // Native Core holds an empty flock before publishing ready. Enumerate exact argv, not a stale ready PID.
    for (pid, identity) in core_processes(p).await? {
        terminate_verified(pid, &identity).await?;
    }
    let _lock = core_lock_probe(p)?;
    if !core_processes(p).await?.is_empty() {
        return Err(release::refusal(
            "control.core-process-started-during-shutdown",
            json!({}),
        ));
    }
    raw_core_socket_silent(p).await
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
            .join("dist/sidevoice-rust")
            .canonicalize()?;
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
    let mut now = status(p, false).await?;
    if matches!(action, Action::Install | Action::Start | Action::Restart) {
        let end =
            Instant::now() + Duration::from_secs(if action == Action::Install { 60 } else { 10 });
        while Instant::now() < end {
            let state = now["state"].as_str().unwrap_or("");
            let transient = matches!(state, "starting" | "backoff")
                || state == "failed"
                    && matches!(
                        now["failure"]["step"].as_str(),
                        Some("run" | "health" | "ready")
                    );
            if !transient {
                break;
            }
            sleep(Duration::from_millis(200)).await;
            now = status(p, false).await?;
        }
    }
    let mut result = json!({"ok":true,"state":now["state"],"service":manager()});
    if !now["failure"].is_null() {
        result["failure"] = now["failure"].clone();
    }
    if action == Action::Install && manager() == "systemd" {
        result["linger"] = linger().await;
    }
    Ok(result)
}

pub async fn reload_after_uninstall() -> Result<()> {
    if manager() == "systemd" {
        checked(vec!["--user".into(), "daemon-reload".into()]).await?;
    }
    Ok(())
}

pub async fn linger() -> Value {
    let user = std::env::var("USER").unwrap_or_default();
    let output = timeout(
        Duration::from_secs(3),
        Command::new(std::env::var("SIDEVOICE_LOGINCTL").unwrap_or("loginctl".into()))
            .args(["show-user", &user, "-p", "Linger"])
            .kill_on_drop(true)
            .output(),
    )
    .await;
    let enabled = output.ok().and_then(Result::ok).is_some_and(|out| {
        out.status.success()
            && String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|l| l == "Linger=yes")
    });
    json!({"enabled":enabled,"command":format!("loginctl enable-linger {user}"),"reason":crate::agents::message("service.linger-reason",&json!({}))})
}

pub async fn managed_owners(
    p: &Paths,
    core_pid: Option<u32>,
    connector_pid: Option<u32>,
) -> Result<bool> {
    if core_pid.is_none()
        || connector_pid.is_none()
        || !definition(p, true)?.exists()
        || !definition(p, false)?.exists()
    {
        return Ok(false);
    }
    let core = job(true).await?;
    let connector = job(false).await?;
    Ok(core.loaded
        && !core.unknown
        && core.running
        && core.pid == core_pid
        && connector.loaded
        && !connector.unknown
        && connector.running
        && connector.pid == connector_pid)
}
pub async fn runtime_present(p: &Paths) -> Result<bool> {
    Ok(ipc(p, "status", json!({})).await?.is_some() || !core_processes(p).await?.is_empty())
}

#[cfg(test)]
mod native_tests {
    use super::*;
    #[test]
    fn manager_serializers_keep_values_in_their_directive() {
        assert_eq!(
            quote("/path with spaces/$HOME/%x/\"quoted\"", true).unwrap(),
            "\"/path with spaces/$$HOME/%%x/\\\"quoted\\\"\""
        );
        assert_eq!(quote("KEY=$VALUE%", false).unwrap(), "\"KEY=$VALUE%%\"");
        assert!(quote("path\nExecStartPre=/bin/false", true).is_err());
        assert!(xml("path\r<key>Injected</key>").is_err());
        assert_eq!(xml("A<&\"B").unwrap(), "A&lt;&amp;&quot;B");
    }
    #[test]
    fn definitions_use_current_and_keep_managers_independent() {
        let root =
            std::env::temp_dir().join(format!("sidevoice-service-test-{}", uuid::Uuid::new_v4()));
        let p = Paths {
            home: root.join("home"),
            data: root.join("data"),
            root: root.join("sidevoice"),
            config: root.join("config"),
        };
        p.prepare().unwrap();
        release::private_directory(&p.root.join("releases/native")).unwrap();
        release::write_json(&p.root.join("releases/native/release.json"),&json!({"id":"native","core_kind":"rust-native-v1","runtime_kind":"rust-native-v1","format":"rust-native"})).unwrap();
        p.point("current", "native").unwrap();
        write_definitions(&p).unwrap();
        let core = fs::read_to_string(definition(&p, true).unwrap()).unwrap();
        let connector = fs::read_to_string(definition(&p, false).unwrap()).unwrap();
        assert!(core.contains("current/core/bin/sidevoice-core-rust"));
        assert!(connector.contains("current/dist/sidevoice-rust"));
        assert!(connector.contains("--installed"));
        assert!(!core.contains("sidevoice-rust"));
        assert!(!connector.contains("sidevoice-core-rust"));
        if manager() == "systemd" {
            assert!(core.contains("Restart=on-failure"));
            assert!(connector.contains("Restart=always"));
            assert!(core.contains("TimeoutStopSec=20"));
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_status_rejects_another_healthy_process() {
        let ready = crate::proof::Ready {
            pid: 202,
            launch_id: "old".into(),
            socket: "/tmp/test.sock".into(),
            connector_id: "id".into(),
            token: "token".into(),
            connector_protocols: Some(vec![3]),
        };
        let mut observation = Observation {
            service: "systemd",
            installed: true,
            core: Job {
                defined: true,
                loaded: true,
                running: true,
                pid: Some(101),
                ..Default::default()
            },
            connector: Job {
                defined: true,
                ..Default::default()
            },
            stopped: false,
            health: Some(json!({"pid":202,"launch_id":"old","calls":1,"api":1})),
            ready: Some(ready),
            failure: None,
            core_age: Some(1),
            connector_running: true,
            definition_error: None,
            manager_error: None,
            program_error: None,
        };
        let result = crate::service::derive_status(&observation);
        assert_eq!(result["reachable"], false);
        assert_eq!(result["state"], "starting");
        observation.core.running = false;
        let result = crate::service::derive_status(&observation);
        assert_eq!(result["reachable"], false);
        assert_ne!(result["state"], "running");
    }
    #[test]
    fn process_matching_requires_exact_arguments_not_path_prefixes() {
        let data = Path::new("/private/profile/core with spaces");
        let program = Path::new("/private/release/core/bin/sidevoice-core-rust");
        let args = vec![
            program.display().to_string(),
            "--data-dir".into(),
            data.display().to_string(),
        ];
        assert!(core_data_argument(&args, data));
        assert!(core_program_argument(&args, program));
        let mut wrong = args.clone();
        wrong[2].push_str("-foreign");
        assert!(!core_data_argument(&wrong, data));
        assert!(!core_program_argument(
            &[
                "unrelated".into(),
                "--description".into(),
                program.display().to_string()
            ],
            program
        ));
    }
    #[tokio::test]
    async fn stops_owned_core_before_ready_and_refuses_unknown_lock_holder() {
        use fs2::FileExt;
        let root =
            std::env::temp_dir().join(format!("sidevoice-unready-test-{}", uuid::Uuid::new_v4()));
        let p = Paths {
            home: root.join("home"),
            data: root.join("data"),
            root: root.join("sidevoice"),
            config: root.join("config"),
        };
        p.prepare().unwrap();
        let executable = p.root.join("releases/native/core/bin/sidevoice-core-rust");
        release::private_directory(executable.parent().unwrap()).unwrap();
        release::write(&executable,b"import fcntl, os, pathlib, sys, time\ndata = pathlib.Path(sys.argv[sys.argv.index('--data-dir')+1])\nlock = os.open(data / 'core.lock', os.O_CREAT | os.O_RDWR, 0o600)\nfcntl.flock(lock, fcntl.LOCK_EX)\n(data / 'fixture-started').write_text('started')\ntime.sleep(60)\n",0o700).unwrap();
        release::write_json(&p.root.join("releases/native/release.json"),&json!({"id":"native","core_kind":"rust-native-v1","runtime_kind":"rust-native-v1","format":"rust-native"})).unwrap();
        p.point("current", "native").unwrap();
        let mut child = Command::new("python3")
            .arg(&executable)
            .arg("--data-dir")
            .arg(p.data.join("core"))
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        while !p.data.join("core/fixture-started").exists() && Instant::now() < end {
            sleep(Duration::from_millis(20)).await;
        }
        let started = p.data.join("core/fixture-started").exists();
        let ready = p.data.join("core/core.json").exists();
        let stopped = stop_core(&p).await;
        // Reap the fixture even when startup or shutdown assertions fail.
        let _ = child.start_kill();
        let exited = child.wait().await;
        assert!(started);
        assert!(!ready);
        stopped.unwrap();
        assert!(!exited.unwrap().success());
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(p.data.join("core/core.lock"))
            .unwrap();
        lock.try_lock_exclusive().unwrap();
        let error = stop_core(&p).await.unwrap_err();
        assert_eq!(
            error.downcast_ref::<release::ControlError>().unwrap().key,
            "control.core-lock-owner-unverified"
        );
        assert!(p.root.join("releases/native").exists());
        assert!(p.data.join("core/fixture-started").exists());
        drop(lock);
        fs::remove_dir_all(root).unwrap();
    }
}
