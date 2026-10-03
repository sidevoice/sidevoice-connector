mod daemon;
mod link;
mod mcp;
mod proof;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use proof::Profile;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use tokio::process::Command;

#[derive(Parser)]
#[command(name = "sidevoice-rust-proof", version, about = "Isolated Codex connector proof")]
struct Cli { #[command(subcommand)] command: Action }

#[derive(Subcommand)]
enum Action {
    /// Serve MCP over stdio for one isolated Codex conversation.
    Mcp,
    /// Link the isolated Core to Codex sessions; never launches Core or a service.
    Connector,
    /// Inspect or explicitly register only this isolated Codex profile.
    Codex { #[command(subcommand)] command: CodexAction },
}

#[derive(Subcommand)]
enum CodexAction { Inspect, Connect }

fn codex_binary() -> String { std::env::var("SIDEVOICE_CODEX_BIN").unwrap_or_else(|_| "codex".into()) }

async fn codex_state(profile: &Profile) -> Result<Value> {
    let config = profile.codex.join("config.toml");
    let parsed = if config.exists() {
        let text = fs::read_to_string(&config)?;
        match toml::from_str::<toml::Value>(&text) { Ok(value) => Some(value), Err(_) => return Ok(json!({"state":"invalid"})) }
    } else { None };
    let has_entry = parsed.as_ref().and_then(|v| v.get("mcp_servers")).and_then(|v| v.get("sidevoice")).is_some();
    let output = Command::new(codex_binary()).env("CODEX_HOME", &profile.codex).args(["mcp", "get", "sidevoice", "--json"]).output().await?;
    if !output.status.success() { return Ok(json!({"state":if has_entry {"invalid"} else {"absent"}})); }
    let entry: Value = match serde_json::from_slice(&output.stdout) { Ok(v) => v, Err(_) => return Ok(json!({"state":"invalid"})) };
    let command = entry.pointer("/transport/command").and_then(Value::as_str).unwrap_or("");
    let args = entry.pointer("/transport/args").and_then(Value::as_array);
    let env = entry.pointer("/transport/env");
    let exe = std::env::current_exe()?.canonicalize()?;
    let ours = Path::new(command).canonicalize().ok().as_deref() == Some(exe.as_path()) &&
        args.is_some_and(|v| v.len() == 1 && v[0] == "mcp") &&
        env.and_then(|v| v.get("SIDEVOICE_DATA_DIR")).and_then(Value::as_str) == Some(profile.data.to_str().context("non-UTF8 data path")?) &&
        env.and_then(|v| v.get("CODEX_HOME")).and_then(Value::as_str) == Some(profile.codex.to_str().context("non-UTF8 Codex path")?);
    Ok(json!({"state":if ours {"ours"} else {"foreign"},"enabled":entry.get("enabled")}))
}

async fn codex_connect(profile: &Profile) -> Result<Value> {
    let state = codex_state(profile).await?;
    match state.get("state").and_then(Value::as_str) {
        Some("ours") => return Ok(state),
        Some("absent") => {},
        Some("foreign") | Some("invalid") => bail!("Codex sidevoice entry is foreign or invalid; explicit manual resolution required"),
        _ => bail!("Codex registration state unknown"),
    }
    let exe = std::env::current_exe()?.canonicalize()?;
    let output = Command::new(codex_binary()).env("CODEX_HOME", &profile.codex)
        .args(["mcp", "add", "sidevoice", "--env"])
        .arg(format!("SIDEVOICE_DATA_DIR={}", profile.data.display()))
        .arg("--env").arg(format!("CODEX_HOME={}", profile.codex.display()))
        .arg("--").arg(&exe).arg("mcp").output().await?;
    if !output.status.success() { bail!("Codex refused isolated MCP registration"); }
    let after = codex_state(profile).await?;
    if after.get("state") != Some(&json!("ours")) { bail!("Codex registration could not be verified as owned"); }
    Ok(after)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let profile = Profile::from_env()?;
    match cli.command {
        Action::Mcp => mcp::run(profile).await,
        Action::Connector => daemon::run(profile).await,
        Action::Codex { command } => {
            let value = match command { CodexAction::Inspect => codex_state(&profile).await?, CodexAction::Connect => codex_connect(&profile).await? };
            println!("{}", value);
            Ok(())
        }
    }
}
