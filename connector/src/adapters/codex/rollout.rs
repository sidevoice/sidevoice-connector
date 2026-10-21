//! Following a Codex thread's rollout: its engine, its turns (working) and the messages it read.

use super::{default_effort, rollout_engine};
use crate::adapters::conversation::{Conversation, Reporter};
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::time::Duration;

/// Follows the rollout until the conversation stops, finding it again when it moves.
pub fn follow(conversation: Arc<Conversation>, reporter: Arc<dyn Reporter>) {
    tokio::spawn(async move {
        let binding = &*conversation;
        let mut path: Option<PathBuf> = None;
        let mut offset = 0u64;
        let mut file_id: Option<(u64, u64)> = None;
        let mut partial = Vec::new();
        let mut oversized = false;
        let mut turn: Option<String> = None;
        let mut working = None;
        let mut catching_up = true;
        let mut defaults = DefaultEffort::new(reporter.profile().agent_home(super::HOME.var));
        loop {
            if binding.stopped.load(Ordering::Relaxed) {
                break;
            }
            if path.is_none() {
                path = rollout_path(
                    reporter.profile().agent_home(super::HOME.var),
                    &binding.thread,
                );
                if let Some(file) = &path {
                    if let Ok(meta) = fs::metadata(file) {
                        offset = 0;
                        file_id = Some((meta.dev(), meta.ino()));
                    }
                }
            }
            if let Some(file) = &path {
                if let Ok(mut input) = fs::File::open(file) {
                    let Ok(meta) = input.metadata() else {
                        continue;
                    };
                    let current = (meta.dev(), meta.ino());
                    if file_id != Some(current) || meta.len() < offset {
                        offset = 0;
                        partial.clear();
                        oversized = false;
                        turn = None;
                        working = None;
                        catching_up = true;
                    }
                    file_id = Some(current);
                    if input.seek(SeekFrom::Start(offset)).is_ok() {
                        let mut bytes = [0u8; 65536];
                        let limit = bytes.len().min(meta.len().saturating_sub(offset) as usize);
                        if let Ok(n) = input.read(&mut bytes[..limit]) {
                            offset += n as u64;
                            for segment in bytes[..n].split_inclusive(|byte| *byte == b'\n') {
                                if !oversized {
                                    if partial.len() + segment.len() > 1 << 20 {
                                        partial.clear();
                                        oversized = true;
                                    } else {
                                        partial.extend_from_slice(segment);
                                    }
                                }
                                if segment.last() == Some(&b'\n') {
                                    if !oversized {
                                        if let Ok(item) = serde_json::from_slice::<Value>(&partial)
                                        {
                                            observe(
                                                &*reporter,
                                                binding,
                                                &item,
                                                &mut turn,
                                                &mut working,
                                                &mut defaults,
                                                catching_up,
                                            )
                                            .await;
                                        }
                                    }
                                    partial.clear();
                                    oversized = false;
                                }
                            }
                        }
                        if catching_up && offset >= meta.len() {
                            catching_up = false;
                            if let Some(state) = working {
                                reporter.working(
                                    binding,
                                    json!({
                                        "working":state,"turn_id":if state { turn.clone() } else { None }
                                    }),
                                )
                                .await;
                            }
                        }
                    }
                } else {
                    path = None;
                    file_id = None;
                    partial.clear();
                    oversized = false;
                }
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    });
}

/// The default reasoning level of the model a rollout last named, read from Codex's catalogue once per model: a
/// rollout being caught up names the same model in every turn.
struct DefaultEffort {
    home: PathBuf,
    last: Option<(String, Option<String>)>,
}

impl DefaultEffort {
    fn new(home: &Path) -> Self {
        Self {
            home: home.to_owned(),
            last: None,
        }
    }

    /// `engine` as Codex runs it: the effort it records, or else the model's default level.
    fn complete(&mut self, mut engine: Value) -> Value {
        if !engine["effort"].is_null() {
            return engine;
        }
        let model = engine["model"].as_str().unwrap_or_default();
        if self.last.as_ref().is_none_or(|(last, _)| last != model) {
            self.last = Some((model.to_owned(), default_effort(&self.home, model)));
        }
        engine["effort"] = json!(self.last.as_ref().and_then(|(_, effort)| effort.clone()));
        engine
    }
}

async fn observe(
    reporter: &dyn Reporter,
    binding: &Conversation,
    item: &Value,
    turn: &mut Option<String>,
    working: &mut Option<bool>,
    defaults: &mut DefaultEffort,
    catching_up: bool,
) {
    if let Some(engine) = rollout_engine(item) {
        reporter.engine(binding, defaults.complete(engine)).await;
    }
    if item.get("type") == Some(&json!("event_msg")) {
        let phase = item
            .pointer("/payload/type")
            .and_then(Value::as_str)
            .unwrap_or("");
        let turn_id = item.pointer("/payload/turn_id").and_then(Value::as_str);
        if phase == "task_started" {
            if let Some(turn_id) = turn_id {
                *turn = Some(turn_id.to_owned());
                *working = Some(true);
                if !catching_up {
                    reporter
                        .working(binding, json!({"working":true,"turn_id":turn_id}))
                        .await;
                }
            }
        }
        if phase == "task_complete" || phase == "turn_aborted" {
            if let Some(turn_id) = turn_id {
                if turn.as_deref() == Some(turn_id) {
                    *turn = None;
                }
                *working = Some(false);
                if !catching_up {
                    reporter
                        .working(binding, json!({"working":false,"turn_id":turn_id}))
                        .await;
                }
            }
        }
    }
    if item.get("type") == Some(&json!("response_item"))
        && item.pointer("/payload/type") == Some(&json!("message"))
        && item.pointer("/payload/role") == Some(&json!("user"))
    {
        let text = item
            .pointer("/payload/content")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter(|part| part.get("type").and_then(Value::as_str) == Some("input_text"))
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let Some(start) = text.find("{\"channel\":") else {
            return;
        };
        let Some(end) = text[start..].find('}') else {
            return;
        };
        let Ok(header) = serde_json::from_str::<Value>(&text[start..=start + end]) else {
            return;
        };
        let Some(message_id) = header.get("message_id").and_then(Value::as_str) else {
            return;
        };
        reporter
            .read(
                binding,
                message_id,
                header["session_id"].clone(),
                header["revision"].clone(),
                json!(turn),
            )
            .await;
    }
}

/// The rollout of `thread` below `home/sessions/<year>/<month>/<day>/`, newest first.
fn rollout_path(home: &Path, thread: &str) -> Option<PathBuf> {
    let root = home.join("sessions");
    let mut years: Vec<_> = fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    years.sort();
    years.reverse();
    for year in years {
        let mut months: Vec<_> = fs::read_dir(year)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .collect();
        months.sort();
        months.reverse();
        for month in months {
            let mut days: Vec<_> = fs::read_dir(month)
                .ok()?
                .flatten()
                .map(|e| e.path())
                .collect();
            days.sort();
            days.reverse();
            for day in days {
                if let Ok(files) = fs::read_dir(day) {
                    for entry in files.flatten() {
                        let path = entry.path();
                        if path.file_name().is_some_and(|n| {
                            n.to_string_lossy().ends_with(&format!("-{thread}.jsonl"))
                        }) {
                            return Some(path);
                        }
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with(catalogue: &Value) -> PathBuf {
        let home = std::env::temp_dir().join(format!("sv-codex-rollout-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("models_cache.json"), catalogue.to_string()).unwrap();
        home
    }

    fn levels(luna: &str) -> Value {
        json!({"models":[{"slug":"gpt-6-luna","default_reasoning_level":luna},
            {"slug":"gpt-6-astra","default_reasoning_level":"medium"}]})
    }

    #[test]
    fn a_turn_that_names_no_effort_runs_at_the_models_default() {
        let home = home_with(&levels("medium"));
        let mut defaults = DefaultEffort::new(&home);
        // A turn context as Codex 0.157 writes it when nothing chose an effort: no `effort`, none in the mode.
        let item = json!({"type":"turn_context","payload":{"model":"gpt-6-luna","summary":"none",
            "collaboration_mode":{"mode":"default","settings":{"model":"gpt-6-luna","reasoning_effort":null}}}});
        assert_eq!(
            defaults.complete(rollout_engine(&item).unwrap()),
            json!({"model":"gpt-6-luna","effort":"medium","thinking":null})
        );
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn the_effort_a_turn_records_is_kept() {
        let home = home_with(&levels("medium"));
        let mut defaults = DefaultEffort::new(&home);
        let item = json!({"type":"turn_context","payload":{"model":"gpt-6-luna","effort":"xhigh"}});
        assert_eq!(
            defaults.complete(rollout_engine(&item).unwrap()),
            json!({"model":"gpt-6-luna","effort":"xhigh","thinking":null})
        );
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_model_without_a_default_keeps_no_effort() {
        let home = std::env::temp_dir().join(format!("sv-codex-rollout-{}", uuid::Uuid::new_v4()));
        let mut defaults = DefaultEffort::new(&home);
        let engine = json!({"model":"gpt-6-luna","effort":null,"thinking":null});
        assert_eq!(defaults.complete(engine.clone()), engine);
    }

    #[test]
    fn the_catalogue_is_read_once_per_model() {
        let home = home_with(&levels("medium"));
        let mut defaults = DefaultEffort::new(&home);
        let luna = json!({"model":"gpt-6-luna","effort":null,"thinking":null});
        assert_eq!(defaults.complete(luna.clone())["effort"], "medium");
        fs::write(home.join("models_cache.json"), levels("low").to_string()).unwrap();
        assert_eq!(
            defaults.complete(luna.clone())["effort"],
            "medium",
            "same model, not read again"
        );
        let astra = json!({"model":"gpt-6-astra","effort":null,"thinking":null});
        assert_eq!(defaults.complete(astra)["effort"], "medium");
        assert_eq!(
            defaults.complete(luna)["effort"],
            "low",
            "another model in between reads it again"
        );
        fs::remove_dir_all(&home).unwrap();
    }
}
