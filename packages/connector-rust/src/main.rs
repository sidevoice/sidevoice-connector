mod adapters;
mod agents;
mod cursor_app;
mod daemon;
mod link;
mod mcp;
mod pairing;
mod proof;
mod service;

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
    about = "Isolated Rust connector proof"
)]
struct Cli {
    /// Reconstruct the complete isolated profile in a fresh process.
    #[arg(long, global = true)]
    profile_root: Option<PathBuf>,
    /// Use the currently selected production release and the user's normal profile paths.
    #[arg(long, global = true)]
    installed: bool,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Report this target binary's build identity for the trusted SEA packager.
    RuntimeIdentity {
        #[arg(long)]
        json: bool,
    },
    /// Serve MCP over stdio for one supported conversation adapter.
    Mcp,
    /// Link the isolated Core to Codex sessions; --service is for its private launchd job.
    Connector {
        #[arg(long)]
        service: bool,
    },
    /// Inspect or explicitly register only this isolated Codex profile.
    Codex {
        #[command(subcommand)]
        command: CodexAction,
    },
    /// Manage this proof profile's private launchd service.
    Service {
        #[command(subcommand)]
        command: ServiceAction,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    Install {
        #[arg(long)]
        json: bool,
    },
    Start {
        #[arg(long)]
        json: bool,
    },
    Stop {
        #[arg(long)]
        json: bool,
    },
    Restart {
        #[arg(long)]
        json: bool,
    },
    Status {
        #[arg(long)]
        json: bool,
    },
    Uninstall {
        #[arg(long)]
        json: bool,
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
    if let Action::RuntimeIdentity { json } = &cli.command {
        let identity = json!({"kind":"rust-native-v1", "target":env!("SIDEVOICE_CONNECTOR_TARGET"),
            "source_sha":env!("SIDEVOICE_CONNECTOR_BUILD_SHA"), "version":env!("SIDEVOICE_CONNECTOR_VERSION")});
        if *json {
            println!("{identity}");
        } else {
            println!(
                "Rust Connector {} ({})",
                identity["version"], identity["target"]
            );
        }
        return Ok(());
    }
    let explicit_profile = cli.profile_root.is_some();
    if explicit_profile && cli.installed {
        bail!("proof and installed profiles cannot be combined");
    }
    let profile = match cli.profile_root {
        Some(root) => Profile::from_root(&root)?,
        None if cli.installed => Profile::from_installed_env()?,
        None => Profile::from_env()?,
    };
    match cli.command {
        Action::RuntimeIdentity { .. } => {
            unreachable!("runtime identity returned before profile setup")
        }
        Action::Mcp => mcp::run(profile).await,
        Action::Connector { service } => {
            if service && (cli.installed || !explicit_profile) {
                eprintln!(
                    "{}",
                    crate::agents::message(
                        "service.proof-profile-required",
                        &serde_json::Value::Null
                    )
                );
                std::process::exit(1);
            }
            let managed = if profile.is_installed() {
                matches!(
                    std::env::var("SIDEVOICE_SERVICE").as_deref(),
                    Ok("launchd" | "systemd")
                )
            } else {
                service
            };
            daemon::run(profile, managed).await
        }
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
        Action::Service { command } => {
            if !explicit_profile {
                let answer = service::proof_profile_required();
                println!("{answer}");
                std::process::exit(1);
            }
            let action = match command {
                ServiceAction::Install { .. } => service::Action::Install,
                ServiceAction::Start { .. } => service::Action::Start,
                ServiceAction::Stop { .. } => service::Action::Stop,
                ServiceAction::Restart { .. } => service::Action::Restart,
                ServiceAction::Status { .. } => service::Action::Status,
                ServiceAction::Uninstall { .. } => service::Action::Uninstall,
            };
            let answer = service::run(profile, action).await;
            let failed = answer.get("ok") != Some(&serde_json::Value::Bool(true));
            println!("{answer}");
            if failed {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}
