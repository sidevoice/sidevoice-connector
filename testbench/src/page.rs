//! The bench's page and its small JSON API, on 127.0.0.1 only:
//!
//! - `GET /`: the page (`page.html`);
//! - `GET /api/state?since=N`: whether the call is open, the connectors, the conversations (bindings) and the call's
//!   events after N;
//! - `GET /api/history?thread=T`: what was said into conversation T, with its receipt, and the replies;
//! - `POST /api/say` `{"thread", "text"}`: says the text into the conversation, as if spoken.
//!
//! The page names its API relative to itself, so a proxy may serve it under a prefix.
//!
//! A request must name the bench itself as its `Host`, or a name given with `--allow-host` (no other site can reach it
//! through a name of its own), and a write must be JSON (no plain form from another page can make one).

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::call::Call;

const PAGE: &str = include_str!("page.html");
const MAX_BODY: usize = 64 * 1024;

/// `hosts`: the `Host` values the bench answers to.
pub async fn serve(listener: TcpListener, hosts: Vec<String>, call: Arc<Call>) -> Result<()> {
    let hosts = Arc::new(hosts);
    loop {
        let (stream, _) = listener.accept().await?;
        let (call, hosts) = (call.clone(), hosts.clone());
        tokio::spawn(async move {
            let _ = handle(stream, &hosts, &call).await;
        });
    }
}

struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    host: String,
    json: bool,
    body: Vec<u8>,
}

async fn read_request(stream: &mut TcpStream) -> Result<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let target = parts.next().unwrap_or("").to_owned();
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let query = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(name, value)| (decode(name), decode(value)))
        .collect();
    let (mut host, mut json, mut length) = (String::new(), false, 0usize);
    for _ in 0..100 {
        let mut header = String::new();
        if reader.read_line(&mut header).await? == 0 || header == "\r\n" {
            break;
        }
        let (name, value) = header.split_once(':').unwrap_or((&header, ""));
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "host" => host = value.to_owned(),
            "content-type" => json = value.to_ascii_lowercase().starts_with("application/json"),
            "content-length" => length = value.parse().unwrap_or(usize::MAX),
            _ => {}
        }
    }
    let mut body = vec![0; length.min(MAX_BODY)];
    if length <= MAX_BODY {
        reader.read_exact(&mut body).await?;
    }
    Ok(Request {
        method,
        path: path.to_owned(),
        query,
        host,
        json,
        body: if length <= MAX_BODY { body } else { Vec::new() },
    })
}

async fn handle(mut stream: TcpStream, hosts: &[String], call: &Call) -> Result<()> {
    let request = read_request(&mut stream).await?;
    let own_host = hosts.contains(&request.host);
    let param = |name: &str| {
        request
            .query
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    let (status, kind, body) = if !own_host {
        (421, "text/plain", "Misdirected request\n".to_owned())
    } else {
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/") => (200, "text/html; charset=utf-8", PAGE.to_owned()),
            ("GET", "/api/state") => {
                let since = param("since")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
                (200, "application/json", call.state(since).await.to_string())
            }
            ("GET", "/api/history") => {
                answer(call.history(&param("thread").unwrap_or_default()).await)
            }
            ("POST", "/api/say") if request.json => {
                let said: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
                match (said["thread"].as_str(), said["text"].as_str()) {
                    (Some(thread), Some(text)) if !text.trim().is_empty() => {
                        answer(call.say(thread, text).await)
                    }
                    _ => (
                        400,
                        "application/json",
                        json!({"error": "thread and text are required"}).to_string(),
                    ),
                }
            }
            ("POST", _) => (415, "text/plain", "JSON only\n".to_owned()),
            _ => (404, "text/plain", "Not found\n".to_owned()),
        }
    };
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nConnection: close\r\n\r\n",
        if status == 200 { "OK" } else { "Error" },
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    Ok(())
}

fn answer(result: Result<Value>) -> (u16, &'static str, String) {
    match result {
        Ok(value) => (200, "application/json", value.to_string()),
        Err(error) => (
            502,
            "application/json",
            json!({"error": format!("{error:#}")}).to_string(),
        ),
    }
}

/// A percent-encoded query value (`+` is a space).
fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => out.push(b' '),
            b'%' => match bytes
                .get(index + 1..index + 3)
                .and_then(|hex| std::str::from_utf8(hex).ok())
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            {
                Some(byte) => {
                    out.push(byte);
                    index += 2;
                }
                None => out.push(b'%'),
            },
            byte => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::encode;

    #[test]
    fn a_query_value_round_trips() {
        for value in ["plain", "with space/and?&=", "019a-thread", "ñ"] {
            assert_eq!(decode(&encode(value)), value);
        }
        assert_eq!(decode("a+b%2"), "a b%2");
    }

    /// The page reaches its API relative to where it is served, so it works the same under a prefix (the
    /// playground serves it at /connector/, through a proxy).
    #[test]
    fn the_page_names_its_api_relative_to_itself() {
        assert!(
            PAGE.contains("api/state") && PAGE.contains("api/history") && PAGE.contains("api/say")
        );
        assert!(
            !PAGE.contains("\"/api/") && !PAGE.contains("`/api/"),
            "an absolute /api/ URL"
        );
    }
}
