//! What the connector reads of its core: the ready file the core writes once it serves (`D/core/core.json`) and
//! its health on the private socket, which must name the same launch and process.

use crate::platform::fs::{Files, Fs};
use crate::profile::Profile;
use crate::secure_fs::verify_socket;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::{timeout, Duration};

/// `core.json` as the core writes it. A core of another version may add fields: they are ignored, never refused.
#[derive(Clone, Debug, Deserialize)]
pub struct Ready {
    pub pid: u32,
    pub launch_id: String,
    pub socket: PathBuf,
    pub connector_id: String,
    pub token: String,
    /// The link protocols this core speaks; a number this connector does not know is only not one it speaks.
    pub connector_protocols: Option<Vec<u64>>,
}

/// The ready file at `path`, when it is this user's private file naming a launch on `socket`.
pub fn read(path: &Path, socket: &Path) -> Result<Ready> {
    Files::check_private_file(path)?;
    let bytes = fs::read(path)?;
    if bytes.len() > 65536 {
        bail!("Core ready file too large");
    }
    let ready: Ready = serde_json::from_slice(&bytes)?;
    if ready.socket != socket || ready.launch_id.is_empty() || ready.pid == 0 {
        bail!("Core ready identity mismatch");
    }
    Ok(ready)
}

/// `GET /api/local/health` on the core's private socket, within `limit`: the body of its 200.
pub async fn health(socket: &Path, limit: Duration) -> Result<Value> {
    verify_socket(socket)?;
    let ask = async {
        let mut stream = UnixStream::connect(socket).await?;
        stream
            .write_all(
                b"GET /api/local/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .await?;
        let mut body = Vec::new();
        stream.take(65536).read_to_end(&mut body).await?;
        anyhow::Ok(body)
    };
    let response = String::from_utf8(timeout(limit, ask).await??)?;
    if !response.starts_with("HTTP/1.1 200 ") && !response.starts_with("HTTP/1.0 200 ") {
        bail!("Core health refused");
    }
    let (_, payload) = response
        .split_once("\r\n\r\n")
        .context("Core health body missing")?;
    Ok(serde_json::from_str(payload)?)
}

fn require_v3(ready: &Ready) -> Result<()> {
    if !ready
        .connector_protocols
        .as_ref()
        .is_some_and(|versions| versions.contains(&3))
    {
        bail!("Core v3 upgrade required");
    }
    Ok(())
}

impl Profile {
    pub async fn ready(&self) -> Result<Ready> {
        self.health().await.map(|(ready, _)| ready)
    }

    pub fn read_ready(&self) -> Result<Ready> {
        let ready = read(&self.core_ready, &self.core_socket)?;
        require_v3(&ready)?;
        verify_socket(&ready.socket)?;
        Ok(ready)
    }

    pub async fn health_ready(&self, ready: &Ready) -> Result<Value> {
        if ready.socket != self.core_socket || ready.launch_id.is_empty() || ready.pid == 0 {
            bail!("Core ready identity mismatch");
        }
        require_v3(ready)?;
        let health = health(&ready.socket, Duration::from_secs(2)).await?;
        if health.get("launch_id").and_then(Value::as_str) != Some(&ready.launch_id)
            || health.get("pid").and_then(Value::as_u64) != Some(ready.pid as u64)
        {
            bail!("Core health identity mismatch");
        }
        Ok(health)
    }

    pub async fn health(&self) -> Result<(Ready, Value)> {
        let ready = self.read_ready()?;
        let health = self.health_ready(&ready).await?;
        Ok((ready, health))
    }
}

#[cfg(test)]
mod tests {
    use super::{read, require_v3};
    use crate::platform::fs::tests::Scratch;
    use std::os::unix::fs::PermissionsExt;

    /// A newer core's ready file: fields this connector does not know, at the top and nested, and a protocol
    /// number it has never heard of. It is read for what it knows, and v3 is still found among the protocols.
    #[test]
    fn a_ready_file_with_fields_and_protocols_this_connector_does_not_know_is_read() {
        let scratch = Scratch::new("ready");
        let socket = scratch.0.join("local.sock");
        let path = scratch.0.join("core.json");
        let ready = serde_json::json!({"pid":42,"launch_id":"launch-1","socket":socket,"connector_id":"c-1",
            "token":"t","connector_protocols":[3, 4, 1000],"future_field":{"nested":[1, 2]},"listen":"x"});
        std::fs::write(&path, serde_json::to_vec(&ready).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let read = read(&path, &socket).unwrap();
        assert_eq!((read.pid, read.launch_id.as_str()), (42, "launch-1"));
        assert_eq!(read.connector_protocols, Some(vec![3, 4, 1000]));
        require_v3(&read).unwrap();
    }
}
