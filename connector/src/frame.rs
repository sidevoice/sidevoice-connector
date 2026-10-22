//! The connector's IPC frames: one JSON message per line, over `connector.sock` (an agent's `sidevoice mcp`, the
//! launcher, health probes). Bounded reading, and reading a refusal from any daemon version.

use anyhow::{bail, Result};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// A core frame is capped at 1 MiB. Rendering complete pull evidence can expand it before the
/// daemon returns it to MCP, so this IPC hop has a larger bound and paginates below that bound.
pub const IPC_FRAME_LIMIT: usize = 16 << 20;

pub async fn bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<Option<String>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            bail!("truncated IPC frame");
        }
        let length = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if line.len() + length > limit {
            bail!("IPC frame too large");
        }
        line.extend_from_slice(&available[..length]);
        reader.consume(length);
        if line.last() == Some(&b'\n') {
            line.pop();
            return Ok(Some(String::from_utf8(line)?));
        }
    }
}

/// What a refused answer over `connector.sock` (`{"ok":false,"error":…}`) says: the daemon's English text, which is
/// a string today; a daemon of another version may say it keyed (`{"key","params","message"}`), whose message (else
/// its key) is read. Anything else is `fallback`.
pub fn ipc_failure(answer: &Value, fallback: &str) -> String {
    let error = answer.get("error");
    error
        .and_then(Value::as_str)
        .or_else(|| {
            error
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            error
                .and_then(|error| error.get("key"))
                .and_then(Value::as_str)
        })
        .unwrap_or(fallback)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A refusal over `connector.sock` is read whether the daemon says it as text (today) or keyed (another version).
    #[test]
    fn an_ipc_refusal_is_read_as_text_or_keyed() {
        let fallback = "IPC error";
        assert_eq!(
            ipc_failure(
                &json!({"ok":false,"error":"CLOSED_BY_CLIENT:x:t"}),
                fallback
            ),
            "CLOSED_BY_CLIENT:x:t"
        );
        let keyed = json!({"ok":false,"error":{"key":"ipc.future","params":{},"message":"Refused.","future":1}});
        assert_eq!(ipc_failure(&keyed, fallback), "Refused.");
        assert_eq!(
            ipc_failure(&json!({"ok":false,"error":{"key":"ipc.future"}}), fallback),
            "ipc.future"
        );
        for shapeless in [json!({"ok":false}), json!({"ok":false,"error":{"code":3}})] {
            assert_eq!(ipc_failure(&shapeless, fallback), fallback);
        }
    }
}
