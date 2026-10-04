//! Native install transaction. No provider or package registry is contacted at install time.
use crate::{
    installed_service as service,
    release::{self, Paths},
};
use anyhow::{Context, Result};
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
    let parse = || -> Result<Value> {
        let manifest: Value = serde_json::from_slice(payload.manifest)?;
        let mut canonical = serde_json::to_vec(&manifest)?;
        canonical.push(b'\n');
        if canonical != payload.manifest
            || !release::exact_keys(
                &manifest,
                &[
                    "schema",
                    "kind",
                    "source_sha",
                    "cargo_lock_sha256",
                    "entrypoint",
                    "bundles",
                ],
            )
            || manifest["schema"] != 1
            || manifest["kind"] != "rust-native-v1"
            || manifest["entrypoint"] != "bin/sidevoice-core-rust"
        {
            return Err(release::authenticity("manifest"));
        }
        let hex = |value: &Value, length: usize| {
            value.as_str().is_some_and(|s| {
                s.len() == length
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        };
        if !hex(&manifest["source_sha"], 40) || !hex(&manifest["cargo_lock_sha256"], 64) {
            return Err(release::authenticity("manifest"));
        }
        let targets = ["macos-aarch64", "linux-x86_64", "linux-aarch64"];
        if !release::exact_keys(&manifest["bundles"], &targets) {
            return Err(release::authenticity("manifest"));
        }
        for target in targets {
            let bundle = &manifest["bundles"][target];
            let expected = format!(
                "sidevoice-core-rust-{}-{target}.tar.zst",
                manifest["source_sha"].as_str().unwrap()
            );
            if !release::exact_keys(bundle, &["name", "size", "sha256"])
                || bundle["name"] != expected
                || !bundle["size"]
                    .as_u64()
                    .is_some_and(|n| n > 0 && n <= 250_000_000)
                || !hex(&bundle["sha256"], 64)
            {
                return Err(release::authenticity("manifest"));
            }
        }
        Ok(manifest)
    };
    parse().map_err(|_| release::authenticity("manifest"))
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
        release::write(&temp.join("dist/sidevoice"), &bytes, 0o700)?;
        // Both compatibility paths name the same native executable, without duplicating its embedded Core.
        fs::hard_link(
            temp.join("dist/sidevoice"),
            temp.join("dist/sidevoice-rust"),
        )?;
        release::unpack_core(
            payload.archive,
            manifest,
            env!("SIDEVOICE_CONNECTOR_TARGET"),
            &temp.join("core"),
        )?;
        cancelled(c)?;
        let mut core = Command::new(temp.join("core/bin/sidevoice-core-rust"));
        core.arg("--self-test")
            .arg(temp.join("core/checks/detector-16k.wav"))
            .arg(temp.join("core/models"))
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
        let output = timeout(Duration::from_secs(120), core.output()).await??;
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
    previous_id: Option<String>,
    token: String,
    committed: bool,
}
impl Gate {
    fn begin(p: &Paths, next: &Value) -> Result<Self> {
        let old_stop = release::read_json(&p.data.join("node-stopped.json"))?;
        let old_switch = release::read_json(&p.data.join("runtime-switch.json"))?;
        let token = uuid::Uuid::new_v4().to_string();
        let previous_id = p
            .selected("current")?
            .and_then(|value| value["id"].as_str().map(str::to_owned));
        release::write_json(
            &p.data.join("runtime-switch.json"),
            &json!({"phase":"active","token":token,"pid":std::process::id(),"start":null,"from":previous_id,"to":next["id"],"runtimeKind":kind(next),"previousSwitch":if old_switch.is_null(){Value::Null}else{json!(old_switch.to_string())},"previousStop":if old_stop.is_null(){Value::Null}else{json!(old_stop.to_string())}}),
        )?;
        release::write_json(
            &p.data.join("node-stopped.json"),
            &json!({"runtime_switch_token":token}),
        )?;
        Ok(Self {
            paths: p.clone(),
            previous_stop: old_stop,
            previous_switch: old_switch,
            previous_id,
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
        // A pointer already committed must retain its barrier for recovery, never restore the old gate.
        match self.paths.selected("current") {
            Ok(value)
                if value
                    .as_ref()
                    .and_then(|value| value["id"].as_str().map(str::to_owned))
                    == self.previous_id => {}
            _ => return,
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
    if v["to"] == current["id"]
        && !current.is_null()
        && (v.get("from").is_none() || v["from"] != v["to"])
    {
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
fn native_identity_matches(selected: &Value, identity: &Value, managed: bool) -> bool {
    identity["release_id"] == selected["id"]
        && identity["runtime_kind"] == selected["runtime_kind"]
        && identity["version"] == selected["connector"]
        && identity["runtime_sha256"] == selected["runtime_sha256"]
        && identity["runtime_build_sha"] == selected["runtime_build_sha"]
        && identity["runtime_target"] == selected["runtime_target"]
        && identity["managed"].as_bool() == Some(managed)
}
async fn selection_running(
    p: &Paths,
    selected: &Value,
    managed: bool,
    previous_launch: Option<&str>,
) -> Result<bool> {
    let Some(health) = service::health(p).await? else {
        return Ok(false);
    };
    if health["version"] != selected["core"]
        || health["api"] != 1
        || previous_launch.is_some_and(|id| health["launch_id"].as_str() == Some(id))
    {
        return Ok(false);
    }
    let connector_pid = if kind(selected) == "rust-native-v1" {
        let Some(identity) = service::ipc(p, "identity", json!({})).await? else {
            return Ok(false);
        };
        let expected = p.root.join("current/dist/sidevoice-rust").canonicalize()?;
        if !native_identity_matches(selected, &identity, managed)
            || identity["executable"].as_str() != expected.to_str()
        {
            return Ok(false);
        }
        identity["pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
    } else {
        if !service::ipc(p, "status", json!({}))
            .await?
            .is_some_and(|v| v["version"] == selected["connector"])
        {
            return Ok(false);
        }
        release::read_json(&p.data.join("connector.lock"))?["pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
    };
    if managed {
        service::managed_owners(
            p,
            health["pid"]
                .as_u64()
                .and_then(|pid| u32::try_from(pid).ok()),
            connector_pid,
        )
        .await
    } else {
        Ok(true)
    }
}
async fn verify(
    p: &Paths,
    selected: &Value,
    managed: bool,
    previous_launch: Option<&str>,
) -> Result<()> {
    let end = Instant::now() + Duration::from_secs(60);
    while Instant::now() < end {
        if selection_running(p, selected, managed, previous_launch).await? {
            return Ok(());
        }
        sleep(Duration::from_millis(200)).await;
    }
    Err(release::refusal("install.verify", json!({})))
}
fn needs_handoff(
    action: &str,
    verified: Option<&Value>,
    chosen: &Value,
    mode_change: bool,
    running: bool,
) -> bool {
    action != "noop" || verified.is_none_or(|v| v["id"] != chosen["id"]) || mode_change || !running
}
async fn reconcile() -> Result<Value> {
    let result = crate::agents::reconcile_owned(crate::proof::Profile::for_control_env()?).await?;
    require_reconciled(&result)?;
    Ok(result)
}
fn require_reconciled(result: &Value) -> Result<()> {
    if result["ok"] != true {
        return Err(release::refusal(
            "agents.reconciliation-failed",
            json!({"failures":result["failures"],"next":result["next"]}),
        ));
    }
    Ok(())
}
async fn resume_unchanged(
    p: &Paths,
    current: &Value,
    managed: bool,
    was_running: bool,
    was_stopped: bool,
) -> Result<()> {
    if was_running
        && !was_stopped
        && p.selected("current")?
            .is_some_and(|selected| selected["id"] == current["id"])
    {
        start_selection(p, managed).await?;
    }
    Ok(())
}
async fn go_back(p: &Paths, managed: bool, show: bool, explicit: bool) -> Result<Option<Value>> {
    let current = p.selected("current")?.unwrap_or(Value::Null);
    let back = [p.selected("verified")?, p.selected("previous")?]
        .into_iter()
        .flatten()
        .find(|v| v["id"] != current["id"]);
    let Some(back) = back else { return Ok(None) };
    let cross_runtime = kind(&current) != kind(&back);
    if cross_runtime && explicit {
        empty_outbox(p)?;
    }
    let was_stopped = service::stopped(p)?;
    let was_running = managed || service::runtime_present(p).await?;
    progress(show, "rollback");
    let old_launch = service::health(p)
        .await?
        .and_then(|h| h["launch_id"].as_str().map(str::to_owned));
    let mut gate = Gate::begin(p, &back)?;
    let switching = async {
        quiesce(p).await?;
        if cross_runtime {
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
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = switching {
        drop(gate);
        resume_unchanged(p, &current, managed, was_running, was_stopped).await?;
        return Err(error);
    }
    start_selection(p, managed).await?;
    verify(p, &back, managed, old_launch.as_deref()).await?;
    p.point("verified", id(&back)?)?;
    reconcile().await?;

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
    release::remove_leftovers(&p)?;
    let manifest = manifest(&payload)
        .map_err(|_| release::refusal("install.authenticity", json!({"check":"manifest"})))?;
    let next = candidate(&payload, &manifest)?;
    let current = p.selected("current")?;
    let action = release::decide(current.as_ref(), &next);
    let chosen = if action == "noop" {
        current.clone().context("noop requires selection")?
    } else {
        next
    };
    let managed_before = service::defined(&p)?;
    let managed = options.service || managed_before;
    let mode_change = managed && !managed_before;
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
    }
    cancelled(&cancellation)?;
    let old_launch = service::health(&p)
        .await?
        .and_then(|h| h["launch_id"].as_str().map(str::to_owned));
    let verified = p.selected("verified")?;
    let running = action == "noop"
        && !service::stopped(&p)?
        && selection_running(&p, &chosen, managed_before, None).await?;
    let needs_restart = needs_handoff(action, verified.as_ref(), &chosen, mode_change, running);
    if needs_restart && current.is_some() && !options.apply_now {
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
    if needs_restart {
        let was_stopped = service::stopped(&p)?;
        let was_running = managed_before || service::runtime_present(&p).await?;
        let mut gate = Gate::begin(&p, &chosen)?;
        let switching = async {
            quiesce(&p).await?;
            if current.as_ref().is_some_and(|v| kind(v) != kind(&chosen)) {
                empty_outbox(&p)?;
            }
            cancelled(&cancellation)?;
            progress(options.progress, "commit");
            if current
                .as_ref()
                .is_none_or(|value| value["id"] != chosen["id"])
            {
                if let Some(v) = verified {
                    p.point("previous", id(&v)?)?;
                }
                p.point("current", id(&chosen)?)?;
            }
            gate.commit(&chosen)?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = switching {
            drop(gate);
            if let Some(current) = &current {
                resume_unchanged(&p, current, managed_before, was_running, was_stopped).await?;
            }
            return Err(error);
        }
    }
    service::restore_gate(&p)?;
    let started = if needs_restart {
        progress(options.progress, "service-start");
        start_selection(&p, managed).await
    } else {
        Ok(())
    };
    let verification = match started {
        Ok(()) => {
            verify(
                &p,
                &chosen,
                managed,
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
    reconcile().await?;
    let mut registrations = if options.no_agents {
        json!({})
    } else {
        crate::agents::register_requested(
            crate::proof::Profile::for_control_env()?,
            &options.harnesses,
        )
        .await?
    };
    if !options.no_agents
        && (options.harnesses.iter().any(|id| id == "claude")
            || registrations["requested"]
                .as_array()
                .is_some_and(|ids| ids.iter().any(|id| id == "claude")))
    {
        let skill = crate::skill::cleanup_owned(&crate::proof::Profile::for_control_env()?)?;
        merge_notices(&mut registrations, &skill);
    }
    release::prune(&p)?;
    progress(options.progress, "pairing");
    let paired = release::read_json(&p.data.join("credentials.json"))?;
    let state = service::status(&p, false).await?;
    let mut result = json!({"ok":true,"action":action,"installed":chosen["id"],"previous":current.as_ref().map(|value| value["id"].clone()),"connector":chosen["connector"],"core":chosen["core"],"channel":chosen["channel"],"command":service::stable_command(&p)?,"service":if managed{service::manager()}else{"none"},"state":state["state"],"agents":registrations,"paired":!paired.is_null(),"room":paired["url"]});
    if managed && service::manager() == "systemd" {
        result["linger"] = service::linger().await;
    }
    Ok(result)
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
    let mut registrations =
        crate::agents::cleanup_owned(crate::proof::Profile::for_control_env()?).await?;
    if registrations.get("error").is_some() {
        return Err(release::refusal("install.failed", json!({})));
    }
    let skill = crate::skill::cleanup_owned(&crate::proof::Profile::for_control_env()?)?;
    merge_notices(&mut registrations, &skill);
    for core in [true, false] {
        release::remove_file(&service::definition(&p, core)?)?;
    }
    service::reload_after_uninstall().await?;
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
        "core.log",
        "core.stderr.log",
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
    release::cleanup_legacy_runtimes(&p, true)?;
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
    clean_core_state(&p.data.join("core"))?;
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

fn clean_core_state(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    crate::proof::private_dir(path)?;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().ends_with(".lock") {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(release::refusal(
                "control.uninstall-state-contains-a-link",
                json!({}),
            ));
        }
        if kind.is_dir() {
            clean_core_state(&entry.path())?;
            let _ = fs::remove_dir(entry.path());
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn merge_notices(target: &mut Value, source: &Value) {
    for key in ["done", "next"] {
        if let Some(values) = source[key].as_array() {
            if !target[key].is_array() {
                target[key] = json!([]);
            }
            target[key]
                .as_array_mut()
                .unwrap()
                .extend(values.iter().cloned());
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (std::path::PathBuf, Paths) {
        let root =
            std::env::temp_dir().join(format!("sidevoice-lifecycle-test-{}", uuid::Uuid::new_v4()));
        let p = Paths {
            home: root.join("home"),
            data: root.join("data"),
            root: root.join("sidevoice"),
            config: root.join("config"),
        };
        p.prepare().unwrap();
        for name in ["old", "new"] {
            release::private_directory(&p.root.join("releases").join(name)).unwrap();
            release::write_json(
                &p.root.join("releases").join(name).join("release.json"),
                &json!({"id":name,"runtime_kind":"rust-native-v1"}),
            )
            .unwrap();
        }
        p.point("current", "old").unwrap();
        (root, p)
    }
    #[test]
    fn aborted_gate_preserves_persons_stop_and_previous_marker() {
        let (root, p) = fixture();
        let stop = json!({"at":"2026-10-04T00:00:00Z"});
        let marker = json!({"phase":"committed","token":"older","to":"old"});
        release::write_json(&p.data.join("node-stopped.json"), &stop).unwrap();
        release::write_json(&p.data.join("runtime-switch.json"), &marker).unwrap();
        {
            let _gate =
                Gate::begin(&p, &json!({"id":"new","runtime_kind":"rust-native-v1"})).unwrap();
        }
        assert_eq!(
            release::read_json(&p.data.join("node-stopped.json")).unwrap(),
            stop
        );
        assert_eq!(
            release::read_json(&p.data.join("runtime-switch.json")).unwrap(),
            marker
        );
        assert!(service::stopped(&p).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn interrupted_pointer_flip_recovers_selected_launch_gate() {
        let (root, p) = fixture();
        let gate = Gate::begin(&p, &json!({"id":"new","runtime_kind":"rust-native-v1"})).unwrap();
        p.point("current", "new").unwrap();
        std::mem::forget(gate);
        recover_gate(&p).unwrap();
        assert!(!service::stopped(&p).unwrap());
        let marker = release::read_json(&p.data.join("runtime-switch.json")).unwrap();
        assert_eq!(marker["phase"], "committed");
        assert_eq!(marker["to"], "new");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn candidate_cleanup_never_deletes_verified_or_previous_release() {
        let (root, p) = fixture();
        p.point("previous", "new").unwrap();
        drop(CandidateCleanup {
            paths: p.clone(),
            candidate: Some("new".into()),
        });
        assert!(p.root.join("releases/new").exists());
        release::remove_file(&p.root.join("previous")).unwrap();
        drop(CandidateCleanup {
            paths: p.clone(),
            candidate: Some("new".into()),
        });
        assert!(!p.root.join("releases/new").exists());
        assert!(p.root.join("releases/old").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn healthy_noop_is_verify_only_but_mode_conversion_requires_handoff() {
        let current = json!({"id":"verified"});
        assert!(!needs_handoff(
            "noop",
            Some(&current),
            &current,
            false,
            true
        ));
        assert!(needs_handoff("noop", Some(&current), &current, true, true));
        assert!(needs_handoff(
            "noop",
            Some(&current),
            &current,
            false,
            false
        ));
        assert!(needs_handoff(
            "upgrade",
            Some(&current),
            &json!({"id":"new"}),
            false,
            true
        ));
        let selected = json!({"id":"same","runtime_sha256":"digest","runtime_build_sha":"source","runtime_target":"target"});
        let identity = json!({"release_id":"same","runtime_sha256":"digest","runtime_build_sha":"source","runtime_target":"target","managed":false});
        assert!(native_identity_matches(&selected, &identity, false));
        assert!(!native_identity_matches(&selected, &identity, true));
    }
    #[tokio::test]
    async fn refused_cross_runtime_rollback_never_quiesces_or_changes_selection() {
        let (root, p) = fixture();
        release::write_json(
            &p.root.join("releases/new/release.json"),
            &json!({"id":"new","runtime_kind":"javascript"}),
        )
        .unwrap();
        p.point("previous", "new").unwrap();
        p.point("verified", "old").unwrap();
        let pending = json!([{"id":"pending-speech","text":"keep"}]);
        release::write_json(&p.data.join("outbox.json"), &pending).unwrap();
        let marker = json!({"at":"stopped-by-person"});
        release::write_json(&p.data.join("node-stopped.json"), &marker).unwrap();
        let _lock = p.lock().await.unwrap();
        let error = go_back(&p, false, false, true).await.unwrap_err();
        assert_eq!(
            error.downcast_ref::<release::ControlError>().unwrap().key,
            "install.runtime-switch-outbox"
        );
        assert_eq!(p.selected("current").unwrap().unwrap()["id"], "old");
        assert_eq!(p.selected("previous").unwrap().unwrap()["id"], "new");
        assert_eq!(
            release::read_json(&p.data.join("node-stopped.json")).unwrap(),
            marker
        );
        assert_eq!(
            release::read_json(&p.data.join("outbox.json")).unwrap(),
            pending
        );
        assert!(!p.data.join("runtime-switch.json").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn unverified_core_lock_refuses_rollback_before_pointer_or_data_mutation() {
        use fs2::FileExt;
        use std::os::unix::fs::OpenOptionsExt;
        let (root, p) = fixture();
        p.point("previous", "new").unwrap();
        let stop = json!({"at":"human"});
        release::write_json(&p.data.join("node-stopped.json"), &stop).unwrap();
        release::write_json(
            &p.data.join("core/keep.json"),
            &json!({"secret":"preserve"}),
        )
        .unwrap();
        let core_lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(p.data.join("core/core.lock"))
            .unwrap();
        core_lock.try_lock_exclusive().unwrap();
        let _install = p.lock().await.unwrap();
        assert!(go_back(&p, false, false, true).await.is_err());
        assert_eq!(p.selected("current").unwrap().unwrap()["id"], "old");
        assert_eq!(
            release::read_json(&p.data.join("node-stopped.json")).unwrap(),
            stop
        );
        assert!(p.data.join("core/keep.json").exists());
        drop(core_lock);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn reconciliation_failure_is_actionable_and_never_success() {
        let result = json!({"ok":false,"failures":[{"id":"claude","error":{"key":"agents.registration-failed"}}],"next":["manual repair"]});
        let error = require_reconciled(&result).unwrap_err();
        let value = error_value(&error);
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["key"], "agents.reconciliation-failed");
        assert_eq!(value["error"]["params"]["failures"][0]["id"], "claude");
        assert!(require_reconciled(&json!({"done":[],"next":["failure"]})).is_err());
    }
    #[test]
    fn invalid_manifest_keeps_authenticity_key_and_named_check() {
        for bytes in [b"not-json".as_slice(), b"{\"schema\":1}\n".as_slice()] {
            let payload = Payload {
                manifest: bytes,
                archive: &[],
                core_version: "0.1.0",
                channel: "nightly",
                build_seq: 1,
            };
            let error = manifest(&payload).unwrap_err();
            let value = error_value(&error);
            assert_eq!(value["error"]["key"], "install.authenticity");
            assert_eq!(value["error"]["params"]["check"], "manifest");
        }
    }
}
