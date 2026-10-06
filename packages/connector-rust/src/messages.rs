//! What the connector says to people when something is refused or fails, by stable key. The command line is
//! English only and is not translated; what a program reads (`--json`, the core link, the desktop app) carries the
//! key, so that the client can say it in its person's language, plus the English text, never something to parse.
//!
//! The English texts are `messages/errors.json`, compiled in: the binary has nothing beside it to read.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::sync::OnceLock;

fn catalogue() -> &'static HashMap<String, String> {
    static MESSAGES: OnceLock<HashMap<String, String>> = OnceLock::new();
    MESSAGES.get_or_init(|| {
        serde_json::from_str(include_str!("../messages/errors.json"))
            .expect("the embedded message catalogue must be valid JSON")
    })
}

/// The English text for `key` with its `{name}` placeholders filled from `params`. A key the catalogue lacks is
/// said as itself, so a missing text shows up as a key rather than as silence.
pub fn message(key: &str, params: &Value) -> String {
    let Some(template) = catalogue().get(key) else {
        return key.to_owned();
    };
    let mut rendered = String::new();
    let mut remaining = template.as_str();
    loop {
        let Some(open) = remaining.find('{') else {
            rendered.push_str(remaining);
            break;
        };
        rendered.push_str(&remaining[..open]);
        let tail = &remaining[open + 1..];
        let Some(close) = tail.find('}') else {
            rendered.push_str(&remaining[open..]);
            break;
        };
        let name = &tail[..close];
        if let Some(value) = params.get(name) {
            rendered.push_str(
                value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string())
                    .as_str(),
            );
        } else {
            rendered.push_str(&remaining[open..open + close + 2]);
        }
        remaining = &tail[close + 1..];
    }
    rendered
}

/// A refusal or failure a client can act on: a stable key, what fills its text, and the English text.
#[derive(Debug, Clone)]
pub struct Keyed {
    pub key: &'static str,
    pub params: Value,
}

impl Keyed {
    pub fn new(key: &'static str, params: Value) -> Self {
        Self { key, params }
    }

    /// Anything else that went wrong, said with its detail under the generic key.
    pub fn failed(detail: impl Display) -> Self {
        let detail: String = detail
            .to_string()
            .chars()
            .filter(|ch| !ch.is_control())
            .take(240)
            .collect();
        Self::new("connector.failed", json!({"detail":detail}))
    }

    pub fn message(&self) -> String {
        message(self.key, &self.params)
    }

    /// `{"ok":false,"error":{"key","params","message"}}`: what `--json` prints on failure.
    pub fn value(&self) -> Value {
        json!({"ok":false,"error":{"key":self.key,"params":self.params,"message":self.message()}})
    }
}

impl Display for Keyed {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for Keyed {}

/// The keyed form of any error: itself when it already is one, else the generic failure with its text.
pub fn keyed(error: &anyhow::Error) -> Keyed {
    error
        .downcast_ref::<Keyed>()
        .cloned()
        .unwrap_or_else(|| Keyed::failed(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_are_filled_from_params_and_unknown_keys_say_themselves() {
        assert_eq!(
            message("agents.not-present", &json!({"agent":"Codex"})),
            "Codex is not detected on this computer."
        );
        assert_eq!(
            message("agents.not-present", &json!({})),
            "{agent} is not detected on this computer."
        );
        assert_eq!(message("no.such-key", &json!({})), "no.such-key");
    }

    #[test]
    fn json_failures_carry_a_stable_key_and_an_english_message() {
        let error = anyhow::Error::new(Keyed::new("pair.missing", json!({})));
        let value = keyed(&error).value();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["key"], "pair.missing");
        assert_eq!(
            value["error"]["message"],
            "Pairing needs the room's address and the one-time code."
        );
        let other = keyed(&anyhow::anyhow!("disk\nfull")).value();
        assert_eq!(other["error"]["key"], "connector.failed");
        assert_eq!(
            other["error"]["message"],
            "Sidevoice could not complete the command: diskfull"
        );
    }

    #[test]
    fn the_catalogue_is_english_and_names_no_proof_mode() {
        for (key, text) in catalogue() {
            assert!(
                text.is_ascii() || text.chars().all(|ch| ch.is_ascii() || ch == '…'),
                "{key}: {text}"
            );
            assert!(!text.to_lowercase().contains("proof"), "{key}: {text}");
        }
    }
}
