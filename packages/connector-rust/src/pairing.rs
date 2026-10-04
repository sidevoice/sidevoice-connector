use crate::proof::{private_file, Profile};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::fs;
use std::process::Stdio;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

fn room_origin(profile: &Profile) -> Option<String> {
    if !profile.is_installed() {
        return None;
    }
    let file = profile.data.join("credentials.json");
    private_file(&file).ok()?;
    let saved: Value = serde_json::from_slice(&fs::read(file).ok()?).ok()?;
    saved
        .get("url")
        .and_then(Value::as_str)
        .and_then(normalize_origin)
}

fn normalize_origin(address: &str) -> Option<String> {
    let (scheme, remainder) = address.trim().split_once("://")?;
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "https" | "wss" => "https",
        "http" | "ws" => "http",
        _ => return None,
    };
    let authority = remainder.split(['/', '?', '#']).next()?;
    if authority.is_empty() || authority.contains('@') || authority.chars().any(char::is_control) {
        return None;
    }
    Some(format!("{scheme}://{}", authority.to_ascii_lowercase()))
}

pub async fn run_pair(profile: &Profile, room: &str, code: &str) -> Result<Value> {
    if !profile.is_installed() {
        bail!("pairing is unavailable in the isolated Rust proof");
    }
    if room.is_empty() || code.is_empty() {
        bail!("Hacen falta la dirección de la sala y el código.");
    }
    let control = profile.control_executable()?;
    let output = timeout(
        Duration::from_secs(18),
        Command::new(control)
            .args(["pair", "--json", "--", room, code])
            .env("SIDEVOICE_DATA_DIR", &profile.data)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("pairing timed out")??;
    if output.stdout.len() > 16 * 1024 || output.stderr.len() > 16 * 1024 {
        bail!("the selected Sidevoice control command returned an oversized pairing result");
    }
    let result: Value = serde_json::from_slice(&output.stdout)
        .context("the selected Sidevoice control command returned invalid pairing JSON")?;
    if output.status.success()
        && result.get("ok") == Some(&json!(true))
        && result.get("room").and_then(Value::as_str).is_some()
        && result.get("connector_id").and_then(Value::as_str).is_some()
    {
        return Ok(result);
    }
    let detail = result
        .pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| std::str::from_utf8(&output.stderr).ok())
        .unwrap_or("pairing failed")
        .trim();
    bail!("{detail}");
}

pub fn previous_room(profile: &Profile) -> Option<String> {
    room_origin(profile)
}

#[cfg(test)]
mod tests {
    use super::normalize_origin;

    #[test]
    fn previous_room_uses_the_same_http_origin_as_the_javascript_pairing_surface() {
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
    }
}
