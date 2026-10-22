//! Messages over the connector's local channel (`platform::ipc`, the connector's endpoint: an agent's `sidevoice
//! mcp`, the launcher, health probes): one JSON value per line, at most [`MESSAGE_LIMIT`] bytes. A [`Connection`]
//! owns that framing, so callers read and write whole messages and never see a line or a limit. Host-agnostic: the
//! same on every OS, over whatever stream the channel gives. Also how an answer's refusal is read, from any daemon
//! version.

use crate::platform::ipc::Stream;
use anyhow::{bail, Result};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};

/// The largest message, its newline included. A core frame is capped at 1 MiB; rendering complete pull evidence can
/// expand it before the daemon returns it to MCP, so this hop has a larger bound, and pages are sized below it.
pub const MESSAGE_LIMIT: usize = 16 << 20;

/// A connection on the local channel, as whole messages.
pub struct Connection {
    reader: Reader,
    writer: Writer,
}

impl Connection {
    pub fn new(stream: Stream) -> Connection {
        let (read, write) = tokio::io::split(stream);
        Connection {
            reader: Reader(BufReader::new(read)),
            writer: Writer(write),
        }
    }

    /// Its two directions, to be used apart (a reading task and a writing one).
    pub fn split(self) -> (Reader, Writer) {
        (self.reader, self.writer)
    }

    /// See [`Reader::read`].
    pub async fn read(&mut self) -> Result<Option<Value>> {
        self.reader.read().await
    }

    /// See [`Writer::write`].
    pub async fn write(&mut self, message: &Value) -> Result<()> {
        self.writer.write(message).await
    }
}

/// The receiving direction of a [`Connection`].
pub struct Reader(BufReader<ReadHalf<Stream>>);

impl Reader {
    /// The next message; `None` when the peer closed between messages. A message that is too large, cut short or
    /// not JSON is an error: the connection is not to be read further.
    pub async fn read(&mut self) -> Result<Option<Value>> {
        match bounded_line(&mut self.0, MESSAGE_LIMIT).await? {
            Some(line) => Ok(Some(serde_json::from_str(&line)?)),
            None => Ok(None),
        }
    }
}

/// The sending direction of a [`Connection`].
pub struct Writer(WriteHalf<Stream>);

impl Writer {
    /// Sends one message. One larger than [`MESSAGE_LIMIT`] is refused, not sent: the peer would refuse it.
    pub async fn write(&mut self, message: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(message)?;
        line.push(b'\n');
        if line.len() > MESSAGE_LIMIT {
            bail!("IPC frame too large");
        }
        self.0.write_all(&line).await?;
        Ok(())
    }
}

/// An answer's `result`, or what its refusal says ([`failure`]); `refused` when it says nothing readable.
pub fn outcome(answer: &Value, refused: &str) -> std::result::Result<Value, String> {
    if answer.get("ok") == Some(&Value::Bool(true)) {
        Ok(answer.get("result").cloned().unwrap_or(Value::Null))
    } else {
        Err(failure(answer, refused))
    }
}

/// One line of at most `limit` bytes (its newline included), without the newline; `None` at a clean end. The one
/// bounded line reader: [`Reader`] uses it, and so does what reads another program's line protocol (an MCP
/// server's stdout).
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
            let mut connection = Connection::new(listener.accept().await.unwrap());
            while let Some(message) = connection.read().await.unwrap() {
                connection.write(&message).await.unwrap();
            }
        });
        let (mut reader, mut writer) =
            Connection::new(Channels::connect(&endpoint).await.unwrap()).split();
        for message in [
            json!({"id":1,"method":"status"}),
            json!("line\nbreak"),
            json!([]),
        ] {
            writer.write(&message).await.unwrap();
            assert_eq!(reader.read().await.unwrap(), Some(message));
        }
        let oversized = json!("x".repeat(MESSAGE_LIMIT));
        assert!(writer.write(&oversized).await.is_err());
        // The peer sees the end once the connection is gone (both halves).
        drop((reader, writer));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_line_beyond_its_limit_or_cut_short_is_an_error() {
        let mut long = &b"0123456789\n"[..];
        assert!(bounded_line(&mut long, 10).await.is_err());
        let mut cut = &b"{\"id\":1"[..];
        assert!(bounded_line(&mut cut, 100).await.is_err());
        let mut fine = &b"{}\n"[..];
        assert_eq!(
            bounded_line(&mut fine, 3).await.unwrap().as_deref(),
            Some("{}")
        );
        assert_eq!(bounded_line(&mut fine, 3).await.unwrap(), None);
    }
}
