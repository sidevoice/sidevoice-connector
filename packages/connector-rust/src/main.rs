mod adapters;
mod agents;
mod core_package;
mod core_ready;
mod cursor_app;
mod daemon;
mod identity;
mod install;
mod link;
mod lock;
mod logfile;
mod mcp;
mod messages;
mod pairing;
mod process;
mod profile;
mod secure_fs;
mod service;

use anyhow::{bail, Result};
use clap::{CommandFactory, Parser, Subcommand};
use messages::Keyed;
use profile::Profile;
use serde_json::{json, Value};
use std::process::ExitCode;
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

/// The command line is English only and is not translated. With `--json`, a command prints one JSON object on
/// stdout, and a failure is `{"ok":false,"error":{"key","params","message"}}` with exit status 1.
#[derive(Parser)]
#[command(
    name = "sidevoice-connector",
    about = "Sidevoice connector: links this machine's agent conversations to the Sidevoice core",
    disable_version_flag = true
)]
struct Cli {
    /// Print this build's version (with --json: version, target and source commit).
    #[arg(short = 'V', long)]
    version: bool,
    /// Print one JSON object instead of text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Option<Action>,
}

#[derive(Subcommand)]
enum Action {
    /// Install this package's Sidevoice on this computer (or update to it), run it, and register it with the agents
    /// found here.
    Install {
        /// Register with no agent (connect them later with `agents connect`).
        #[arg(long)]
        no_agents: bool,
    },
    /// Remove Sidevoice from this computer: its service, its agent registrations, its releases and its data.
    Uninstall,
    /// Serve MCP over stdio for one agent conversation.
    Mcp,
    /// Run this machine's connector daemon; --service when a service manager runs it.
    Connector {
        #[arg(long)]
        service: bool,
    },
    /// List the agents on this computer and whether they use Sidevoice, or change one's registration.
    Agents {
        #[command(subcommand)]
        action: Option<AgentsAction>,
    },
    /// Run Sidevoice at login (launchd, systemd --user), or stop, restart or report it.
    Service {
        #[command(subcommand)]
        command: ServiceAction,
    },
    /// Pair this machine with a room, with the one-time code the room shows for pairing a machine.
    Pair {
        /// The room's address (https://…).
        room: String,
        /// The one-time code the room shows.
        code: String,
    },
    /// Show a one-time code (and its QR) to pair a device, such as the Sidevoice app, with this machine.
    PairDevice,
    /// The installer's step for the core: stage the core this package carries into RELEASE/core and run its
    /// self-test there.
    #[command(hide = true)]
    StageCore { release: std::path::PathBuf },
}

#[derive(Subcommand)]
enum AgentsAction {
    /// Register Sidevoice with the agent.
    Connect { id: String },
    /// Remove Sidevoice's registration from the agent.
    Disconnect { id: String },
    /// Dismiss the new-agent notice for the agent.
    Dismiss { id: String },
}

#[derive(Subcommand)]
enum ServiceAction {
    Install,
    Start,
    Stop,
    Restart,
    Status,
    Uninstall,
}

/// A command line that does not parse, asked for `--json`: said as the keyed failure, not as clap's text. Help and
/// version requests, and any error without `--json`, are left to clap.
fn usage_refusal(error: &clap::Error, args: impl Iterator<Item = String>) -> Option<Value> {
    use clap::error::ErrorKind;
    if matches!(
        error.kind(),
        ErrorKind::DisplayHelp
            | ErrorKind::DisplayVersion
            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    ) || !args.into_iter().any(|arg| arg == "--json")
    {
        return None;
    }
    let text = error.kind().as_str().unwrap_or("invalid command line");
    Some(Keyed::new("connector.usage", json!({"detail":text})).value())
}

/// What `--version` says: the version alone, or with `--json` the build's identity.
fn version(json: bool) -> String {
    if json {
        json!({"ok":true,"version":identity::VERSION,"target":env!("SIDEVOICE_CONNECTOR_TARGET"),
            "source_sha":env!("SIDEVOICE_CONNECTOR_BUILD_SHA")})
        .to_string()
    } else {
        format!("sidevoice-connector {}", identity::VERSION)
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            if let Some(refusal) = usage_refusal(&error, std::env::args()) {
                println!("{refusal}");
                return ExitCode::FAILURE;
            }
            error.exit();
        }
    };
    if cli.version {
        println!("{}", version(cli.json));
        return ExitCode::SUCCESS;
    }
    let Some(action) = cli.command else {
        let _ = Cli::command().print_help();
        return ExitCode::from(2);
    };
    match run(action, cli.json).await {
        Ok(code) => code,
        Err(error) => {
            // A refusal from the service layer keeps its own key; anything else is keyed here.
            let (value, message) = match error.downcast_ref::<service::Failure>() {
                Some(failure) => (failure.value(), failure.message()),
                None => {
                    let keyed = messages::keyed(&error);
                    (keyed.value(), keyed.message())
                }
            };
            if cli.json {
                println!("{value}");
            } else {
                eprintln!("sidevoice: {message}");
            }
            ExitCode::FAILURE
        }
    }
}

async fn run(action: Action, json: bool) -> Result<ExitCode> {
    // The service commands act on the installation the environment names, whatever binary runs them.
    if let Action::Service { command } = &action {
        let action = match command {
            ServiceAction::Install => service::Action::Install,
            ServiceAction::Start => service::Action::Start,
            ServiceAction::Stop => service::Action::Stop,
            ServiceAction::Restart => service::Action::Restart,
            ServiceAction::Status => service::Action::Status,
            ServiceAction::Uninstall => service::Action::Uninstall,
        };
        let answer = service::run(action).await;
        let ok = answer.get("ok") == Some(&Value::Bool(true));
        if json {
            println!("{answer}");
        } else if ok {
            println!("{}", service::human(&answer));
        } else {
            eprintln!("sidevoice: {}", service::human(&answer));
        }
        return Ok(if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }
    if let Action::StageCore { release } = &action {
        let installed = core_package::install(&core_package::package_root()?, release)?;
        if json {
            println!("{}", installed.value());
        } else {
            println!(
                "Installed the Sidevoice core {} in {}; its self-test passed.",
                installed.core.version,
                installed.path.display()
            );
        }
        return Ok(ExitCode::SUCCESS);
    }
    let profile = Profile::from_env()?;
    match action {
        Action::Install { no_agents } => {
            let package = install::Package::running()?;
            if !json {
                println!("Installing Sidevoice {}…", package.version);
            }
            let options = install::Options { agents: !no_agents };
            let answer =
                install::install(&profile, &package, options, &install::Runtime::Machine).await?;
            if json {
                println!("{answer}");
            } else {
                println!("{}", install::human_install(&answer));
            }
            Ok(ExitCode::SUCCESS)
        }
        Action::Uninstall => {
            let options = install::Options { agents: true };
            let answer = install::uninstall(&profile, options, &install::Runtime::Machine).await?;
            if json {
                println!("{answer}");
            } else {
                println!("{}", install::human_uninstall(&answer));
            }
            Ok(ExitCode::SUCCESS)
        }
        Action::Mcp => mcp::run(profile).await.map(|()| ExitCode::SUCCESS),
        Action::Connector { service } => {
            let managed = service
                || matches!(
                    std::env::var("SIDEVOICE_SERVICE").as_deref(),
                    Ok("launchd" | "systemd")
                );
            daemon::run(profile, managed)
                .await
                .map(|()| ExitCode::SUCCESS)
        }
        Action::Agents { action } => agents_command(profile, action, json).await,
        Action::Pair { room, code } => {
            let paired = pairing::pair(&profile, &room, &code).await?;
            if json {
                println!(
                    "{}",
                    json!({"ok":true,"room":paired.origin,"connector_id":paired.connector_id})
                );
            } else {
                println!(
                    "Paired with {} as connector {}; credential saved to {}",
                    paired.origin,
                    paired.connector_id,
                    paired.file.display()
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Action::PairDevice => {
            let answer = pairing::device_code(&profile).await?;
            if json {
                println!("{}", pairing::device_json(&answer));
            } else {
                println!("{}", pairing::device_text(&answer)?);
            }
            Ok(ExitCode::SUCCESS)
        }
        Action::Service { .. } => unreachable!("service commands run before the profile"),
        Action::StageCore { .. } => unreachable!("the core is staged before the profile"),
    }
}

/// `agents [--json]`, `agents connect|disconnect|dismiss <id> [--json]`.
async fn agents_command(
    profile: Profile,
    action: Option<AgentsAction>,
    json: bool,
) -> Result<ExitCode> {
    let host_agents = agents::HostAgents::new(profile)?;
    let (method, id, done) = match &action {
        None => ("agents.list", None, ""),
        Some(AgentsAction::Connect { id }) => (
            "agents.connect",
            Some(id),
            "Connected {agent} to Sidevoice.",
        ),
        Some(AgentsAction::Disconnect { id }) => (
            "agents.disconnect",
            Some(id),
            "Disconnected Sidevoice from {agent}.",
        ),
        Some(AgentsAction::Dismiss { id }) => (
            "agents.dismiss",
            Some(id),
            "Dismissed the new-agent notice for {agent}.",
        ),
    };
    let params = match id {
        Some(id) => json!({"id":id}),
        None => json!({"rescan":true}),
    };
    let mut answer = host_agents.handle(method, params).await;
    if let Some(error) = answer.get("error") {
        let key = error.get("key").and_then(Value::as_str).unwrap_or("");
        let params = error.get("params").cloned().unwrap_or(json!({}));
        if json {
            println!("{}", json!({"ok":false,"error":error}));
        } else {
            eprintln!("sidevoice: {}", messages::message(key, &params));
        }
        return Ok(ExitCode::FAILURE);
    }
    if json {
        answer["ok"] = json!(true);
        println!("{answer}");
        return Ok(ExitCode::SUCCESS);
    }
    let rows = answer
        .get("agents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let label = |id: &str| {
        rows.iter()
            .find(|row| row["id"] == id)
            .and_then(|row| row["label"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| messages::message(&format!("harness.{id}"), &json!({})))
    };
    match id {
        Some(id) => println!("{}", done.replace("{agent}", &label(id))),
        None if rows.is_empty() => println!("No supported agents were found on this computer."),
        None => {
            for row in &rows {
                let state = match row["registration"].as_str() {
                    Some("connected") => "connected to Sidevoice",
                    Some("not-connected") => "not connected",
                    Some("foreign") => "another Sidevoice registration is present",
                    Some("invalid") => "its Sidevoice registration is unreadable",
                    _ => "registration state unknown",
                };
                let version = row["version"].as_str().unwrap_or("version unknown");
                println!(
                    "{}: {state} ({version})",
                    row["label"].as_str().unwrap_or("?")
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_the_release_version_and_its_json_names_the_build() {
        assert_eq!(
            version(false),
            format!("sidevoice-connector {}", identity::VERSION)
        );
        let value: Value = serde_json::from_str(&version(true)).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["version"], identity::VERSION);
        assert_eq!(value["target"], env!("SIDEVOICE_CONNECTOR_TARGET"));
        assert_eq!(value["source_sha"], env!("SIDEVOICE_CONNECTOR_BUILD_SHA"));
    }

    #[test]
    fn the_command_line_parses_and_names_no_proof_mode() {
        Cli::command().debug_assert();
        assert!(Cli::try_parse_from(["x", "--profile-root", "/tmp/p", "mcp"]).is_err());
        assert!(Cli::try_parse_from(["x", "codex", "inspect"]).is_err());
        assert!(Cli::try_parse_from(["x", "runtime-identity"]).is_err());
        let cli = Cli::try_parse_from(["x", "agents", "connect", "codex", "--json"]).unwrap();
        assert!(cli.json);
        let cli = Cli::try_parse_from(["x", "--version", "--json"]).unwrap();
        assert!(cli.version && cli.json);
        let error = Cli::try_parse_from(["x", "agents", "frobnicate", "--json"])
            .err()
            .unwrap();
        let args = || {
            ["x", "agents", "frobnicate", "--json"]
                .map(String::from)
                .into_iter()
        };
        let refusal = usage_refusal(&error, args()).unwrap();
        assert_eq!(refusal["ok"], false);
        assert_eq!(refusal["error"]["key"], "connector.usage");
        assert!(usage_refusal(
            &error,
            ["x", "agents", "frobnicate"].map(String::from).into_iter()
        )
        .is_none());
        let help = Cli::command().render_long_help().to_string();
        assert!(!help.to_lowercase().contains("proof"), "{help}");
    }
}
