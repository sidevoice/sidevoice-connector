//! The model selection of one Cursor chat. Only chat-local records may identify its engine.

use super::{bridge_instances, safe_chat_id, BridgeInstance};
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const MAX_RESULT: usize = 32 * 1024;

fn text(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 160)
}

/// A Cursor model selector may include explicit parameter overrides in brackets.
fn model_selector(raw: &str) -> (&str, Option<&str>) {
    let Some((model, parameters)) = raw.split_once('[') else {
        return (raw, None);
    };
    let Some(parameters) = parameters.strip_suffix(']') else {
        return (raw, None);
    };
    if model.is_empty() {
        return (raw, None);
    }
    let effort = parameters.split(',').find_map(|parameter| {
        let (id, value) = parameter.trim().split_once('=')?;
        (id.trim() == "effort").then_some(value.trim())
    });
    (model, effort)
}

fn parameter<'a>(parameters: Option<&'a Value>, id: &str) -> Option<&'a str> {
    match parameters? {
        Value::Array(parameters) => parameters.iter().find_map(|parameter| {
            (parameter.get("id").and_then(Value::as_str) == Some(id))
                .then(|| text(parameter.get("value")))
                .flatten()
        }),
        Value::Object(parameters) => text(parameters.get(id)),
        _ => None,
    }
}

fn selected_engine(model: &str, configuration: Option<&Value>) -> Option<Value> {
    let model = model.trim();
    if model.is_empty() || model.len() > 160 || model == "default" {
        return None;
    }
    let (model, selector_effort) = model_selector(model);
    let explicit = configuration.and_then(|config| {
        text(config.get("effort"))
            .or_else(|| parameter(config.get("parameters"), "effort"))
            .or_else(|| parameter(config.get("modelParams"), "effort"))
    });
    // Only a default stored with this chat's model configuration can fill an absent selection.
    // Cursor's global picker can describe a different open chat and is never used here.
    let default = configuration.and_then(|config| {
        text(config.get("defaultEffort"))
            .or_else(|| parameter(config.get("defaultParameters"), "effort"))
    });
    let effort = explicit.or(selector_effort).or(default);
    Some(json!({"model":model,"effort":effort,"thinking":Value::Null}))
}

fn query(database: &Path, sql: &str) -> Option<Vec<Value>> {
    let output = Command::new("sqlite3")
        .args(["-readonly", "-json"])
        .arg(database)
        .arg(sql)
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.len() > MAX_RESULT {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

/// The Cursor CLI session's last model is in its own store.db metadata. It may itself contain
/// an `effort=` override. A global CLI model selection does not establish this chat's effort.
pub fn engine(chat_id: &str) -> Option<Value> {
    let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let mut roots = Vec::new();
    if let Some(path) = env::var_os("CURSOR_CONFIG_DIR") {
        roots.push(PathBuf::from(path));
    }
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        roots.push(PathBuf::from(path).join("cursor"));
    }
    roots.extend([home.join(".config/cursor"), home.join(".cursor")]);
    engine_in_roots(chat_id, &roots)
}

fn engine_in_roots(chat_id: &str, roots: &[PathBuf]) -> Option<Value> {
    if !safe_chat_id(chat_id) {
        return None;
    }
    for root in roots {
        let Ok(workspaces) = fs::read_dir(root.join("chats")) else {
            continue;
        };
        for workspace in workspaces.flatten() {
            let database = workspace.path().join(chat_id).join("store.db");
            if !database.is_file() {
                continue;
            }
            let Some(rows) = query(&database, "SELECT value FROM meta WHERE key = '0' LIMIT 1;")
            else {
                continue;
            };
            let Some(value) = rows
                .first()
                .and_then(|row| row.get("value"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            let Some(metadata) = serde_json::from_str::<Value>(value).ok().or_else(|| {
                let decoded = hex::decode(value).ok()?;
                serde_json::from_slice::<Value>(&decoded).ok()
            }) else {
                continue;
            };
            if let Some(model) = text(metadata.get("lastUsedModel")) {
                if let Some(engine) = selected_engine(model, None) {
                    return Some(engine);
                }
            }
        }
    }
    None
}

/// Cursor's editor keeps each composer's selection in its Desktop Bridge user-data database.
/// Query only the named composer's model configuration; message bodies never leave SQLite.
pub fn editor_engine(chat_id: &str) -> Option<Value> {
    editor_engine_in(chat_id, &bridge_instances())
}

fn editor_engine_in(chat_id: &str, instances: &[BridgeInstance]) -> Option<Value> {
    if !safe_chat_id(chat_id) {
        return None;
    }
    let sql = format!("SELECT substr(json_extract(CAST(value AS TEXT), '$.modelConfig'), 1, 16384) AS config FROM cursorDiskKV WHERE key = 'composerData:{chat_id}' AND json_valid(CAST(value AS TEXT)) LIMIT 1;");
    for instance in instances {
        let database = instance.user_data.join("User/globalStorage/state.vscdb");
        if !database.is_file() {
            continue;
        }
        let Some(rows) = query(&database, &sql) else {
            continue;
        };
        let Some(config) = rows
            .first()
            .and_then(|row| row.get("config"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let Some(config) = serde_json::from_str::<Value>(config).ok() else {
            continue;
        };
        let Some(model) = text(config.get("modelName")).or_else(|| text(config.get("modelId")))
        else {
            continue;
        };
        if let Some(engine) = selected_engine(model, Some(&config)) {
            return Some(engine);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure_fs::tests::Scratch;

    fn database(path: &Path, sql: &str) {
        assert!(Command::new("sqlite3")
            .arg(path)
            .arg(sql)
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn selector_effort_and_chat_local_default_have_ordered_precedence() {
        assert_eq!(
            selected_engine("claude-opus[effort=high]", None),
            Some(json!({"model":"claude-opus","effort":"high","thinking":null}))
        );
        let config = json!({"effort":"low","defaultParameters":[{"id":"effort","value":"medium"}]});
        assert_eq!(
            selected_engine("claude-opus[effort=high]", Some(&config)).unwrap()["effort"],
            "low"
        );
        assert_eq!(
            selected_engine(
                "claude-opus",
                Some(&json!({"defaultParameters":[{"id":"effort","value":"medium"}]}))
            )
            .unwrap()["effort"],
            "medium"
        );
        assert_eq!(
            selected_engine("claude-opus", None).unwrap()["effort"],
            Value::Null
        );
        assert_eq!(selected_engine("default", None), None);
    }

    #[test]
    fn cli_model_comes_from_its_own_store_and_never_another_chat() {
        let scratch = Scratch::new("cursor-cli-engine");
        let root = scratch.0.join("cursor");
        let chat = root.join("chats/workspace/chat-a");
        fs::create_dir_all(&chat).unwrap();
        let db = chat.join("store.db");
        database(&db, "CREATE TABLE meta(key TEXT, value TEXT); INSERT INTO meta VALUES ('0', '{\"lastUsedModel\":\"claude-opus[effort=high]\"}');");
        assert_eq!(
            engine_in_roots("chat-a", std::slice::from_ref(&root)).unwrap(),
            json!({"model":"claude-opus","effort":"high","thinking":null})
        );
        assert_eq!(engine_in_roots("chat-b", &[root.clone()]), None);
        assert_eq!(engine_in_roots("../chat-a", &[root]), None);
    }

    #[test]
    fn editor_model_and_effort_are_scoped_to_the_composer() {
        let scratch = Scratch::new("cursor-editor-engine");
        let user_data = scratch.0.join("Cursor");
        let storage = user_data.join("User/globalStorage");
        fs::create_dir_all(&storage).unwrap();
        let db = storage.join("state.vscdb");
        database(&db, "CREATE TABLE cursorDiskKV(key TEXT, value TEXT); INSERT INTO cursorDiskKV VALUES ('composerData:chat-a', '{\"modelConfig\":{\"modelName\":\"gpt-5\",\"parameters\":[{\"id\":\"effort\",\"value\":\"high\"}],\"privateMessage\":\"do-not-report\"}}'); INSERT INTO cursorDiskKV VALUES ('composerData:chat-b', '{\"modelConfig\":{\"modelName\":\"claude-opus\",\"defaultEffort\":\"medium\"}}');");
        let instance = BridgeInstance {
            socket: PathBuf::new(),
            token: String::new(),
            user_data,
            created_at: 0,
        };
        assert_eq!(
            editor_engine_in("chat-a", std::slice::from_ref(&instance)).unwrap(),
            json!({"model":"gpt-5","effort":"high","thinking":null})
        );
        assert_eq!(
            editor_engine_in("chat-b", std::slice::from_ref(&instance)).unwrap()["effort"],
            "medium"
        );
        assert_eq!(editor_engine_in("chat-c", &[instance]), None);
    }
}
