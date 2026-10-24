//! `cargo xtask e2e ARCHIVE`: the release archive installed end to end on this machine, as a person installs it, with
//! nothing standing in: this user's real service manager (Task Scheduler, the systemd user manager, launchd), the
//! standard data directory, and the real Claude Code and Codex CLIs ([`CLAUDE_CODE_VERSION`], [`codex::VERSION`])
//! installed with npm into its global prefix, with no login. Step by step:
//!
//! 1. Nothing of Sidevoice is here yet; the two agents' CLIs are installed and answer `--version`.
//! 2. `install` from the unpacked archive: both jobs defined and running under the manager, `D/core/core.json`
//!    there, `service status` running (the core's health on its local channel), the core's HTTP port answering
//!    `/api/rendezvous`, and both agents found and left unconnected.
//! 3. `pair-device` gives a code.
//! 4. `agents` finds both agents; `agents connect` registers Sidevoice with each, which lands in the agent's own
//!    configuration (`claude mcp list` and `~/.claude.json`; `codex mcp get` and `~/.codex/config.toml`) naming the
//!    installation's command, and the agents' rows say connected with a working registration.
//! 5. `uninstall` from the unpacked archive (Windows cannot delete a running program, so never from the release):
//!    no job left in the manager and no definition, no process of the installation, no release, the agents'
//!    registrations gone from their configuration, and nothing in the data directory but what uninstall keeps on
//!    purpose (its locks and the stop: connector/README.md, "uninstall").
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

use serde_json::Value;

use crate::util::*;
use crate::{codex, Result};

/// The Claude Code CLI release installed, from npm (`@anthropic-ai/claude-code`). Bumped by hand, like any pin.
pub(crate) const CLAUDE_CODE_VERSION: &str = "2.1.308";
/// The core's default HTTP port (connector/README.md, "Where things are").
const CORE_PORT: u16 = 10829;
/// The core and connector jobs, by their base name (`service/jobs.rs`).
const JOBS: [&str; 2] = ["sidevoice-core", "sidevoice-connector"];
/// What uninstall leaves in the data directory on purpose: the locks and a person's stop.
const KEPT: [&str; 4] = [
    "install.lock",
    "connector.lock",
    "agents.lock",
    "node-stopped.json",
];

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
            Os::Windows => PathBuf::from(var("LOCALAPPDATA")?).join("Sidevoice"),
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
            Os::Macos => format!("dev.sidevoice.{}", job.trim_start_matches("sidevoice-")),
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
    let what = format!("{} {}", program.display(), args.join(" "));
    let mut child = Command::new(program)
        .args(args)
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
    let mut all = args.to_vec();
    all.push("--json");
    println!("$ sidevoice-connector {}", all.join(" "));
    let ran = run_path(program, &all, limit)?;
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

    step("the agents' CLIs, installed with npm and no login");
    run_ok(
        "npm",
        &[
            "install",
            "--global",
            "--no-audit",
            "--no-fund",
            &format!("@anthropic-ai/claude-code@{CLAUDE_CODE_VERSION}"),
            &format!("@openai/codex@{}", codex::VERSION),
        ],
        Duration::from_secs(300),
    )?;
    for (cli, version) in [("claude", CLAUDE_CODE_VERSION), ("codex", codex::VERSION)] {
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

    step("the archive, unpacked as a person unpacks it");
    let (root, inventory) = crate::archive::unpack_checked(archive, scratch)?;
    let target = inventory["target"].as_str().unwrap_or_default();
    check(target == host_target()?, || {
        format!("{} is for {target}, not this machine", archive.display())
    })?;
    let package = root.join(crate::entrypoint(target));

    step("install");
    let installed = ok(
        sidevoice(
            &package,
            &["install", "--no-prompt"],
            Duration::from_secs(300),
        )?,
        "install",
    )?;
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
    let claude_listed = run_ok("claude", &["mcp", "list"], Duration::from_secs(120))?;
    println!("claude mcp list:\n{claude_listed}");
    check(claude_listed.contains("sidevoice:"), || {
        format!("claude mcp list does not list sidevoice: {claude_listed}")
    })?;
    let claude = machine
        .claude_config()?
        .ok_or("no ~/.claude.json after connecting Claude Code")?;
    let entry = &claude["mcpServers"]["sidevoice"];
    check(entry["command"] == command.as_str(), || {
        format!("~/.claude.json's sidevoice entry does not run {command}: {entry}")
    })?;
    let codex_entry = run_ok("codex", &["mcp", "get", "sidevoice", "--json"], LIMIT)?;
    println!("codex mcp get sidevoice: {}", codex_entry.trim());
    let codex = machine
        .codex_config()?
        .ok_or("no ~/.codex/config.toml after connecting Codex")?;
    let entry = codex
        .get("mcp_servers")
        .and_then(|servers| servers.get("sidevoice"))
        .ok_or("~/.codex/config.toml has no [mcp_servers.sidevoice]")?;
    check(
        entry.get("command").and_then(toml::Value::as_str) == Some(command.as_str()),
        || format!("~/.codex/config.toml's sidevoice entry does not run {command}: {entry}"),
    )?;
    let listed = ok(
        sidevoice(&stable, &["agents"], Duration::from_secs(120))?,
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

    step("uninstall, from the package");
    let removed = ok(
        sidevoice(&package, &["uninstall"], Duration::from_secs(300))?,
        "uninstall",
    )?;
    check(removed["state"] == "absent", || {
        format!("uninstall: {removed}")
    })?;
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
    if machine.data.exists() {
        let mut names: Vec<String> = fs::read_dir(&machine.data)
            .map_err(|error| format!("{}: {error}", machine.data.display()))?
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        println!("{} holds {names:?}", machine.data.display());
        let extra: Vec<&String> = names
            .iter()
            .filter(|name| !KEPT.contains(&name.as_str()))
            .collect();
        check(extra.is_empty(), || {
            format!("left in {}: {extra:?}", machine.data.display())
        })?;
    }
    let claude_listed = run_ok("claude", &["mcp", "list"], Duration::from_secs(120))?;
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
    let codex_entry = run("codex", &["mcp", "get", "sidevoice", "--json"], LIMIT)?;
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
