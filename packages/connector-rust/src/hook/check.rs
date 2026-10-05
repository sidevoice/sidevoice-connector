//! The common check: one bounded request to the local connector, counts only.

use crate::proof::{verify_socket, Profile};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::{timeout, Duration};

/// Whether `thread`, if this machine joined it for pulled input with `harness`, has messages
/// waiting: `{connected, pending, count, fresh}`.
pub(super) async fn pending(profile: &Profile, harness: &str, thread: &str) -> Result<Value> {
    verify_socket(&profile.socket)?;
    let stream = timeout(Duration::from_secs(2), UnixStream::connect(&profile.socket)).await??;
    let (read, mut write) = stream.into_split();
    let request =
        json!({"id":1,"method":"pull_check","params":{"harness":harness,"thread":thread}});
    write.write_all(format!("{request}\n").as_bytes()).await?;
    let mut reader = BufReader::new(read);
    let line = timeout(
        Duration::from_secs(6),
        crate::bounded_line(&mut reader, 1 << 16),
    )
    .await??
    .context("connector closed the check")?;
    let reply: Value = serde_json::from_str(&line)?;
    if reply.get("ok") != Some(&json!(true)) {
        bail!(
            "{}",
            reply
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("refused")
        );
    }
    Ok(reply.get("result").cloned().unwrap_or(Value::Null))
}
