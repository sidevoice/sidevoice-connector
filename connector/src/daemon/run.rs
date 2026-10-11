//! The daemon's run loop: the core link, the façades' socket, replays and shutdown.

use super::Daemon;
use crate::adapters::Registry;
use crate::link::Link;
use crate::profile::Profile;
use crate::secure_fs::{private_dir, verify_socket};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot, watch, Semaphore};
use tokio::task::JoinSet;
use tokio::time::{Duration, Instant};

pub(super) fn prune_binding_order(order: &mut HashMap<String, oneshot::Receiver<()>>) {
    order.retain(|_, tail| matches!(tail.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
}

pub async fn run(profile: Profile, managed: bool) -> Result<()> {
    // Refused, not repaired: a data directory others can write into is not one to serve from.
    crate::secure_fs::ensure_private_dir(&profile.data)?;
    crate::logfile::init(profile.data.join("connector.log"), managed);
    profile.validate_existing_private()?;
    if managed {
        profile.validate_service_environment()?;
        // Started by its manager (at login, or by `service start`): a person's stop from before no longer holds.
        if !crate::service::managed_start_clears_stop(&profile).await? {
            return Ok(());
        }
    }
    if profile.service_stopped()? {
        crate::logfile::log(&crate::messages::message(
            "service.node-stopped",
            &Value::Null,
        ));
        return Ok(());
    }
    private_dir(&profile.data)?;
    let lock = profile.try_connector_lock()?;
    if profile.service_stopped()? {
        return Ok(());
    }
    if profile.socket.exists() {
        verify_socket(&profile.socket)?;
        fs::remove_file(&profile.socket)?;
    }
    let link = Link::new();
    let (replay_tx, mut replay_rx) = mpsc::channel(1);
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let daemon = Daemon::new(
        profile.clone(),
        link.clone(),
        replay_tx,
        Registry::standard(),
        managed,
        shutdown_tx,
    )?;
    let (incoming_tx, mut incoming_rx) = mpsc::channel(128);
    let clients = Arc::new(Semaphore::new(32));
    let mut core_tasks = JoinSet::new();
    let mut replay_tasks = JoinSet::new();
    let mut client_tasks = JoinSet::new();
    let mut replay_again = false;
    let mut core_session = 0u64;
    let mut binding_order = HashMap::<String, oneshot::Receiver<()>>::new();
    // Accept MCP before Core is ready. The link task retries the private Core socket until the
    // separately manager-owned Core job becomes healthy.
    let listener = UnixListener::bind(&profile.socket)?;
    fs::set_permissions(&profile.socket, fs::Permissions::from_mode(0o600))?;
    let (ready_tx, ready_rx) = oneshot::channel();
    let supervisor = Arc::new(crate::service::core_supervisor(&profile));
    let link_task = tokio::spawn(link.run(profile.clone(), incoming_tx, ready_tx, supervisor));
    drop(ready_rx);
    let scanner = if managed {
        let host_agents = daemon.host_agents.clone();
        let scan_profile = profile.clone();
        Some(tokio::spawn(async move {
            loop {
                if !scan_profile.service_stopped().unwrap_or(true) {
                    let _ = host_agents
                        .handle("agents.list", serde_json::json!({"rescan":true}))
                        .await;
                }
                tokio::time::sleep(Duration::from_secs(6 * 60 * 60)).await;
            }
        }))
    } else {
        None
    };
    let mut idle_since = Instant::now();
    let mut last_activity = daemon.activity_generation.load(Ordering::Relaxed);
    let mut idle_check = tokio::time::interval_at(
        Instant::now() + Duration::from_millis(200),
        Duration::from_millis(200),
    );
    idle_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut link_exited = false;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            Some(result) = replay_tasks.join_next(), if !replay_tasks.is_empty() => {
                if let Err(error) = result { crate::logfile::log(&format!("outbox replay: {error}")); }
                if replay_again {
                    replay_again = false;
                    let daemon = daemon.clone();
                    replay_tasks.spawn(async move { daemon.flush_outbox().await; });
                }
            }
            Some(result) = client_tasks.join_next(), if !client_tasks.is_empty() => {
                if let Err(error) = result { crate::logfile::log(&format!("IPC task: {error}")); }
            }
            Some(()) = replay_rx.recv() => {
                if replay_tasks.is_empty() {
                    let daemon = daemon.clone();
                    replay_tasks.spawn(async move { daemon.flush_outbox().await; });
                } else {
                    replay_again = true;
                }
            }
            Some(result) = core_tasks.join_next(), if !core_tasks.is_empty() => {
                if let Err(error) = result { crate::logfile::log(&format!("Core handler: {error}")); }
                prune_binding_order(&mut binding_order);
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                if let Ok(permit) = clients.clone().try_acquire_owned() {
                    let daemon = daemon.clone();
                    client_tasks.spawn(async move { let _permit = permit; daemon.serve_client(stream).await });
                }
            }
            item = incoming_rx.recv() => {
                let Some(item) = item else { link_exited=true; break; };
                if item.method == "connector.lost" {
                    if item.session == core_session {
                        core_tasks.abort_all();
                        while core_tasks.join_next().await.is_some() {}
                        replay_tasks.abort_all();
                        while replay_tasks.join_next().await.is_some() {}
                        replay_again = false;
                        binding_order.clear();
                        core_session = 0;
                    }
                    continue;
                }
                if item.session != core_session {
                    core_tasks.abort_all();
                    while core_tasks.join_next().await.is_some() {}
                    replay_tasks.abort_all();
                    while replay_tasks.join_next().await.is_some() {}
                    replay_again = false;
                    binding_order.clear();
                    core_session = item.session;
                }
                if core_tasks.len() >= 64 {
                    if let Some(tx) = item.reply {
                        let _ = tx.send(json!({"status":"failed","detail":"connector handler capacity reached"}));
                    } else {
                        crate::logfile::log("Core notification capacity reached");
                    }
                    continue;
                }
                if item.method == "binding.close" {
                    if let Some(id) = item.params.get("binding_id").and_then(Value::as_str) {
                        binding_order.remove(id);
                    }
                }
                let daemon = daemon.clone();
                let (previous, completed) = if item.method == "input.deliver" {
                    let id = item.params.get("binding_id")
                        .and_then(Value::as_str).unwrap_or("").to_owned();
                    let (tx, rx) = oneshot::channel();
                    (binding_order.insert(id, rx), Some(tx))
                } else {
                    (None, None)
                };
                core_tasks.spawn(async move {
                    if let Some(previous) = previous {
                        let _ = previous.await;
                    }
                    daemon.incoming(item).await;
                    if let Some(completed) = completed {
                        let _ = completed.send(());
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => { break; }
            _ = terminate.recv() => { break; }
            _ = shutdown_rx.changed() => { break; }
            _ = idle_check.tick(), if !managed => {
                let activity = daemon.activity_generation.load(Ordering::Relaxed);
                if activity != last_activity {
                    idle_since = Instant::now();
                    last_activity = activity;
                }
                if daemon.is_idle().await {
                    if idle_since.elapsed() >= Duration::from_secs(15) { break; }
                } else {
                    idle_since = Instant::now();
                }
            }
        }
    }
    daemon.shutdown.send_replace(true);
    if !client_tasks.is_empty() {
        let drained = tokio::time::timeout(Duration::from_secs(2), async {
            while client_tasks.join_next().await.is_some() {}
        })
        .await;
        if drained.is_err() {
            client_tasks.abort_all();
            while client_tasks.join_next().await.is_some() {}
        }
    }
    if let Some(scanner) = scanner {
        scanner.abort();
        let _ = scanner.await;
    }
    link_task.abort();
    let _ = link_task.await;
    replay_tasks.abort_all();
    while replay_tasks.join_next().await.is_some() {}
    core_tasks.abort_all();
    while core_tasks.join_next().await.is_some() {}
    daemon.host_agents.shutdown().await;
    fs::remove_file(&profile.socket)?;
    drop(lock);
    if link_exited {
        bail!("Core authentication was refused or its link task exited");
    }
    Ok(())
}
