//! Why the link to the core ended, as a type: each variant is a fixed code the log can name, so no error text (a ready
//! file path, a peer's words) has to be logged to say what happened.

/// A failure of the core link. `Display` is the sentence the link has always reported; [`LinkError::code`] is what a
/// log line carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError {
    /// No core answered on its endpoint in time.
    Unreachable,
    /// The core refused this connector's hello.
    AuthRefused,
    /// The core does not speak link protocol 3.
    UpgradeRequired,
    /// The core closed the socket before answering the hello.
    HelloClosed,
    /// The core closed the socket.
    Closed,
    Disconnected,
    WriterEnded,
    PongTimeout,
    FrameTooLarge,
    InvalidJsonRpc,
    ResponseIdInvalid,
    HandlerLimit,
    QueueUnavailable,
}

impl LinkError {
    /// Every variant: the codes a log line may carry (`error::codes`), and what the tests hold unique.
    pub const ALL: &'static [LinkError] = &[
        Self::Unreachable,
        Self::AuthRefused,
        Self::UpgradeRequired,
        Self::HelloClosed,
        Self::Closed,
        Self::Disconnected,
        Self::WriterEnded,
        Self::PongTimeout,
        Self::FrameTooLarge,
        Self::InvalidJsonRpc,
        Self::ResponseIdInvalid,
        Self::HandlerLimit,
        Self::QueueUnavailable,
    ];

    pub fn code(self) -> &'static str {
        match self {
            Self::Unreachable => "connector.link-unreachable",
            Self::AuthRefused => "connector.link-auth-refused",
            Self::UpgradeRequired => "connector.link-upgrade-required",
            Self::HelloClosed => "connector.link-hello-closed",
            Self::Closed => "connector.link-closed",
            Self::Disconnected => "connector.link-disconnected",
            Self::WriterEnded => "connector.link-writer-ended",
            Self::PongTimeout => "connector.link-pong-timeout",
            Self::FrameTooLarge => "connector.link-frame-too-large",
            Self::InvalidJsonRpc => "connector.link-invalid-json-rpc",
            Self::ResponseIdInvalid => "connector.link-response-id-invalid",
            Self::HandlerLimit => "connector.link-handler-limit",
            Self::QueueUnavailable => "connector.link-queue-unavailable",
        }
    }

    /// The core will not take this connector until something changes: retrying is pointless.
    pub fn is_final(self) -> bool {
        matches!(self, Self::AuthRefused | Self::UpgradeRequired)
    }
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::Unreachable => "Core link unavailable",
            Self::AuthRefused => "Core v3 authentication refused",
            Self::UpgradeRequired => "Core v3 upgrade required",
            Self::HelloClosed => "hello closed",
            Self::Closed => "Core WebSocket closed",
            Self::Disconnected => "Core disconnected",
            Self::WriterEnded => "Core writer ended",
            Self::PongTimeout => "Core Pong timeout",
            Self::FrameTooLarge => "Core frame too large",
            Self::InvalidJsonRpc => "invalid Core JSON-RPC",
            Self::ResponseIdInvalid => "Core response ID invalid",
            Self::HandlerLimit => "Core handler limit",
            Self::QueueUnavailable => "Core inbound queue unavailable",
        })
    }
}

impl std::error::Error for LinkError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_is_never_the_code_and_a_final_error_is_known() {
        for error in LinkError::ALL {
            assert!(!error.to_string().starts_with("connector."));
        }
        assert!(LinkError::AuthRefused.is_final() && !LinkError::Closed.is_final());
    }
}
