//! Pairing, both ways, and only ever as the person's act.
//!
//! - **This machine with a room** (`pair <room-url> <code>`, `voice_pair`, the core's `pair.request`): the one-time
//!   code the room shows under "Emparejar máquina" is redeemed at the room's `POST /api/connectors/pair` for this
//!   machine's connector credential, written to `D/credentials.json` (0600, whole or not at all). The core follows
//!   that file itself and links with the room it names, so pairing restarts nothing. Registering with a room is open
//!   by decision: a room is a relay and grants nothing by itself; which device may use this machine is this
//!   machine's own pairing (below).
//! - **A device with this machine** (`pair-device`, `voice_pair_device`): the core issues a one-time code, asked
//!   through the connector (`pair_device` on `connector.sock`, the core's `device.pairing_code`), and it is said the
//!   way a person can use it: the code, the same code as a QR, how long it is valid, and where it can be used from.
//!
//! Nothing here asks a room for a code: linking a room is the person's act, by hand.

use crate::messages::Keyed;
use crate::profile::Profile;
use crate::secure_fs::{ensure_private_dir, read_trusted, write_private};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use ureq::http::Uri;

/// How long the room has to answer a pairing.
const PAIR_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the connector has to hand over a device code: its own wait for the core included (`daemon.rs`).
const DEVICE_CODE_TIMEOUT: Duration = Duration::from_secs(28);
/// The largest answer read from a room.
const ANSWER_LIMIT: u64 = 64 * 1024;

fn room_origin(profile: &Profile) -> Option<String> {
    let file = profile.data.join("credentials.json");
    let bytes = read_trusted(&file, 65536).ok()??;
    let saved: Value = serde_json::from_slice(&bytes).ok()?;
    saved
        .get("url")
        .and_then(Value::as_str)
        .and_then(normalize_origin)
}

pub fn normalize_origin(address: &str) -> Option<String> {
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

/// The room this machine is paired with, as an origin (`https://host[:port]`), from its trusted credentials.
pub fn previous_room(profile: &Profile) -> Option<String> {
    room_origin(profile)
}

/// Where a credential may travel in clear: loopback, and the hosts in `SIDEVOICE_TRUSTED_CLUSTER_HOSTS`
/// (comma-separated; an entry starting with a dot is a suffix, `.svc.cluster.local`, any other one exact host) — the
/// same rule the core applies when it dials the room. Listing a host there is whoever runs this machine saying the
/// network to it is theirs; a host name proves nothing about where it resolves, so no spelling is trusted by
/// default. Anything else, a private address or a cluster service name included, needs TLS.
pub fn plaintext_allowed(host: &str, trusted: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let host = host.strip_suffix('.').unwrap_or(&host);
    if host.is_empty() {
        return false;
    }
    if ["127.0.0.1", "localhost", "::1", "[::1]"].contains(&host) {
        return true;
    }
    trusted
        .split(',')
        .map(|entry| entry.trim().to_ascii_lowercase())
        .filter(|entry| entry.chars().any(|ch| ch != '.'))
        .any(|entry| match entry.strip_prefix('.') {
            Some(_) => host.ends_with(&entry),
            None => host == entry,
        })
}

/// The room's origin (`https://host[:port]`, the default port left out), if this machine may send it a code and
/// receive a credential from it: https, or in clear only to loopback and the trusted cluster hosts.
pub fn room_base(room: &str, trusted: &str) -> Result<String> {
    let bad = || Keyed::new("pair.bad-url", json!({"room": printable(room)}));
    let uri: Uri = room.trim().parse().map_err(|_| bad())?;
    let (Some(scheme), Some(authority)) = (uri.scheme_str(), uri.authority()) else {
        bail!(bad());
    };
    if authority.as_str().contains('@') || authority.host().is_empty() {
        bail!(bad());
    }
    let scheme = scheme.to_ascii_lowercase();
    let host = authority.host().to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "https" => 443,
        "http" => 80,
        _ => 0,
    };
    let origin = match authority.port_u16() {
        Some(port) if port != default_port => format!("{scheme}://{host}:{port}"),
        _ => format!("{scheme}://{host}"),
    };
    if scheme != "https" && !(scheme == "http" && plaintext_allowed(&host, trusted)) {
        bail!(Keyed::new("pair.insecure", json!({"origin": origin})));
    }
    Ok(origin)
}

/// What a person typed, safe to say back: no control characters, bounded.
fn printable(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control())
        .take(200)
        .collect()
}

/// A pairing redeemed: the room, this machine's connector id there, and where the credential was written.
#[derive(Debug)]
pub struct Paired {
    pub origin: String,
    pub connector_id: String,
    pub file: PathBuf,
}

/// Redeems `code` at `room` for this machine's connector credential and writes it to `D/credentials.json`.
pub async fn pair(profile: &Profile, room: &str, code: &str) -> Result<Paired> {
    let code = code.trim();
    if room.trim().is_empty() || code.is_empty() {
        bail!(Keyed::new("pair.missing", json!({})));
    }
    let trusted = std::env::var("SIDEVOICE_TRUSTED_CLUSTER_HOSTS").unwrap_or_default();
    let origin = room_base(room, &trusted)?;
    // Before the code is spent: a data directory that cannot hold the credential privately is refused now.
    ensure_private_dir(&profile.data)?;
    // The code, and who this machine is: the room lists what it is told, so it is told here too and not only on
    // every link, and a machine paired and never yet connected still reads as a machine.
    let mut body = crate::identity::machine(profile);
    body["code"] = json!(code);
    let url = format!("{origin}/api/connectors/pair");
    let (status, answer) = tokio::task::spawn_blocking(move || post(&url, &body))
        .await?
        .map_err(|detail| {
            Keyed::new(
                "pair.unreachable",
                json!({"origin": origin, "detail": printable(&detail)}),
            )
        })?;
    if !(200..300).contains(&status) {
        let detail = answer
            .get("detail")
            .and_then(Value::as_str)
            .map(printable)
            .unwrap_or_else(|| format!("HTTP {status}"));
        bail!(Keyed::new(
            "pair.refused",
            json!({"origin": origin, "detail": detail})
        ));
    }
    let text = |name: &str| {
        answer
            .get(name)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let (Some(connector_id), Some(token)) = (text("connector_id"), text("token")) else {
        bail!(Keyed::new("pair.bad-answer", json!({"origin": origin})));
    };
    // Where the room is, not how to reach it: the path that carries the link belongs to the core and moves with its
    // version, so an upgrade never rewrites what pairing wrote. The dial key is what the room shows this machine's
    // core if the room is ever the one to open the link.
    let mut saved = json!({"url": origin, "connector_id": connector_id, "token": token,
        "protocol": answer.get("protocol").cloned().unwrap_or(Value::Null)});
    if let Some(dial_key) = answer.get("dial_key").filter(|key| !key.is_null()) {
        saved["dial_key"] = dial_key.clone();
    }
    let file = profile.data.join("credentials.json");
    write_private(&file, serde_json::to_string_pretty(&saved)?.as_bytes())?;
    Ok(Paired {
        origin,
        connector_id,
        file,
    })
}

/// One JSON POST: the status and the answer's JSON (`{}` when it is not JSON), or why nothing came back. No proxy,
/// no redirect: the code goes to the origin that was checked and nowhere else.
fn post(url: &str, body: &Value) -> std::result::Result<(u16, Value), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(PAIR_TIMEOUT))
        .http_status_as_error(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .proxy(None)
        .user_agent(format!("sidevoice-connector/{}", crate::identity::VERSION))
        .build()
        .into();
    let mut response = agent
        .post(url)
        .header("content-type", "application/json")
        .send(body.to_string())
        .map_err(|error| error.to_string())?;
    let status = response.status().as_u16();
    let answer = response
        .body_mut()
        .with_config()
        .limit(ANSWER_LIMIT)
        .read_to_string()
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    Ok((status, answer))
}

/// Where a device holding this code can reach the machine: through its room (`room`), at an address that is not
/// this computer's (`direct`), or only from this computer (`local-only`).
pub fn reach(payload: &Value) -> &'static str {
    if payload.get("rv").is_some_and(|rv| !rv.is_null()) {
        return "room";
    }
    let loopback = |url: &str| {
        let Ok(uri) = url.parse::<Uri>() else {
            return true;
        };
        let host = uri.host().unwrap_or("");
        let host = host.trim_start_matches('[').trim_end_matches(']');
        host.is_empty()
            || host.eq_ignore_ascii_case("localhost")
            || host == "::1"
            || host.starts_with("127.")
    };
    let direct = payload
        .get("urls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|url| !loopback(url));
    if direct {
        "direct"
    } else {
        "local-only"
    }
}

/// The code as a QR a terminal shows as it is: two modules per character (UTF-8 half blocks, a dark module drawn),
/// error correction L, a margin of two modules.
pub fn qr_text(code: &str) -> Result<String> {
    use qrcodegen::{QrCode, QrCodeEcc};
    const MARGIN: i32 = 2;
    let qr = QrCode::encode_text(code, QrCodeEcc::Low)
        .map_err(|_| anyhow::anyhow!("the pairing code is too long for a QR"))?;
    let total = qr.size() + 2 * MARGIN;
    let dark = |x: i32, y: i32| qr.get_module(x - MARGIN, y - MARGIN);
    let mut rows = Vec::new();
    for y in (0..total).step_by(2) {
        let row: String = (0..total)
            .map(|x| match (dark(x, y), dark(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            })
            .collect();
        rows.push(row);
    }
    Ok(rows.join("\n"))
}

fn validity(seconds: Option<f64>) -> String {
    match seconds {
        Some(seconds) if seconds.is_finite() && seconds > 0.0 => {
            if seconds < 120.0 {
                format!("{} seconds", seconds.round())
            } else {
                format!("{} minutes", (seconds / 60.0).round())
            }
        }
        _ => "only a short while".into(),
    }
}

/// What the person reads: the code, its QR, its validity and where it can be used from.
pub fn device_text(answer: &Value) -> Result<String> {
    let code = answer.get("code").and_then(Value::as_str).unwrap_or("");
    let payload = answer.get("payload").cloned().unwrap_or(Value::Null);
    let host = payload
        .get("host")
        .and_then(Value::as_str)
        .filter(|host| !host.is_empty())
        .map(|host| format!(" ({})", printable(host)))
        .unwrap_or_default();
    let valid = validity(answer.get("expires_in").and_then(Value::as_f64));
    let mut text = format!(
        "One-time code to pair a device with this machine{host}, valid for {valid}:\n\n{code}\n\n{}\n\nPaste it in the Sidevoice app where it asks to pair a machine.",
        qr_text(code)?
    );
    if reach(&payload) == "local-only" {
        text.push_str("\n\nThis code only works on this computer: the machine is not reachable from other devices. To use it from elsewhere, pair this machine with your room: run  sidevoice pair <room-url> <code>  with the code the room shows for pairing a machine.");
    }
    Ok(text)
}

/// The core's answer to `pair_device`, checked to carry a code.
pub fn device_answer(answer: Value) -> Result<Value> {
    match answer.get("code").and_then(Value::as_str) {
        Some(code) if !code.is_empty() => Ok(answer),
        _ => bail!(Keyed::new(
            "pair-device.failed",
            json!({"detail": "the core answered without a pairing code"})
        )),
    }
}

/// `pair-device`: a code from this machine's core, through its connector (started on demand when nothing runs it).
pub async fn device_code(profile: &Profile) -> Result<Value> {
    crate::service::ensure_connector(profile).await?;
    let layout = crate::service::layout::Layout::from_profile(profile);
    let answer = crate::service::launcher::request_connector(
        &layout,
        "pair_device",
        json!({}),
        DEVICE_CODE_TIMEOUT,
    )
    .await
    .map_err(|detail| Keyed::new("pair-device.failed", json!({"detail": printable(&detail)})))?;
    device_answer(answer)
}

/// What `pair-device --json` says: the fields the desktop app reads (`code`, `expires_in`, `reach`) and the payload.
pub fn device_json(answer: &Value) -> Value {
    let payload = answer.get("payload").cloned().unwrap_or(Value::Null);
    json!({"ok": true, "code": answer["code"], "expires_in": answer.get("expires_in").cloned().unwrap_or(Value::Null),
        "reach": reach(&payload), "payload": payload})
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn key(error: anyhow::Error) -> &'static str {
        crate::messages::keyed(&error).key
    }

    #[test]
    fn a_credential_travels_in_clear_only_to_loopback_and_named_cluster_hosts() {
        assert_eq!(
            room_base("https://Room.Example:443/path", "").unwrap(),
            "https://room.example"
        );
        assert_eq!(
            room_base("https://room.example:8443", "").unwrap(),
            "https://room.example:8443"
        );
        assert_eq!(
            room_base("http://127.0.0.1:8123/x", "").unwrap(),
            "http://127.0.0.1:8123"
        );
        assert_eq!(
            room_base("http://localhost", "").unwrap(),
            "http://localhost"
        );
        assert_eq!(room_base("http://[::1]:9", "").unwrap(), "http://[::1]:9");
        for (room, trusted) in [
            ("http://room.example", ""),
            ("http://10.0.0.5:80", ""),
            ("http://room.svc.cluster.local", ""),
            ("http://room.svc.cluster.local", "other.svc.cluster.local"),
            ("http://evilsvc.cluster.local", ".svc.cluster.local"),
            ("http://room.example", "., . ,"),
            ("ftp://room.example", ""),
        ] {
            assert_eq!(
                key(room_base(room, trusted).unwrap_err()),
                "pair.insecure",
                "{room} {trusted}"
            );
        }
        assert!(room_base("http://room.svc.cluster.local", " .SVC.cluster.local ").is_ok());
        assert!(room_base("http://room.svc.cluster.local.", ".svc.cluster.local").is_ok());
        assert!(room_base("http://relay", "relay,other").is_ok());
        for room in [
            "room.example",
            "https://user@room.example",
            "https://",
            "/path",
        ] {
            assert_eq!(
                key(room_base(room, "").unwrap_err()),
                "pair.bad-url",
                "{room}"
            );
        }
    }

    #[test]
    fn reach_says_where_a_device_code_works() {
        assert_eq!(
            reach(&json!({"rv": {"room": "https://r"}, "urls": []})),
            "room"
        );
        assert_eq!(
            reach(&json!({"rv": null, "urls": ["http://127.0.0.1:9", "http://192.168.1.4:9"]})),
            "direct"
        );
        assert_eq!(
            reach(&json!({"urls": ["http://127.0.0.1:9", "http://[::1]:9", "http://localhost:9"]})),
            "local-only"
        );
        assert_eq!(reach(&Value::Null), "local-only");
    }

    #[test]
    fn the_qr_is_half_blocks_with_a_two_module_margin() {
        let qr = qr_text("SV1.eyJ2IjoxfQ").unwrap();
        let rows: Vec<&str> = qr.lines().collect();
        // The smallest code is 21 modules: 25 with the margin, drawn as 13 rows of 25 characters.
        let width = rows[0].chars().count();
        assert!(width >= 25 && (width - 4 - 21).is_multiple_of(4), "{width}");
        assert_eq!(rows.len(), width.div_ceil(2));
        assert!(rows.iter().all(|row| row.chars().count() == width));
        assert!(qr
            .chars()
            .all(|ch| matches!(ch, '█' | '▀' | '▄' | ' ' | '\n')));
        // The margin is light: the first row is blank, and the finder pattern's top edge starts the second.
        assert!(rows[0].chars().all(|ch| ch == ' '), "{qr}");
        assert!(rows[1].starts_with("  █▀▀▀▀▀█"), "{qr}");
    }

    #[test]
    fn the_device_text_says_the_code_its_qr_its_validity_and_its_reach() {
        let answer = json!({"code": "SV1.abc", "expires_in": 600,
            "payload": {"host": "studio", "urls": ["http://127.0.0.1:9"], "rv": null}});
        let text = device_text(&answer).unwrap();
        assert!(text.starts_with(
            "One-time code to pair a device with this machine (studio), valid for 10 minutes:\n\nSV1.abc\n\n"
        ));
        assert!(text.contains(&qr_text("SV1.abc").unwrap()));
        assert!(text.contains("only works on this computer"));
        let remote =
            json!({"code": "SV1.abc", "expires_in": 90, "payload": {"rv": {"room": "https://r"}}});
        let text = device_text(&remote).unwrap();
        assert!(text.contains("valid for 90 seconds"), "{text}");
        assert!(!text.contains("only works on this computer"));
        assert!(!text.contains("(studio)"));
        let json = device_json(&answer);
        assert_eq!(json["ok"], true);
        assert_eq!(json["code"], "SV1.abc");
        assert_eq!(json["expires_in"], 600);
        assert_eq!(json["reach"], "local-only");
        assert_eq!(
            key(device_answer(json!({"payload": {}})).unwrap_err()),
            "pair-device.failed"
        );
    }
}
