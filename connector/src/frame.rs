//! Messages over the connector's local channel (`platform::ipc`, the connector's endpoint: an agent's `sidevoice
//! mcp`, the launcher, health probes): one JSON value per line, at most [`MESSAGE_LIMIT`] bytes, newline included.
//! The lines are `tokio-util`'s `LinesCodec`; what is ours is one message as JSON ([`read`], [`write`]) and how an
//! answer's refusal is read, from any daemon version ([`outcome`]). Host-agnostic: the same on every OS.

use crate::platform::ipc::Stream;
use anyhow::{bail, Result};
use futures_util::{Sink, SinkExt, StreamExt};
use serde_json::Value;
use tokio_util::codec::{Framed, LinesCodec, LinesCodecError};

/// The largest message, its newline included. A core frame is capped at 1 MiB; rendering complete pull evidence can
/// expand it before the daemon returns it to MCP, so this hop has a larger bound, and pages are sized below it.
pub const MESSAGE_LIMIT: usize = 16 << 20;

/// A connection on the local channel, as lines. `StreamExt::split` gives its two directions for a reading task and a
/// writing one; [`read`] and [`write`] take either.
pub type Connection = Framed<Stream, LinesCodec>;

/// `stream` as lines of at most [`MESSAGE_LIMIT`] bytes.
pub fn connection(stream: Stream) -> Connection {
    Framed::new(stream, LinesCodec::new_with_max_length(MESSAGE_LIMIT - 1))
}

/// The next message; `None` when the peer closed. A line that is too long or not JSON is an error: the connection
/// is not to be read further.
pub async fn read<L>(lines: &mut L) -> Result<Option<Value>>
where
    L: futures_util::Stream<Item = Result<String, LinesCodecError>> + Unpin,
{
    match lines.next().await {
        Some(line) => Ok(Some(serde_json::from_str(&line?)?)),
        None => Ok(None),
    }
}

/// Sends one message. One larger than [`MESSAGE_LIMIT`] is refused, not sent: the peer would refuse it.
pub async fn write<L>(lines: &mut L, message: &Value) -> Result<()>
where
    L: Sink<String, Error = LinesCodecError> + Unpin,
{
    let line = serde_json::to_string(message)?;
    if line.len() >= MESSAGE_LIMIT {
        bail!("IPC frame too large");
    }
    lines.send(line).await?;
    Ok(())
}

/// An answer's `result`, or what its refusal says ([`failure`]); `refused` when it says nothing readable.
pub fn outcome(answer: &Value, refused: &str) -> std::result::Result<Value, String> {
    if answer.get("ok") == Some(&Value::Bool(true)) {
        Ok(answer.get("result").cloned().unwrap_or(Value::Null))
    } else {
        Err(failure(answer, refused))
    }
}

/// What a refused answer (`{"ok":false,"error":…}`) says: the daemon's English text, which is a string today; a
/// daemon of another version may say it keyed (`{"key","params","message"}`), whose message (else its key) is read.
/// Anything else is `fallback`.
fn failure(answer: &Value, fallback: &str) -> String {
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
            failure(
                &json!({"ok":false,"error":"CLOSED_BY_CLIENT:x:t"}),
                fallback
            ),
            "CLOSED_BY_CLIENT:x:t"
        );
        let keyed = json!({"ok":false,"error":{"key":"ipc.future","params":{},"message":"Refused.","future":1}});
        assert_eq!(failure(&keyed, fallback), "Refused.");
        assert_eq!(
            failure(&json!({"ok":false,"error":{"key":"ipc.future"}}), fallback),
            "ipc.future"
        );
        for shapeless in [json!({"ok":false}), json!({"ok":false,"error":{"code":3}})] {
            assert_eq!(failure(&shapeless, fallback), fallback);
        }
    }

    #[test]
    fn an_answer_is_its_result_or_its_refusal() {
        assert_eq!(
            outcome(&json!({"id":1,"ok":true,"result":{"a":1}}), "refused"),
            Ok(json!({"a":1}))
        );
        assert_eq!(
            outcome(&json!({"id":1,"ok":true}), "refused"),
            Ok(Value::Null)
        );
        assert_eq!(
            outcome(&json!({"id":1,"ok":false,"error":"no"}), "refused"),
            Err("no".to_owned())
        );
        assert_eq!(
            outcome(&json!({"id":1}), "refused"),
            Err("refused".to_owned())
        );
    }

    #[tokio::test]
    async fn whole_messages_cross_the_channel_and_oversized_ones_do_not() {
        use crate::platform::fs::{Files, Fs};
        use crate::platform::ipc::{Address, Channel, Channels, Endpoint};
        let scratch = crate::test_support::temp_dir("frame");
        let dir = scratch.path().join("private");
        Files::create_private_dir(&dir).unwrap();
        let endpoint = Endpoint::new(&dir, "frame");
        let listener = Channels::listen(&endpoint).unwrap();
        let server = tokio::spawn(async move {
            let mut lines = connection(listener.accept().await.unwrap());
            while let Some(message) = read(&mut lines).await.unwrap() {
                write(&mut lines, &message).await.unwrap();
            }
        });
        let (mut sink, mut stream) =
            connection(Channels::connect(&endpoint).await.unwrap()).split();
        for message in [
            json!({"id":1,"method":"status"}),
            json!("line\nbreak"),
            json!([]),
        ] {
            write(&mut sink, &message).await.unwrap();
            assert_eq!(read(&mut stream).await.unwrap(), Some(message));
        }
        let oversized = json!("x".repeat(MESSAGE_LIMIT));
        assert!(write(&mut sink, &oversized).await.is_err());
        // The peer sees the end once the connection is gone (both halves).
        drop((sink, stream));
        server.await.unwrap();
    }

    /// The wire as before the codec: one compact JSON per line, `\n`-terminated, at most `MESSAGE_LIMIT` bytes with
    /// the newline; a longer line, or one that is not JSON, is an error.
    #[tokio::test]
    async fn the_wire_is_one_json_per_line_within_the_limit() {
        use tokio_util::codec::{FramedRead, FramedWrite};
        let codec = || LinesCodec::new_with_max_length(MESSAGE_LIMIT - 1);
        let mut sent = FramedWrite::new(Vec::new(), codec());
        write(&mut sent, &json!({"id":1,"ok":true,"result":{"a":[1,2]}}))
            .await
            .unwrap();
        assert_eq!(
            sent.get_ref(),
            b"{\"id\":1,\"ok\":true,\"result\":{\"a\":[1,2]}}\n"
        );

        let longest = format!("\"{}\"\n", "x".repeat(MESSAGE_LIMIT - 3));
        assert_eq!(longest.len(), MESSAGE_LIMIT);
        let mut lines = FramedRead::new(longest.as_bytes(), codec());
        assert!(read(&mut lines).await.unwrap().is_some());
        let over = format!("\"{}\"\n", "x".repeat(MESSAGE_LIMIT - 2));
        assert!(read(&mut FramedRead::new(over.as_bytes(), codec()))
            .await
            .is_err());
        assert!(read(&mut FramedRead::new(&b"{\"id\":1\n"[..], codec()))
            .await
            .is_err());
        assert!(read(&mut FramedRead::new(&b""[..], codec()))
            .await
            .unwrap()
            .is_none());
    }
}
