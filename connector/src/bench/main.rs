//! `sidevoice-bench`: the connector on this machine without the desktop app or a room, to talk to real agent
//! sessions (Claude Code, Codex) by typing. A developer tool: it is not shipped in the release archive.
//!
//! It makes a private profile (every directory the connector reads, under one root), installs this build there with
//! the pinned core as a person's machine without a service manager has it (`install --no-agents`, Sidevoice on
//! demand), pairs a local device with that core and holds a call open as the app would, and serves a page on
//! loopback: the conversations (bindings), what was said into each and its receipt (pending, delivered, read…), the
//! agent's `voice_say` replies and the call's events (working state among them). What is typed there is voice input,
//! exactly as if it had been spoken. How to point an agent at it: connector/README.md, "Test bench".

mod call;
mod page;
mod setup;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;

/// Drive the connector and the pinned core from a local page, without the desktop app or a room.
#[derive(Parser)]
#[command(name = "sidevoice-bench", version)]
struct Args {
    /// The bench's private profile; kept between runs (agents' logins live in it).
    #[arg(long, default_value_os_t = default_profile())]
    profile: PathBuf,
    /// The page's port on 127.0.0.1 (0: any free port).
    #[arg(long, default_value_t = 4477)]
    port: u16,
    /// Where `cargo xtask core` put the pinned core (its `core.json` and archive).
    #[arg(long, default_value_os_t = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../target/sidevoice-core")))]
    core: PathBuf,
    /// Answer also to this `Host` (repeatable): the name a tunnel to the page forwards (README, "Test bench").
    #[arg(long = "allow-host", value_name = "HOST")]
    allow_hosts: Vec<String>,
    /// Uninstall from the profile and delete it before starting.
    #[arg(long)]
    reset: bool,
    /// Uninstall from the profile (stopping its connector and core) and exit.
    #[arg(long)]
    uninstall: bool,
}

fn default_profile() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".sidevoice-bench")
}

#[tokio::main]
async fn main() {
    if let Err(error) = run(Args::parse()).await {
        eprintln!("sidevoice-bench: {error:#}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<()> {
    let profile = setup::Profile::new(&args.profile)?;
    if args.reset || args.uninstall {
        profile.uninstall();
        if args.uninstall {
            return Ok(());
        }
        profile.remove()?;
    }
    let profile = setup::Profile::new(&args.profile)?;
    eprintln!(
        "Installing this build with the pinned core into {} …",
        profile.root.display()
    );
    let installed = profile.install(&args.core)?;
    eprintln!("Installed {}.", installed["version"]);
    let command = profile.installed_command()?;
    profile.write_agent_files(&command)?;
    let call = Arc::new(call::Call::new(profile.clone(), command.clone()));
    tokio::spawn(call.clone().keep_open());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", args.port)).await?;
    let port = listener.local_addr()?.port();
    println!("{}", profile.instructions(&command, port));
    // The last line, which the bench's test waits for.
    println!("Bench ready: http://127.0.0.1:{port}/");
    let mut hosts = vec![format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    hosts.extend(args.allow_hosts);
    page::serve(listener, hosts, call).await
}
