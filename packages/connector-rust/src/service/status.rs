//! What the node is doing, never stored: read fresh from the manager, the core's failure report and the core's
//! health, the same answer for `service status --json` and for a connector's `node.status`.

use super::definition;
use super::layout::Layout;
use super::manager::{self, Job, JobState};
use super::text::message;
use crate::core_ready::Ready;
use crate::secure_fs::{private_file, verify_socket};
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::{timeout, Duration};

/// A core job running this long without being ready is not starting any more.
const STARTING_SECONDS: u64 = 60;

#[derive(Clone, Debug, Default)]
pub struct Observation {
    pub service: &'static str,
    pub installed: bool,
    pub defined_core: bool,
    pub defined_connector: bool,
    /// The core job as its manager reports it (only when it is defined).
    pub core_job: Option<JobState>,
    pub limit: Option<u64>,
    pub stopped: bool,
    /// The core's health answer, when it answered 200.
    pub health: Option<Value>,
    pub ready: Option<Ready>,
    /// The core's own report of its last failed start, as a person reads it.
    pub failure: Option<Value>,
    pub core_age: Option<u64>,
    /// Why the core job's program cannot run, if it cannot.
    pub program: Option<&'static str>,
    pub log_tail: Vec<String>,
    pub connector_running: bool,
    /// When this was observed (RFC 3339), for the failures derived from it.
    pub at: String,
}

/// The node's state from what was observed, in this order:
/// 1. the core answers its health → `running`, unless the service condition says otherwise (no job, a person's
///    stop, a manager that does not run the job), which wins, with `reachable: true`;
/// 2. nothing installed, nothing defined → `absent`; 3. no core job → `not-installed`; 4. stopped →
///    `stopped-by-person`;
/// 5. the manager does not have the job loaded, hit its start limit, or its program is missing →
///    `service-failed`;
/// 6. job running, not ready yet, for less than 60 s → `starting` (longer: `failed`, `ready.timeout`);
/// 7. job running and ready, health silent → `failed`, `hang`;
/// 8. job not running, the core's failure report there → `failed` with it;
/// 9. job not running, the manager will start it again → `backoff`, with the manager's count;
/// 10. otherwise → `failed`, `launch.exited`.
pub fn derive(o: &Observation) -> Value {
    let body = o.health.as_ref();
    let mut status = json!({
        "ok": true, "service": o.service, "installed": o.installed,
        "core": body.map(|body| json!({"pid": body.get("pid").cloned().unwrap_or(Value::Null),
            "version": body.get("version").cloned().unwrap_or(Value::Null),
            "api": body.get("api").cloned().unwrap_or(Value::Null),
            "launch_id": body.get("launch_id").cloned().unwrap_or(Value::Null)})),
        "calls": body.and_then(|body| body.get("calls")).filter(|calls| calls.is_number()).cloned(),
        "failure": null, "attempts": null, "limit": null, "since": null, "window_started": null,
        "next_retry_at": null, "reachable": body.is_some(), "connector": {"running": o.connector_running},
    });
    let job = o.core_job.as_ref();
    let service_failure = if o.defined_core {
        job.and_then(|job| job.reason).or(o.program)
    } else {
        None
    };
    let held = if !o.installed && !o.defined_core && !o.defined_connector {
        Some(("absent", None))
    } else if !o.defined_core {
        Some(("not-installed", None))
    } else if o.stopped {
        Some(("stopped-by-person", None))
    } else {
        service_failure.map(|key| {
            (
                "service-failed",
                Some(json!({"key": key, "message": message(key, &Value::Null)})),
            )
        })
    };
    let failed = |key: &str, step: &str, detail: Option<Value>| {
        let mut failure = json!({"key": key, "step": step, "at": o.at, "log_tail": o.log_tail,
            "message": message(key, &json!({"detail": detail.clone().unwrap_or(Value::Null)}))});
        if let Some(detail) = detail {
            failure["detail"] = detail;
        }
        failure
    };
    let (state, failure) = match held {
        Some(held) => held,
        None if body.is_some() => ("running", None),
        None => {
            let job = job.cloned().unwrap_or_default();
            if job.running {
                if o.ready.as_ref().map(|ready| ready.pid) != job.pid {
                    if o.core_age.is_some_and(|age| age > STARTING_SECONDS) {
                        ("failed", Some(failed("ready.timeout", "ready", None)))
                    } else {
                        ("starting", None)
                    }
                } else {
                    ("failed", Some(failed("hang", "health", None)))
                }
            } else if let Some(report) = &o.failure {
                ("failed", Some(report.clone()))
            } else {
                let last_exit = job
                    .signal
                    .clone()
                    .map(Value::from)
                    .or(job.exit.map(Value::from));
                let exited = failed(
                    "launch.exited",
                    "run",
                    Some(last_exit.unwrap_or(Value::Null)),
                );
                if job.restarting {
                    status["attempts"] = json!(job.runs);
                    status["limit"] = json!(o.limit);
                    ("backoff", Some(exited))
                } else {
                    ("failed", Some(exited))
                }
            }
        }
    };
    status["state"] = json!(state);
    if let Some(failure) = failure {
        status["failure"] = failure;
    }
    status
}

/// Whether a status is final for whoever just started the core: running, or a failure that will not change by
/// waiting. An exit the manager may follow with a start, a health probe unanswered a moment after a restart, a
/// core still starting: not yet.
pub fn settled(status: &Value) -> bool {
    match status.get("state").and_then(Value::as_str) {
        Some("failed") => !matches!(
            status.pointer("/failure/step").and_then(Value::as_str),
            Some("run" | "health" | "ready")
        ),
        Some("starting" | "backoff") => false,
        _ => true,
    }
}

/// The core's ready file (`core.json`) when it is this user's private file naming this data directory's socket.
pub fn read_ready(layout: &Layout) -> Option<Ready> {
    let path = layout.core_ready();
    private_file(&path).ok()?;
    if fs::metadata(&path).ok()?.len() > 64 * 1024 {
        return None;
    }
    let ready: Ready = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
    (ready.pid > 0 && !ready.launch_id.is_empty() && ready.socket == layout.core_socket())
        .then_some(ready)
}

/// `GET /api/local/health` on the core's socket: the body of a 200 within `limit`, or none.
pub async fn health(socket: &Path, limit: Duration) -> Option<Value> {
    verify_socket(socket).ok()?;
    let ask = async {
        let mut stream = UnixStream::connect(socket).await.ok()?;
        stream
            .write_all(
                b"GET /api/local/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .await
            .ok()?;
        let mut response = Vec::new();
        stream
            .take(64 * 1024)
            .read_to_end(&mut response)
            .await
            .ok()?;
        let response = String::from_utf8(response).ok()?;
        if !response.starts_with("HTTP/1.1 200 ") && !response.starts_with("HTTP/1.0 200 ") {
            return None;
        }
        let (_, body) = response.split_once("\r\n\r\n")?;
        serde_json::from_str::<Value>(body)
            .ok()
            .filter(Value::is_object)
    };
    timeout(limit, ask).await.ok().flatten()
}

/// The core serving now: its ready file, and its health answering for that same launch.
pub async fn serving(layout: &Layout, limit: Duration) -> Option<(Ready, Value)> {
    let ready = read_ready(layout)?;
    let body = health(&ready.socket, limit).await?;
    (body.get("launch_id").and_then(Value::as_str) == Some(ready.launch_id.as_str()))
        .then_some((ready, body))
}

fn bounded_private(path: &Path, limit: u64) -> Option<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
        || metadata.len() > limit
    {
        return None;
    }
    fs::read(path).ok()
}

/// The last lines of the core's log, for a failure a person reads.
pub fn log_tail(layout: &Layout, lines: usize) -> Vec<String> {
    let Ok(mut file) = fs::File::open(layout.core_log()) else {
        return Vec::new();
    };
    let length = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(length.saturating_sub(16 * 1024)));
    let mut text = String::new();
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_ok() {
        text = String::from_utf8_lossy(&bytes).into_owned();
    }
    let all: Vec<&str> = text.trim_end().lines().collect();
    all[all.len().saturating_sub(lines)..]
        .iter()
        .map(|line| {
            line.chars()
                .filter(|ch| !ch.is_control())
                .take(400)
                .collect()
        })
        .collect()
}

/// The core's report of its last failed start (written before ready, deleted once it holds its lock), as a person
/// reads it: its own key, step and message, with the log tail.
pub fn read_failure(layout: &Layout, at: &str) -> Option<Value> {
    let report: Value =
        serde_json::from_slice(&bounded_private(&layout.core_failure(), 64 * 1024)?).ok()?;
    let text = |name: &str, limit: usize| {
        report.get(name).and_then(Value::as_str).map(|value| {
            value
                .chars()
                .filter(|ch| !ch.is_control())
                .take(limit)
                .collect::<String>()
        })
    };
    let key = text("key", 80).filter(|key| !key.is_empty())?;
    let detail = report
        .get("detail")
        .cloned()
        .filter(|detail| !detail.is_null());
    let message = text("message", 400).unwrap_or_else(|| {
        message(
            &key,
            &json!({"detail": detail.clone().unwrap_or(Value::Null)}),
        )
    });
    Some(
        json!({"key": key, "step": text("step", 40), "message": message, "detail": detail,
        "at": text("at", 40).unwrap_or_else(|| at.to_owned()), "launch_id": text("launch_id", 80),
        "log_tail": log_tail(layout, 10)}),
    )
}

/// Whether `D/node-stopped.json` is there.
pub fn stopped(layout: &Layout) -> bool {
    fs::symlink_metadata(layout.stop_marker()).is_ok()
}

/// Everything `derive` needs, read fresh. `connector_running`: the connector answering this (itself), else asked.
pub async fn observe(layout: &Layout, connector_running: Option<bool>) -> Observation {
    let installed = manager::installed(layout).await;
    let kind = match &installed {
        Some(installed) => installed.kind,
        None => manager::kind().await,
    };
    let defined_core = installed.as_ref().is_some_and(|found| found.core.is_some());
    let defined_connector = installed
        .as_ref()
        .is_some_and(|found| found.connector.is_some());
    let core_job = if defined_core {
        Some(manager::state(kind, Job::Core).await)
    } else {
        None
    };
    let program = match installed.as_ref().and_then(|found| found.core.clone()) {
        Some(file) => definition::program_problem(
            fs::read_to_string(file)
                .ok()
                .and_then(|text| definition::definition_program(kind, &text))
                .as_deref(),
        ),
        None => None,
    };
    let ready = read_ready(layout);
    let socket = ready
        .as_ref()
        .map(|ready| ready.socket.clone())
        .unwrap_or_else(|| layout.core_socket());
    let health = health(&socket, Duration::from_millis(1500)).await;
    let at = now();
    let core_age = match core_job
        .as_ref()
        .filter(|job| job.running)
        .and_then(|job| job.pid)
    {
        Some(pid) => super::process::age(pid),
        None => None,
    };
    let connector_running = match connector_running {
        Some(running) => running,
        None => super::launcher::ask_connector(layout, "status", json!({}), Duration::from_secs(1))
            .await
            .is_some(),
    };
    Observation {
        service: kind.as_str(),
        installed: layout.installed(),
        defined_core,
        defined_connector,
        core_job,
        limit: manager::limit(kind),
        stopped: stopped(layout),
        health,
        ready,
        failure: read_failure(layout, &at),
        core_age,
        program,
        log_tail: log_tail(layout, 10),
        connector_running,
        at,
    }
}

/// `service status --json`: derived, read-only, never starts anything.
pub async fn status(layout: &Layout, connector_running: Option<bool>) -> Value {
    derive(&observe(layout, connector_running).await)
}

/// Now, as RFC 3339 UTC, with no date crate.
pub fn now() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (seconds / 86_400) as i64;
    let clock = seconds % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        clock / 3600,
        (clock % 3600) / 60,
        clock % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(pid: u32) -> Ready {
        Ready {
            pid,
            launch_id: "launch".into(),
            socket: "/d/core/local.sock".into(),
            connector_id: "id".into(),
            token: "secret".into(),
            connector_protocols: Some(vec![2, 3]),
        }
    }

    fn running() -> Observation {
        Observation {
            service: "launchd",
            installed: true,
            defined_core: true,
            defined_connector: true,
            core_job: Some(JobState {
                loaded: true,
                running: true,
                pid: Some(7),
                ..JobState::default()
            }),
            health: Some(
                json!({"pid": 7, "version": "0.2.0", "api": 1, "launch_id": "launch", "calls": 3}),
            ),
            ready: Some(ready(7)),
            core_age: Some(4),
            connector_running: true,
            at: "2026-10-06T00:00:00Z".into(),
            ..Observation::default()
        }
    }

    #[test]
    fn a_healthy_core_runs_and_reports_itself() {
        let status = derive(&running());
        assert_eq!(status["state"], "running");
        assert_eq!(status["reachable"], true);
        assert_eq!(status["calls"], 3);
        assert_eq!(status["core"]["launch_id"], "launch");
        assert_eq!(status["connector"]["running"], true);
        assert_eq!(status["service"], "launchd");
        for key in [
            "failure",
            "attempts",
            "limit",
            "since",
            "window_started",
            "next_retry_at",
        ] {
            assert!(status[key].is_null(), "{key}: {status}");
        }
    }

    #[test]
    fn the_service_condition_wins_over_a_healthy_core() {
        let mut o = running();
        o.stopped = true;
        let status = derive(&o);
        assert_eq!(
            (status["state"].as_str(), status["reachable"].as_bool()),
            (Some("stopped-by-person"), Some(true))
        );

        let mut o = running();
        o.core_job.as_mut().unwrap().reason = Some("start-limit");
        let status = derive(&o);
        assert_eq!(status["state"], "service-failed");
        assert_eq!(status["failure"]["key"], "start-limit");

        let mut o = running();
        o.program = Some("executable-missing");
        assert_eq!(derive(&o)["failure"]["key"], "executable-missing");

        // On demand, with no service: a core that answers is still `not-installed`, and usable.
        let mut o = running();
        o.defined_core = false;
        o.defined_connector = false;
        o.core_job = None;
        let status = derive(&o);
        assert_eq!(
            (status["state"].as_str(), status["reachable"].as_bool()),
            (Some("not-installed"), Some(true))
        );
    }

    #[test]
    fn absent_and_not_installed_are_told_apart() {
        let nothing = Observation {
            service: "systemd",
            at: "t".into(),
            ..Observation::default()
        };
        assert_eq!(derive(&nothing)["state"], "absent");
        let release_only = Observation {
            installed: true,
            ..nothing.clone()
        };
        assert_eq!(derive(&release_only)["state"], "not-installed");
        let leftover_job = Observation {
            defined_connector: true,
            ..nothing
        };
        assert_eq!(derive(&leftover_job)["state"], "not-installed");
    }

    #[test]
    fn a_core_job_without_health_is_starting_then_timed_out_or_hung() {
        let mut o = running();
        o.health = None;
        o.ready = None;
        assert_eq!(derive(&o)["state"], "starting");
        o.ready = Some(ready(9));
        assert_eq!(
            derive(&o)["state"],
            "starting",
            "a ready file of an earlier launch"
        );
        o.core_age = Some(61);
        let status = derive(&o);
        assert_eq!(
            (status["state"].as_str(), status["failure"]["key"].as_str()),
            (Some("failed"), Some("ready.timeout"))
        );
        assert_eq!(status["failure"]["step"], "ready");
        o.ready = Some(ready(7));
        let status = derive(&o);
        assert_eq!(
            (status["state"].as_str(), status["failure"]["key"].as_str()),
            (Some("failed"), Some("hang"))
        );
        assert_eq!(status["reachable"], false);
    }

    #[test]
    fn a_stopped_core_job_is_failed_with_its_report_or_backing_off() {
        let mut o = running();
        o.health = None;
        o.core_job = Some(JobState {
            loaded: true,
            exit: Some(1),
            runs: Some(2),
            restarting: true,
            ..JobState::default()
        });
        o.limit = Some(5);
        let status = derive(&o);
        assert_eq!(status["state"], "backoff");
        assert_eq!(
            (status["attempts"].as_u64(), status["limit"].as_u64()),
            (Some(2), Some(5))
        );
        assert_eq!(status["failure"]["key"], "launch.exited");
        assert_eq!(status["failure"]["detail"], 1);
        assert_eq!(status["failure"]["message"], "The core exited (1).");

        o.failure = Some(json!({"key": "bind.port-in-use", "step": "bind", "message": "taken"}));
        let status = derive(&o);
        assert_eq!(
            (status["state"].as_str(), status["failure"]["key"].as_str()),
            (Some("failed"), Some("bind.port-in-use"))
        );

        o.failure = None;
        o.core_job.as_mut().unwrap().restarting = false;
        o.core_job.as_mut().unwrap().signal = Some("Killed: 9".into());
        let status = derive(&o);
        assert_eq!(status["state"], "failed");
        assert_eq!(status["failure"]["detail"], "Killed: 9");
        assert!(status["attempts"].is_null());
    }

    #[test]
    fn only_final_states_are_settled() {
        assert!(settled(&json!({"state": "running"})));
        assert!(settled(&json!({"state": "service-failed"})));
        assert!(!settled(&json!({"state": "starting"})));
        assert!(!settled(&json!({"state": "backoff"})));
        assert!(!settled(
            &json!({"state": "failed", "failure": {"step": "run"}})
        ));
        assert!(!settled(
            &json!({"state": "failed", "failure": {"step": "health"}})
        ));
        assert!(settled(
            &json!({"state": "failed", "failure": {"step": "bind"}})
        ));
    }

    #[test]
    fn timestamps_are_rfc3339() {
        let now = now();
        assert_eq!(now.len(), 20, "{now}");
        assert!(now.ends_with('Z') && now.as_bytes()[10] == b'T', "{now}");
        assert!(now.as_str() > "2026-01-01");
    }
}
