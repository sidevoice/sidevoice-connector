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
    pub fn code(self) -> &'static str {
        match self {
            Self::Unreachable => "unreachable",
            Self::AuthRefused => "authentication_refused",
            Self::UpgradeRequired => "upgrade_required",
            Self::HelloClosed => "hello_closed",
            Self::Closed => "closed",
            Self::Disconnected => "disconnected",
            Self::WriterEnded => "writer_ended",
            Self::PongTimeout => "pong_timeout",
            Self::FrameTooLarge => "frame_too_large",
            Self::InvalidJsonRpc => "invalid_json_rpc",
            Self::ResponseIdInvalid => "response_id_invalid",
            Self::HandlerLimit => "handler_limit",
            Self::QueueUnavailable => "queue_unavailable",
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
    fn display_is_never_the_code_but_the_code_is_a_fixed_word() {
        for error in [
            LinkError::AuthRefused,
            LinkError::PongTimeout,
            LinkError::Unreachable,
        ] {
            assert!(error
                .code()
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'_'));
        }
        assert!(LinkError::AuthRefused.is_final() && !LinkError::Closed.is_final());
    }
}
