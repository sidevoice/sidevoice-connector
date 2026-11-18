//! The connector's error codes, for logs. Each variant is one stable code, `connector.<kebab-name>`, that a log line
//! carries as `error.type` (the shared logging guideline, section 6). `Display` is for people and callers: it is never
//! logged, because an error's text can hold a path or a peer's words. [`crate::link_error::LinkError`] is the same for
//! the core link and keeps its own codes, in the same form.

use crate::link_error::LinkError;

/// What a log line can say went wrong in this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConnectorError {
    #[error("SIDEVOICE_LOG is not a valid filter")]
    InvalidLogFilter,
    #[error("another core is already running")]
    CoreAlreadyRunning,
    #[error("the core is not available")]
    CoreUnavailable,
    #[error("the core answered without an id")]
    CoreAnswerUnmatched,
    #[error("the core's notification was dropped")]
    HandlerCapacity,
    #[error("the conversation state is too large")]
    StateTooLarge,
    #[error("the conversation state could not be encoded")]
    StateEncode,
    #[error("the conversation state could not be written")]
    StateWrite,
    #[error("the speech outbox was retained")]
    SpeechOutboxRetained,
    #[error("the speech retry was retained")]
    SpeechRetryRetained,
    #[error("the speech recovery was retained")]
    SpeechRecoveryRetained,
    #[error("the replay of agent leaves failed")]
    AgentLeaveReplayFailed,
    #[error("the agent leave confirmation failed")]
    AgentLeaveConfirmFailed,
    #[error("a binding replay failed")]
    BindingReplayFailed,
    #[error("closing a binding failed")]
    BindingCloseFailed,
    #[error("the conversation guard failed")]
    ConversationGuardFailed,
    #[error("an operating system operation failed")]
    Io,
    #[error("an operation timed out")]
    Timeout,
    #[error("a WebSocket operation failed")]
    WebSocket,
    #[error("a JSON document was invalid")]
    Json,
    #[error("the crash report setting or records cannot be read or changed")]
    CrashReportsUnavailable,
    #[error("a crash report could not be sent")]
    CrashReportFailed,
}

impl ConnectorError {
    /// Every variant: the codes a log line may carry (`codes`), and what the tests hold unique and well formed.
    pub const ALL: &'static [ConnectorError] = &[
        Self::InvalidLogFilter,
        Self::CoreAlreadyRunning,
        Self::CoreUnavailable,
        Self::CoreAnswerUnmatched,
        Self::HandlerCapacity,
        Self::StateTooLarge,
        Self::StateEncode,
        Self::StateWrite,
        Self::SpeechOutboxRetained,
        Self::SpeechRetryRetained,
        Self::SpeechRecoveryRetained,
        Self::AgentLeaveReplayFailed,
        Self::AgentLeaveConfirmFailed,
        Self::BindingReplayFailed,
        Self::BindingCloseFailed,
        Self::ConversationGuardFailed,
        Self::Io,
        Self::Timeout,
        Self::WebSocket,
        Self::Json,
        Self::CrashReportsUnavailable,
        Self::CrashReportFailed,
    ];

    /// The stable code: the value of `error.type`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidLogFilter => "connector.invalid-log-filter",
            Self::CoreAlreadyRunning => "connector.core-already-running",
            Self::CoreUnavailable => "connector.core-unavailable",
            Self::CoreAnswerUnmatched => "connector.core-answer-unmatched",
            Self::HandlerCapacity => "connector.handler-capacity",
            Self::StateTooLarge => "connector.state-too-large",
            Self::StateEncode => "connector.state-encode",
            Self::StateWrite => "connector.state-write",
            Self::SpeechOutboxRetained => "connector.speech-outbox-retained",
            Self::SpeechRetryRetained => "connector.speech-retry-retained",
            Self::SpeechRecoveryRetained => "connector.speech-recovery-retained",
            Self::AgentLeaveReplayFailed => "connector.agent-leave-replay-failed",
            Self::AgentLeaveConfirmFailed => "connector.agent-leave-confirm-failed",
            Self::BindingReplayFailed => "connector.binding-replay-failed",
            Self::BindingCloseFailed => "connector.binding-close-failed",
            Self::ConversationGuardFailed => "connector.conversation-guard-failed",
            Self::Io => "connector.io",
            Self::Timeout => "connector.timeout",
            Self::WebSocket => "connector.websocket",
            Self::Json => "connector.json",
            Self::CrashReportsUnavailable => "connector.crash-reports-unavailable",
            Self::CrashReportFailed => "connector.crash-report-failed",
        }
    }

    /// The code of a failure that travelled as an `anyhow::Error`: from the types it is known to be, else the code of
    /// the site that logs it (`site`). Never the error's text: chains carry paths, peer words and, through a core
    /// refusal, content.
    pub fn of(error: &anyhow::Error, site: ConnectorError) -> &'static str {
        error
            .chain()
            .find_map(|cause| {
                if let Some(link) = cause.downcast_ref::<LinkError>() {
                    Some(link.code())
                } else if cause.is::<tokio::time::error::Elapsed>() {
                    Some(Self::Timeout.code())
                } else if cause.is::<std::io::Error>() {
                    Some(Self::Io.code())
                } else if cause.is::<tokio_tungstenite::tungstenite::Error>() {
                    Some(Self::WebSocket.code())
                } else if cause.is::<serde_json::Error>() {
                    Some(Self::Json.code())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| site.code())
    }
}

/// Every code the connector's own log names: the ones of [`ConnectorError`] and of [`LinkError`]. The connector has no other
/// source of an `error.type` (no refusal or fallback table, and `ConnectorError::of` only returns one of these). What leaves the
/// machine as a handled error's type is taken from this list and from nowhere else (`crash::handled`): a value that merely has
/// the shape of a code, a host name or an address a future call site logs by mistake among them, is never sent.
pub fn codes() -> impl Iterator<Item = &'static str> {
    ConnectorError::ALL
        .iter()
        .map(ConnectorError::code)
        .chain(LinkError::ALL.iter().map(|error| error.code()))
}

/// A code is `connector.` and lowercase words joined by `-`.
#[cfg(test)]
fn well_formed(code: &str) -> bool {
    code.strip_prefix("connector.").is_some_and(|name| {
        !name.is_empty()
            && name
                .split('-')
                .all(|word| !word.is_empty() && word.bytes().all(|byte| byte.is_ascii_lowercase()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique_stable_and_well_formed() {
        let codes: Vec<&str> = codes().collect();
        for code in &codes {
            assert!(well_formed(code), "{code}");
        }
        let mut unique = codes.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), codes.len(), "a code is used twice");
        assert_eq!(ConnectorError::Io.code(), "connector.io");
        assert_eq!(LinkError::AuthRefused.code(), "connector.link-auth-refused");
    }

    /// A variant left out of `ALL` would never be reported as a handled error: every arm of `code()` is in the list.
    #[test]
    fn every_variant_is_in_the_list_of_codes() {
        let arms = |source: &str| {
            source
                .split("\n#[cfg(test)]\nmod tests")
                .next()
                .unwrap()
                .lines()
                .filter(|line| line.contains("=> \"connector."))
                .count()
        };
        assert_eq!(
            arms(include_str!("error.rs")),
            ConnectorError::ALL.len(),
            "a variant of ConnectorError is not in ALL"
        );
        assert_eq!(
            arms(include_str!("link_error.rs")),
            LinkError::ALL.len(),
            "a variant of LinkError is not in ALL"
        );
    }

    #[test]
    fn display_is_never_a_code() {
        for error in ConnectorError::ALL {
            assert!(!error.to_string().starts_with("connector."));
        }
    }

    #[test]
    fn an_error_is_a_code_never_its_text() {
        let io =
            anyhow::Error::from(std::io::Error::other("/home/canary-user-7f3a")).context("wrapped");
        assert_eq!(
            ConnectorError::of(&io, ConnectorError::StateWrite),
            "connector.io"
        );
        let unknown = anyhow::anyhow!("/home/canary-user-7f3a");
        assert_eq!(
            ConnectorError::of(&unknown, ConnectorError::StateWrite),
            "connector.state-write",
            "an error of no known type takes the code of the site that logs it"
        );
        let link = anyhow::Error::new(LinkError::PongTimeout);
        assert_eq!(
            ConnectorError::of(&link, ConnectorError::StateWrite),
            "connector.link-pong-timeout"
        );
    }
}
