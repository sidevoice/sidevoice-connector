//! The bench's private profile and the installation in it: this build, packaged with the pinned core as the release
//! archive lays it out, installed by its own `install` with no service manager (Sidevoice on demand), and the files
//! and commands that point an agent at it.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

/// Every directory the connector reads, below one private root.
#[derive(Clone)]
pub struct Profile {
    pub root: PathBuf,
    /// The agents' homes below the root and their CLIs as this shell finds them, by variable: what the connector
    /// says its harnesses declare (`bench-agents`).
    pub agents: BTreeMap<String, String>,
}

const DIRECTORIES: &[&str] = &["home", "sidevoice", "xdg/config", "xdg/data", "bench"];

/// Asks this build's connector about the agents its harnesses declare (`bench-agents`).
fn bench_agents(request: Value) -> Result<Value> {
    use std::io::Write;
    let mut child = Command::new(connector_binary()?)
        .arg("bench-agents")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .context("connector stdin")?
        .write_all(request.to_string().as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!("the connector did not say which agents it has");
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

/// A JSON object of strings, as a map.
fn strings(value: &Value) -> BTreeMap<String, String> {
    value
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_owned())))
        .collect()
}

impl Profile {
    /// The profile at `root`, its directories created private (0700) when missing. A directory with anything in it
    /// that is not a bench profile is refused.
    pub fn new(root: &Path) -> Result<Self> {
        let root = if root.is_absolute() {
            root.to_path_buf()
        } else {
            std::env::current_dir()?.join(root)
        };
        // Never someone else's directory: one that has things in it must be a profile the bench made.
        let foreign = fs::read_dir(&root).is_ok_and(|mut entries| entries.next().is_some())
            && !root.join("bench").is_dir();
        if foreign {
            bail!(
                "{} is not a bench profile (it has no bench/ directory): use another",
                root.display()
            );
        }
        let answer = bench_agents(json!({"root": root}))?;
        let homes = strings(&answer["homes"]);
        let directories = std::iter::once(root.clone())
            .chain(DIRECTORIES.iter().map(|directory| root.join(directory)))
            .chain(homes.values().map(PathBuf::from));
        for path in directories {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&path)
                .with_context(|| path.display().to_string())?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        }
        let mut agents = homes;
        agents.extend(strings(&answer["binaries"]));
        Ok(Self { root, agents })
    }

    fn path(&self, child: &str) -> String {
        self.root.join(child).to_string_lossy().into_owned()
    }

    pub fn data(&self) -> PathBuf {
        self.root.join("sidevoice")
    }

    pub fn core_socket(&self) -> PathBuf {
        self.data().join("core/local.sock")
    }

    /// What every connector process of this profile is given, beyond `PATH`: the profile's directories (a home of
    /// its own among them) and the settings of a machine without a service manager (any free port for the core, no STUN). The agent's
    /// MCP server needs the same, so that it finds this profile's connector and starts it on demand.
    pub fn mcp_env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::from([
            ("HOME".into(), self.path("home")),
            ("SIDEVOICE_DATA_DIR".into(), self.path("sidevoice")),
            ("XDG_CONFIG_HOME".into(), self.path("xdg/config")),
            ("XDG_DATA_HOME".into(), self.path("xdg/data")),
            ("SIDEVOICE_SERVICE_MANAGER".into(), "none".into()),
            ("SIDEVOICE_CORE_PORT".into(), "0".into()),
            ("SIDEVOICE_STUN_URLS".into(), String::new()),
        ]);
        // Every agent home the harnesses declare, the bench's own; and an agent's CLI the connector runs (delivery,
        // registration) as this shell finds it: a connector started on demand has a fixed PATH that may not hold it.
        env.extend(self.agents.clone());
        env
    }

    /// What the agent itself runs with (`bench/env.sh`): [`Self::mcp_env`] without `HOME` and the XDG directories, which
    /// the agents keep for their own files.
    pub fn agent_env(&self) -> BTreeMap<String, String> {
        let mut env = self.mcp_env();
        env.retain(|name, _| name != "HOME" && !name.starts_with("XDG_"));
        env
    }

    /// A command of this profile: nothing inherited but `PATH`, so an agent session the bench is run
    /// from (its own variables, `SIDEVOICE_*` among them) cannot leak in.
    pub fn command(&self, program: impl AsRef<Path>) -> Command {
        let mut command = Command::new(program.as_ref());
        command.env_clear().envs(self.mcp_env());
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        command
    }

    /// Packages this build with the pinned core (`core/core.json` and its archive, from `cargo xtask core`) and
    /// installs it into the profile; returns `install --json`'s answer.
    pub fn install(&self, core: &Path) -> Result<Value> {
        let record: Value =
            serde_json::from_slice(&fs::read(core.join("core.json")).with_context(|| {
                format!(
                    "no pinned core in {}: run `cargo xtask core`",
                    core.display()
                )
            })?)?;
        let name = record["archive"]
            .as_str()
            .ok_or_else(|| anyhow!("core.json names no archive"))?;
        let archive = core.join(name);
        let package = self.root.join("package");
        let _ = fs::remove_dir_all(&package);
        fs::create_dir_all(package.join("bin"))?;
        fs::create_dir_all(package.join("core"))?;
        let binary = package.join("bin/sidevoice-connector");
        fs::copy(connector_binary()?, &binary)?;
        let packaged = format!("core/{name}");
        if fs::hard_link(&archive, package.join(&packaged)).is_err() {
            fs::copy(&archive, package.join(&packaged))
                .with_context(|| archive.display().to_string())?;
        }
        let identity = run_json(Command::new(&binary).args(["--version", "--json"]))?;
        let inventory = json!({"version": identity["version"], "target": record["target"],
            "core": {"version": record["version"], "archive": packaged, "sha256": record["sha256"],
                     "size": fs::metadata(&archive)?.len(), "source_sha": record["source_sha"]}});
        fs::write(package.join("connector.json"), inventory.to_string())?;
        let answer = run_json(
            self.command(&binary)
                .args(["install", "--no-agents", "--json"])
                .stdin(Stdio::null()),
        )?;
        if answer["ok"] != true {
            bail!("install failed: {}", answer["error"]);
        }
        Ok(answer)
    }

    /// The installation's command (`install.json`'s `command`): what the agent's MCP server runs, with `mcp`.
    pub fn installed_command(&self) -> Result<Vec<String>> {
        let record: Value = serde_json::from_slice(&fs::read(self.data().join("install.json"))?)?;
        record["command"]
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|part| part.as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
            })
            .filter(|parts| !parts.is_empty())
            .ok_or_else(|| anyhow!("install.json names no command"))
    }

    /// `bench/env.sh` (what an agent runs with) and `bench/mcp.json` (an `mcpServers` configuration naming this
    /// installation).
    pub fn write_agent_files(&self, command: &[String]) -> Result<()> {
        let mut script = String::from(
            "# The Sidevoice bench's profile, for an agent started from this shell.\n",
        );
        for (name, value) in self.agent_env() {
            script.push_str(&format!("export {name}={}\n", shell_quote(&value)));
        }
        fs::write(self.root.join("bench/env.sh"), script)?;
        let mut args: Vec<&str> = command[1..].iter().map(String::as_str).collect();
        args.push("mcp");
        let config = json!({"mcpServers": {"sidevoice": {
            "command": command[0], "args": args, "env": self.mcp_env()}}});
        fs::write(
            self.root.join("bench/mcp.json"),
            serde_json::to_string_pretty(&config)?,
        )?;
        Ok(())
    }

    /// What to run to talk to an agent through the bench, printed when it starts.
    pub fn instructions(&self, command: &[String], port: u16) -> String {
        let root = self.root.display();
        let mut program = command
            .iter()
            .map(|part| shell_quote(part))
            .collect::<Vec<_>>();
        program.push("mcp".into());
        let env_script = format!("{root}/bench/env.sh");
        let mcp_config = format!("{root}/bench/mcp.json");
        let server = program.join(" ");
        let env = self.mcp_env();
        let steps = bench_agents(
            json!({"setup": {"env_script": env_script, "mcp_config": mcp_config,
            "server": server, "env": env}}),
        )
        .map(|answer| answer["steps"].as_array().cloned().unwrap_or_default())
        .unwrap_or_default();
        let mut text = format!("\nThe bench: http://127.0.0.1:{port}/  (profile {root})\n\n");
        for steps in steps.iter().filter_map(Value::as_str) {
            text.push_str(steps);
            text.push_str("\n\n");
        }
        text.push_str("Then ask the agent to join the voice call, and type to it on the page.\n");
        text
    }

    /// Stops this profile's connector and core and removes the installation (`uninstall`), when there is one.
    pub fn uninstall(&self) {
        let Ok(command) = self.installed_command() else {
            return;
        };
        let result = self
            .command(&command[0])
            .args(&command[1..])
            .args(["uninstall", "--json"])
            .stdin(Stdio::null())
            .output();
        if let Ok(output) = result {
            eprintln!("{}", String::from_utf8_lossy(&output.stdout).trim());
        }
    }

    pub fn remove(&self) -> Result<()> {
        fs::remove_dir_all(&self.root).with_context(|| self.root.display().to_string())
    }
}

/// The connector binary of this build: beside the bench's own.
fn connector_binary() -> Result<PathBuf> {
    let binary = std::env::current_exe()?.with_file_name("sidevoice-connector");
    if !binary.is_file() {
        bail!(
            "no connector beside the bench at {}: build both (`cargo build`)",
            binary.display()
        );
    }
    Ok(binary)
}

/// A command's one JSON object on stdout.
fn run_json(command: &mut Command) -> Result<Value> {
    let output = command.output()?;
    serde_json::from_slice(&output.stdout).map_err(|_| {
        anyhow!(
            "{:?} printed no JSON: {}{}",
            command.get_program(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/._-+=:@,".contains(&byte))
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_quoted_for_the_shell_only_when_needed() {
        assert_eq!(shell_quote("/tmp/a-b_c.d"), "/tmp/a-b_c.d");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn the_agent_keeps_its_own_xdg_directories() {
        let profile = Profile {
            root: PathBuf::from("/p"),
            agents: BTreeMap::from([(
                "SOME_AGENT_HOME".to_owned(),
                "/p/agents/some_agent_home".to_owned(),
            )]),
        };
        let mcp = profile.mcp_env();
        assert_eq!(mcp["SIDEVOICE_DATA_DIR"], "/p/sidevoice");
        assert_eq!(mcp["XDG_DATA_HOME"], "/p/xdg/data");
        assert_eq!(mcp["SIDEVOICE_SERVICE_MANAGER"], "none");
        let agent = profile.agent_env();
        assert!(agent.keys().all(|name| !name.starts_with("XDG_")));
        assert_eq!(agent["SOME_AGENT_HOME"], "/p/agents/some_agent_home");
    }
}
