//! Whether a registered MCP server works (`adapters::health`), against fake servers that are this test binary itself:
//! started with `SV_FAKE_MCP` set, it plays the server that variable names, so the same fixtures run on every OS (no
//! `sh` scripts). Its own small runner instead of libtest (`harness = false` in Cargo.toml): libtest writes its own
//! lines to stdout, where a server's protocol goes.

use serde_json::Value;
use sidevoice_connector::adapters::health::{check, Health};
use sidevoice_connector::adapters::registration::Launch;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::future::Future;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::{Duration, Instant};

/// An answer `initialize` may get: a version this handshake knows.
const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"fake","version":"1"}}}"#;
/// An answer `tools/list` may get.
const TOOLS: &str = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#;

type Test = fn() -> Pin<Box<dyn Future<Output = ()>>>;

fn main() {
    if let Ok(mode) = std::env::var("SV_FAKE_MCP") {
        fake(&mode);
    }
    let tests: [(&str, Test); 8] = [
        (
            "a_server_that_answers_the_handshake_works_and_is_stopped",
            || Box::pin(a_server_that_answers_the_handshake_works_and_is_stopped()),
        ),
        ("a_missing_command_is_not_found", || {
            Box::pin(a_missing_command_is_not_found())
        }),
        ("a_server_that_exits_early_says_its_status", || {
            Box::pin(a_server_that_exits_early_says_its_status())
        }),
        ("a_silent_server_times_out", || {
            Box::pin(a_silent_server_times_out())
        }),
        (
            "garbage_or_an_unknown_version_is_a_protocol_mismatch",
            || Box::pin(garbage_or_an_unknown_version_is_a_protocol_mismatch()),
        ),
        (
            "results_the_protocol_does_not_define_are_a_protocol_mismatch",
            || Box::pin(results_the_protocol_does_not_define_are_a_protocol_mismatch()),
        ),
        (
            "the_server_starts_in_the_working_directory_its_entry_names",
            || Box::pin(the_server_starts_in_the_working_directory_its_entry_names()),
        ),
        ("cancelling_stops_the_check", || {
            Box::pin(cancelling_stops_the_check())
        }),
    ];
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--list") {
        for (name, _) in &tests {
            println!("{name}: test");
        }
        return;
    }
    let filters: Vec<&String> = args.iter().filter(|arg| !arg.starts_with('-')).collect();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut passed = 0;
    for (name, test) in tests {
        if !filters.is_empty() && !filters.iter().any(|filter| name.contains(filter.as_str())) {
            continue;
        }
        print!("test {name} ... ");
        std::io::stdout().flush().unwrap();
        // A failing test panics out of `main`: the binary fails, after its name.
        runtime.block_on(test());
        println!("ok");
        passed += 1;
    }
    println!("\ntest result: ok. {passed} passed");
}

/// The fake server `mode` names, on this process's stdin and stdout; never returns.
///
/// - `serve`: answers `initialize` (with `SV_FAKE_VERSION`) and `tools/list`, sending a notification first, and logs
///   to `$LOG`, when set, what it read, its arguments, `HEALTH_MARKER` and `ENTRY_VALUE`, and to `$LOG.cwd` where it
///   runs.
/// - `answers`: answers `initialize` with `SV_FAKE_INITIALIZE` and `tools/list` with `SV_FAKE_TOOLS`, as given.
/// - `garbage`: answers `initialize` with a line that is not JSON.
/// - `refused`: answers `initialize` with a JSON-RPC error.
/// - `exit`: says why on stderr and exits with status 3.
/// - anything else: reads nothing and says nothing.
fn fake(mode: &str) -> ! {
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut read = || lines.next().and_then(Result::ok).unwrap_or_default();
    let say = |line: &str| {
        let mut stdout = std::io::stdout();
        writeln!(stdout, "{line}").unwrap();
        stdout.flush().unwrap();
    };
    let var = |name: &str| std::env::var(name).unwrap_or_default();
    match mode {
        "serve" => {
            let log = std::env::var_os("LOG").map(PathBuf::from);
            let mut logged = Vec::new();
            logged.push(read());
            let args: Vec<String> = std::env::args().skip(1).collect();
            logged.push(format!("args={}", args.join(" ")));
            logged.push(format!("marker={}", var("HEALTH_MARKER")));
            logged.push(format!("entry={}", var("ENTRY_VALUE")));
            if let Some(log) = &log {
                let cwd = std::env::current_dir().unwrap();
                std::fs::write(log.with_extension("cwd"), cwd.to_string_lossy().as_bytes())
                    .unwrap();
            }
            say(
                r#"{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":"hi"}}"#,
            );
            say(&format!(
                r#"{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"{}","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"fake","version":"1"}}}}}}"#,
                var("SV_FAKE_VERSION")
            ));
            logged.push(read());
            logged.push(read());
            if let Some(log) = &log {
                std::fs::write(log, logged.join("\n")).unwrap();
            }
            say(
                r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"voice_say","inputSchema":{"type":"object"}}]}}"#,
            );
        }
        "answers" => {
            read();
            say(&var("SV_FAKE_INITIALIZE"));
            read();
            read();
            say(&var("SV_FAKE_TOOLS"));
        }
        "garbage" => {
            read();
            say("Welcome!");
        }
        "refused" => {
            read();
            say(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"no"}}"#);
        }
        "exit" => {
            eprintln!("no daemon");
            std::process::exit(3);
        }
        _ => {}
    }
    std::thread::sleep(Duration::from_secs(30));
    std::process::exit(0)
}

/// This binary as the fake server `mode` names, started as an agent would with the argument `mcp`.
fn fake_server(mode: &str, env: &[(&str, &str)]) -> Launch {
    let mut entry: BTreeMap<String, String> = env
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    entry.insert("SV_FAKE_MCP".into(), mode.into());
    Launch {
        command: std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        args: vec!["mcp".into()],
        env: entry,
        cwd: None,
    }
}

async fn run(launch: &Launch, environment: &[(String, OsString)], limit: Duration) -> Health {
    check(launch, environment, limit, std::future::pending())
        .await
        .unwrap()
}

fn mismatch() -> Health {
    Health::NotStarting("protocol mismatch".into())
}

async fn a_server_that_answers_the_handshake_works_and_is_stopped() {
    let scratch = tempfile::tempdir().unwrap();
    let log = scratch.path().join("log");
    let launch = fake_server(
        "serve",
        &[
            ("SV_FAKE_VERSION", "2025-11-25"),
            ("ENTRY_VALUE", "from the entry"),
        ],
    );
    let environment = vec![
        ("LOG".to_owned(), log.clone().into_os_string()),
        ("HEALTH_MARKER".to_owned(), OsString::from("1")),
        ("ENTRY_VALUE".to_owned(), OsString::from("overridden")),
    ];
    let started = Instant::now();
    assert_eq!(
        run(&launch, &environment, Duration::from_secs(5)).await,
        Health::Working
    );
    // It does not wait for the server's sleep: the server is stopped as soon as it answered.
    assert!(started.elapsed() < Duration::from_secs(4));
    // Written before its last answer: there once the check has returned.
    let log = std::fs::read_to_string(log).unwrap();
    let lines = log.lines().collect::<Vec<_>>();
    let initialize: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(initialize["method"], "initialize");
    assert_eq!(
        initialize["params"]["clientInfo"]["name"],
        "sidevoice-health"
    );
    assert_eq!(initialize["params"]["protocolVersion"], "2025-11-25");
    assert_eq!(lines[1], "args=mcp");
    assert_eq!(lines[2], "marker=1");
    assert_eq!(lines[3], "entry=from the entry");
    let initialized: Value = serde_json::from_str(lines[4]).unwrap();
    assert_eq!(initialized["method"], "notifications/initialized");
    let list: Value = serde_json::from_str(lines[5]).unwrap();
    assert_eq!(list["method"], "tools/list");
    assert!(!log.contains("tools/call"));
}

async fn a_missing_command_is_not_found() {
    let scratch = tempfile::tempdir().unwrap();
    let mut launch = fake_server("serve", &[]);
    launch.command = scratch.path().join("absent").to_string_lossy().into_owned();
    assert_eq!(
        run(&launch, &[], Duration::from_secs(5)).await,
        Health::NotStarting("command not found".into())
    );
}

async fn a_server_that_exits_early_says_its_status() {
    assert_eq!(
        run(&fake_server("exit", &[]), &[], Duration::from_secs(5)).await,
        Health::NotStarting("exited with status 3".into())
    );
}

async fn a_silent_server_times_out() {
    let started = Instant::now();
    assert_eq!(
        run(&fake_server("silent", &[]), &[], Duration::from_millis(300)).await,
        Health::NotStarting("timed out".into())
    );
    assert!(started.elapsed() < Duration::from_secs(3));
}

async fn garbage_or_an_unknown_version_is_a_protocol_mismatch() {
    for launch in [
        fake_server("garbage", &[]),
        fake_server("serve", &[("SV_FAKE_VERSION", "1999-01-01")]),
        fake_server("refused", &[]),
    ] {
        assert_eq!(
            run(&launch, &[], Duration::from_secs(5)).await,
            mismatch(),
            "{:?}",
            launch.env
        );
    }
}

async fn results_the_protocol_does_not_define_are_a_protocol_mismatch() {
    for (name, initialize, tools) in [
        // Only the version: no capabilities, no serverInfo.
        (
            "bare",
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25"}}"#,
            TOOLS,
        ),
        // serverInfo without its version.
        (
            "nameless",
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"fake"}}}"#,
            TOOLS,
        ),
        (
            "null-tool",
            INITIALIZE,
            r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[null]}}"#,
        ),
        (
            "unnamed-tool",
            INITIALIZE,
            r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"inputSchema":{"type":"object"}}]}}"#,
        ),
        (
            "no-tools",
            INITIALIZE,
            r#"{"jsonrpc":"2.0","id":2,"result":{}}"#,
        ),
    ] {
        let launch = fake_server(
            "answers",
            &[("SV_FAKE_INITIALIZE", initialize), ("SV_FAKE_TOOLS", tools)],
        );
        assert_eq!(
            run(&launch, &[], Duration::from_secs(5)).await,
            mismatch(),
            "{name}"
        );
    }
    // The same answers, well formed, work.
    let launch = fake_server(
        "answers",
        &[("SV_FAKE_INITIALIZE", INITIALIZE), ("SV_FAKE_TOOLS", TOOLS)],
    );
    assert_eq!(
        run(&launch, &[], Duration::from_secs(5)).await,
        Health::Working
    );
}

async fn the_server_starts_in_the_working_directory_its_entry_names() {
    let scratch = tempfile::tempdir().unwrap();
    let log = scratch.path().join("log");
    let mut launch = fake_server("serve", &[("SV_FAKE_VERSION", "2025-11-25")]);
    let environment = vec![("LOG".to_owned(), log.clone().into_os_string())];
    let directory = scratch.path().join("work");
    std::fs::create_dir(&directory).unwrap();
    launch.cwd = Some(directory.clone());
    assert_eq!(
        run(&launch, &environment, Duration::from_secs(5)).await,
        Health::Working
    );
    let started_in = std::fs::read_to_string(log.with_extension("cwd")).unwrap();
    assert_eq!(
        Path::new(started_in.trim()).canonicalize().unwrap(),
        directory.canonicalize().unwrap()
    );
    launch.cwd = Some(scratch.path().join("missing"));
    assert_eq!(
        run(&launch, &environment, Duration::from_secs(5)).await,
        Health::NotStarting("working directory not found".into())
    );
}

async fn cancelling_stops_the_check() {
    let launch = fake_server("silent", &[]);
    let started = Instant::now();
    let result = check(
        &launch,
        &[],
        Duration::from_secs(5),
        tokio::time::sleep(Duration::from_millis(100)),
    )
    .await;
    assert!(result.is_err());
    assert!(started.elapsed() < Duration::from_secs(3));
}
