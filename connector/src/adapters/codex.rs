mod binary;
mod harness;
mod registration;
mod rollout;
pub(crate) mod steer;

pub use harness::Codex;

/// Codex's home (`config.toml`, sessions).
pub const HOME: crate::profile::AgentHome = crate::profile::AgentHome {
    var: "CODEX_HOME",
    default: crate::profile::HomeDefault::Home(".codex"),
    always_passed: true,
    config: true,
    check_parent: false,
};

use super::{envelope, Identity};
use crate::platform::process::{Process, Processes};
use crate::profile::Profile;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// Codex's model catalogue in its home: the models the account can use, each with its default reasoning level.
const MODEL_CATALOGUE: &str = "models_cache.json";

pub fn capabilities() -> Value {
    json!({"deliver":"supported","inspectInbound":"unsupported","working":"supported","endOfTurn":"supported","sessionIdentity":"supported"})
}

/// The engine a rollout's turn context or session header records: its model and reasoning effort. A turn context
/// records the effort chosen for the session (`model_reasoning_effort`, a profile, `-c`, the app's picker), and none
/// when nothing chose one: Codex then runs the model at its default level, [`default_effort`].
pub fn rollout_engine(item: &Value) -> Option<Value> {
    if !matches!(
        item.get("type").and_then(Value::as_str),
        Some("turn_context" | "session_meta")
    ) {
        return None;
    }
    let text = |pointer| {
        item.pointer(pointer)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    };
    let model = text("/payload/model")?;
    Some(json!({"model":model,"effort":text("/payload/effort"),"thinking":Value::Null}))
}

/// The reasoning effort Codex runs `model` at when nothing chose one: the model's `default_reasoning_level` in
/// Codex's own model catalogue, in its `home`. Codex sends that level with every request whose turn names no effort.
/// None when the catalogue is missing or unreadable, or does not give the model a level.
pub fn default_effort(home: &Path, model: &str) -> Option<String> {
    let catalogue = fs::read(home.join(MODEL_CATALOGUE)).ok()?;
    catalogue_default_effort(&serde_json::from_slice(&catalogue).ok()?, model)
}

fn catalogue_default_effort(catalogue: &Value, model: &str) -> Option<String> {
    catalogue
        .get("models")?
        .as_array()?
        .iter()
        .find(|entry| entry.get("slug").and_then(Value::as_str) == Some(model))?
        .get("default_reasoning_level")?
        .as_str()
        .filter(|level| !level.is_empty())
        .map(str::to_owned)
}

pub fn identity(meta: &Value) -> Result<Option<Identity>> {
    let turn = meta.get("x-codex-turn-metadata").and_then(|value| {
        if let Some(text) = value.as_str() {
            serde_json::from_str::<Value>(text).ok()
        } else {
            Some(value.clone())
        }
    });
    let thread = [
        meta.get("openai/threadId"),
        meta.get("openai/thread_id"),
        meta.get("codexThreadId"),
        meta.get("codex_thread_id"),
        turn.as_ref().and_then(|value| value.get("thread_id")),
        meta.get("session_id"),
        meta.get("thread_id"),
    ]
    .into_iter()
    .flatten()
    .find_map(|value| {
        value
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    })
    .or_else(|| {
        env::var("CODEX_THREAD_ID")
            .ok()
            .filter(|text| !text.is_empty())
    });
    let Some(thread) = thread else {
        return Ok(None);
    };
    let delivery = if let Some(url) = env::var("SIDEVOICE_DELIVERY_URL")
        .ok()
        .filter(|url| !url.is_empty())
    {
        json!({"kind":"http","url":url,"thread":thread})
    } else {
        json!({"kind":"codex-queue","thread":thread})
    };
    Ok(Some(Identity::new("codex", thread, delivery)))
}

/// Delivers `event` to the thread: into the running turn when the installed Codex can be steered from outside
/// (`steer`), else — and on any steer error — queued behind it with `codex queue`. Both carry the same envelope, so a
/// message that arrives twice is the same message (same `message_id`).
pub async fn deliver(delivery: &Value, event: &Value, profile: &Profile) -> Result<Value> {
    if delivery.get("kind").and_then(Value::as_str) != Some("codex-queue") {
        bail!("Unsupported Codex delivery route");
    }
    let thread = delivery
        .get("thread")
        .and_then(Value::as_str)
        .context("Codex thread missing")?;
    let envelope = envelope(event)?;
    // The Codex CLI the agent scan found, not whatever `codex` a service manager's PATH reaches
    // (`agents::agent_command`).
    let codex = crate::agents::agent_command(profile, "codex");
    let program = codex
        .program
        .context("Codex not on the captured login PATH")?;
    let command = || {
        let mut command = tokio::process::Command::new(&program);
        if let Some(path) = &codex.path {
            command.env("PATH", path);
        }
        command.env(HOME.var, profile.agent_home(HOME.var));
        command
    };
    let message_id = event
        .get("message_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if let Ok(turn) = try_steer(command(), profile, thread, message_id, &envelope).await {
        return Ok(json!({"status":"accepted","detail":format!("codex steered turn {turn}")}));
    }
    let mut queue = command();
    queue
        .args(["queue", "--thread", thread, "--message"])
        .arg(envelope);
    match run(queue, false, Duration::from_secs(30)).await {
        Ok((status, _)) if status.success() => {
            Ok(json!({"status":"accepted","detail":"codex queue confirmed the thread"}))
        }
        _ => bail!("Codex queue failed"),
    }
}

/// Runs `command` (a Codex CLI call) to its end within `limit`, giving its exit status and, when `capture`, its
/// output. Started through [`Processes::spawn_detached`], so that on Windows it has no console window of its own (the
/// daemon has none, and a console program started without one opens a visible window); the program is stopped when
/// this is dropped or `limit` passes.
async fn run(
    mut command: tokio::process::Command,
    capture: bool,
    limit: Duration,
) -> Result<(ExitStatus, Vec<u8>)> {
    command
        .stdin(Stdio::null())
        .stdout(if capture {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = Processes::spawn_detached(command).await?;
    let mut stdout = child.stdout();
    let finished = tokio::time::timeout(limit, async {
        let read = async {
            let mut output = Vec::new();
            if let Some(stdout) = stdout.as_mut() {
                stdout.read_to_end(&mut output).await?;
            }
            std::io::Result::Ok(output)
        };
        let (status, output) = tokio::join!(child.wait(), read);
        Ok::<_, std::io::Error>((status?, output?))
    })
    .await;
    match finished {
        Ok(done) => Ok(done?),
        Err(_) => {
            child.kill_tree().await;
            bail!("Codex did not answer in time")
        }
    }
}

/// Steers the thread's running turn: only when the Codex daemon's socket is there and the installed Codex is at
/// least [`steer::MIN_VERSION`]. Any error means: queue instead.
async fn try_steer(
    mut version: tokio::process::Command,
    profile: &Profile,
    thread: &str,
    message_id: &str,
    text: &str,
) -> Result<String> {
    let socket = steer::socket_path(profile.agent_home(HOME.var));
    if !socket.exists() {
        bail!("no Codex daemon socket");
    }
    version.arg("--version");
    let (_, stdout) = run(version, true, Duration::from_secs(10)).await?;
    if !steer::supports_steer(steer::parse_version(&String::from_utf8_lossy(&stdout))) {
        bail!("installed Codex cannot be steered from outside");
    }
    steer::steer(&socket, thread, message_id, text).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollout_engine_reads_model_and_effort() {
        let item = json!({"type":"turn_context","payload":{"model":"gpt-6-luna","effort":"xhigh"}});
        assert_eq!(
            rollout_engine(&item),
            Some(json!({"model":"gpt-6-luna","effort":"xhigh","thinking":null}))
        );
    }

    #[test]
    fn rollout_engine_leaves_an_unknown_effort_null() {
        let item = json!({"type":"session_meta","payload":{"model":"gpt-6-luna"}});
        assert_eq!(
            rollout_engine(&item),
            Some(json!({"model":"gpt-6-luna","effort":null,"thinking":null}))
        );
    }

    #[test]
    fn rollout_engine_needs_a_model_and_a_context_item() {
        let effort_only = json!({"type":"turn_context","payload":{"effort":"high"}});
        assert_eq!(rollout_engine(&effort_only), None);
        let event = json!({"type":"event_msg","payload":{"model":"gpt-6-luna"}});
        assert_eq!(rollout_engine(&event), None);
    }

    /// A catalogue as Codex 0.157 caches it, trimmed to what is read, plus fields it does not know.
    fn catalogue() -> Value {
        json!({"client_version":"0.157.0","etag":"e","future":{"nested":1},"models":[
            {"slug":"gpt-6-astra","default_reasoning_level":"medium","supported_reasoning_levels":[{"effort":"low"}]},
            {"slug":"gpt-6-luna","default_reasoning_level":"high","priority":3,"future":true},
            {"slug":"gpt-plain","default_reasoning_level":null},
            {"slug":"gpt-blank","default_reasoning_level":""}]})
    }

    #[test]
    fn the_default_effort_is_the_models_level_in_the_catalogue() {
        assert_eq!(
            catalogue_default_effort(&catalogue(), "gpt-6-luna"),
            Some("high".to_owned())
        );
        assert_eq!(
            catalogue_default_effort(&catalogue(), "gpt-6-astra"),
            Some("medium".to_owned())
        );
    }

    #[test]
    fn a_model_the_catalogue_gives_no_level_has_no_default_effort() {
        for model in ["gpt-unknown", "gpt-plain", "gpt-blank"] {
            assert_eq!(
                catalogue_default_effort(&catalogue(), model),
                None,
                "{model}"
            );
        }
        assert_eq!(
            catalogue_default_effort(&json!({"models":{}}), "gpt-6-luna"),
            None
        );
        assert_eq!(catalogue_default_effort(&json!([]), "gpt-6-luna"), None);
    }

    #[test]
    fn the_default_effort_is_read_from_the_catalogue_in_codex_home() {
        let home =
            std::env::temp_dir().join(format!("sv-codex-catalogue-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&home).unwrap();
        assert_eq!(default_effort(&home, "gpt-6-luna"), None, "no catalogue");
        fs::write(home.join(MODEL_CATALOGUE), "{not json").unwrap();
        assert_eq!(
            default_effort(&home, "gpt-6-luna"),
            None,
            "unreadable catalogue"
        );
        fs::write(home.join(MODEL_CATALOGUE), catalogue().to_string()).unwrap();
        assert_eq!(default_effort(&home, "gpt-6-luna"), Some("high".to_owned()));
        fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn a_codex_call_gives_its_status_and_output_and_is_stopped_at_its_limit() {
        use crate::platform::stand_in::{StandIn, StandIns as _};
        use crate::platform::target::{Host, Target as _};
        let dir = crate::test_support::temp_dir("codex-run");
        let program = dir.path().join(Host::exe_name("codex"));
        StandIn::make(
            &program,
            r#"{"play":"script","print":"codex-cli 0.157.0\n","exit":3}"#,
        );
        let (status, stdout) = run(
            tokio::process::Command::new(&program),
            true,
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        assert_eq!(status.code(), Some(3));
        assert_eq!(String::from_utf8_lossy(&stdout), "codex-cli 0.157.0\n");
        let (_, nothing) = run(
            tokio::process::Command::new(&program),
            false,
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        assert!(nothing.is_empty());
        let late = run(tokio::process::Command::new(&program), true, Duration::ZERO).await;
        assert!(late.is_err(), "no time to answer");
    }
}
