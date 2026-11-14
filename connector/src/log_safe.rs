//! What a log field may hold. The rules are in the shared logging guideline
//! (https://github.com/sidevoice/.github-private/blob/main/docs/logging.md, section 8): a value is logged only if its
//! kind is allowed, so the kind is a type here and the call site cannot get it wrong.

use sha2::{Digest, Sha256};
use std::fmt;

/// A harness conversation id (or any other high-entropy id this connector does not mint) as a log names it: `h:` and
/// the first 12 hex digits of SHA-256 over the exact UTF-8 id. The same function in every Sidevoice repository, so
/// lines correlate across processes.
pub struct IdHash<'a>(pub &'a str);

impl fmt::Display for IdHash<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            out,
            "h:{}",
            &hex::encode(Sha256::digest(self.0.as_bytes()))[..12]
        )
    }
}

/// An id a client, an agent or a page named (`message_id`, `session`, `utterance`): the id when it matches
/// `[A-Za-z0-9._:-]{1,64}`, else `invalid`. The sender chose the bytes, so nothing else is safe to print.
pub struct ClientId<'a>(pub &'a str);

impl fmt::Display for ClientId<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let id = self.0;
        let allowed = (1..=64).contains(&id.len())
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte));
        out.write_str(if allowed { id } else { "invalid" })
    }
}

/// A path of ours with the home this process resolved shown as `~`.
pub struct HomePath<'a> {
    pub path: &'a std::path::Path,
    pub home: &'a std::path::Path,
}

impl fmt::Display for HomePath<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.path.strip_prefix(self.home) {
            Ok(rest) if rest.as_os_str().is_empty() => out.write_str("~"),
            Ok(rest) => write!(out, "~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
            Err(_) => write!(out, "{}", self.path.display()),
        }
    }
}

/// How a task ended badly, as a fixed word: a `JoinError` holds the panic message, so it is never logged.
pub fn join_result(error: &tokio::task::JoinError) -> &'static str {
    if error.is_panic() {
        "panic"
    } else {
        "cancelled"
    }
}

/// A key the core named an error by, when it is one: `[a-z0-9_.-]{1,64}`. Anything else is not logged.
pub fn error_key(key: &str) -> Option<&str> {
    let allowed = (1..=64).contains(&key.len())
        && key.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_.-".contains(&byte)
        });
    allowed.then_some(key)
}

/// A fixed code for an error, from the types it is known to be, else `error`. Never the error's text: chains carry
/// paths, peer words and, through a core refusal, content.
pub fn error_code(error: &anyhow::Error) -> &'static str {
    error
        .chain()
        .find_map(|cause| {
            if cause.is::<tokio::time::error::Elapsed>() {
                Some("timeout")
            } else if cause.is::<std::io::Error>() {
                Some("io")
            } else if cause.is::<tokio_tungstenite::tungstenite::Error>() {
                Some("websocket")
            } else if cause.is::<serde_json::Error>() {
                Some("json")
            } else {
                None
            }
        })
        .unwrap_or("error")
}

#[cfg(test)]
mod tests;
