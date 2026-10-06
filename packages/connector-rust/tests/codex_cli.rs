//! Registration with the real Codex CLI (`cargo xtask codex`, pinned), confined to a private profile: the connector
//! registers itself and reads its registration back through Codex's own commands; an entry somebody else wrote and a
//! configuration Codex cannot read are refused and left exactly as they were.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use support::*;

fn codex_cli() -> Option<PathBuf> {
    let path = std::env::var_os("SIDEVOICE_TEST_CODEX")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/codex/node_modules/.bin/codex")
        });
    if path.is_file() {
        return Some(path.canonicalize().unwrap());
    }
    if std::env::var_os("CI").is_some() {
        panic!(
            "no Codex CLI at {}: run `cargo xtask codex` (or `cargo xtask fixtures`) before `cargo test`",
            path.display()
        );
    }
    eprintln!(
        "skipped: no Codex CLI at {} (run `cargo xtask codex`)",
        path.display()
    );
    None
}

struct Fixture {
    profile: Profile,
    codex: PathBuf,
}

impl Fixture {
    fn command(&self, program: &Path) -> Command {
        let mut command = self.profile.command(program);
        // The CLI is a Node script: it needs the caller's PATH to find `node`.
        command
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("SIDEVOICE_CODEX_BIN", &self.codex);
        command
    }

    /// `agents [<action> codex] --json`: one line of output, its JSON answer.
    fn connector(&self, action: &str, succeed: bool) -> Value {
        let mut command = self.command(Path::new(CONNECTOR));
        command.arg("agents");
        if action != "list" {
            command.args([action, "codex"]);
        }
        let output = command.arg("--json").output().unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            output.status.success(),
            succeed,
            "agents {action}: {text} {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(text.lines().count(), 1, "agents {action}: {text}");
        let answer: Value = serde_json::from_str(&text)
            .unwrap_or_else(|_| panic!("agents {action}: no JSON answer: {text}"));
        assert_eq!(answer["ok"], succeed, "{answer}");
        answer
    }

    fn codex(&self, args: &[&str]) -> String {
        let output = self.command(&self.codex).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "codex {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn registration(&self) -> Value {
        let answer = self.connector("list", true);
        answer["agents"]
            .as_array()
            .and_then(|agents| agents.iter().find(|agent| agent["id"] == "codex"))
            .map(|agent| agent["registration"].clone())
            .unwrap_or_else(|| panic!("no codex row: {answer}"))
    }
}

#[test]
fn the_connector_registers_with_the_real_codex_cli_and_leaves_foreign_entries_alone() {
    let Some(codex) = codex_cli() else { return };
    let fixture = Fixture {
        profile: Profile::new("codex"),
        codex,
    };
    let config = fixture.profile.root.join("codex/config.toml");

    assert_eq!(fixture.registration(), "not-connected");
    fixture.connector("connect", true);
    let entry: Value =
        serde_json::from_str(&fixture.codex(&["mcp", "get", "sidevoice", "--json"])).unwrap();
    let transport = &entry["transport"];
    assert_eq!(transport["type"], "stdio", "{entry}");
    assert_eq!(
        fs::canonicalize(transport["command"].as_str().unwrap()).unwrap(),
        fs::canonicalize(CONNECTOR).unwrap(),
        "{entry}"
    );
    assert_eq!(transport["args"], serde_json::json!(["mcp"]), "{entry}");
    assert_eq!(fixture.registration(), "connected");
    fixture.codex(&["mcp", "remove", "sidevoice"]);

    // An entry named sidevoice that somebody else wrote: refused, untouched.
    fixture.codex(&[
        "mcp",
        "add",
        "sidevoice",
        "--",
        "/bin/echo",
        "keep-existing",
    ]);
    let foreign = fs::read(&config).unwrap();
    let refused = fixture.connector("connect", false);
    assert_eq!(refused["error"]["key"], "agents.foreign", "{refused}");
    assert_eq!(fs::read(&config).unwrap(), foreign);
    fixture.codex(&["mcp", "remove", "sidevoice"]);

    // A configuration Codex itself cannot parse: refused, untouched.
    let invalid = b"[mcp_servers.sidevoice\ncommand = [broken\n";
    fs::write(&config, invalid).unwrap();
    fs::set_permissions(&config, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let refused = fixture.connector("connect", false);
    assert_eq!(refused["error"]["key"], "agents.invalid", "{refused}");
    assert_eq!(fs::read(&config).unwrap(), invalid);

    assert!(!fixture.profile.data().join("install.json").exists());
}
