//! Native install transaction. No provider or package registry is contacted at install time.
use crate::{
    installed_service as service,
    release::{self, Paths},
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::{
    process::Command,
    time::{sleep, timeout, Duration, Instant},
};

pub struct Payload<'a> {
    pub manifest: &'a [u8],
    pub archive: &'a [u8],
    pub core_version: &'a str,
    pub channel: &'a str,
    pub build_seq: u64,
}
#[derive(Default)]
pub struct InstallOptions {
    pub service: bool,
    pub apply_now: bool,
    pub progress: bool,
    pub no_agents: bool,
    pub harnesses: Vec<String>,
}
fn progress(enabled: bool, step: &str) {
    if enabled {
        eprintln!(
            "{}",
            json!({"type":"progress","step":step,"done":null,"total":null})
        );
    }
}
fn cancelled(c: &AtomicBool) -> Result<()> {
    if c.load(Ordering::SeqCst) {
        return Err(release::refusal("install.cancelled", json!({})));
    }
    Ok(())
}
fn id(v: &Value) -> Result<&str> {
    v["id"]
        .as_str()
        .filter(|s| release::valid_id(s))
        .context("release identity missing")
}
fn kind(v: &Value) -> &str {
    v["runtime_kind"].as_str().unwrap_or("javascript")
}
fn manifest(payload: &Payload<'_>) -> Result<Value> {
    let manifest: Value = serde_json::from_slice(payload.manifest)?;
    let mut canonical = serde_json::to_vec(&manifest)?;
    canonical.push(b'\n');
    if canonical != payload.manifest
        || manifest["schema"] != 1
        || manifest["kind"] != "rust-native-v1"
        || manifest["entrypoint"] != "bin/sidevoice-core-rust"
    {
        return Err(crate::release::refusal(
            "control.native-core-manifest-identity-mismatch",
            json!({}),
        ));
    }
    for (key, len) in [("source_sha", 40), ("cargo_lock_sha256", 64)] {
        let s = manifest[key]
            .as_str()
            .context("native Core source identity")?;
        if s.len() != len
            || !s
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(crate::release::refusal(
                "control.native-core-source-identity-invalid",
                json!({}),
            ));
        }
    }
    Ok(manifest)
}
fn candidate(payload: &Payload<'_>, manifest: &Value) -> Result<Value> {
    let executable = std::env::current_exe()?;
    let sha = release::file_digest(&executable)?;
    let size = fs::metadata(&executable)?.len();
    let target = env!("SIDEVOICE_CONNECTOR_TARGET");
    let bundle = &manifest["bundles"][target];
    let archive_sha = bundle["sha256"]
        .as_str()
        .context("native Core target missing")?;
    if archive_sha.len() != 64 {
        return Err(crate::release::refusal(
            "control.native-core-archive-identity-missing",
            json!({}),
        ));
    }
    let source = manifest["source_sha"].as_str().context("Core source")?;
    let core_id = format!("rust-native-v1-{target}-{source}-{archive_sha}");
    let version = env!("SIDEVOICE_CONNECTOR_VERSION");
    let base = if payload.channel == "nightly" {
        format!("{version}-nightly.{}", payload.build_seq)
    } else {
        version.to_string()
    };
    Ok(
        json!({"id":format!("{base}-rustd-{target}-{}-core-{target}-{}-native",&sha[..12],&archive_sha[..12]),"connector":version,"core":payload.core_version,"core_kind":"rust-native-v1","core_build":core_id,"pair_id":format!("pair-v1:rust-native-v1:{sha}:core:{core_id}"),"runtime_kind":"rust-native-v1","runtime_build_sha":env!("SIDEVOICE_CONNECTOR_BUILD_SHA"),"runtime_sha256":sha,"runtime_size":size,"runtime_target":target,"distributor_sha256":sha,"distributor_size":size,"channel":payload.channel,"build_seq":payload.build_seq,"format":"rust-native","core_source_sha":source,"core_cargo_lock_sha256":manifest["cargo_lock_sha256"],"core_manifest_sha256":release::digest(payload.manifest),"core_target":target,"core_archive_sha256":archive_sha,"core_archive_size":bundle["size"],"core_entrypoint":"bin/sidevoice-core-rust"}),
    )
}
async fn stage(
    p: &Paths,
    payload: &Payload<'_>,
    manifest: &Value,
    next: &Value,
    c: &AtomicBool,
    show: bool,
) -> Result<()> {
    let final_dir = p.root.join("releases").join(id(next)?);
    if final_dir.exists() {
        let existing = release::read_json(&final_dir.join("release.json"))?;
        if existing == *next
            && release::file_digest(&final_dir.join("dist/sidevoice"))?
                == next["distributor_sha256"]
                    .as_str()
                    .context("distributor digest")?
        {
            return Ok(());
        }
        return Err(crate::release::refusal(
            "control.existing-release-differs-from-candidate",
            json!({}),
        ));
    }
    let temp = p
        .root
        .join("releases")
        .join(format!("{}.tmp-{}", id(next)?, uuid::Uuid::new_v4()));
    release::private_directory(&temp)?;
    let result = async {
        cancelled(c)?;
        progress(show, "stage");
        release::private_directory(&temp.join("dist"))?;
        let bytes = fs::read(std::env::current_exe()?)?;
        for name in ["sidevoice", "sidevoice-rust"] {
            release::write(&temp.join("dist").join(name), &bytes, 0o700)?;
        }
        release::unpack_core(
            payload.archive,
            manifest,
            env!("SIDEVOICE_CONNECTOR_TARGET"),
            &temp.join("core"),
        )?;
        cancelled(c)?;
        let mut core = Command::new(temp.join("core/bin/sidevoice-core-rust"));
        core.arg("--self-test")
            .env("RUSTVANI_CACHE_DIR", temp.join("core/models"))
            .kill_on_drop(true);
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
            core.env_remove(key);
        }
        let output = timeout(Duration::from_secs(60), core.output()).await??;
        if !output.status.success() {
            return Err(crate::release::refusal(
                "control.core-self-test-failed",
                json!({}),
            ));
        }
        let output = timeout(
            Duration::from_secs(30),
            Command::new(temp.join("dist/sidevoice"))
                .arg("--version")
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        if !output.status.success()
            || String::from_utf8_lossy(&output.stdout).trim() != env!("SIDEVOICE_CONNECTOR_VERSION")
        {
            return Err(crate::release::refusal(
                "control.connector-self-test-failed",
                json!({}),
            ));
        }
        cancelled(c)?;
        release::write_json(&temp.join("release.json"), next)?;
        release::sync_tree(&temp)?;
        fs::rename(&temp, &final_dir)?;
        fs::File::open(p.root.join("releases"))?.sync_all()?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if temp.exists() {
        let _ = fs::remove_dir_all(temp);
    }
    result
}
fn empty_outbox(p: &Paths) -> Result<()> {
    let outbox = release::read_json(&p.data.join("outbox.json"))?;
    if !outbox.is_null() && !outbox.as_array().is_some_and(|v| v.is_empty()) {
        return Err(release::refusal("install.runtime-switch-outbox", json!({})));
    }
    Ok(())
}
struct Gate {
    paths: Paths,
    previous_stop: Value,
    previous_switch: Value,
    token: String,
    committed: bool,
}
impl Gate {
    fn begin(p: &Paths, next: &Value) -> Result<Self> {
        let old_stop = release::read_json(&p.data.join("node-stopped.json"))?;
        let old_switch = release::read_json(&p.data.join("runtime-switch.json"))?;
        let token = uuid::Uuid::new_v4().to_string();
        release::write_json(
            &p.data.join("runtime-switch.json"),
            &json!({"phase":"active","token":token,"pid":std::process::id(),"start":null,"to":next["id"],"runtimeKind":kind(next),"previousSwitch":if old_switch.is_null(){Value::Null}else{json!(old_switch.to_string())},"previousStop":if old_stop.is_null(){Value::Null}else{json!(old_stop.to_string())}}),
        )?;
        release::write_json(
            &p.data.join("node-stopped.json"),
            &json!({"runtime_switch_token":token}),
        )?;
        Ok(Self {
            paths: p.clone(),
            previous_stop: old_stop,
            previous_switch: old_switch,
            token,
            committed: false,
        })
    }
    fn commit(&mut self, next: &Value) -> Result<()> {
        if kind(next) == "javascript" {
            release::remove_file(&self.paths.data.join("runtime-switch.json"))?;
            release::remove_file(&self.paths.data.join("node-stopped.json"))?;
        } else {
            release::write_json(
                &self.paths.data.join("runtime-switch.json"),
                &json!({"phase":"committed","token":self.token,"to":next["id"]}),
            )?;
            service::restore_gate(&self.paths)?;
        }
        self.committed = true;
        Ok(())
    }
}
impl Drop for Gate {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for (name, value) in [
            ("runtime-switch.json", &self.previous_switch),
            ("node-stopped.json", &self.previous_stop),
        ] {
            let _ = if value.is_null() {
                release::remove_file(&self.paths.data.join(name))
            } else {
                release::write_json(&self.paths.data.join(name), value)
            };
        }
    }
}
fn recover_gate(p: &Paths) -> Result<()> {
    let v = release::read_json(&p.data.join("runtime-switch.json"))?;
    if v["phase"] != "active" {
        return Ok(());
    }
    let current = p.selected("current")?.unwrap_or(Value::Null);
    if v["to"] == current["id"] && !current.is_null() {
        if kind(&current) == "javascript" {
            release::remove_file(&p.data.join("runtime-switch.json"))?;
            release::remove_file(&p.data.join("node-stopped.json"))?;
        } else {
            release::write_json(
                &p.data.join("runtime-switch.json"),
                &json!({"phase":"committed","token":v["token"],"to":current["id"]}),
            )?;
            service::restore_gate(p)?;
        }
    } else {
        for (name, key) in [
            ("runtime-switch.json", "previousSwitch"),
            ("node-stopped.json", "previousStop"),
        ] {
            if let Some(raw) = v[key].as_str() {
                release::write(&p.data.join(name), raw.as_bytes(), 0o600)?;
            } else {
                release::remove_file(&p.data.join(name))?;
            }
        }
    }
    Ok(())
}
async fn quiesce(p: &Paths) -> Result<()> {
    if service::defined(p)? {
        service::unload(p, false).await?;
    }
    service::stop_on_demand(p).await
}
async fn start_selection(p: &Paths, managed: bool) -> Result<()> {
    service::record(p, false)?;
    if managed {
        service::write_definitions(p)?;
        service::start_jobs(p, true, false).await?;
    } else {
        service::ensure_core(p).await?;
        let selected = p.selected("current")?.context("selected release")?;
        let mut cmd = if kind(&selected) == "rust-native-v1" {
            let mut c = Command::new(p.root.join("current/dist/sidevoice-rust"));
            c.args(["--installed", "connector"]);
            c
        } else {
            let argv = service::stable_command(p)?;
            let mut c = Command::new(&argv[0]);
            c.args(&argv[1..]).arg("connector");
            c
        };
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
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
    }
    Ok(())
}
async fn verify(p: &Paths, selected: &Value, previous_launch: Option<&str>) -> Result<()> {
    let end = Instant::now() + Duration::from_secs(60);
    while Instant::now() < end {
        if let Some(h) = service::health(p).await? {
            let same_launch =
                previous_launch.is_some() && h["launch_id"].as_str() == previous_launch;
            if h["version"] == selected["core"] && !same_launch {
                let connector = service::ipc(p, "identity", json!({})).await?;
                let correct = if kind(selected) == "rust-native-v1" {
                    connector.is_some_and(|i| {
                        i["release_id"] == selected["id"]
                            && i["runtime_sha256"] == selected["runtime_sha256"]
                            && i["runtime_build_sha"] == selected["runtime_build_sha"]
                    })
                } else {
                    service::ipc(p, "status", json!({}))
                        .await?
                        .is_some_and(|v| v["version"] == selected["connector"])
                };
                if correct {
                    return Ok(());
                }
            }
        }
        sleep(Duration::from_millis(200)).await;
    }
    bail!("{}", crate::agents::message("install.verify", &json!({})))
}
async fn go_back(p: &Paths, managed: bool, show: bool, explicit: bool) -> Result<Option<Value>> {
    let current = p.selected("current")?.unwrap_or(Value::Null);
    let back = [p.selected("verified")?, p.selected("previous")?]
        .into_iter()
        .flatten()
        .find(|v| v["id"] != current["id"]);
    let Some(back) = back else { return Ok(None) };
    progress(show, "rollback");
    let mut gate = Gate::begin(p, &back)?;
    quiesce(p).await?;
    if kind(&current) != kind(&back) {
        if explicit {
            empty_outbox(p)?;
        } else if empty_outbox(p).is_err() {
            fs::rename(
                p.data.join("outbox.json"),
                p.data
                    .join(format!("outbox.{}.quarantine.json", uuid::Uuid::new_v4())),
            )?;
        }
    }
    p.point("current", id(&back)?)?;
    gate.commit(&back)?;
    start_selection(p, managed).await?;
    verify(p, &back, None).await?;
    p.point("verified", id(&back)?)?;
    crate::agents::reconcile_owned(crate::proof::Profile::for_control_env()?).await?;
    Ok(Some(back))
}
pub async fn install(
    payload: Payload<'_>,
    options: InstallOptions,
    cancellation: Arc<AtomicBool>,
) -> Result<Value> {
    let p = Paths::environment()?;
    p.prepare()?;
    progress(options.progress, "wait-lock");
    let lock = tokio::select! { result=p.lock()=>result?, _=async{while !cancellation.load(Ordering::SeqCst){sleep(Duration::from_millis(50)).await;}}=>{cancelled(&cancellation)?;unreachable!()}};
    let _lock = lock;
    cancelled(&cancellation)?;
    recover_gate(&p)?;
    let manifest = manifest(&payload)?;
    let next = candidate(&payload, &manifest)?;
    let current = p.selected("current")?;
    let action = release::decide(current.as_ref(), &next);
    let chosen = if action == "noop" {
        current.clone().context("noop requires selection")?
    } else {
        next
    };
    let managed = options.service || service::defined(&p)?;
    let _candidate_cleanup = CandidateCleanup {
        paths: p.clone(),
        candidate: if action != "noop" {
            Some(id(&chosen)?.to_owned())
        } else {
            None
        },
    };
    if action != "noop" {
        if current.as_ref().is_some_and(|v| kind(v) != kind(&chosen)) {
            empty_outbox(&p)?;
        }
        stage(
            &p,
            &payload,
            &manifest,
            &chosen,
            &cancellation,
            options.progress,
        )
        .await?;
        if current.is_some() && !options.apply_now {
            loop {
                cancelled(&cancellation)?;
                if service::health(&p)
                    .await?
                    .and_then(|h| h["calls"].as_u64())
                    .unwrap_or(0)
                    == 0
                {
                    break;
                }
                progress(options.progress, "wait-calls");
                sleep(Duration::from_secs(2)).await;
            }
        }
    }
    cancelled(&cancellation)?;
    let old_launch = service::health(&p)
        .await?
        .and_then(|h| h["launch_id"].as_str().map(str::to_owned));
    let verified = p.selected("verified")?;
    let needs_restart = action != "noop"
        || verified.as_ref().map(|v| v["id"].clone()) != Some(chosen["id"].clone());
    if needs_restart {
        let was_stopped = service::stopped(&p)?;
        let mut gate = Gate::begin(&p, &chosen)?;
        let switching = async {
            quiesce(&p).await?;
            if current.as_ref().is_some_and(|v| kind(v) != kind(&chosen)) {
                empty_outbox(&p)?;
            }
            cancelled(&cancellation)?;
            progress(options.progress, "commit");
            if let Some(v) = verified {
                p.point("previous", id(&v)?)?;
            }
            p.point("current", id(&chosen)?)?;
            gate.commit(&chosen)?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = switching {
            drop(gate);
            if !was_stopped
                && current.is_some()
                && p.selected("current")?.as_ref().map(|v| v["id"].clone())
                    == current.as_ref().map(|v| v["id"].clone())
            {
                start_selection(&p, managed).await?;
            }
            return Err(error);
        }
    }
    service::restore_gate(&p)?;
    progress(options.progress, "service-start");
    let started = start_selection(&p, managed).await;
    let verification = match started {
        Ok(()) => {
            verify(
                &p,
                &chosen,
                if needs_restart {
                    old_launch.as_deref()
                } else {
                    None
                },
            )
            .await
        }
        Err(e) => Err(e),
    };
    if let Err(error) = verification {
        let back = go_back(&p, managed, options.progress, false).await?;
        return Ok(
            json!({"ok":false,"action":if back.is_some(){"rollback"}else{"failed"},"installed":back.as_ref().unwrap_or(&chosen)["id"],"from":current.as_ref().map(|v|v["id"].clone()),"service":if managed{service::manager()}else{"none"},"failure":{"key":"install.verify","message":error.to_string()},"back":back.is_some()}),
        );
    }
    p.point("verified", id(&chosen)?)?;
    crate::agents::reconcile_owned(crate::proof::Profile::for_control_env()?).await?;
    let registrations = if options.no_agents {
        json!({})
    } else {
        crate::agents::register_requested(
            crate::proof::Profile::for_control_env()?,
            &options.harnesses,
        )
        .await?
    };
    progress(options.progress, "pairing");
    let paired = release::read_json(&p.data.join("credentials.json"))?;
    let state = service::status(&p, false).await?;
    Ok(
        json!({"ok":true,"action":action,"installed":chosen["id"],"connector":chosen["connector"],"core":chosen["core"],"channel":chosen["channel"],"command":service::stable_command(&p)?,"service":if managed{service::manager()}else{"none"},"state":state["state"],"agents":registrations,"paired":!paired.is_null(),"room":paired["url"]}),
    )
}
pub async fn rollback(show: bool) -> Result<Value> {
    let p = Paths::environment()?;
    p.prepare()?;
    let _lock = p.lock().await?;
    recover_gate(&p)?;
    let from = p.selected("current")?;
    let managed = service::defined(&p)?;
    let back = go_back(&p, managed, show, true)
        .await?
        .context("no previous verified release")?;
    Ok(
        json!({"ok":true,"action":"rollback","installed":back["id"],"from":from.map(|v|v["id"].clone()),"service":if managed{service::manager()}else{"none"}}),
    )
}
pub async fn uninstall() -> Result<Value> {
    let p = Paths::environment()?;
    p.prepare()?;
    let _lock = p.lock().await?;
    release::write_json(
        &p.data.join("node-stopped.json"),
        &json!({"at":crate::service::chrono_free_iso()}),
    )?;
    service::unload(&p, true).await?;
    service::stop_on_demand(&p).await?;
    let registrations =
        crate::agents::cleanup_owned(crate::proof::Profile::for_control_env()?).await?;
    for core in [true, false] {
        release::remove_file(&service::definition(&p, core)?)?;
    }
    // Delete only known Sidevoice files; foreign state is preserved. Permanent lock inodes stay in place.
    for name in [
        "install.json",
        "agents.json",
        "node-stopped.json",
        "runtime-switch.json",
        "credentials.json",
        "outbox.json",
        "proof.json",
        "connector.log",
        "connector.sock",
    ] {
        let path = p.data.join(name);
        if path.exists() {
            let meta = fs::symlink_metadata(&path)?;
            if meta.file_type().is_symlink() {
                return Err(crate::release::refusal(
                    "control.uninstall-state-contains-a-link",
                    json!({}),
                ));
            }
            release::remove_file(&path)?;
        }
    }
    if p.root.join("releases").exists() {
        for entry in fs::read_dir(p.root.join("releases"))? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && !release::read_json(&entry.path().join("release.json"))?.is_null()
            {
                fs::remove_dir_all(entry.path())?;
            }
        }
    }
    for name in ["current", "previous", "verified"] {
        release::remove_file(&p.root.join(name))?;
    }
    Ok(json!({"ok":true,"done":registrations["done"],"next":registrations["next"]}))
}

pub fn error_value(error: &anyhow::Error) -> Value {
    if let Some(error) = error.downcast_ref::<release::ControlError>() {
        return json!({"ok":false,"error":{"key":error.key,"params":error.params,"message":error.to_string()}});
    }
    let key = if error
        .chain()
        .filter_map(|e| e.downcast_ref::<std::io::Error>())
        .any(|e| matches!(e.raw_os_error(), Some(libc::ENOSPC | libc::EDQUOT)))
    {
        "install.disk"
    } else {
        "install.failed"
    };
    json!({"ok":false,"error":{"key":key,"message":crate::agents::message(key,&json!({}))}})
}

struct CandidateCleanup {
    paths: Paths,
    candidate: Option<String>,
}
impl Drop for CandidateCleanup {
    fn drop(&mut self) {
        let Some(id) = &self.candidate else {
            return;
        };
        for name in ["current", "previous", "verified"] {
            match self.paths.selected(name) {
                Ok(Some(v)) if v["id"].as_str() == Some(id) => return,
                Err(_) => return,
                _ => {}
            }
        }
        let path = self.paths.root.join("releases").join(id);
        if path.exists() {
            let _ = fs::remove_dir_all(path);
        }
    }
}
