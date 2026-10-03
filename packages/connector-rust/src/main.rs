mod agents;
mod daemon;
mod link;
mod mcp;
mod proof;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use proof::Profile;
use serde_json::json;
use std::path::PathBuf;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

async fn bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<Option<String>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            bail!("truncated IPC frame");
        }
        let length = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if line.len() + length > limit {
            bail!("IPC frame too large");
        }
        line.extend_from_slice(&available[..length]);
        reader.consume(length);
        if line.last() == Some(&b'\n') {
            line.pop();
            return Ok(Some(String::from_utf8(line)?));
        }
    }
}

#[derive(Parser)]
#[command(
    name = "sidevoice-rust-proof",
    version,
    about = "Isolated Codex connector proof"
)]
struct Cli {
    /// Reconstruct the complete isolated profile in a fresh process.
    #[arg(long, global = true)]
    profile_root: Option<PathBuf>,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Serve MCP over stdio for one isolated Codex conversation.
    Mcp,
    /// Link the isolated Core to Codex sessions; never launches Core or a service.
    Connector,
    /// Inspect or explicitly register only this isolated Codex profile.
    Codex {
        #[command(subcommand)]
        command: CodexAction,
    },
}

#[derive(Subcommand)]
enum CodexAction {
    Inspect,
    Connect,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let profile = match cli.profile_root {
        Some(root) => Profile::from_root(&root)?,
        None => Profile::from_env()?,
    };
    match cli.command {
        Action::Mcp => mcp::run(profile).await,
        Action::Connector => daemon::run(profile).await,
        Action::Codex { command } => {
            let host_agents = agents::HostAgents::new(profile.clone())?;
            let _connector_lock = profile.try_connector_lock()?;
            let (method, params) = match command {
                CodexAction::Inspect => ("agents.list", json!({"rescan":true,"watch":"codex"})),
                CodexAction::Connect => ("agents.connect", json!({"id":"codex"})),
            };
            let answer = host_agents.handle(method, params).await;
            let failed = answer.get("error").is_some();
            println!("{answer}");
            if failed {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}
