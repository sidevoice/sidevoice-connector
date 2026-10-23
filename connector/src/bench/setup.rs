//! The bench's private profile and the installation in it: this build, packaged with the pinned core as the release
//! archive lays it out, installed by its own `install` with no service manager (Sidevoice on demand), and the files
//! and commands that point an agent at it.

use sidevoice_connector::platform::target::{Host, Target as _};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use sidevoice_connector::platform::env::{Env, Environment};
use sidevoice_connector::platform::ipc::Endpoint;

/// Every directory the connector reads, below one private root.
#[derive(Clone)]
pub struct Profile {
    pub root: PathBuf,
    /// The agents' homes below the root, as declared by `bench-agents`.
    pub agents: BTreeMap<String, String>,
    login_path: String,
}

fn capture_person_login_path() -> Result<String> {
    let shell = match std::env::var("SHELL") {
        Ok(shell) => shell,
        Err(std::env::VarError::NotPresent) => {
            // Noninteractive test runners have no login shell; their process PATH is already the
            // environment in which the bench found its agents. Capture it before private HOME.
            return checked_bench_path(std::env::var("PATH")?);
        }
        Err(error) => return Err(error.into()),
    };
    if !Path::new(&shell).is_absolute() {
        bail!("SHELL must be absolute");
    }
    let command = if Path::new(&shell)
        .file_name()
        .is_some_and(|name| name == "fish")
    {
        "printf '\\036SIDEVOICE_PATH_BEGIN\\036%s\\036SIDEVOICE_PATH_END\\036' (string join : $PATH)"
    } else {
        "printf '\\036SIDEVOICE_PATH_BEGIN\\036%s\\036SIDEVOICE_PATH_END\\036' \"$PATH\""
    };
    let output = Command::new(shell).args(["-lc", command]).output()?;
    if !output.status.success() {
        bail!("login shell failed while capturing bench PATH");
    }
    let stdout = String::from_utf8(output.stdout)?;
    let (_, framed) = stdout
        .split_once("\u{1e}SIDEVOICE_PATH_BEGIN\u{1e}")
        .context("login shell omitted PATH frame")?;
    let (path, _) = framed
        .split_once("\u{1e}SIDEVOICE_PATH_END\u{1e}")
        .context("login shell truncated PATH frame")?;
    checked_bench_path(path.to_owned())
}

fn checked_bench_path(path: String) -> Result<String> {
    if path.is_empty()
        || path.len() > 32768
        || !path.split(':').all(|part| Path::new(part).is_absolute())
    {
        bail!("login shell supplied an invalid bench PATH");
    }
    Ok(path)
}

const DIRECTORIES: &[&str] = &["home", "home/.local/share", "xdg/config", "bench"];

/// The data directory of the profile at `root`: the program's own directory (`Environment::DATA_NAME`) in the
/// profile's home, the same rule as the tests' profiles. Every connector process of the profile is given it as
/// `SIDEVOICE_DATA_DIR`.
fn data_dir(root: &Path) -> PathBuf {
    root.join("home").join(Environment::DATA_NAME)
}

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
        let login_path = capture_person_login_path()?;
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
            .chain([data_dir(&root)])
            .chain(homes.values().map(PathBuf::from));
        for path in directories {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&path)
                .with_context(|| path.display().to_string())?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        }
        write_bench_login_shell(&root, &login_path)?;
        Ok(Self {
            root,
            agents: homes,
            login_path,
        })
    }

    fn path(&self, child: &str) -> String {
        self.root.join(child).to_string_lossy().into_owned()
    }

    /// The connector's data directory in this profile ([`data_dir`]): every read of the installation's files (its
    /// record, discovery state, log, sockets) goes through it.
    pub fn data(&self) -> PathBuf {
        data_dir(&self.root)
    }

    /// Where the core listens, as it published it in its ready file; an error while it has not.
    pub fn core_endpoint(&self) -> Result<Endpoint> {
        Ok(sidevoice_connector::core_ready::read(&self.data().join("core/core.json"))?.endpoint)
    }

    /// What every connector process of this profile is given, beyond `PATH`: the profile's directories (a home of
    /// its own among them) and the settings of a machine without a service manager (any free port for the core, no STUN). The agent's
    /// MCP server needs the same, so that it finds this profile's connector and starts it on demand.
    pub fn mcp_env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::from([
            ("HOME".into(), self.path("home")),
            (
                "SIDEVOICE_DATA_DIR".into(),
                self.data().to_string_lossy().into_owned(),
            ),
            ("XDG_CONFIG_HOME".into(), self.path("xdg/config")),
            ("XDG_DATA_HOME".into(), self.path("home/.local/share")),
            ("SIDEVOICE_SERVICE_MANAGER".into(), "none".into()),
            ("SIDEVOICE_CORE_PORT".into(), "0".into()),
            ("SIDEVOICE_STUN_URLS".into(), String::new()),
            ("SHELL".into(), self.path("bench/login-shell")),
        ]);
        // Every agent home the harnesses declare, the bench's own.
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
        command.env("PATH", &self.login_path);
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
        let binary = package.join(sidevoice_connector::install::binary());
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

    /// Populate discovery state before the bench can accept a conversation and its first delivery.
    pub fn discover_agents(&self, command: &[String]) -> Result<()> {
        let answer = run_json(
            self.command(&command[0])
                .args(&command[1..])
                .args(["agents", "--json"]),
        )?;
        if answer["ok"] != true {
            bail!("bench agent discovery failed: {}", answer["error"]);
        }
        let state: Value = serde_json::from_slice(&fs::read(self.data().join("agents.json"))?)?;
        if state["login_path"].as_str() != Some(self.login_path.as_str()) {
            bail!("bench agent discovery did not capture its login PATH");
        }
        Ok(())
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

fn write_bench_login_shell(root: &Path, login_path: &str) -> Result<()> {
    let shell = root.join("bench/login-shell");
    fs::write(
        &shell,
        format!(
            "#!/bin/sh\nprintf '\\036SIDEVOICE_PATH_BEGIN\\036%s\\036SIDEVOICE_PATH_END\\036' {}\n",
            shell_quote(login_path)
        ),
    )?;
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// The connector binary of this build: beside the bench's own.
fn connector_binary() -> Result<PathBuf> {
    let binary = std::env::current_exe()?.with_file_name(Host::exe_name("sidevoice-connector"));
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
#[path = "tests.rs"]
mod tests;
