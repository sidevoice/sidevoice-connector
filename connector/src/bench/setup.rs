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
}

const DIRECTORIES: &[&str] = &[
    "home",
    "sidevoice",
    "claude",
    "codex",
    "cursor/config",
    "cursor/data",
    "xdg/config",
    "xdg/data",
    "bench",
];

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
        for directory in std::iter::once("").chain(DIRECTORIES.iter().copied()) {
            let path = root.join(directory);
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&path)
                .with_context(|| path.display().to_string())?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self { root })
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
            ("CLAUDE_CONFIG_DIR".into(), self.path("claude")),
            ("CODEX_HOME".into(), self.path("codex")),
            ("CURSOR_CONFIG_DIR".into(), self.path("cursor/config")),
            ("CURSOR_DATA_DIR".into(), self.path("cursor/data")),
            ("XDG_CONFIG_HOME".into(), self.path("xdg/config")),
            ("XDG_DATA_HOME".into(), self.path("xdg/data")),
            ("SIDEVOICE_SERVICE_MANAGER".into(), "none".into()),
            ("SIDEVOICE_CORE_PORT".into(), "0".into()),
            ("SIDEVOICE_STUN_URLS".into(), String::new()),
        ]);
        // Codex delivery runs `codex queue`; a connector started on demand has a fixed PATH that may not hold it.
        if let Some(codex) = on_path("codex") {
            env.insert(
                "SIDEVOICE_CODEX_BIN".into(),
                codex.to_string_lossy().into_owned(),
            );
        }
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
    /// from (its `CLAUDE_CODE_*`, `CODEX_*`, `SIDEVOICE_*`) cannot leak in.
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

    /// `bench/env.sh` (what an agent runs with) and `bench/claude-mcp.json` (Claude Code's MCP configuration).
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
            self.root.join("bench/claude-mcp.json"),
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
        let codex_env: String = self
            .mcp_env()
            .iter()
            .map(|(name, value)| format!(" --env {}", shell_quote(&format!("{name}={value}"))))
            .collect();
        format!(
            "\nThe bench: http://127.0.0.1:{port}/  (profile {root})\n\n\
             Claude Code (log in once inside the profile with /login):\n  \
             . {root}/bench/env.sh && claude --mcp-config {root}/bench/claude-mcp.json --strict-mcp-config\n\n\
             Codex (log in once inside the profile, register once, then start it):\n  \
             . {root}/bench/env.sh && codex login\n  \
             . {root}/bench/env.sh && codex mcp add sidevoice{codex_env} -- {}\n  \
             . {root}/bench/env.sh && codex\n\n\
             Then ask the agent to join the voice call, and type to it on the page.\n",
            program.join(" ")
        )
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

fn on_path(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
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
        };
        let mcp = profile.mcp_env();
        assert_eq!(mcp["SIDEVOICE_DATA_DIR"], "/p/sidevoice");
        assert_eq!(mcp["XDG_DATA_HOME"], "/p/xdg/data");
        assert_eq!(mcp["SIDEVOICE_SERVICE_MANAGER"], "none");
        let agent = profile.agent_env();
        assert!(agent.keys().all(|name| !name.starts_with("XDG_")));
        assert_eq!(agent["CODEX_HOME"], "/p/codex");
    }
}
