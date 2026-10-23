//! The `sidevoice service …` commands: `install`, `start`, `stop`, `restart`, `uninstall`, `login`, and `run`, which
//! dispatches an [`Action`] and answers in JSON, with `human` to read that answer as a person does.

use super::definition;
use super::jobs::{self, Job, Kind};
use super::layout::Layout;
use super::{
    clear_stop, data_dir, define_and_start, install_lock, launcher, status, stop_on_demand_or_fail,
    Action, Failure, Result,
};
use crate::platform::fs::{Files, Fs};
use crate::platform::service::Service;
use serde_json::{json, Value};
use tokio::time::{sleep, Duration, Instant};

/// How long `install` waits for the core to settle, and `start` and `restart`.
const INSTALL_SETTLE: Duration = Duration::from_secs(60);
const SETTLE: Duration = Duration::from_secs(10);

fn write_stop(layout: &Layout) -> Result<()> {
    data_dir(layout)?;
    Files::write_json(
        &layout.stop_marker(),
        &json!({"at": crate::logfile::timestamp()}),
    )
    .map_err(Failure::plain)
}

/// Wait, up to `limit`, for the core's state to settle: what start, restart and install answer with.
async fn settle(layout: &Layout, limit: Duration) -> Value {
    let deadline = Instant::now() + limit;
    let mut now = status::status(layout, None).await;
    while !status::settled(&now) && Instant::now() < deadline {
        sleep(Duration::from_millis(200)).await;
        now = status::status(layout, None).await;
    }
    now
}

fn answer(state: &Value, service: Kind) -> Value {
    let mut answer = json!({"ok": true, "state": state.get("state").cloned().unwrap_or(Value::Null),
        "service": service.as_str()});
    if let Some(failure) = state.get("failure").filter(|failure| !failure.is_null()) {
        answer["failure"] = failure.clone();
    }
    answer
}

/// `service install`: both definitions written for the selected installation, anything running on demand stopped,
/// both jobs started; answered with the core's state once it settles. Linux says what linger would add.
pub async fn install(layout: &Layout) -> Result<Value> {
    let _lock = install_lock(layout).await?;
    if !layout.installed() {
        return Err(Failure::keyed("service.no-installation", json!({})));
    }
    let kind = jobs::kind().await;
    if kind == Kind::None {
        return Err(Failure::keyed("service.no-manager", json!({})));
    }
    definition::definitions(layout)?;
    clear_stop(layout)?;
    define_and_start(layout, false).await?;
    let now = settle(layout, INSTALL_SETTLE).await;
    let mut result = answer(&now, kind);
    if let Some(linger) = jobs::linger(&jobs::manager(layout)).await {
        result["linger"] = linger;
    }
    Ok(result)
}

fn killed_note(result: &mut Value, down: &launcher::Teardown) {
    if !down.killed.is_empty() {
        let pids = down
            .killed
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        result["note"] = json!(crate::messages::message(
            "service.killed",
            &json!({"pids": pids})
        ));
    }
}

/// `service start`: a stop no longer holds; the manager starts both jobs. With no jobs, nothing is started here:
/// the next conversation starts Sidevoice on demand.
pub async fn start(layout: &Layout) -> Result<Value> {
    let installed = jobs::installed(layout).await;
    {
        let _lock = install_lock(layout).await?;
        clear_stop(layout)?;
        if let Some(found) = &installed {
            let service = jobs::manager(layout);
            let login = jobs::keeps_login(&service, Some(found)).await;
            jobs::start(layout, &service, &[], false, login, &Job::ALL).await?;
        }
    }
    let now = settle(layout, SETTLE).await;
    Ok(answer(
        &now,
        installed.map_or(Kind::None, |_| Kind::Manager),
    ))
}

/// `service stop`: the person's stop, written first so nothing starts Sidevoice again meanwhile; then the manager
/// stops both jobs (launchd: bootout, so KeepAlive does not bring them back until the next login). With no jobs,
/// what runs on demand is stopped.
pub async fn stop(layout: &Layout) -> Result<Value> {
    let _lock = install_lock(layout).await?;
    write_stop(layout)?;
    let installed = jobs::installed(layout).await;
    if let Some(found) = &installed {
        if let Some(refused) = jobs::unload(&jobs::manager(layout), found, false).await {
            return Err(Failure::keyed(
                "service.unload-failed",
                json!({"detail": refused}),
            ));
        }
    }
    let down = stop_on_demand_or_fail(layout).await?;
    let mut result = json!({"ok": true, "state": "stopped-by-person",
        "service": installed.map_or(Kind::None, |_| Kind::Manager).as_str()});
    killed_note(&mut result, &down);
    Ok(result)
}

/// `service restart`, the person's retry: the core job restarted (its start limit cleared first); stopped, it is a
/// start. With no jobs, the core running on demand is ended, and the connector starts it again when it needs it.
pub async fn restart(layout: &Layout) -> Result<Value> {
    if status::stopped(layout) {
        return start(layout).await;
    }
    let installed = jobs::installed(layout).await;
    {
        let _lock = install_lock(layout).await?;
        match &installed {
            Some(found) if found.core => {
                let service = jobs::manager(layout);
                let login = jobs::keeps_login(&service, Some(found)).await;
                jobs::start(layout, &service, &[], true, login, &[Job::Core]).await?;
            }
            _ => {
                for pid in launcher::core_processes(layout) {
                    launcher::terminate_core(layout, pid, Duration::from_secs(15)).await;
                }
            }
        }
    }
    let now = settle(layout, SETTLE).await;
    Ok(answer(
        &now,
        installed.map_or(Kind::None, |_| Kind::Manager),
    ))
}

/// `service uninstall`: the stop written, both jobs unloaded (any refusal stops here, with nothing deleted), what
/// runs on demand stopped, both definitions deleted and the manager told. `keep_stopped`: the caller goes on
/// taking the installation apart and clears the marker itself. Idempotent.
pub async fn uninstall(layout: &Layout, keep_stopped: bool) -> Result<Value> {
    let _lock = install_lock(layout).await?;
    uninstall_held(layout, keep_stopped).await
}

/// `uninstall`, for a caller that already holds the install lock.
pub async fn uninstall_held(layout: &Layout, keep_stopped: bool) -> Result<Value> {
    write_stop(layout)?;
    let installed = jobs::installed(layout).await;
    let kind = installed.as_ref().map_or(Kind::None, |_| Kind::Manager);
    if let Some(found) = &installed {
        if let Some(refused) = jobs::unload(&jobs::manager(layout), found, true).await {
            return Err(Failure::keyed(
                "service.unload-failed",
                json!({"detail": refused}),
            ));
        }
    }
    let down = stop_on_demand_or_fail(layout).await?;
    if let Some(found) = &installed {
        let service = jobs::manager(layout);
        for job in Job::ALL.into_iter().filter(|&job| found.has(job)) {
            if let Err(refusal) = service.remove(job.id()).await {
                return Err(jobs::failure(&service, refusal).await);
            }
        }
        jobs::forget(&service).await?;
    }
    if !keep_stopped {
        clear_stop(layout)?;
    }
    let mut result = json!({"ok": true, "state": if layout.installed() { "not-installed" } else { "absent" },
        "service": kind.as_str()});
    killed_note(&mut result, &down);
    Ok(result)
}

/// `service login on|off`: whether both jobs start at the next login, and only that (`jobs::set_start_at_login`):
/// a running service keeps running, a stopped one stays stopped, a person's stop is left as it is. `service login
/// status` (`on` none) reads it. Answered as `start`, `stop` and `status` are, `{ok, state, service}` from the status
/// read afterwards, with its `start_at_login`: `null` when the manager cannot tell or the service is not whole.
pub async fn login(layout: &Layout, on: Option<bool>) -> Result<Value> {
    if let Some(on) = on {
        let _lock = install_lock(layout).await?;
        let Some(found) = jobs::installed(layout).await else {
            return Err(Failure::keyed(
                if jobs::kind().await == Kind::None {
                    "service.no-manager"
                } else {
                    "service.login-no-service"
                },
                json!({}),
            ));
        };
        // Half a service cannot be said to start at login or not: nothing is changed until it is whole again.
        let missing = jobs::missing(&found);
        if !missing.is_empty() {
            return Err(Failure::keyed(
                "service.login-incomplete",
                json!({"missing": jobs::names(&missing)}),
            ));
        }
        if let Err(detail) = jobs::set_start_at_login(&jobs::manager(layout), &found, on).await {
            return Err(Failure::keyed(
                "service.login-failed",
                json!({"detail": detail}),
            ));
        }
    }
    let now = status::status(layout, None).await;
    let mut result = json!({"ok": true, "state": now["state"], "service": now["service"],
        "start_at_login": now["start_at_login"]});
    if let Some(failure) = now.get("failure").filter(|failure| !failure.is_null()) {
        result["failure"] = failure.clone();
    }
    Ok(result)
}

/// One service command against the installation this environment names: its JSON answer.
pub async fn run(action: Action, agent_vars: &[&'static str]) -> Value {
    let layout = match Layout::from_env(agent_vars) {
        Ok(layout) => layout,
        Err(error) => return Failure::plain(error).value(),
    };
    let result = match action {
        Action::Status => Ok(status::status(&layout, None).await),
        Action::Install => install(&layout).await,
        Action::Start => start(&layout).await,
        Action::Stop => stop(&layout).await,
        Action::Restart => restart(&layout).await,
        Action::Uninstall => uninstall(&layout, false).await,
        Action::Login(on) => login(&layout, on).await,
    };
    result.unwrap_or_else(|failure| failure.value())
}

/// An answer as a person reads it: the state, its failure, and what to do about linger.
pub fn human(answer: &Value) -> String {
    if answer.get("ok") != Some(&json!(true)) {
        return answer
            .pointer("/error/message")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                crate::messages::message("service.failed", &status::detail_params(None))
            });
    }
    let state = answer
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("failed");
    let mut out = crate::messages::message(
        &format!("service.state.{state}"),
        &json!({"service": answer["service"]}),
    );
    if let Some(failure) = answer.get("failure").filter(|failure| failure.is_object()) {
        let reason = failure
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                crate::messages::message(failure["key"].as_str().unwrap_or(""), &Value::Null)
            });
        out.push_str(&format!(" ({reason})"));
    }
    if let Some(login) = answer.get("start_at_login") {
        let key = match login.as_bool() {
            Some(true) => "service.login.on",
            Some(false) => "service.login.off",
            None => "service.login.unknown",
        };
        out.push('\n');
        out.push_str(&crate::messages::message(key, &Value::Null));
    }
    if let Some(note) = answer.get("note").and_then(Value::as_str) {
        out.push('\n');
        out.push_str(note);
    }
    if let Some(linger) = answer
        .get("linger")
        .filter(|linger| linger["enabled"] == json!(false))
    {
        out.push_str(&format!(
            "\n\n{}\n    {}",
            linger["reason"].as_str().unwrap_or_default(),
            linger["command"].as_str().unwrap_or_default()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_login_answer_reads_as_its_state_and_whether_it_starts_at_login() {
        let off =
            json!({"ok": true, "state": "running", "service": "launchd", "start_at_login": false});
        assert_eq!(
            human(&off),
            "Sidevoice is running (launchd).\nSidevoice does not start at login."
        );
        let unknown = json!({"ok": true, "state": "not-installed", "service": "systemd", "start_at_login": null});
        assert!(
            human(&unknown).ends_with("\nWhether Sidevoice starts at login is not known: it has no login service here, or the service manager cannot tell."),
            "{}",
            human(&unknown)
        );
        let refused = Failure::keyed(
            "service.login-incomplete",
            json!({"missing": "dev.sidevoice.connector"}),
        );
        assert_eq!(
            human(&refused.value()),
            "The Sidevoice login service is incomplete (dev.sidevoice.connector is missing), so whether it starts at login was not changed. Repair it with `npx sidevoice service install`, then try again."
        );
    }
}
