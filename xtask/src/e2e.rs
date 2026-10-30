//! `cargo xtask e2e ARCHIVE`: this build packed for npm and installed through npx end to end on this machine,
//! using explicit local launcher and platform tarballs with pinned npm (not optional-dependency resolution).
//! It checks this user's real service manager (Task Scheduler, the systemd user manager, launchd), the
//! standard data directory, and the real Claude Code and Codex CLIs ([`CLAUDE_CODE_VERSION`], [`codex::VERSION`])
//! installed with npm into its global prefix, with no login. Step by step:
//!
//! 1. Nothing of Sidevoice is here yet; the two agents' CLIs are installed and answer `--version`.
//! 2. `install` through npx from the local tarballs: both jobs defined and running under the manager, `D/core/core.json`
//!    there, `service status` running (the core's health on its local channel), the core's HTTP port answering
//!    `/api/rendezvous`, and both agents found and left unconnected.
//! 3. `pair-device` gives a code.
//! 4. `agents` finds both agents; `agents connect` registers Sidevoice with each, which lands in the agent's own
//!    configuration (`claude mcp list` and `~/.claude.json`; `codex mcp get` and `~/.codex/config.toml`) naming the
//!    installation's command, and the agents' rows say connected with a working registration.
//!    Then the agents are moved to where their desktop apps keep them (nothing of them left on `PATH`, except Codex on
//!    Linux, which has no app there) and the same is asserted again: found, working, disconnected and connected
//!    through the fallback (`connector/src/adapters/{claude,codex}/binary.rs`).
//! 5. `uninstall` through npx from the local tarballs (Windows cannot delete a running program, so never from
//!    the release):
//!    no job left in the manager and no definition, no process of the installation, no release, the agents'
//!    registrations gone from their configuration, and the data directory gone (it holds only its own locks and the stop, which go
//!    with it; connector/README.md, "uninstall").
//!
//! It changes this user's service manager, agents and data for real, so it runs only on a disposable machine: a CI
//! runner (`CI=true`), or where `SIDEVOICE_E2E=1` says so. On failure it prints what the jobs and logs say. The runner's
//! preparation (Linux: lingering and the user bus a login session would give) is the workflow's
//! (`.github/workflows/e2e.yml`).
//!
//! What differs by OS is data here (job names, where things are, how a manager is asked), chosen by
//! `std::env::consts::OS`: the test runs on the machine it checks.

use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::util::*;
use crate::{codex, Result};

/// The Claude Code CLI release installed, from npm (`@anthropic-ai/claude-code`). Bumped by hand, like any pin.
pub(crate) const CLAUDE_CODE_VERSION: &str = "2.1.308";
/// CI can exercise concrete newer releases without moving the reproducible defaults.
fn agent_version(name: &str, pinned: &str) -> String {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| pinned.to_owned())
}

/// The core's default HTTP port (connector/README.md, "Where things are").
const CORE_PORT: u16 = 10829;
/// The core and connector jobs, by their base name (`service/jobs.rs`).
const JOBS: [&str; 2] = ["sidevoice-core", "sidevoice-connector"];

#[derive(Clone, Copy, PartialEq)]
enum Os {
    Windows,
    Linux,
    Macos,
}

impl Os {
    fn here() -> Result<Os> {
        match env::consts::OS {
            "windows" => Ok(Os::Windows),
            "linux" => Ok(Os::Linux),
            "macos" => Ok(Os::Macos),
            other => Err(format!("no end-to-end test for {other}")),
        }
    }

    /// The `service` the install answer names.
    fn manager(self) -> &'static str {
        match self {
            Os::Windows => "task-scheduler",
            Os::Linux => "systemd",
            Os::Macos => "launchd",
        }
    }
}

/// The places and names the test checks, worked out here from the documented rules rather than asked of the
/// connector, so the connector is checked against them.
struct Machine {
    os: Os,
    home: PathBuf,
    /// The data directory `D`.
    data: PathBuf,
    /// The user's name (Windows task names carry it) and, on macOS, the uid (launchd's `gui/<uid>` domain).
    user: String,
    uid: String,
}

fn var(name: &str) -> Result<String> {
    env::var(name).map_err(|_| format!("{name} is not set"))
}

impl Machine {
    fn new() -> Result<Machine> {
        let os = Os::here()?;
        let home = PathBuf::from(var(if os == Os::Windows {
            "USERPROFILE"
        } else {
            "HOME"
        })?);
        let data = match os {
            Os::Windows => PathBuf::from(var("LOCALAPPDATA")?)
                .join("Sidevoice")
                .join("Data"),
            Os::Macos => home.join("Library/Application Support/Sidevoice"),
            Os::Linux => env::var("XDG_DATA_HOME")
                .ok()
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .unwrap_or_else(|| home.join(".local/share"))
                .join("sidevoice"),
        };
        if env::var_os("SIDEVOICE_DATA_DIR").is_some() {
            return Err(
                "SIDEVOICE_DATA_DIR is set: the end-to-end test checks the standard place".into(),
            );
        }
        let user = match os {
            Os::Windows => var("USERNAME")?,
            _ => output("id", &["-un"], None)?.trim().to_owned(),
        };
        let uid = match os {
            Os::Windows => String::new(),
            _ => output("id", &["-u"], None)?.trim().to_owned(),
        };
        Ok(Machine {
            os,
            home,
            data,
            user,
            uid,
        })
    }

    /// The manager's name for a job.
    fn job(&self, job: &str) -> String {
        match self.os {
            Os::Windows => format!("{job}-{}", self.user),
            Os::Linux => format!("{job}.service"),
            Os::Macos => format!("ai.sidevoice.{}", job.trim_start_matches("sidevoice-")),
        }
    }

    /// Where the manager reads a job's definition from, where that is outside `D` (Windows keeps it in
    /// `D/service`).
    fn definition(&self, job: &str) -> Option<PathBuf> {
        match self.os {
            Os::Windows => None,
            Os::Linux => Some(
                env::var("XDG_CONFIG_HOME")
                    .ok()
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .unwrap_or_else(|| self.home.join(".config"))
                    .join("systemd/user")
                    .join(self.job(job)),
            ),
            Os::Macos => Some(
                self.home
                    .join("Library/LaunchAgents")
                    .join(format!("{}.plist", self.job(job))),
            ),
        }
    }

    /// What the manager says of a job, `None` when it says it has no such job: Task Scheduler's state and last
    /// result, systemd's load, active and sub state, launchd's state. A query that fails for any other reason is an
    /// error, never read as "no job".
    fn job_state(&self, job: &str) -> Result<Option<String>> {
        let name = self.job(job);
        match self.os {
            Os::Windows => {
                // Not found is the one failure that means absent (CIM's ObjectNotFound); any other stops the script.
                let script = format!(
                    "$ErrorActionPreference = 'Stop'; \
                     try {{ $t = Get-ScheduledTask -TaskName '{name}' }} \
                     catch {{ if ($_.CategoryInfo.Category -eq 'ObjectNotFound') {{ 'ABSENT'; exit 0 }} throw }}; \
                     $i = $t | Get-ScheduledTaskInfo; \
                     \"$($t.State) LastTaskResult=$($i.LastTaskResult)\""
                );
                let said = run(
                    "powershell",
                    &["-NoProfile", "-NonInteractive", "-Command", &script],
                    LIMIT,
                )?;
                let text = said.stdout.trim();
                if !said.ok || text.is_empty() {
                    return Err(format!(
                        "Get-ScheduledTask {name} failed ({:?}): {text} {}",
                        said.code, said.stderr
                    ));
                }
                Ok((text != "ABSENT").then(|| text.to_owned()))
            }
            Os::Linux => {
                // `show` answers for a unit it has never heard of too: `LoadState=not-found`.
                let shown = run(
                    "systemctl",
                    &[
                        "--user",
                        "show",
                        "-p",
                        "LoadState,ActiveState,SubState",
                        &name,
                    ],
                    LIMIT,
                )?;
                if !shown.ok || !shown.stdout.contains("LoadState=") {
                    return Err(format!(
                        "systemctl --user show {name} failed ({:?}): {}{}",
                        shown.code, shown.stdout, shown.stderr
                    ));
                }
                if shown.stdout.contains("LoadState=not-found") {
                    return Ok(None);
                }
                Ok(Some(
                    shown
                        .stdout
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" "),
                ))
            }
            Os::Macos => {
                let printed = run(
                    "launchctl",
                    &["print", &format!("gui/{}/{name}", self.uid)],
                    LIMIT,
                )?;
                if printed.ok {
                    let state = printed
                        .stdout
                        .lines()
                        .find_map(|line| line.trim().strip_prefix("state = "))
                        .map(|state| format!("state={state}"))
                        .unwrap_or_else(|| "state=unknown".into());
                    return Ok(Some(state));
                }
                // launchctl's answer for a service the domain does not have: exit 113, "Could not find service".
                let said = format!("{}{}", printed.stdout, printed.stderr);
                if printed.code == Some(113) && said.contains("Could not find service") {
                    return Ok(None);
                }
                Err(format!(
                    "launchctl print gui/{}/{name} failed ({:?}): {said}",
                    self.uid, printed.code
                ))
            }
        }
    }

    /// The program a Windows task runs (its action's `Execute`), as Task Scheduler reports it.
    fn task_program(&self, job: &str) -> Result<String> {
        let script = format!(
            "$ErrorActionPreference = 'Stop'; (Get-ScheduledTask -TaskName '{}').Actions[0].Execute",
            self.job(job)
        );
        let said = run(
            "powershell",
            &["-NoProfile", "-NonInteractive", "-Command", &script],
            LIMIT,
        )?;
        let program = said.stdout.trim().to_owned();
        if !said.ok || program.is_empty() {
            return Err(format!(
                "the action of {} could not be read ({:?}): {program} {}",
                self.job(job),
                said.code,
                said.stderr
            ));
        }
        Ok(program)
    }

    fn job_running(state: &str, os: Os) -> bool {
        match os {
            Os::Windows => state.starts_with("Running "),
            Os::Linux => state.contains("ActiveState=active") && state.contains("SubState=running"),
            Os::Macos => state == "state=running",
        }
    }

    /// The processes whose command line names `D` (every program of the installation runs from `D/install`).
    fn processes(&self) -> Result<Vec<String>> {
        let data = path_str(&self.data)?.to_owned();
        let listed = match self.os {
            Os::Windows => run(
                "powershell",
                &[
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "Get-CimInstance Win32_Process | ForEach-Object { \"$($_.ProcessId) $($_.CommandLine)\" }",
                ],
                LIMIT,
            )?,
            _ => run("ps", &["-A", "-o", "pid=,command="], LIMIT)?,
        };
        let wanted = data.to_lowercase();
        if !listed.ok || listed.stdout.trim().is_empty() {
            return Err(format!(
                "listing the processes failed ({:?}): {}",
                listed.code, listed.stderr
            ));
        }
        Ok(listed
            .stdout
            .lines()
            .filter(|line| {
                if self.os == Os::Windows {
                    line.to_lowercase().contains(&wanted)
                } else {
                    line.contains(&data)
                }
            })
            .map(str::to_owned)
            .collect())
    }

    /// Claude Code's user configuration (`CLAUDE_CONFIG_DIR` unset: `~/.claude.json`): `None` only when there is no
    /// such file; one that cannot be read or parsed is an error.
    fn claude_config(&self) -> Result<Option<Value>> {
        let file = self.home.join(".claude.json");
        let Some(bytes) = read_if_there(&file)? else {
            return Ok(None);
        };
        parse_json(&bytes, &file.display().to_string()).map(Some)
    }

    /// Codex's configuration (`CODEX_HOME` unset: `~/.codex/config.toml`): `None` only when there is no such file;
    /// one that cannot be read or parsed is an error.
    fn codex_config(&self) -> Result<Option<toml::Value>> {
        let file = self.home.join(".codex/config.toml");
        let Some(bytes) = read_if_there(&file)? else {
            return Ok(None);
        };
        let text =
            String::from_utf8(bytes).map_err(|_| format!("{}: not UTF-8", file.display()))?;
        text.parse()
            .map(Some)
            .map_err(|error| format!("{}: {error}", file.display()))
    }

    /// On failure: what the manager says of each job and the end of every log in `D`.
    fn diagnose(&self) {
        eprintln!("--- e2e diagnostics");
        for job in JOBS {
            eprintln!("{}: {:?}", self.job(job), self.job_state(job));
        }
        if self.os == Os::Windows {
            if let Ok(sessions) = run("query", &["user"], LIMIT) {
                eprintln!("query user: {}{}", sessions.stdout, sessions.stderr);
            }
        }
        if self.os == Os::Linux {
            if let Ok(journal) = run(
                "journalctl",
                &[
                    "--user",
                    "--no-pager",
                    "-n",
                    "80",
                    "-u",
                    "sidevoice-core",
                    "-u",
                    "sidevoice-connector",
                ],
                LIMIT,
            ) {
                eprintln!("journal:\n{}{}", journal.stdout, journal.stderr);
            }
        }
        let mut logs = Vec::new();
        collect_logs(&self.data, 3, &mut logs);
        for log in logs {
            let text = fs::read_to_string(&log).unwrap_or_default();
            let tail: Vec<&str> = text.lines().rev().take(60).collect();
            eprintln!("--- {}", log.display());
            for line in tail.into_iter().rev() {
                eprintln!("{line}");
            }
        }
    }
}

fn collect_logs(dir: &Path, depth: u32, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && depth > 0 {
            collect_logs(&path, depth - 1, found);
        } else if path.extension().is_some_and(|ext| ext == "log") {
            found.push(path);
        }
    }
}

/// How long one command may take, unless it says otherwise.
const LIMIT: Duration = Duration::from_secs(60);

struct Ran {
    ok: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Runs `program` (found as the OS finds it) to completion within `limit`, with no standard input; killed and an
/// error past it.
fn run(program: &str, args: &[&str], limit: Duration) -> Result<Ran> {
    run_path(&resolve(program), args, limit)
}

fn run_path(program: &Path, args: &[&str], limit: Duration) -> Result<Ran> {
    let mut command = Command::new(program);
    command.args(args);
    run_command(command, limit)
}

fn run_command(mut command: Command, limit: Duration) -> Result<Ran> {
    let what = format!("{command:?}");
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{what}: {error}"))?;
    let mut out = child.stdout.take().expect("piped");
    let mut err = child.stderr.take().expect("piped");
    let out = thread::spawn(move || {
        let mut text = Vec::new();
        let _ = out.read_to_end(&mut text);
        String::from_utf8_lossy(&text).into_owned()
    });
    let err = thread::spawn(move || {
        let mut text = Vec::new();
        let _ = err.read_to_end(&mut text);
        String::from_utf8_lossy(&text).into_owned()
    });
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("{what}: {error}"))?
        {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{what}: no answer within {} s", limit.as_secs()));
        }
        thread::sleep(Duration::from_millis(100));
    };
    // A program that left a child holding its output (a daemon it started) must not hold the test: what was said
    // until the deadline is enough.
    let join = |handle: thread::JoinHandle<String>| {
        let until = Instant::now() + Duration::from_secs(10);
        while !handle.is_finished() && Instant::now() < until {
            thread::sleep(Duration::from_millis(50));
        }
        if handle.is_finished() {
            handle.join().unwrap_or_default()
        } else {
            String::new()
        }
    };
    Ok(Ran {
        ok: status.success(),
        code: status.code(),
        stdout: join(out),
        stderr: join(err),
    })
}

/// The connector at `program`: `args… --json`, its one JSON object, whose `ok` must match the exit status.
fn sidevoice(program: &Path, args: &[&str], limit: Duration) -> Result<Value> {
    sidevoice_command(Command::new(program), args, limit)
}

fn sidevoice_command(mut command: Command, args: &[&str], limit: Duration) -> Result<Value> {
    let mut all = args.to_vec();
    all.push("--json");
    println!("$ sidevoice-connector {}", all.join(" "));
    command.args(&all);
    let ran = run_command(command, limit)?;
    let lines: Vec<&str> = ran
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let [line] = lines.as_slice() else {
        return Err(format!(
            "{}: not one JSON line: {:?}; stderr: {}",
            all.join(" "),
            ran.stdout,
            ran.stderr
        ));
    };
    let answer = parse_json(line.as_bytes(), &all.join(" "))?;
    println!("{answer}");
    if ran.ok != (answer["ok"] == true) {
        return Err(format!(
            "{}: exit status {} but ok is {}",
            all.join(" "),
            ran.ok,
            answer["ok"]
        ));
    }
    Ok(answer)
}

fn ok(answer: Value, what: &str) -> Result<Value> {
    if answer["ok"] != true {
        return Err(format!("{what} failed: {answer}"));
    }
    Ok(answer)
}

fn check(condition: bool, what: impl FnOnce() -> String) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(what())
    }
}

/// Asks `probe` until it gives something or `seconds` pass; the error names `what` and the last answer.
fn until<T>(what: &str, seconds: u64, mut probe: impl FnMut() -> Result<Option<T>>) -> Result<T> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut last = String::new();
    loop {
        match probe() {
            Ok(Some(found)) => return Ok(found),
            Ok(None) => {}
            Err(error) => last = error,
        }
        if Instant::now() > deadline {
            return Err(format!("timed out after {seconds} s: {what} {last}"));
        }
        thread::sleep(Duration::from_millis(500));
    }
}

fn step(name: &str) {
    println!("\n== {name}");
}

/// Both packages are explicit local inputs: no Sidevoice build may come from the registry.
/// Keep npm's config, prefix and cache separate from the agents' online global installation.
struct Npx {
    scratch: PathBuf,
    packages: Vec<crate::npm::Packed>,
}

impl Npx {
    fn new(archive: &Path, scratch: &Path) -> Result<Self> {
        let out = scratch.join("tarballs");
        mkdir(&out)?;
        let started = Instant::now();
        let packages = crate::npm::build(&[archive], &out)?;
        println!("npm packing took {:.1} s", started.elapsed().as_secs_f64());
        for config in ["user-npmrc", "global-npmrc"] {
            write(&scratch.join(config), b"")?;
        }
        Ok(Self {
            scratch: scratch.to_owned(),
            packages,
        })
    }

    fn command(&self) -> Result<Command> {
        let mut command = crate::npm::npm_command("npx-cli.js")?;
        command
            .current_dir(&self.scratch)
            .env("npm_config_userconfig", self.scratch.join("user-npmrc"))
            .env("npm_config_globalconfig", self.scratch.join("global-npmrc"))
            .env("npm_config_cache", self.scratch.join("cache"))
            .env("npm_config_prefix", self.scratch.join("prefix"))
            .env("npm_config_offline", "true")
            .env("npm_config_registry", "http://127.0.0.1:9")
            .args(["--yes", "--offline"]);
        for package in &self.packages {
            command.arg("--package").arg(&package.tarball);
        }
        command.args(["--", "sidevoice"]);
        Ok(command)
    }
}

pub(crate) fn e2e(archive: &Path) -> Result<()> {
    if env::var("CI").as_deref() != Ok("true") && env::var("SIDEVOICE_E2E").as_deref() != Ok("1") {
        return Err("e2e installs Sidevoice into this user's real service manager, data directory and agents: it \
                    runs only on a CI runner (CI=true) or with SIDEVOICE_E2E=1"
            .into());
    }
    let machine = Machine::new()?;
    let scratch = TempDir::new_in(&crate::host::scratch_base(), "sidevoice e2e")?;
    let result = steps(&machine, archive, &scratch.0);
    if result.is_err() {
        machine.diagnose();
    }
    result
}

fn steps(machine: &Machine, archive: &Path, scratch: &Path) -> Result<()> {
    step("nothing of Sidevoice is here yet");
    check(!machine.data.exists(), || {
        format!(
            "{} already exists: not a fresh machine",
            machine.data.display()
        )
    })?;
    for job in JOBS {
        let state = machine.job_state(job)?;
        check(state.is_none(), || {
            format!("{} is already defined: {state:?}", machine.job(job))
        })?;
    }
    // The core listens on all IPv4 interfaces at its default port: something else there would make the install
    // fail for a reason that is not ours, or answer the port checks below in the core's place.
    std::net::TcpListener::bind(("0.0.0.0", CORE_PORT)).map_err(|error| {
        format!("port {CORE_PORT}, the core's, is not free on this machine ({error}): not a fresh machine")
    })?;

    let claude_version = agent_version("SIDEVOICE_E2E_CLAUDE_VERSION", CLAUDE_CODE_VERSION);
    let codex_version = agent_version("SIDEVOICE_E2E_CODEX_VERSION", codex::VERSION);
    step("the agents' CLIs, installed with npm and no login");
    run_ok(
        "npm",
        &[
            "install",
            "--global",
            "--no-audit",
            "--no-fund",
            &format!("@anthropic-ai/claude-code@{claude_version}"),
            &format!("@openai/codex@{codex_version}"),
        ],
        Duration::from_secs(300),
    )?;
    // The CLIs the rest of the test runs: the ones on PATH, until they are moved to the apps' places.
    let mut claude_cli = resolve("claude");
    let mut codex_cli = resolve("codex");
    for (cli, version) in [
        ("claude", claude_version.as_str()),
        ("codex", codex_version.as_str()),
    ] {
        let found = which::which(cli)
            .map_err(|error| format!("{cli} is not on PATH after npm install: {error}"))?;
        let said = run_path(&found, &["--version"], LIMIT)?;
        println!("{} --version: {}", found.display(), said.stdout.trim());
        check(said.ok && said.stdout.contains(version), || {
            format!(
                "{} --version says {:?} {:?}, not {version}",
                found.display(),
                said.stdout,
                said.stderr
            )
        })?;
    }

    step("this archive's npm tarballs, installed through npx offline");
    let package = Npx::new(archive, scratch)?;
    let identity = ok(
        sidevoice_command(package.command()?, &["--version"], LIMIT)?,
        "npx --version",
    )?;
    let (_, inventory) = crate::archive::unpack_checked(archive, &scratch.join("identity"))?;
    check(inventory["target"] == host_target()?, || {
        format!("archive is for another machine: {}", inventory["target"])
    })?;
    check_identity(&identity, &inventory)?;
    let started = Instant::now();
    let installed = ok(
        sidevoice_command(
            package.command()?,
            &["install", "--no-prompt"],
            Duration::from_secs(300),
        )?,
        "npx install",
    )?;
    println!("npx install took {:.1} s", started.elapsed().as_secs_f64());
    check(installed["action"] == "installed", || {
        format!("not a first installation: {installed}")
    })?;
    check(installed["service"] == machine.os.manager(), || {
        format!(
            "install used service {}, not {}: {installed}",
            installed["service"],
            machine.os.manager()
        )
    })?;
    let releases = machine.data.join("install");
    check(
        installed["releases"].as_str() == Some(path_str(&releases)?),
        || format!("releases are not at {}: {installed}", releases.display()),
    )?;
    for agent in ["claude", "codex"] {
        let outcome = &installed["agents"][agent]["outcome"];
        check(outcome == "not-connected", || {
            format!("install did not find {agent} unconnected (outcome {outcome}): {installed}")
        })?;
    }
    let command = installed["command"][0]
        .as_str()
        .ok_or_else(|| format!("install names no command: {installed}"))?
        .to_owned();
    check(Path::new(&command).starts_with(&releases), || {
        format!(
            "the installation's command {command} is not in {}",
            releases.display()
        )
    })?;
    let stable = PathBuf::from(&command);

    step("both jobs running under the manager");
    for job in JOBS {
        let name = machine.job(job);
        let state = until(&format!("{name} running"), 90, || {
            let state = machine.job_state(job)?;
            Ok(state.filter(|state| Machine::job_running(state, machine.os)))
        })?;
        println!("{name}: {state}");
        if let Some(definition) = machine.definition(job) {
            check(definition.is_file(), || {
                format!("no definition at {}", definition.display())
            })?;
        }
    }

    if machine.os == Os::Windows {
        step("the tasks run the windowless launcher beside the connector");
        let launcher = stable.with_file_name("sidevoice-background.exe");
        check(launcher.is_file(), || {
            format!("the release has no launcher at {}", launcher.display())
        })?;
        let running = machine.processes()?;
        for job in JOBS {
            let program = machine.task_program(job)?;
            println!("{}: runs {program}", machine.job(job));
            check(
                program
                    .trim_matches('"')
                    .eq_ignore_ascii_case(path_str(&launcher)?),
                || {
                    format!(
                        "the task of {job} runs {program}, not {}",
                        launcher.display()
                    )
                },
            )?;
            let definition = format!("{job}.json").to_lowercase();
            check(
                running.iter().any(|line| {
                    let line = line.to_lowercase();
                    line.contains("sidevoice-background.exe") && line.contains(&definition)
                }),
                || format!("no launcher process is running the job {job}: {running:#?}"),
            )?;
        }
    }

    step("the core: core.json, its health, its port");
    let core_json = machine.data.join("core/core.json");
    let ready = until("core.json written", 60, || {
        Ok(fs::read(&core_json)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()))
    })?;
    println!("{}: {ready}", core_json.display());
    check(ready.get("endpoint").is_some(), || {
        format!("core.json names no endpoint: {ready}")
    })?;
    let status = until("service status: running", 90, || {
        let status = sidevoice(&stable, &["service", "status"], LIMIT)?;
        Ok((status["state"] == "running").then_some(status))
    })?;
    check(status["service"] == machine.os.manager(), || {
        format!("status names another manager: {status}")
    })?;
    let rendezvous = run(
        "curl",
        &[
            "--fail",
            "--silent",
            "--show-error",
            "--max-time",
            "10",
            &format!("http://127.0.0.1:{CORE_PORT}/api/rendezvous"),
        ],
        LIMIT,
    )?;
    check(rendezvous.ok, || {
        format!("the core's port does not answer: {}", rendezvous.stderr)
    })?;
    let node = parse_json(rendezvous.stdout.as_bytes(), "/api/rendezvous")?;
    check(node["kind"] == "node", || {
        format!("/api/rendezvous: {node}")
    })?;
    println!("/api/rendezvous: {node}");

    step("pair-device");
    let paired = ok(sidevoice(&stable, &["pair-device"], LIMIT)?, "pair-device")?;
    check(
        paired["code"].as_str().is_some_and(|code| !code.is_empty()),
        || format!("pair-device gave no code: {paired}"),
    )?;

    step("agents: both found");
    let listed = ok(
        sidevoice(&stable, &["agents"], Duration::from_secs(120))?,
        "agents",
    )?;
    for agent in ["claude", "codex"] {
        let row = agent_row(&listed, agent)?;
        check(row["present"] == true, || {
            format!("{agent} is not found: {row}")
        })?;
    }

    step("agents connect: Sidevoice registered with both");
    // Claude Code installed and never started has no home yet: connecting it must work all the same, its CLI making
    // the home (sidevoice/sidevoice-connector#266).
    for (agent, home) in [("claude", ".claude"), ("codex", ".codex")] {
        let home = machine.home.join(home);
        println!(
            "{} exists before connecting: {}",
            home.display(),
            home.exists()
        );
        check(agent != "claude" || !home.exists(), || {
            format!(
                "{} already exists: the never-started case is not exercised",
                home.display()
            )
        })?;
        ok(
            sidevoice(
                &stable,
                &["agents", "connect", agent],
                Duration::from_secs(120),
            )?,
            &format!("agents connect {agent}"),
        )?;
        check(home.is_dir(), || {
            format!("{} is not there after connecting {agent}", home.display())
        })?;
    }
    registered(machine, &stable, &command, &claude_cli, &codex_cli)?;

    step("agents in the desktop apps' places: moved off PATH, found, still working, disconnected and connected");
    let (claude_app, codex_app) = move_to_apps(machine, &mut claude_cli, &mut codex_cli)?;
    println!("claude at {}", claude_app.display());
    match &codex_app {
        Some(path) => println!("codex at {}", path.display()),
        None => println!(
            "codex stays on PATH: no desktop app keeps it on {}",
            env::consts::OS
        ),
    }
    let listed = ok(
        sidevoice(&stable, &["agents"], Duration::from_secs(120))?,
        "agents",
    )?;
    resolved_to(
        machine,
        &[
            ("claude", &claude_app),
            ("codex", codex_app.as_ref().unwrap_or(&codex_cli)),
        ],
    )?;
    for agent in ["claude", "codex"] {
        let row = agent_row(&listed, agent)?;
        check(row["present"] == true, || {
            format!("{agent} is not found at the app's place: {row}")
        })?;
        check(row["registration"] == "connected", || {
            format!("{agent} is not connected, read through the app's copy: {row}")
        })?;
        check(row["health"]["state"] == "working", || {
            format!(
                "{agent}'s registration does not work, read through the app's copy: {}",
                row["health"]
            )
        })?;
    }
    for agent in ["claude", "codex"] {
        ok(
            sidevoice(
                &stable,
                &["agents", "disconnect", agent],
                Duration::from_secs(120),
            )?,
            &format!("agents disconnect {agent}"),
        )?;
    }
    let listed = ok(
        sidevoice(&stable, &["agents"], Duration::from_secs(120))?,
        "agents",
    )?;
    for agent in ["claude", "codex"] {
        let row = agent_row(&listed, agent)?;
        check(
            row["present"] == true && row["registration"] == "not-connected",
            || format!("{agent} is not found and unconnected after disconnecting: {row}"),
        )?;
        ok(
            sidevoice(
                &stable,
                &["agents", "connect", agent],
                Duration::from_secs(120),
            )?,
            &format!("agents connect {agent} through the app's copy"),
        )?;
    }
    registered(machine, &stable, &command, &claude_cli, &codex_cli)?;
    resolved_to(
        machine,
        &[
            ("claude", &claude_app),
            ("codex", codex_app.as_ref().unwrap_or(&codex_cli)),
        ],
    )?;

    // A connector of ours that outlives the service and ignores a request to leave (an MCP server an app keeps open):
    // on Windows its program is held open, so uninstall must end it itself, and say so.
    let mut survivor = if machine.os == Os::Windows {
        step("a surviving connector is running from the releases");
        let copy = stable.with_file_name("survivor.exe");
        fs::copy(&stable, &copy).map_err(|error| format!("{}: {error}", copy.display()))?;
        let mut child = Command::new(&copy)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("{}: {error}", copy.display()))?;
        thread::sleep(Duration::from_secs(3));
        check(
            child.try_wait().map_err(|e| e.to_string())?.is_none(),
            || "the surviving connector exited by itself".to_owned(),
        )?;
        Some(child)
    } else {
        None
    };

    step("uninstall, from the package");
    let removed = ok(
        sidevoice_command(package.command()?, &["uninstall"], Duration::from_secs(300))?,
        "uninstall",
    )?;
    check(removed["state"] == "absent", || {
        format!("uninstall: {removed}")
    })?;
    if let Some(mut child) = survivor.take() {
        check(
            removed["terminated"]
                .as_array()
                .is_some_and(|pids| pids.iter().any(|pid| *pid == child.id())),
            || format!("uninstall did not report ending the surviving connector: {removed}"),
        )?;
        check(
            child.try_wait().map_err(|e| e.to_string())?.is_some(),
            || "the surviving connector is still running after uninstall".to_owned(),
        )?;
    }
    for agent in ["claude", "codex"] {
        let outcome = &removed["agents"][agent]["outcome"];
        check(outcome == "removed", || {
            format!("uninstall did not remove {agent}'s registration: {removed}")
        })?;
    }

    step("nothing left");
    for job in JOBS {
        let state = until(
            &format!("{} gone from the manager", machine.job(job)),
            30,
            || Ok(machine.job_state(job)?.is_none().then_some(())),
        );
        state.map_err(|error| format!("{error}: {:?}", machine.job_state(job)))?;
        if let Some(definition) = machine.definition(job) {
            check(!definition.exists(), || {
                format!("{} is left", definition.display())
            })?;
        }
    }
    let left = until("no process of the installation", 30, || {
        let left = machine.processes()?;
        Ok(left.is_empty().then_some(()))
    });
    left.map_err(|error| format!("{error}: {:?}", machine.processes()))?;
    check(!releases.exists(), || {
        format!("{} is left", releases.display())
    })?;
    // The stop survives cleanup. Models keep every remaining file beside them.
    if machine.data.exists() {
        let names: Vec<String> = fs::read_dir(&machine.data)
            .map_err(|error| format!("{}: {error}", machine.data.display()))?
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        println!("{} holds {names:?}", machine.data.display());
        let mut expected = if names.iter().any(|name| name == "Models") {
            vec![
                "Models",
                "agents.lock",
                "connector.lock",
                "install.lock",
                "node-stopped.json",
            ]
        } else {
            vec!["node-stopped.json"]
        };
        expected.sort();
        let mut names = names;
        names.sort();
        check(names == expected, || {
            format!("left in {}: {names:?}", machine.data.display())
        })?;
        check(
            removed["data_dir"]["kept"]
                == if names.iter().any(|name| name == "Models") {
                    json!(["Models"])
                } else {
                    json!(["node-stopped.json"])
                },
            || format!("uninstall did not say what it kept: {removed}"),
        )?;
    } else {
        check(removed["data_dir"]["removed"] == true, || {
            format!("uninstall did not say it removed the data directory: {removed}")
        })?;
    }
    let claude_listed = run_ok_path(&claude_cli, &["mcp", "list"], Duration::from_secs(120))?;
    check(!claude_listed.contains("sidevoice:"), || {
        format!("claude mcp list still lists sidevoice: {claude_listed}")
    })?;
    if let Some(claude) = machine.claude_config()? {
        check(claude["mcpServers"].get("sidevoice").is_none(), || {
            "~/.claude.json still has the sidevoice entry".into()
        })?;
    }
    if let Some(codex) = machine.codex_config()? {
        let entry = codex
            .get("mcp_servers")
            .and_then(|servers| servers.get("sidevoice"));
        check(entry.is_none(), || {
            "~/.codex/config.toml still has [mcp_servers.sidevoice]".into()
        })?;
    }
    // Codex's own word, as for Claude Code above: it must fail, saying there is no such server (the wording the
    // connector itself reads as absent, `adapters::registration::looks_absent`).
    let codex_entry = run_path(&codex_cli, &["mcp", "get", "sidevoice", "--json"], LIMIT)?;
    let said = format!("{}{}", codex_entry.stdout, codex_entry.stderr).to_ascii_lowercase();
    println!("codex mcp get sidevoice after uninstall: {}", said.trim());
    check(
        !codex_entry.ok
            && ["no mcp server", "no such", "not found", "does not exist"]
                .iter()
                .any(|absent| said.contains(absent)),
        || format!("codex mcp get sidevoice does not say it is gone: {said}"),
    )?;
    let port = run(
        "curl",
        &[
            "--silent",
            "--max-time",
            "5",
            &format!("http://127.0.0.1:{CORE_PORT}/api/rendezvous"),
        ],
        LIMIT,
    )?;
    check(!port.ok, || {
        format!("the core's port still answers: {}", port.stdout)
    })?;
    println!("\ne2e: install, pair-device, agents connect and uninstall all as promised");
    Ok(())
}

/// Sidevoice is registered with both agents, in each one's own configuration and by its own CLI, and the connector's
/// rows say connected and working.
fn registered(
    machine: &Machine,
    stable: &Path,
    command: &str,
    claude_cli: &Path,
    codex_cli: &Path,
) -> Result<()> {
    let claude_listed = run_ok_path(claude_cli, &["mcp", "list"], Duration::from_secs(120))?;
    println!("claude mcp list:\n{claude_listed}");
    check(claude_listed.contains("sidevoice:"), || {
        format!("claude mcp list does not list sidevoice: {claude_listed}")
    })?;
    let claude = machine
        .claude_config()?
        .ok_or("no ~/.claude.json after connecting Claude Code")?;
    let entry = &claude["mcpServers"]["sidevoice"];
    check(entry["command"] == command, || {
        format!("~/.claude.json's sidevoice entry does not run {command}: {entry}")
    })?;
    let codex_entry = run_ok_path(codex_cli, &["mcp", "get", "sidevoice", "--json"], LIMIT)?;
    println!("codex mcp get sidevoice: {}", codex_entry.trim());
    let codex = machine
        .codex_config()?
        .ok_or("no ~/.codex/config.toml after connecting Codex")?;
    let entry = codex
        .get("mcp_servers")
        .and_then(|servers| servers.get("sidevoice"))
        .ok_or("~/.codex/config.toml has no [mcp_servers.sidevoice]")?;
    check(
        entry.get("command").and_then(toml::Value::as_str) == Some(command),
        || format!("~/.codex/config.toml's sidevoice entry does not run {command}: {entry}"),
    )?;
    let listed = ok(
        sidevoice(stable, &["agents"], Duration::from_secs(120))?,
        "agents",
    )?;
    for agent in ["claude", "codex"] {
        let row = agent_row(&listed, agent)?;
        check(row["registration"] == "connected", || {
            format!("{agent} is not connected: {row}")
        })?;
        check(row["health"]["state"] == "working", || {
            format!("{agent}'s registration does not work: {}", row["health"])
        })?;
    }
    Ok(())
}

/// The program called `exe` (the native one, not npm's script shim) inside `package`: the largest file of that name
/// of at least `least` bytes. The shim and any wrapper are far smaller.
fn native_program(package: &Path, exe: &str, least: u64) -> Option<PathBuf> {
    let mut best: Option<(u64, PathBuf)> = None;
    let mut pending = vec![package.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                pending.push(path);
            } else if meta.len() >= least
                && path.file_name().is_some_and(|name| name == exe)
                && best.as_ref().is_none_or(|(size, _)| meta.len() > *size)
            {
                best = Some((meta.len(), path));
            }
        }
    }
    best.map(|(_, path)| path)
}

/// Copies `from` to `to` (making its folders) as the app would have put it there.
fn place(from: &Path, to: &Path) -> Result<()> {
    let parent = to.parent().ok_or("no parent")?;
    fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    fs::copy(from, to)
        .map_err(|error| format!("{} to {}: {error}", from.display(), to.display()))?;
    println!("placed {} at {}", from.display(), to.display());
    Ok(())
}

/// Moves the npm-installed CLIs to where the desktop apps keep theirs, exactly where the connector looks
/// (`connector/src/adapters/{claude,codex}/binary.rs`), and removes them from npm so nothing is left on `PATH`.
/// Claude Code: Windows, the desktop app's `%LOCALAPPDATA%\Claude-Data\claude_<id>\claude-code\<version>\<hash>` (and a
/// lower version in the older MSIX package layout, which must lose); elsewhere `~/.claude/local` (a
/// fallback place on no `PATH`; `~/.claude` exists by now). Codex: Windows, the ChatGPT app's cache
/// `%LOCALAPPDATA%\OpenAI\Codex\bin\<hash>`; macOS, `~/Applications/ChatGPT.app`'s bundle; Linux has no app, so it
/// stays where npm put it. Returns where each now is (`None`: still on `PATH`) and sets the two CLI paths to use.
fn move_to_apps(
    machine: &Machine,
    claude_cli: &mut PathBuf,
    codex_cli: &mut PathBuf,
) -> Result<(PathBuf, Option<PathBuf>)> {
    let claude_version = agent_version("SIDEVOICE_E2E_CLAUDE_VERSION", CLAUDE_CODE_VERSION);
    let codex_version = agent_version("SIDEVOICE_E2E_CODEX_VERSION", codex::VERSION);
    let root = PathBuf::from(run_ok("npm", &["root", "--global"], LIMIT)?.trim());
    let windows = machine.os == Os::Windows;
    let claude_exe = if windows { "claude.exe" } else { "claude" };
    let codex_exe = if windows { "codex.exe" } else { "codex" };
    // Real programs are tens of megabytes; the Codex detector itself skips a copy under 4 MiB.
    let least = 4 * 1024 * 1024;
    let claude_from = native_program(&root.join("@anthropic-ai"), claude_exe, least)
        .ok_or_else(|| format!("no native {claude_exe} under {}", root.display()))?;
    let codex_from = native_program(&root.join("@openai"), codex_exe, least)
        .ok_or_else(|| format!("no native {codex_exe} under {}", root.display()))?;
    let local = || var("LOCALAPPDATA").map(PathBuf::from);
    let claude_to = match machine.os {
        Os::Windows => local()?
            .join("Claude-Data/claude_e2e0000000000/claude-code")
            .join(&claude_version)
            .join("e2e")
            .join(claude_exe),
        _ => machine.home.join(".claude/local").join(claude_exe),
    };
    let codex_to = match machine.os {
        Os::Windows => Some(local()?.join("OpenAI/Codex/bin/e2e").join(codex_exe)),
        Os::Macos => Some(
            machine
                .home
                .join("Applications/ChatGPT.app/Contents/Resources/codex-cli/bin")
                .join(codex_exe),
        ),
        Os::Linux => None,
    };
    place(&claude_from, &claude_to)?;
    if machine.os == Os::Windows {
        // The older MSIX layout holds a copy too, of a lower version: the connector must still pick the Claude-Data one.
        let older = local()?
            .join("Packages/Claude_e2e0000000000/LocalCache/Roaming/Claude/claude-code/0.0.1/e2e")
            .join(claude_exe);
        place(&claude_from, &older)?;
    }
    if let Some(to) = &codex_to {
        place(&codex_from, to)?;
    }
    let mut args = vec![
        "uninstall",
        "--global",
        "--no-audit",
        "--no-fund",
        "@anthropic-ai/claude-code",
    ];
    if codex_to.is_some() {
        args.push("@openai/codex");
    }
    run_ok("npm", &args, Duration::from_secs(300))?;
    check(which::which("claude").is_err(), || {
        format!("claude is still on PATH: {:?}", which::which("claude"))
    })?;
    if codex_to.is_some() {
        check(which::which("codex").is_err(), || {
            format!("codex is still on PATH: {:?}", which::which("codex"))
        })?;
    }
    for (cli, version, path) in [
        ("claude", claude_version.as_str(), Some(&claude_to)),
        ("codex", codex_version.as_str(), codex_to.as_ref()),
    ] {
        if let Some(path) = path {
            let said = run_path(path, &["--version"], LIMIT)?;
            check(said.ok && said.stdout.contains(version), || {
                format!(
                    "{cli} at {} says {:?} {:?}",
                    path.display(),
                    said.stdout,
                    said.stderr
                )
            })?;
        }
    }
    *claude_cli = claude_to.clone();
    if let Some(to) = &codex_to {
        *codex_cli = to.clone();
    }
    Ok((claude_to, codex_to))
}

/// The binary the connector itself resolved for each agent (`binaries` in `D/agents.json`, written by the scan the
/// last `agents` ran) is exactly the placed one. `which` on this process's PATH cannot say it: the connector reads
/// the login PATH (Unix) or the registry's (Windows).
fn resolved_to(machine: &Machine, expected: &[(&str, &PathBuf)]) -> Result<()> {
    let file = machine.data.join("agents.json");
    let bytes = read_if_there(&file)?.ok_or_else(|| format!("{} is not there", file.display()))?;
    let state = parse_json(&bytes, &file.display().to_string())?;
    for (agent, placed) in expected {
        let resolved = state["binaries"][agent]
            .as_str()
            .ok_or_else(|| format!("{} names no binary for {agent}: {state}", file.display()))?;
        println!("{agent}: the connector resolved {resolved}");
        let same = |a: &Path, b: &Path| match (fs::canonicalize(a), fs::canonicalize(b)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        check(same(Path::new(resolved), placed), || {
            format!(
                "the connector resolved {agent} to {resolved}, not the placed {}",
                placed.display()
            )
        })?;
    }
    Ok(())
}

fn run_ok_path(program: &Path, args: &[&str], limit: Duration) -> Result<String> {
    let ran = run_path(program, args, limit)?;
    if !ran.ok {
        return Err(format!(
            "{} {}: {}{}",
            program.display(),
            args.join(" "),
            ran.stderr,
            ran.stdout
        ));
    }
    Ok(ran.stdout)
}

fn run_ok(program: &str, args: &[&str], limit: Duration) -> Result<String> {
    let ran = run(program, args, limit)?;
    if !ran.ok {
        return Err(format!(
            "{program} {}: {}{}",
            args.join(" "),
            ran.stderr,
            ran.stdout
        ));
    }
    Ok(ran.stdout)
}

fn agent_row<'a>(listed: &'a Value, id: &str) -> Result<&'a Value> {
    listed["agents"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"] == id))
        .ok_or_else(|| format!("agents lists no {id}: {listed}"))
}

/// A file's bytes, `None` when it does not exist; any other failure to read it is an error.
fn read_if_there(file: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(file) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", file.display())),
    }
}

fn check_identity(identity: &Value, inventory: &Value) -> Result<()> {
    for field in ["version", "target", "source_sha"] {
        check(identity[field] == inventory[field], || {
            format!(
                "npx runs another build: {field} is {}, archive carries {}",
                identity[field], inventory[field]
            )
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn npx_must_run_the_archives_build_even_when_the_version_matches() {
        let inventory = json!({
            "version": "0.15.0",
            "target": "linux-x86_64",
            "source_sha": "a".repeat(40),
        });
        assert!(check_identity(&inventory, &inventory).is_ok());
        for field in ["version", "target", "source_sha"] {
            let mut other = inventory.clone();
            other[field] = "another build".into();
            assert!(check_identity(&other, &inventory).is_err(), "{field}");
        }
    }
}
