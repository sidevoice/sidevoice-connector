//! Native pairing: no control subprocess and no credential-bearing redirects.
use crate::proof::{atomic_json, private_file, Profile};
use anyhow::{bail, Result};
use qrcode::{EcLevel, QrCode};
use reqwest::{Client, Url};
use serde_json::{json, Value};
use std::{fs, time::Duration};
fn text(key: &str, params: Value) -> String {
    crate::agents::message(key, &params)
}
pub fn previous_room(profile: &Profile) -> Option<String> {
    let file = profile.data.join("credentials.json");
    private_file(&file).ok()?;
    let saved: Value = serde_json::from_slice(&fs::read(file).ok()?).ok()?;
    if saved.get("connector_id")?.as_str()?.is_empty() || saved.get("token")?.as_str()?.is_empty() {
        return None;
    }
    normalize_origin(saved.get("url")?.as_str()?)
}
fn normalize_origin(address: &str) -> Option<String> {
    let mut url = Url::parse(address.trim()).ok()?;
    let scheme = match url.scheme() {
        "https" | "wss" => "https",
        "http" | "ws" => "http",
        _ => return None,
    };
    url.set_scheme(scheme).ok()?;
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    Some(url.origin().ascii_serialization())
}
fn plaintext_allowed(host: &str, trusted: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let host = host.trim_end_matches('.');
    if matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]") {
        return true;
    }
    trusted
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| s.chars().any(|c| c != '.'))
        .any(|entry| {
            if entry.starts_with('.') {
                host.ends_with(&entry)
            } else {
                host == entry
            }
        })
}
fn room_base(room: &str) -> Result<Url> {
    let url = Url::parse(room).map_err(|_| anyhow::anyhow!(text("pair.invalid-url", json!({}))))?;
    let trusted = std::env::var("SIDEVOICE_TRUSTED_CLUSTER_HOSTS").unwrap_or_default();
    if !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
        || !(url.scheme() == "https"
            || (url.scheme() == "http"
                && plaintext_allowed(url.host_str().unwrap_or(""), &trusted)))
    {
        bail!(
            "{}",
            text(
                "pair.unsafe-room",
                json!({"room":url.origin().ascii_serialization()})
            )
        );
    }
    Ok(url)
}
fn client() -> Result<Client> {
    Ok(Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}
async fn response_json(mut response: reqwest::Response) -> Result<(reqwest::StatusCode, Value)> {
    let status = response.status();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > 64 * 1024 {
            bail!("{}", text("pair.invalid-response", json!({})));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((status, serde_json::from_slice(&bytes).unwrap_or(json!({}))))
}
fn machine_identity(profile: &Profile) -> Value {
    let mut hostname = [0u8; 256];
    let host = std::env::var("SIDEVOICE_HOST_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if unsafe { libc::gethostname(hostname.as_mut_ptr().cast(), hostname.len()) } == 0 {
                let end = hostname
                    .iter()
                    .position(|b| *b == 0)
                    .unwrap_or(hostname.len());
                String::from_utf8_lossy(&hostname[..end]).into_owned()
            } else {
                String::new()
            }
        });
    let platform = match std::env::consts::OS {
        "macos" => "macOS",
        "linux" => "Linux",
        "windows" => "Windows",
        s => s,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        s => s,
    };
    let harnesses: Vec<&str> = [
        ("claude", &profile.claude),
        ("codex", &profile.codex),
        ("cursor", &profile.cursor),
    ]
    .into_iter()
    .filter(|(_, p)| p.exists())
    .map(|(id, _)| id)
    .collect();
    json!({"host":host,"platform":format!("{platform} {arch}"),"version":profile.connector_version(),"harnesses":harnesses})
}
pub async fn run_pair(profile: &Profile, room: &str, code: &str) -> Result<Value> {
    if room.is_empty() || code.is_empty() {
        bail!("{}", text("pair.usage", json!({})));
    }
    let base = room_base(room)?;
    let mut request = machine_identity(profile);
    request["code"] = json!(code);
    let response = client()?
        .post(base.join("/api/connectors/pair")?)
        .json(&request)
        .send()
        .await?;
    let (status, body) = response_json(response).await?;
    if !status.is_success() {
        bail!(
            "{}",
            text(
                "pair.failed-detail",
                json!({"detail":body.get("detail").cloned().unwrap_or(json!(status.as_u16()))})
            )
        );
    }
    let required = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
    };
    if !required("connector_id") || !required("token") || body.get("protocol").is_none() {
        bail!("{}", text("pair.invalid-response", json!({})));
    }
    let origin = base.origin().ascii_serialization();
    let mut credential = json!({"url":origin,"connector_id":body["connector_id"],"token":body["token"],"protocol":body["protocol"]});
    if body
        .get("dial_key")
        .is_some_and(|v| !v.is_null() && v != &json!(""))
    {
        credential["dial_key"] = body["dial_key"].clone();
    }
    atomic_json(&profile.data.join("credentials.json"), &credential)?;
    Ok(json!({"ok":true,"room":origin,"connector_id":body["connector_id"]}))
}
pub async fn link_room(profile: &Profile, room: &str) -> Result<Value> {
    let base = room_base(room)?;
    let response = client()?
        .post(base.join("/api/connectors/pairing-code")?)
        .header("origin", base.origin().ascii_serialization())
        .send()
        .await?;
    let (status, body) = response_json(response).await?;
    let code = body
        .get("code")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    if !status.is_success() || code.is_none() {
        bail!(
            "{}",
            text(
                "pair.no-code",
                json!({"room":base.origin().ascii_serialization(),"detail":body.get("detail").cloned().unwrap_or(json!(status.as_u16()))})
            )
        );
    }
    run_pair(profile, &base.origin().ascii_serialization(), code.unwrap()).await
}
pub fn reach(payload: &Value) -> &'static str {
    if payload
        .get("rv")
        .is_some_and(|v| !v.is_null() && v != &json!(false) && v != &json!(""))
    {
        return "room";
    }
    let direct = payload
        .get("urls")
        .and_then(Value::as_array)
        .is_some_and(|urls| {
            urls.iter().any(|v| {
                v.as_str()
                    .and_then(|s| Url::parse(s).ok())
                    .and_then(|u| u.host_str().map(str::to_owned))
                    .is_some_and(|h| {
                        !matches!(h.as_str(), "localhost" | "::1" | "[::1]")
                            && !h.starts_with("127.")
                    })
            })
        });
    if direct {
        "direct"
    } else {
        "local-only"
    }
}
pub fn device_result(answer: &Value) -> Result<Value> {
    if !answer
        .get("code")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty())
    {
        bail!("{}", text("pair-device.no-code", json!({})));
    }
    Ok(
        json!({"ok":true,"code":answer["code"],"expires_in":answer["expires_in"],"reach":reach(&answer["payload"]),"payload":answer["payload"]}),
    )
}
pub fn device_text(answer: &Value) -> Result<String> {
    device_result(answer)?;
    let code = answer["code"].as_str().unwrap();
    let qr = QrCode::with_error_correction_level(code.as_bytes(), EcLevel::L)
        .map_err(|_| anyhow::anyhow!(text("pair-device.qr-failed", json!({}))))?;
    let width = qr.width() as isize;
    let dark = |x: isize, y: isize| {
        x >= 0
            && y >= 0
            && x < width
            && y < width
            && qr[(x as usize, y as usize)] == qrcode::Color::Dark
    };
    let mut rendered = String::new();
    for y in (-2..width + 2).step_by(2) {
        for x in -2..width + 2 {
            rendered.push(match (dark(x, y), dark(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        rendered.push('\n');
    }
    let seconds = answer
        .get("expires_in")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let validity = if seconds <= 0.0 {
        text("pair-device.short", json!({}))
    } else if seconds < 120.0 {
        text(
            "pair-device.seconds",
            json!({"count":seconds.round() as u64}),
        )
    } else {
        text(
            "pair-device.minutes",
            json!({"count":(seconds / 60.0).round() as u64}),
        )
    };
    let host = answer
        .pointer("/payload/host")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| format!(" ({s})"))
        .unwrap_or_default();
    Ok(format!(
        "{}\n\n{code}\n\n{}\n\n{}",
        text(
            "pair-device.header",
            json!({"host":host,"validity":validity})
        ),
        rendered.trim_end_matches('\n'),
        text("pair-device.paste", json!({}))
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn room_origin_and_transport_policy() {
        assert_eq!(
            normalize_origin("wss://Room.example/api/connectors/ws"),
            Some("https://room.example".into())
        );
        assert_eq!(
            normalize_origin("http://127.0.0.1:8123/a?b=c"),
            Some("http://127.0.0.1:8123".into())
        );
        assert_eq!(normalize_origin("file:///tmp/credentials.json"), None);
        assert_eq!(normalize_origin("https://user@room.example/path"), None);
        assert!(plaintext_allowed("127.0.0.1", ""));
        assert!(!plaintext_allowed("10.0.0.2", ""));
        assert!(!plaintext_allowed(
            "svc.cluster.local",
            ".svc.cluster.local"
        ));
        assert!(plaintext_allowed(
            "room.svc.cluster.local",
            ".svc.cluster.local"
        ));
        assert!(!plaintext_allowed("evilroom.example", "room.example"));
    }
    #[test]
    fn device_reach_and_qr_preserve_code() {
        assert_eq!(reach(&json!({"rv":{}})), "room");
        assert_eq!(
            reach(&json!({"urls":["http://127.0.0.9","http://[::1]"]})),
            "local-only"
        );
        assert_eq!(
            reach(&json!({"urls":["https://machine.example"]})),
            "direct"
        );
        let answer = json!({"code":"abc123", "expires_in":120,"payload":{"host":"test"}});
        assert!(device_text(&answer).unwrap().contains("\n\nabc123\n\n"));
        assert!(device_result(&json!({"code":""})).is_err());
    }
    async fn serve_response(
        status: &str,
        body: Value,
    ) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let body = body.to_string();
        let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let count = stream.read(&mut buffer).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                let text = String::from_utf8_lossy(&request);
                if let Some((headers, content)) = text.split_once("\r\n\r\n") {
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if content.len() >= length {
                        break;
                    }
                }
            }
            stream.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });
        (address, task)
    }

    #[tokio::test]
    async fn pairing_saves_private_credentials_and_failed_response_keeps_them() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let root = std::env::temp_dir().join(format!("sidevoice-pair-{}", uuid::Uuid::new_v4()));
        // The proof constructor validates an existing private profile; it creates nothing.
        for child in [
            "",
            "home",
            "claude",
            "codex",
            "cursor/config",
            "cursor/data",
            "xdg/config",
            "xdg/data",
            "sidevoice/core",
        ] {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(root.join(child))
                .unwrap();
        }
        let profile = Profile::from_root(&root).unwrap();
        let (room, request) = serve_response(
            "200 OK",
            json!({"connector_id":"machine","token":"secret","protocol":2,"dial_key":"dial"}),
        )
        .await;
        let result = run_pair(&profile, &room, "one-time").await.unwrap();
        assert_eq!(result["room"], room);
        let request = request.await.unwrap();
        assert!(request.starts_with("POST /api/connectors/pair HTTP/1.1"));
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["code"], "one-time");
        assert!(body["host"].is_string());
        let file = profile.data.join("credentials.json");
        let original = fs::read(&file).unwrap();
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(previous_room(&profile).as_deref(), Some(room.as_str()));
        let (room, request) =
            serve_response("200 OK", json!({"connector_id":"missing-token"})).await;
        assert!(run_pair(&profile, &room, "bad").await.is_err());
        request.await.unwrap();
        assert_eq!(fs::read(&file).unwrap(), original);
        let (room, request) = serve_response("302 Found", json!({"detail":"redirect"})).await;
        assert!(run_pair(&profile, &room, "redirect").await.is_err());
        request.await.unwrap();
        assert_eq!(fs::read(&file).unwrap(), original);
        fs::remove_dir_all(root).unwrap();
    }
}

/// CLI pairing confirmation; credentials themselves never appear in the result text.
pub fn cli_text(profile: &Profile, result: &Value, linked: bool) -> String {
    if let Some(error) = result.get("error") {
        return error["message"].as_str().unwrap_or("").to_owned();
    }
    text(
        if linked {
            "pair.linked"
        } else {
            "pair.success"
        },
        json!({"room":result["room"],"connector_id":result["connector_id"],"file":profile.data.join("credentials.json")}),
    )
}
