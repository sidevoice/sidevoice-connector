//! Pairing a device with this machine (`pair-device`, `voice_pair_device`), and only ever as the person's act: the core
//! issues a one-time code, asked through the connector (`pair_device` on `connector.sock`, the core's
//! `device.pairing_code`), and it is said the way a person can use it: the code, the same code as a QR, how long it is
//! valid, and where it can be used from.

use crate::messages::Keyed;
use crate::profile::Profile;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::time::Duration;
use ureq::http::Uri;

/// How long the connector has to hand over a device code: its own wait for the core included (`daemon.rs`).
const DEVICE_CODE_TIMEOUT: Duration = Duration::from_secs(28);
/// What a person typed, safe to say back: no control characters, bounded.
fn printable(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control())
        .take(200)
        .collect()
}

/// Where a device holding this code can reach the machine: at an address that is not this computer's (`direct`), or
/// only from this computer (`local-only`).
pub fn reach(payload: &Value) -> &'static str {
    let direct = payload
        .get("urls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|url| !loopback_url(url));
    if direct {
        "direct"
    } else {
        "local-only"
    }
}

fn loopback_url(url: &str) -> bool {
    let Ok(uri) = url.parse::<Uri>() else {
        return true;
    };
    let host = uri.host().unwrap_or("");
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.is_empty()
        || host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host.starts_with("127.")
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
    let direct: Vec<_> = payload
        .get("urls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|url| !loopback_url(url))
        .map(printable)
        .collect();
    if !direct.is_empty() {
        text.push_str(&format!("\n\nAddresses included in this code: {}. A private HTTP connection in the native app asks for your consent before pairing.", direct.join(", ")));
    } else if reach(&payload) == "local-only" {
        text.push_str("\n\nThis code has no address for another device. If this Core listens on the LAN, enter its IP address and port in the native app when pairing; otherwise configure a reachable address.");
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
/// Refused before anything is started when nothing would start the core (`launcher::core_refusal`).
pub async fn device_code(profile: &Profile) -> Result<Value> {
    let layout = crate::service::layout::Layout::from_profile(profile);
    if let Some(refusal) = crate::service::launcher::core_refusal(&layout).await {
        return Err(refusal.into());
    }
    crate::service::ensure_connector(profile).await?;
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

    fn key(error: anyhow::Error) -> &'static str {
        crate::messages::keyed(&error).key
    }

    #[test]
    fn reach_says_where_a_device_code_works() {
        assert_eq!(
            reach(&json!({"urls": ["http://127.0.0.1:9", "http://192.168.1.4:9"]})),
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
            "payload": {"host": "studio", "urls": ["http://127.0.0.1:9"]}});
        let text = device_text(&answer).unwrap();
        assert!(text.starts_with(
            "One-time code to pair a device with this machine (studio), valid for 10 minutes:\n\nSV1.abc\n\n"
        ));
        assert!(text.contains(&qr_text("SV1.abc").unwrap()));
        assert!(text.contains("no address for another device"));
        let lan = json!({"code":"SV1.abc", "expires_in":600,
            "payload":{"host":"studio","urls":["http://127.0.0.1:10829","http://192.168.1.7:10829"]}});
        let lan_text = device_text(&lan).unwrap();
        assert!(lan_text.contains("http://192.168.1.7:10829"));
        assert!(lan_text.contains("asks for your consent"));
        let remote = json!({"code": "SV1.abc", "expires_in": 90, "payload": {"urls": ["https://studio.example"]}});
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
