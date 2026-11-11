//! What the test bench asks the connector about the agents (`bench-agents`, a separate binary that cannot read the
//! harnesses itself): where each agent keeps its home below the bench, which of their CLIs this `PATH` finds, and how
//! to start each one against the bench's profile. Test tooling, by harness id; nothing here is used in production.

use super::registration::{shell_command, strings, Programs};
use crate::platform::env::{Env, Environment};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// The test bench's profile, as an agent is pointed at it: what [`steps`] writes them with.
pub struct BenchSetup<'a> {
    /// The script to source before starting the agent.
    pub env_script: &'a str,
    /// An `mcpServers` configuration file naming the installation.
    pub mcp_config: &'a str,
    /// The installation's MCP server command, quoted for a shell.
    pub server: &'a str,
    /// The environment the server runs with.
    pub env: &'a BTreeMap<String, String>,
}

impl BenchSetup<'_> {
    /// One line that sources [`Self::env_script`] (`. 'script'`, read alike by a POSIX shell and PowerShell), then
    /// runs `command args`, then `tail` (shell text as given, e.g. [`Self::server`]) after it: in sequence
    /// ([`Env::in_sequence`]), the command only once the script was read.
    pub fn line(&self, command: &str, args: &[String], tail: Option<&str>) -> String {
        let source = format!(". {}", Environment::quote(self.env_script));
        let mut run = shell_command(command, args);
        if let Some(tail) = tail {
            run.push(' ');
            run.push_str(tail);
        }
        Environment::in_sequence(&[&source, &run])
    }
}

/// The CLIs the bench looks for, by harness id.
fn programs() -> [(&'static str, &'static Programs); 3] {
    [
        ("claude", &super::claude::PROGRAMS),
        ("codex", &super::codex::PROGRAMS),
        ("cursor", &super::cursor::PROGRAMS),
    ]
}

/// How to start the agent of harness `id` against the bench's profile, when it can be: printed when the bench starts.
fn steps(id: &str, setup: &BenchSetup) -> Option<String> {
    match id {
        "claude" => {
            let start = setup.line(
                "claude",
                &strings(&["--mcp-config", setup.mcp_config, "--strict-mcp-config"]),
                None,
            );
            Some(format!(
                "Claude Code (log in once inside the profile with /login):\n  {start}"
            ))
        }
        "codex" => {
            let mut add = strings(&["mcp", "add", "sidevoice"]);
            for (name, value) in setup.env {
                add.extend(["--env".to_owned(), format!("{name}={value}")]);
            }
            add.push("--".into());
            Some(format!(
                "Codex (log in once inside the profile, register once, then start it):\n  {}\n  {}\n  {}",
                setup.line("codex", &strings(&["login"]), None),
                setup.line("codex", &add, Some(setup.server)),
                setup.line("codex", &[], None),
            ))
        }
        _ => None,
    }
}

/// The bench's `request` answered: `{"root": R}` gives each agent home below `R/agents/` and each agent's CLI as this
/// `PATH` finds it (`SIDEVOICE_<ID>_BIN`); `{"setup": {env_script, mcp_config, server, env}}` gives the steps.
pub fn agents(request: &Value) -> Value {
    let homes = super::Registry::homes();
    if let Some(root) = request.get("root").and_then(Value::as_str) {
        let homes: serde_json::Map<String, Value> = homes
            .iter()
            .map(|home| {
                let dir = Path::new(root)
                    .join("agents")
                    .join(home.var.to_ascii_lowercase());
                (
                    home.var.to_owned(),
                    Value::from(dir.to_string_lossy().into_owned()),
                )
            })
            .collect();
        let path = std::env::var_os("PATH").unwrap_or_default();
        let binaries: serde_json::Map<String, Value> = programs()
            .into_iter()
            .filter_map(|(id, programs)| {
                let found = programs
                    .names
                    .iter()
                    .find_map(|name| Environment::find_program(name, &path))?;
                Some((
                    format!("SIDEVOICE_{}_BIN", id.to_ascii_uppercase()),
                    Value::from(found.to_string_lossy().into_owned()),
                ))
            })
            .collect();
        return serde_json::json!({"homes": homes, "binaries": binaries});
    }
    let setup = request.get("setup").cloned().unwrap_or_default();
    let text = |key: &str| {
        setup
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let env: BTreeMap<String, String> = setup
        .get("env")
        .and_then(Value::as_object)
        .map(|env| {
            env.iter()
                .map(|(name, value)| (name.clone(), value.as_str().unwrap_or("").to_owned()))
                .collect()
        })
        .unwrap_or_default();
    let (env_script, mcp_config, server) = (text("env_script"), text("mcp_config"), text("server"));
    let bench = BenchSetup {
        env_script: &env_script,
        mcp_config: &mcp_config,
        server: &server,
        env: &env,
    };
    let steps: Vec<String> = programs()
        .into_iter()
        .filter_map(|(id, _)| steps(id, &bench))
        .collect();
    serde_json::json!({"steps": steps})
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The bench's lines source its script, then run the agent, in sequence as this OS's shell reads them.
    #[test]
    fn bench_steps_source_the_profile_then_run_the_agent_in_sequence() {
        let answer = agents(
            &json!({"setup": {"env_script": "/b/env.sh", "mcp_config": "/b/mcp.json",
            "server": "'/r/sidevoice' mcp", "env": {"SIDEVOICE_DATA_DIR": "/d"}}}),
        );
        let steps: Vec<&str> = answer["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|step| step.as_str().unwrap())
            .collect();
        let source = format!(". {}", Environment::quote("/b/env.sh"));
        let line = |run: String| Environment::in_sequence(&[&source, &run]);
        let claude = line(shell_command(
            "claude",
            &strings(&["--mcp-config", "/b/mcp.json", "--strict-mcp-config"]),
        ));
        let codex_add = line(format!(
            "{} '/r/sidevoice' mcp",
            shell_command(
                "codex",
                &strings(&[
                    "mcp",
                    "add",
                    "sidevoice",
                    "--env",
                    "SIDEVOICE_DATA_DIR=/d",
                    "--"
                ])
            )
        ));
        for expected in [claude, codex_add, line(shell_command("codex", &[]))] {
            assert!(
                steps
                    .iter()
                    .any(|step| step.lines().any(|text| text.trim() == expected)),
                "{expected} in {steps:?}"
            );
        }
    }
}
