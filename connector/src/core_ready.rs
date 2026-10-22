//! What the connector reads of its core: the ready file the core writes once it serves (`D/core/core.json`) and
//! its health on its local endpoint, which must name the same launch and process.

use crate::platform::fs::{Files, Fs};
use crate::platform::ipc::{Channel, Channels, Endpoint};
use crate::profile::Profile;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::fs;
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{timeout, Duration};

/// `core.json` as the core writes it. A core of another version may add fields: they are ignored, never refused.
#[derive(Clone, Debug, Deserialize)]
pub struct Ready {
    pub pid: u32,
    pub launch_id: String,
    /// Where the core listens: its endpoint record, read with `Endpoint`'s `FromStr`. Required: a ready file without a
    /// valid one is refused. (The ready file is the only source; the endpoint is never derived from where the core
    /// was told to put it.)
    #[serde(deserialize_with = "endpoint")]
    pub endpoint: Endpoint,
    pub connector_id: String,
    pub token: String,
    /// The link protocols this core speaks; a number this connector does not know is only not one it speaks.
    pub connector_protocols: Option<Vec<u64>>,
}

/// An [`Endpoint`] from its record.
fn endpoint<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Endpoint, D::Error> {
    String::deserialize(deserializer)?
        .parse()
        .map_err(serde::de::Error::custom)
}

/// The ready file at `path`, when it is this user's private file naming a launch and an endpoint.
pub fn read(path: &Path) -> Result<Ready> {
    Files::check_private(&Files::open_read(path)?)
        .with_context(|| format!("unsafe private file {}", path.display()))?;
    let bytes = fs::read(path)?;
    if bytes.len() > 65536 {
        bail!("Core ready file too large");
    }
    let ready: Ready = serde_json::from_slice(&bytes)?;
    if ready.launch_id.is_empty() || ready.pid == 0 {
        bail!("Core ready identity mismatch");
    }
    Ok(ready)
}

/// `GET /api/local/health` on the core's local endpoint, within `limit`: the body of its 200.
pub async fn health(endpoint: &Endpoint, limit: Duration) -> Result<Value> {
    let ask = async {
        let mut stream = Channels::connect(endpoint).await?;
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
        let ready = read(&self.core_ready)?;
        require_v3(&ready)?;
        Ok(ready)
    }

    pub async fn health_ready(&self, ready: &Ready) -> Result<Value> {
        if ready.launch_id.is_empty() || ready.pid == 0 {
            bail!("Core ready identity mismatch");
        }
        require_v3(ready)?;
        let health = health(&ready.endpoint, Duration::from_secs(2)).await?;
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
    use crate::platform::ipc::{Address, Endpoint};
    use std::os::unix::fs::PermissionsExt;

    /// A newer core's ready file: fields this connector does not know, at the top and nested, and a protocol
    /// number it has never heard of. It is read for what it knows, and v3 is still found among the protocols.
    #[test]
    fn a_ready_file_with_fields_and_protocols_this_connector_does_not_know_is_read() {
        let scratch = crate::test_support::temp_dir("ready");
        let endpoint = Endpoint::new(scratch.path(), "local").to_string();
        let path = scratch.path().join("core.json");
        let ready = serde_json::json!({"pid":42,"launch_id":"launch-1","endpoint":endpoint,"connector_id":"c-1",
            "token":"t","connector_protocols":[3, 4, 1000],"future_field":{"nested":[1, 2]},"listen":"x"});
        std::fs::write(&path, serde_json::to_vec(&ready).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let read = read(&path).unwrap();
        assert_eq!((read.pid, read.launch_id.as_str()), (42, "launch-1"));
        assert_eq!(read.connector_protocols, Some(vec![3, 4, 1000]));
        require_v3(&read).unwrap();
    }

    /// The ready file names the core's endpoint as its record; without a valid one it is refused.
    #[test]
    fn the_endpoint_is_read_from_the_ready_file_and_required() {
        let ready = |extra: serde_json::Value| {
            let mut file =
                serde_json::json!({"pid":1,"launch_id":"l","connector_id":"c","token":"t"});
            file.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            serde_json::from_value::<super::Ready>(file)
        };
        let endpoint = Endpoint::new(&std::env::temp_dir(), "core");
        let read = ready(serde_json::json!({"endpoint": endpoint.to_string()})).unwrap();
        assert_eq!(read.endpoint, endpoint);
        assert!(ready(serde_json::json!({})).is_err());
        assert!(ready(serde_json::json!({"endpoint":"relative"})).is_err());
        assert!(ready(serde_json::json!({"endpoint":1})).is_err());
    }
}
