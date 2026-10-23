//! The agents' CLIs and the MCP server these tests stand in for, played by this test binary
//! (`crate::test_support::stand_in`): the same parts on every OS.

use crate::platform::env::{Env, Environment};
use crate::platform::fs::{Files, TestFiles};
use crate::test_support::stand_in;
use serde_json::{json, Map, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Makes `program` play `play` (one of the parts below).
pub(super) fn make(program: &Path, play: &str) {
    stand_in::make(program, &json!({ "play": play }));
}

#[ctor::ctor(unsafe)]
fn play_agents() {
    stand_in::serve(|name, _, args| match name {
        "codex" => Some(codex(args)),
        "codex-gate" => Some(codex_gate(args)),
        "claude" => Some(claude(args)),
        "mcp-server" => Some(mcp_server(args)),
        _ => None,
    });
}

/// `$name`, else `$HOME/<fallback>`.
fn dir_or_home(name: &str, fallback: &str) -> PathBuf {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os(Environment::HOME).unwrap()).join(fallback)
        })
}

fn append(file: &Path, line: &str) {
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .unwrap();
    writeln!(out, "{line}").unwrap();
}

fn unsupported() -> i32 {
    eprintln!("unsupported fixture command");
    2
}

/// The `--env`/`-e` pairs before `--` (named by `flag`), and the command and arguments after it.
fn entry(args: &[String], flag: &str) -> Option<(Map<String, Value>, String, Vec<String>)> {
    let mut env = Map::new();
    let mut rest = args.iter();
    loop {
        match rest.next()?.as_str() {
            "--" => break,
            word if word == flag => {
                let pair = rest.next()?;
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                env.insert(key.to_owned(), value.into());
            }
            _ => {}
        }
    }
    let command = rest.next()?.clone();
    Some((env, command, rest.cloned().collect()))
}

/// The arguments as one word, as `"$*"` in a script.
fn joined(args: &[String]) -> String {
    args.join(" ")
}

fn words(args: &[String], count: usize) -> String {
    joined(&args[..count.min(args.len())])
}

/// A Codex CLI that keeps one `sidevoice` entry in `$CODEX_HOME` as the real one shows it (`mcp get --json`, with
/// `transport.env`), and logs every call to `calls` there.
fn codex(args: &[String]) -> i32 {
    let home = dir_or_home("CODEX_HOME", ".codex");
    let entry_file = home.join("fake-entry.json");
    append(&home.join("calls"), &joined(args));
    match words(args, 3).as_str() {
        "mcp get sidevoice" => match std::fs::read(&entry_file) {
            Ok(bytes) => {
                std::io::stdout().write_all(&bytes).unwrap();
                0
            }
            Err(_) => {
                eprintln!("No such server: sidevoice");
                1
            }
        },
        "mcp remove sidevoice" => {
            let _ = std::fs::remove_file(&entry_file);
            let _ = std::fs::remove_file(home.join("config.toml"));
            0
        }
        "mcp add sidevoice" => {
            let Some((env, command, rest)) = entry(&args[3..], "--env") else {
                return unsupported();
            };
            let rest = joined(&rest);
            let value = json!({"name":"sidevoice","enabled":true,
                "transport":{"type":"stdio","command":command,"args":[rest],"env":env}});
            std::fs::write(&entry_file, value.to_string()).unwrap();
            std::fs::write(
                home.join("config.toml"),
                format!(
                    "[mcp_servers.sidevoice]\ncommand = {}\nargs = [{}]\n",
                    json!(command),
                    json!(rest)
                ),
            )
            .unwrap();
            0
        }
        _ => unsupported(),
    }
}

/// The Codex CLI of the request-gate test: its `mcp get` and `mcp add` can be made slow, or time out, or the add left
/// without effect, by marker files in `$CODEX_HOME` that each affect the next call only.
fn codex_gate(args: &[String]) -> i32 {
    let home = PathBuf::from(std::env::var_os("CODEX_HOME").unwrap());
    let state = home.join("fake-entry.json");
    let config = home.join("config.toml");
    // Whether the marker `name` was there (and is now gone).
    let take = |name: &str| std::fs::remove_file(home.join(name)).is_ok();
    let pid = std::process::id().to_string();
    if args == ["--version"] {
        println!("codex fixture 1");
        return 0;
    }
    match words(args, 3).as_str() {
        "mcp get sidevoice" => {
            if take("timeout-next-get") {
                std::thread::sleep(Duration::from_secs(4));
            }
            if take("slow-next-get") {
                std::fs::write(home.join("slow-get-started"), &pid).unwrap();
                std::thread::sleep(Duration::from_millis(800));
            }
            match std::fs::read(&state) {
                Ok(bytes) => {
                    std::io::stdout().write_all(&bytes).unwrap();
                    0
                }
                Err(_) => {
                    eprintln!("No such server: sidevoice");
                    1
                }
            }
        }
        "mcp remove sidevoice" => {
            let _ = std::fs::remove_file(&state);
            let _ = std::fs::remove_file(&config);
            if take("timeout-after-remove") {
                std::fs::write(home.join("timeout-next-get"), "1").unwrap();
            }
            0
        }
        "mcp add sidevoice" => {
            let Some((env, command, rest)) = entry(&args[3..], "--env") else {
                return unsupported();
            };
            if take("leave-unregistered-after-add") {
                return 0;
            }
            if take("slow-next-add") {
                std::fs::write(home.join("slow-add-started"), &pid).unwrap();
                std::thread::sleep(Duration::from_millis(800));
            }
            let value = json!({"name":"sidevoice","transport":{"type":"stdio","command":command,
                "args":rest,"env":env},"enabled":true});
            std::fs::write(&state, value.to_string()).unwrap();
            let listed: Vec<String> = rest.iter().map(|arg| json!(arg).to_string()).collect();
            std::fs::write(
                &config,
                format!(
                    "[mcp_servers.sidevoice]\ncommand = {}\nargs = [{}]\n",
                    json!(command),
                    listed.join(", ")
                ),
            )
            .unwrap();
            0
        }
        _ => unsupported(),
    }
}

/// A Claude Code CLI that keeps its user-scope MCP servers where the real one does: `$CLAUDE_CONFIG_DIR/.claude.json`
/// when the variable is set, else `~/.claude.json`. It logs every call to its data directory (`$CLAUDE_CONFIG_DIR`,
/// else `~/.claude`). Like the real one, `mcp get` starts the server the entry names (its command is kept in the data
/// directory, `fake-command`) to report whether it connects.
fn claude(args: &[String]) -> i32 {
    let home = dir_or_home("CLAUDE_CONFIG_DIR", ".claude");
    let settings = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os(Environment::HOME).unwrap()))
        .join(".claude.json");
    let command_file = home.join("fake-command");
    append(&home.join("calls"), &joined(args));
    match words(args, 2).as_str() {
        "mcp get" => {
            if let Ok(command) = std::fs::read_to_string(&command_file) {
                let _ = std::process::Command::new(command)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
            match std::fs::read(&settings) {
                Ok(bytes) => {
                    std::io::stdout().write_all(&bytes).unwrap();
                    0
                }
                Err(_) => {
                    eprintln!("No MCP server named \"sidevoice\" found.");
                    1
                }
            }
        }
        "mcp remove" => {
            let _ = std::fs::remove_file(&command_file);
            std::fs::write(&settings, "{\"mcpServers\":{}}\n").unwrap();
            0
        }
        "mcp add" => {
            if std::fs::read_to_string(&settings).is_ok_and(|text| text.contains("\"sidevoice\"")) {
                eprintln!("MCP server sidevoice already exists in user config");
                return 1;
            }
            if home.join("fail-add").exists() {
                eprintln!("add failed");
                return 1;
            }
            let Some((env, command, rest)) = entry(args.get(5..).unwrap_or_default(), "-e") else {
                return unsupported();
            };
            std::fs::write(&command_file, &command).unwrap();
            let value = json!({"numStartups":3,"mcpServers":{"sidevoice":{"type":"stdio",
                "command":command,"args":[joined(&rest)],"env":env}}});
            std::fs::write(&settings, format!("{value}\n")).unwrap();
            Files::shut_others_out(&settings);
            0
        }
        _ => unsupported(),
    }
}

/// This installation's `mcp` server: it answers the handshake and logs, to the data directory its entry names, the
/// health-check marker and its arguments.
fn mcp_server(args: &[String]) -> i32 {
    let mut lines = std::io::stdin().lock().lines();
    let _initialize = lines.next();
    let data = PathBuf::from(std::env::var_os("SIDEVOICE_DATA_DIR").unwrap());
    let marker = std::env::var("SIDEVOICE_HEALTH_CHECK").unwrap_or_default();
    append(
        &data.join("health-calls"),
        &format!("{marker} {}", joined(args)),
    );
    println!(
        r#"{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"2025-11-25","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"fake","version":"1"}}}}}}"#
    );
    std::io::stdout().flush().unwrap();
    let _initialized = lines.next();
    let _list = lines.next();
    println!(r#"{{"jsonrpc":"2.0","id":2,"result":{{"tools":[]}}}}"#);
    std::io::stdout().flush().unwrap();
    std::thread::sleep(Duration::from_secs(30));
    0
}
