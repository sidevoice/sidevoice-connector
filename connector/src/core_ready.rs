//! What the connector reads of its core: the ready file the core writes once it serves (`D/core/core.json`) and
//! its health on its local endpoint, which must name the same launch and process.

use crate::platform::fs::{Files, Fs};
use crate::platform::ipc::{Channel, Channels, Endpoint};
use crate::profile::Profile;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
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
    let bytes = Files::read_private(path, 65536)?
        .with_context(|| format!("missing core ready file {}", path.display()))?;
    let ready: Ready = serde_json::from_slice(&bytes)?;
    if ready.launch_id.is_empty() || ready.pid == 0 {
        bail!("Core ready identity mismatch");
    }
    Ok(ready)
}

/// One HTTP/1.1 request on the core's local endpoint, within `limit`: the whole response.
async fn request(endpoint: &Endpoint, text: String, limit: Duration) -> Result<String> {
    let ask = async {
        let mut stream = Channels::connect(endpoint).await?;
        stream.write_all(text.as_bytes()).await?;
        let mut body = Vec::new();
        stream.take(65536).read_to_end(&mut body).await?;
        anyhow::Ok(body)
    };
    Ok(String::from_utf8(timeout(limit, ask).await??)?)
}

/// Whether `response` is a `status` answer.
fn answered(response: &str, status: u16) -> bool {
    ["HTTP/1.1", "HTTP/1.0"]
        .iter()
        .any(|version| response.starts_with(&format!("{version} {status} ")))
}

/// `GET /api/local/health` on the core's local endpoint, within `limit`: the body of its 200.
pub async fn health(endpoint: &Endpoint, limit: Duration) -> Result<Value> {
    let get = "GET /api/local/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let response = request(endpoint, get.to_owned(), limit).await?;
    if !answered(&response, 200) {
        bail!("Core health refused");
    }
    let (_, payload) = response
        .split_once("\r\n\r\n")
        .context("Core health body missing")?;
    Ok(serde_json::from_str(payload)?)
}

/// `POST /api/local/shutdown` on the core's local endpoint, within `limit`, naming the launch `ready` describes: the
/// core's request to leave as it does on `SIGTERM` (the only one a console-less core on Windows can get). Done when
/// the core accepted it (202); it leaves after answering, so what follows is waiting for it to be gone. Another
/// launch there, or a core that does not know the route, is refused.
pub async fn shutdown(ready: &Ready, limit: Duration) -> Result<()> {
    let body = serde_json::json!({"launch_id": ready.launch_id}).to_string();
    let post = format!(
        "POST /api/local/shutdown HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let response = request(&ready.endpoint, post, limit).await?;
    if !answered(&response, 202) {
        bail!(
            "Core refused to shut down: {}",
            response.lines().next().unwrap_or("no answer")
        );
    }
    Ok(())
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
    use super::{read, require_v3, shutdown, Ready};
    use crate::platform::fs::{Files, Fs};
    use crate::platform::ipc::{Address, Endpoint};
    use crate::platform::ipc::{Channel, Channels};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A newer core's ready file: fields this connector does not know, at the top and nested, and a protocol
    /// number it has never heard of. It is read for what it knows, and v3 is still found among the protocols.
    #[test]
    fn a_ready_file_with_fields_and_protocols_this_connector_does_not_know_is_read() {
        let scratch = crate::test_support::temp_dir("ready");
        let dir = scratch.path().join("core");
        Files::create_private_dir(&dir).unwrap();
        let endpoint = Endpoint::new(&dir, "local").to_string();
        let path = dir.join("core.json");
        let ready = serde_json::json!({"pid":42,"launch_id":"launch-1","endpoint":endpoint,"connector_id":"c-1",
            "token":"t","connector_protocols":[3, 4, 1000],"future_field":{"nested":[1, 2]},"listen":"x"});
        Files::write_json(&path, &ready).unwrap();
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

    /// A core listening on its endpoint that answers the one request it gets with `status`, and hands the request
    /// back.
    async fn fake_core(status: &str) -> (Ready, tokio::task::JoinHandle<String>) {
        let scratch = crate::test_support::temp_dir("shutdown");
        let dir = scratch.path().join("core");
        Files::create_private_dir(&dir).unwrap();
        let endpoint = Endpoint::new(&dir, "local");
        let listener = Channels::listen(&endpoint).unwrap();
        let ready = Ready {
            pid: 42,
            launch_id: "launch-1".into(),
            endpoint,
            connector_id: "c-1".into(),
            token: "t".into(),
            connector_protocols: Some(vec![3]),
        };
        let status = status.to_owned();
        let served = tokio::spawn(async move {
            let _scratch = scratch;
            let mut stream = listener.accept().await.unwrap();
            let mut seen = Vec::new();
            let mut chunk = [0u8; 1024];
            // The request is complete once its body (Content-Length bytes after the blank line) is in.
            loop {
                let n = stream.read(&mut chunk).await.unwrap();
                seen.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&seen).into_owned();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .find_map(|line| line.strip_prefix("Content-Length: "))
                        .map_or(0, |value| value.parse::<usize>().unwrap());
                    if body.len() >= length || n == 0 {
                        break;
                    }
                }
            }
            stream
                .write_all(
                    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            String::from_utf8(seen).unwrap()
        });
        (ready, served)
    }

    /// The request to leave names the launch it means, on the route the core serves, and a 202 is accepting it.
    #[tokio::test]
    async fn the_request_to_leave_names_the_launch_and_is_done_when_the_core_accepts() {
        let (ready, served) = fake_core("202 Accepted").await;
        shutdown(&ready, Duration::from_secs(5)).await.unwrap();
        let request = served.await.unwrap();
        assert!(
            request.starts_with("POST /api/local/shutdown HTTP/1.1\r\n"),
            "{request}"
        );
        let (_, body) = request.split_once("\r\n\r\n").unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).unwrap(),
            serde_json::json!({"launch_id": "launch-1"})
        );
    }

    /// Another launch there (409) and a core that predates the route (404) are refusals, not leaves.
    #[tokio::test]
    async fn a_core_that_does_not_accept_the_request_to_leave_is_a_refusal() {
        for status in ["409 Conflict", "404 Not Found"] {
            let (ready, served) = fake_core(status).await;
            let error = shutdown(&ready, Duration::from_secs(5)).await.unwrap_err();
            assert!(error.to_string().contains(status), "{error}");
            served.await.unwrap();
        }
    }

    /// Nobody there is a refusal too, within the limit.
    #[tokio::test]
    async fn without_a_core_the_request_to_leave_fails() {
        let (ready, served) = fake_core("202 Accepted").await;
        served.abort();
        let _ = served.await;
        assert!(shutdown(&ready, Duration::from_secs(2)).await.is_err());
    }
}
