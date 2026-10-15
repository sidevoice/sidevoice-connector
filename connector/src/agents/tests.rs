use super::*;
use fs2::FileExt;
use std::fs::OpenOptions;
use std::os::unix::fs::{symlink, OpenOptionsExt, PermissionsExt};

#[test]
fn login_path_frame_ignores_banners_and_rejects_empty_or_incomplete_output() {
    let framed = "\u{1e}SIDEVOICE_PATH_BEGIN\u{1e}/tmp/bin:/usr/bin\u{1e}SIDEVOICE_PATH_END\u{1e}";
    assert_eq!(
        parse_login_path(&format!("Welcome!\n{framed}\n")),
        Some("/tmp/bin:/usr/bin".into())
    );
    assert_eq!(parse_login_path("Welcome!\n"), None);
    assert_eq!(
        parse_login_path("\u{1e}SIDEVOICE_PATH_BEGIN\u{1e}\u{1e}SIDEVOICE_PATH_END\u{1e}"),
        None
    );
    assert_eq!(
        parse_login_path("\u{1e}SIDEVOICE_PATH_BEGIN\u{1e}/usr/bin"),
        None
    );
    assert_eq!(
        parse_login_path("\u{1e}SIDEVOICE_PATH_BEGIN\u{1e}relative\u{1e}SIDEVOICE_PATH_END\u{1e}"),
        None
    );
}

#[tokio::test]
async fn command_output_limit_reports_truncation_instead_of_parsing_prefix() {
    let payload = vec![b'x'; 90_000];
    assert!(read_capped(&payload[..], 65_536).await.is_err());
    assert_eq!(
        read_capped(&payload[..], 4 * 1024 * 1024)
            .await
            .unwrap()
            .len(),
        payload.len()
    );
}

#[tokio::test]
async fn failed_watched_capture_hides_all_agents_and_recovery_reconciles_all() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let _no_cursor_override = EnvGuard::set("SIDEVOICE_CURSOR_BIN", std::ffi::OsStr::new(""));
    let bin = fixture.root.join("login-bin");
    private_mkdir(&bin);
    for name in ["claude", "cursor-agent"] {
        let program = bin.join(name);
        private_write(&program, b"#!/bin/sh\nexit 0\n");
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let shell = fixture.root.join("login-shell");
    private_write(&shell, format!("#!/bin/sh\nprintf '\\036SIDEVOICE_PATH_BEGIN\\036%s\\036SIDEVOICE_PATH_END\\036' '{}'\n", bin.display()).as_bytes());
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
    let _shell = EnvGuard::set("SHELL", shell.as_os_str());
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    assert_eq!(
        host.capture_login_path(
            &Cancellation::new(),
            Instant::now() + StdDuration::from_secs(5)
        )
        .await
        .unwrap(),
        Some(bin.to_string_lossy().into_owned())
    );
    let first = host.handle("agents.list", json!({"rescan":true})).await;
    assert!(listed_ids(&first).contains(&"claude".into()), "{first}");
    assert!(listed_ids(&first).contains(&"cursor".into()), "{first}");
    private_write(&shell, b"#!/bin/sh\nexit 1\n");
    let failed = host.handle("agents.list", json!({"watch":"cursor"})).await;
    assert!(listed_ids(&failed).is_empty(), "{failed}");
    let cached = host.handle("agents.list", json!({})).await;
    assert!(listed_ids(&cached).is_empty(), "{cached}");
    private_write(
        &shell,
        format!("#!/bin/sh\nprintf '\\036SIDEVOICE_PATH_BEGIN\\036%s\\036SIDEVOICE_PATH_END\\036' '{}'\n", bin.display()).as_bytes(),
    );
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
    let recovered = host.handle("agents.list", json!({"watch":"cursor"})).await;
    assert!(
        listed_ids(&recovered).contains(&"claude".into()),
        "{recovered}"
    );
    assert!(
        listed_ids(&recovered).contains(&"cursor".into()),
        "{recovered}"
    );
    host.shutdown().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn public_marker_evidence_names_kind_without_home_path() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let _no_cli = EnvGuard::set("SIDEVOICE_CURSOR_BIN", std::ffi::OsStr::new(""));
    private_mkdir(&fixture.profile.home.join(".local/share/cursor"));
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let listed = host.handle("agents.list", json!({"rescan":true})).await;
    assert_eq!(row(&listed, "cursor")["evidence"], json!([{"kind":"install_dir"}]));
    assert!(!row(&listed, "cursor")["evidence"].to_string().contains(&fixture.profile.home.to_string_lossy().to_string()));
    host.shutdown().await;
}

struct Fixture {
    root: PathBuf,
    profile: Profile,
    /// Cursor installed: its CLI, which `SIDEVOICE_CURSOR_BIN` names while the fixture lives.
    _cursor: EnvGuard,
    _shell: EnvGuard,
}

/// Agent tests take turns: an agent's CLI is found through process-wide variables (`SIDEVOICE_<AGENT>_BIN`) that some
/// of them set, and every scan resolves every agent, so two at once would run each other's stand-ins.
static AGENT_ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(crate) async fn agent_env() -> tokio::sync::MutexGuard<'static, ()> {
    AGENT_ENV.lock().await
}

/// The same turn, for a test with no runtime: every fixture installs a Cursor CLI stand-in through those variables.
fn agent_env_blocking() -> tokio::sync::MutexGuard<'static, ()> {
    AGENT_ENV.blocking_lock()
}

pub(crate) struct EnvGuard {
    key: String,
    previous: Option<std::ffi::OsString>,
}

impl EnvGuard {
    pub(crate) fn set(key: &str, value: &std::ffi::OsStr) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self {
            key: key.to_owned(),
            previous,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(&self.key, value),
            None => std::env::remove_var(&self.key),
        }
    }
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "sidevoice-agent-test-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        private_mkdir(&root);
        let root = root.canonicalize().unwrap();
        for name in ["home", "claude", "codex", "cursor", "xdg", "sidevoice"] {
            private_mkdir(&root.join(name));
        }
        for name in ["cursor/config", "cursor/data", "xdg/config", "xdg/data"] {
            private_mkdir(&root.join(name));
        }
        private_mkdir(&root.join("sidevoice/core"));
        let path = |child: &str| root.join(child).into_os_string();
        let variables = [
            ("HOME", path("home")),
            ("CLAUDE_CONFIG_DIR", path("claude")),
            ("CODEX_HOME", path("codex")),
            ("CURSOR_CONFIG_DIR", path("cursor/config")),
            ("CURSOR_DATA_DIR", path("cursor/data")),
            ("XDG_CONFIG_HOME", path("xdg/config")),
            ("XDG_DATA_HOME", path("xdg/data")),
            ("SIDEVOICE_DATA_DIR", path("sidevoice")),
        ];
        let profile = Profile::from_vars(
            |key| {
                variables
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| value.clone())
            },
            &crate::adapters::Registry::standard().homes(),
        )
        .unwrap();
        let cursor = root.join("cursor-bin/cursor-agent");
        private_mkdir(cursor.parent().unwrap());
        private_write(&cursor, b"#!/bin/sh\nexit 0\n");
        fs::set_permissions(&cursor, fs::Permissions::from_mode(0o700)).unwrap();
        let _cursor = EnvGuard::set("SIDEVOICE_CURSOR_BIN", cursor.as_os_str());
        let shell = root.join("fixture-login-shell");
        private_write(&shell, b"#!/bin/sh\nprintf '\\036SIDEVOICE_PATH_BEGIN\\036%s\\036SIDEVOICE_PATH_END\\036' /usr/bin\n");
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
        let _shell = EnvGuard::set("SHELL", shell.as_os_str());
        Self {
            root,
            profile,
            _cursor,
            _shell,
        }
    }

    fn cursor(&self) -> PathBuf {
        self.root.join("cursor-bin/cursor-agent")
    }

    fn selected(&self) -> InstalledCommand {
        let releases = self.root.join("selected");
        InstalledCommand::selected(
            vec![releases
                .join("current/dist/sidevoice")
                .to_string_lossy()
                .into_owned()],
            releases,
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn private_mkdir(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn private_write(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn write_json(path: &Path, value: &Value) {
    private_write(path, &serde_json::to_vec_pretty(value).unwrap());
}

/// Points the agent home `var` at `path`, as if the person's environment set it (`set`) or left it to its default.
fn set_agent_home(profile: &mut Profile, var: &str, path: &Path, set: bool) {
    let home = profile
        .agent_homes
        .iter_mut()
        .find(|home| home.spec.var == var)
        .unwrap();
    home.path = path.to_owned();
    home.set = set;
}

fn row<'a>(answer: &'a Value, id: &str) -> &'a Value {
    answer["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["id"] == id)
        .unwrap()
}

fn identity_signature(id: &str, version: Option<&str>, evidence: &[Evidence<'_>], binary: Option<&str>) -> String {
    let identity = format!(
        "{{\"id\":{},\"version\":{},\"evidence\":{},\"binary\":{}}}",
        serde_json::to_string(id).unwrap(),
        serde_json::to_string(&version).unwrap(),
        serde_json::to_string(evidence).unwrap(),
        serde_json::to_string(&binary).unwrap(),
    );
    hex::encode(Sha256::digest(identity.as_bytes()))
}

#[test]
fn agent_refusals_labels_and_manual_text_use_the_shared_english_bundle() {
    assert_eq!(
        message("agents.invalid", &json!({"agent":"Codex"})),
        "Codex has an unreadable or malformed Sidevoice registration; it was left unchanged."
    );
    assert_eq!(label("claude"), "Claude Code");
    assert_eq!(
        message("agents.manual.delete-entry", &json!({"under":"mcpServers"})),
        "Delete the \"sidevoice\" entry under \"mcpServers\"."
    );
    assert_eq!(
        message(
            "agents.manual.delete-other-entry",
            &json!({"under":"mcpServers"})
        ),
        "Delete the other \"sidevoice\" entry under \"mcpServers\"."
    );
}

#[test]
fn ownership_is_exact_and_js_release_fixtures_stay_narrow() {
    let _turn = agent_env_blocking();
    let fixture = Fixture::new();
    let own = InstalledCommand::executable().unwrap();
    assert_eq!(own.args, ["mcp"]);
    assert!(own.owns(&own.command, &own.args));
    assert!(!own.owns(
        &own.command,
        &["mcp".into(), "--profile-root".into(), "/tmp/other".into()]
    ));
    assert!(!own.owns(&own.command, &["connector".into()]));
    assert!(!own.owns("/tmp/other/sidevoice", &own.args));

    let releases = fixture.root.join("js-copies");
    let node_cli = releases.join("current/dist/cli.mjs");
    let node = InstalledCommand::selected(
        vec![
            "/usr/bin/node".into(),
            node_cli.to_string_lossy().into_owned(),
        ],
        releases.clone(),
    );
    assert!(node.owns(
        "/usr/local/bin/node",
        &[node_cli.to_string_lossy().into_owned(), "mcp".into()],
    ));
    let old_native = releases.join("releases/1.2.3/dist/sidevoice");
    let native = InstalledCommand::selected(
        vec![releases
            .join("current/dist/sidevoice")
            .to_string_lossy()
            .into_owned()],
        releases.clone(),
    );
    assert!(native.owns(&old_native.to_string_lossy(), &["mcp".into()]));
    assert!(!native.owns("/tmp/other/dist/sidevoice", &["mcp".into()]));
}

#[tokio::test]
async fn cursor_actions_preserve_js_state_and_refuse_foreign_invalid_and_escaped_files() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let old = fixture.root.join("selected/releases/1.2.3/dist/sidevoice");
    let current = fixture.root.join("selected/current/dist/sidevoice");
    let config_path = fixture
        .profile
        .agent_home("CURSOR_CONFIG_DIR")
        .join("mcp.json");
    write_json(
        &config_path,
        &json!({
            "editor":{"theme":"dark"},
            "mcpServers":{
                "other":{"command":"other","args":["keep"],"private":"untouched"},
                "sidevoice":{"command":old,"args":["mcp"],"description":"preserve me"}
            }
        }),
    );
    let evidence = vec![Evidence { kind: "cli" }];
    let cursor = fixture.cursor().to_string_lossy().into_owned();
    let signature = identity_signature("cursor", None, &evidence, Some(&cursor));
    write_json(
        &fixture.profile.data.join("agents.json"),
        &json!({
            "version":1,
            "scanned_at":"2026-10-03T00:00:00.000Z",
            "login_path":"/usr/bin",
            "binaries":{},
            "seen":{"cursor":{
                "present":true,"signature":signature,"generation":"same-generation",
                "detected_at":"2026-10-02T00:00:00.000Z",
                "agent":{"id":"cursor","label":"Cursor","version":null,
                    "registration":"not-connected","connect":"auto","instructions":{},
                    "evidence":evidence,"present":true,"dismissed":false,"actionable":false}
            }},
            "dismissed":{},
            "custom_notes":{"must":"survive rewrites"}
        }),
    );

    let listed = host
        .handle("agents.list", json!({"rescan":true,"watch":"cursor"}))
        .await;
    // The entry names an older release of ours: told apart, still ours, and not a new agent to offer.
    assert_eq!(row(&listed, "cursor")["registration"], "outdated");
    assert_eq!(row(&listed, "cursor")["owned"], true);
    assert_eq!(row(&listed, "cursor")["dismissed"], false);
    assert_eq!(row(&listed, "cursor")["actionable"], false);
    let mut store: Value =
        serde_json::from_slice(&fs::read(fixture.profile.data.join("agents.json")).unwrap())
            .unwrap();
    store["dismissed"]["cursor"] = json!("same-generation");
    write_json(&fixture.profile.data.join("agents.json"), &store);
    let dismissed = host.handle("agents.dismiss", json!({"id":"cursor"})).await;
    assert_eq!(row(&dismissed, "cursor")["dismissed"], true);
    assert_eq!(row(&dismissed, "cursor")["actionable"], false);

    let connected = host.handle("agents.connect", json!({"id":"cursor"})).await;
    assert!(connected.get("error").is_none(), "{connected}");
    assert_eq!(row(&connected, "cursor")["registration"], "connected");
    assert_eq!(row(&connected, "cursor")["dismissed"], false);
    let saved_config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    assert_eq!(
        saved_config.pointer("/mcpServers/sidevoice/command"),
        Some(&json!(current))
    );
    assert_eq!(
        saved_config.pointer("/mcpServers/sidevoice/description"),
        Some(&json!("preserve me"))
    );
    assert_eq!(
        saved_config.pointer("/mcpServers/other/private"),
        Some(&json!("untouched"))
    );
    assert_eq!(saved_config["editor"]["theme"], "dark");

    let disconnected = host
        .handle("agents.disconnect", json!({"id":"cursor"}))
        .await;
    assert!(disconnected.get("error").is_none(), "{disconnected}");
    assert_eq!(
        row(&disconnected, "cursor")["registration"],
        "not-connected"
    );
    let saved_config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    assert!(saved_config.pointer("/mcpServers/sidevoice").is_none());
    assert_eq!(
        saved_config.pointer("/mcpServers/other/private"),
        Some(&json!("untouched"))
    );
    let saved_store: Value =
        serde_json::from_slice(&fs::read(fixture.profile.data.join("agents.json")).unwrap())
            .unwrap();
    assert_eq!(saved_store["custom_notes"]["must"], "survive rewrites");

    let foreign = json!({"mcpServers":{
        "sidevoice":{"command":"/tmp/unrelated","args":["mcp"],"token":"do not expose"},
        "other":{"command":"keep","args":[]}
    }});
    write_json(&config_path, &foreign);
    let foreign_before = fs::read(&config_path).unwrap();
    let refusal = host
        .handle("agents.connect", json!({"id":"cursor","path":"/tmp/evil"}))
        .await;
    assert_eq!(
        refusal.pointer("/error/key"),
        Some(&json!("agents.foreign"))
    );
    assert_eq!(fs::read(&config_path).unwrap(), foreign_before);
    assert!(refusal.to_string().contains("agents.foreign"));
    assert!(!refusal.to_string().contains("do not expose"));
    assert!(!refusal.to_string().contains("/tmp/evil"));

    private_write(&config_path, b"{bad JSON");
    let invalid_before = fs::read(&config_path).unwrap();
    let invalid = host.handle("agents.connect", json!({"id":"cursor"})).await;
    assert_eq!(
        invalid.pointer("/error/key"),
        Some(&json!("agents.invalid"))
    );
    assert_eq!(fs::read(&config_path).unwrap(), invalid_before);

    let escaped = fixture.root.parent().unwrap().join(format!(
        "sidevoice-agent-escaped-{}-{}.json",
        std::process::id(),
        Uuid::new_v4()
    ));
    private_write(&escaped, br#"{"must":"stay"}"#);
    fs::remove_file(&config_path).unwrap();
    symlink(&escaped, &config_path).unwrap();
    let escaped_before = fs::read(&escaped).unwrap();
    let escaped_result = host.handle("agents.connect", json!({"id":"cursor"})).await;
    assert_eq!(
        escaped_result.pointer("/error/key"),
        Some(&json!("agents.invalid"))
    );
    assert_eq!(fs::read(&escaped).unwrap(), escaped_before);
    let _ = fs::remove_file(escaped);
    host.shutdown().await;
}

fn listed_ids(answer: &Value) -> Vec<String> {
    answer["agents"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|agent| agent["id"].as_str().map(str::to_owned))
        .collect()
}

/// Per agent: its home, even with a Sidevoice entry an uninstall left in it, is not the agent installed; its program
/// is. A machine with only Cursor installed lists Cursor, and the way to connect any other agent by hand.
#[tokio::test]
async fn an_agent_is_listed_when_its_program_is_installed_not_for_its_home_alone() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    // The fixture login PATH contains no fixture programs; test-only overrides supply each program.
    write_json(
        &fixture
            .profile
            .agent_home("CLAUDE_CONFIG_DIR")
            .join(".claude.json"),
        &json!({"mcpServers":{"sidevoice":{"command":"sidevoice","args":["mcp"]}}}),
    );
    private_write(
        &fixture.profile.agent_home("CODEX_HOME").join("config.toml"),
        b"[mcp_servers.sidevoice]\ncommand = \"sidevoice\"\nargs = [\"mcp\"]\n",
    );
    write_json(
        &fixture
            .profile
            .agent_home("CURSOR_CONFIG_DIR")
            .join("mcp.json"),
        &json!({"mcpServers":{"sidevoice":{"command":"sidevoice","args":["mcp"]}}}),
    );
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let claude = fake_claude(&fixture);
    let codex = fake_codex(&fixture);
    let cursor = fixture.cursor();
    for (id, program) in [("claude", &claude), ("codex", &codex), ("cursor", &cursor)] {
        let variable = override_name(id);
        // Its home and a leftover entry, no program: not installed.
        let away = fixture.root.join(format!("{id}-uninstalled"));
        fs::rename(program, &away).unwrap();
        std::env::set_var(&variable, "");
        let missing = host.handle("agents.list", json!({"rescan":true})).await;
        fs::rename(&away, program).unwrap();
        assert!(missing.get("error").is_none(), "{missing}");
        assert!(
            !listed_ids(&missing).contains(&id.to_owned()),
            "{id} is listed for its home alone: {missing}"
        );
        // Its program: installed, and said why.
        std::env::set_var(&variable, program);
        let found = host.handle("agents.list", json!({"rescan":true})).await;
        assert_eq!(row(&found, id)["present"], true, "{found}");
        assert!(
            row(&found, id)["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["kind"] == "cli"),
            "{found}"
        );
        std::env::remove_var(&variable);
    }

    // Only Cursor installed (Claude Code and Codex uninstalled, their homes left): Cursor, and Other (`custom`) to
    // connect any agent by hand.
    fs::remove_file(&claude).unwrap();
    fs::remove_file(&codex).unwrap();
    std::env::set_var(override_name("cursor"), &cursor);
    let listed = host.handle("agents.list", json!({"rescan":true})).await;
    for id in ["claude", "codex"] {
        assert!(!listed_ids(&listed).contains(&id.to_owned()), "{listed}");
    }
    assert!(
        listed_ids(&listed).contains(&"cursor".to_owned()),
        "{listed}"
    );
    assert!(listed["custom"]["command"].is_string(), "{listed}");
    assert!(listed["custom"]["snippet"].is_string(), "{listed}");
    host.shutdown().await;
}

#[tokio::test]
async fn uninstalled_and_reinstalled_agent_gets_a_new_generation() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    private_write(
        &fixture.profile.data.join("agents.json"),
        br#"{"version":1,"scanned_at":"old","login_path":"/usr/bin","binaries":{},"seen":{},"dismissed":{}}"#,
    );
    let first = host
        .handle("agents.list", json!({"watch":"cursor","rescan":true}))
        .await;
    assert_eq!(row(&first, "cursor")["present"], true);
    let state: Value =
        serde_json::from_slice(&fs::read(fixture.profile.data.join("agents.json")).unwrap())
            .unwrap();
    let first_generation = state["seen"]["cursor"]["generation"]
        .as_str()
        .unwrap()
        .to_owned();
    // Uninstalled: its program gone, its home left behind.
    let program = fs::read(fixture.cursor()).unwrap();
    fs::remove_file(fixture.cursor()).unwrap();
    let absent = host
        .handle("agents.list", json!({"watch":"cursor","rescan":true}))
        .await;
    assert!(absent["agents"]
        .as_array()
        .unwrap()
        .iter()
        .all(|agent| agent["id"] != "cursor"));
    let state: Value =
        serde_json::from_slice(&fs::read(fixture.profile.data.join("agents.json")).unwrap())
            .unwrap();
    assert_eq!(state["seen"]["cursor"]["present"], false);
    private_write(&fixture.cursor(), &program);
    fs::set_permissions(fixture.cursor(), fs::Permissions::from_mode(0o700)).unwrap();
    let returned = host
        .handle("agents.list", json!({"watch":"cursor","rescan":true}))
        .await;
    assert_eq!(row(&returned, "cursor")["present"], true);
    let state: Value =
        serde_json::from_slice(&fs::read(fixture.profile.data.join("agents.json")).unwrap())
            .unwrap();
    assert_ne!(state["seen"]["cursor"]["generation"], first_generation);
    host.shutdown().await;
}

#[tokio::test]
async fn replacing_cursor_profile_with_symlink_refuses_connect_without_touching_target() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let external = fixture.root.parent().unwrap().join(format!(
        "sidevoice-agent-real-cursor-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    private_mkdir(&external);
    let config = external.join("mcp.json");
    private_write(
        &config,
        br#"{"mcpServers":{"other":{"command":"keep-me"}}}"#,
    );
    let before = fs::read(&config).unwrap();
    fs::remove_dir_all(fixture.profile.agent_home("CURSOR_CONFIG_DIR")).unwrap();
    symlink(&external, fixture.profile.agent_home("CURSOR_CONFIG_DIR")).unwrap();

    let answer = host.handle("agents.connect", json!({"id":"cursor"})).await;
    assert_eq!(answer.pointer("/error/key"), Some(&json!("agents.invalid")));
    assert_eq!(fs::read(&config).unwrap(), before);
    host.shutdown().await;
    let _ = fs::remove_dir_all(external);
}

#[tokio::test]
async fn replacing_codex_profile_with_symlink_refuses_connect_before_cli_spawn() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let executable = fixture.root.join("bin/codex");
    private_mkdir(executable.parent().unwrap());
    let marker = fixture.root.join("codex-was-started");
    private_write(
        &executable,
        format!("#!/bin/sh\nprintf started > '{}'\n", marker.display()).as_bytes(),
    );
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let _binary = EnvGuard::set("SIDEVOICE_CODEX_BIN", executable.as_os_str());
    let mut state = empty_state();
    state["binaries"]["codex"] = json!(executable);
    write_json(&fixture.profile.data.join("agents.json"), &state);
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let external = fixture.root.parent().unwrap().join(format!(
        "sidevoice-agent-real-codex-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    private_mkdir(&external);
    let config = external.join("config.toml");
    private_write(&config, b"[mcp_servers.other]\ncommand = 'keep-me'\n");
    let before = fs::read(&config).unwrap();
    fs::remove_dir_all(fixture.profile.agent_home("CODEX_HOME")).unwrap();
    symlink(&external, fixture.profile.agent_home("CODEX_HOME")).unwrap();

    let answer = host.handle("agents.connect", json!({"id":"codex"})).await;
    assert_eq!(
        answer.pointer("/error/key"),
        Some(&json!("agents.not-present"))
    );
    assert!(
        !marker.exists(),
        "Codex CLI ran with a replaced private profile"
    );
    assert_eq!(fs::read(&config).unwrap(), before);

    // The spawn boundary also rejects replacement after binary resolution by an earlier request.
    let direct = run_command(
        &fixture.profile,
        Some(fixture.profile.agent_home("CODEX_HOME")),
        executable.to_str().unwrap(),
        &["mcp".into(), "get".into(), "sidevoice".into()],
        &Cancellation::new(),
        Instant::now() + StdDuration::from_secs(1),
    )
    .await;
    assert!(matches!(direct, Err(CommandFailure::Start)));
    assert!(
        !marker.exists(),
        "CLI spawn boundary accepted a replaced profile"
    );
    host.shutdown().await;
    let _ = fs::remove_dir_all(external);
}

#[tokio::test]
async fn dangling_codex_config_symlink_is_invalid_not_absent() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let codex = fake_codex(&fixture);
    let _codex = EnvGuard::set("SIDEVOICE_CODEX_BIN", codex.as_os_str());
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let missing = fixture.root.join("missing-config-target.toml");
    symlink(
        missing,
        fixture.profile.agent_home("CODEX_HOME").join("config.toml"),
    )
    .unwrap();
    let answer = host.handle("agents.connect", json!({"id":"codex"})).await;
    assert_eq!(answer.pointer("/error/key"), Some(&json!("agents.invalid")));
    host.shutdown().await;
}

/// Dotfiles managers link `config.toml` from their own directory: the link is followed, and the file it leads to is
/// judged as `config.toml` itself would be. Ours reads `connected`, another `foreign`; one that cannot be parsed,
/// one others may write, and a FIFO are `invalid` (the FIFO without waiting on it), and are left as they are.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_linked_codex_config_is_judged_where_the_link_leads() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let binary = fake_codex(&fixture);
    let _codex = EnvGuard::set("SIDEVOICE_CODEX_BIN", binary.as_os_str());
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let selected = fixture.selected();
    codex_entry(&fixture, &selected, true, installation_env(&fixture));
    let config = fixture.profile.agent_home("CODEX_HOME").join("config.toml");
    let dotfiles = fixture.root.join("dotfiles");
    private_mkdir(&dotfiles);
    let target = dotfiles.join("config.toml");
    fs::rename(&config, &target).unwrap();
    symlink(&target, &config).unwrap();
    // A scan that must answer, never wait on what the link leads to.
    let list = |host: std::sync::Arc<HostAgents>| async move {
        let scan =
            tokio::spawn(async move { host.handle("agents.list", json!({"rescan":true})).await });
        tokio::time::timeout(StdDuration::from_secs(20), scan)
            .await
            .expect("the scan waited on the linked file")
            .unwrap()
    };

    let listed = list(host.clone()).await;
    assert_eq!(
        row(&listed, "codex")["registration"],
        "connected",
        "{listed}"
    );

    private_write(
        &target,
        b"[mcp_servers.sidevoice]\ncommand = \"/elsewhere/server\"\nargs = []\n",
    );
    let listed = list(host.clone()).await;
    assert_eq!(row(&listed, "codex")["registration"], "foreign", "{listed}");

    let malformed = b"[mcp_servers.sidevoice\ncommand = ";
    private_write(&target, malformed);
    let listed = list(host.clone()).await;
    assert_eq!(row(&listed, "codex")["registration"], "invalid", "{listed}");
    for action in ["agents.connect", "agents.disconnect"] {
        let answer = host.handle(action, json!({"id":"codex"})).await;
        assert_eq!(
            answer.pointer("/error/key"),
            Some(&json!("agents.invalid")),
            "{action}: {answer}"
        );
    }
    assert_eq!(fs::read(&target).unwrap(), malformed);

    fs::write(&target, b"").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o620)).unwrap();
    let listed = list(host.clone()).await;
    assert_eq!(
        row(&listed, "codex")["registration"],
        "invalid",
        "group-writable: {listed}"
    );

    fs::remove_file(&target).unwrap();
    let fifo = std::ffi::CString::new(target.to_string_lossy().into_owned()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let listed = list(host.clone()).await;
    assert_eq!(
        row(&listed, "codex")["registration"],
        "invalid",
        "FIFO: {listed}"
    );
    host.shutdown().await;
}

#[tokio::test]
async fn unparsable_settings_are_listed_invalid_with_their_file_and_left_alone() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let codex = fake_codex(&fixture);
    let claude = fake_claude(&fixture);
    let _codex = EnvGuard::set("SIDEVOICE_CODEX_BIN", codex.as_os_str());
    let _claude = EnvGuard::set("SIDEVOICE_CLAUDE_BIN", claude.as_os_str());
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let files = [
        (
            "claude",
            fixture
                .profile
                .agent_home("CLAUDE_CONFIG_DIR")
                .join(".claude.json"),
            &b"{\"mcpServers\": {"[..],
        ),
        (
            "codex",
            fixture.profile.agent_home("CODEX_HOME").join("config.toml"),
            &b"[mcp_servers.sidevoice\ncommand = "[..],
        ),
        (
            "cursor",
            fixture
                .profile
                .agent_home("CURSOR_CONFIG_DIR")
                .join("mcp.json"),
            &b"{\"mcpServers\": {\"sidevoice\": "[..],
        ),
    ];
    for (_, file, bytes) in &files {
        private_write(file, bytes);
    }

    // A whole scan still answers: each agent says its settings cannot be read, and where they are.
    let listed = host.handle("agents.list", json!({"rescan":true})).await;
    assert!(listed.get("error").is_none(), "{listed}");
    for (id, file, _) in &files {
        let agent = row(&listed, id);
        assert_eq!(agent["registration"], "invalid", "{agent}");
        assert_eq!(agent["owned"], false);
        assert_eq!(agent["actionable"], false);
        assert_eq!(
            agent.pointer("/instructions/file"),
            Some(&json!(file.to_string_lossy()))
        );
    }

    for (id, _, _) in &files {
        for action in ["agents.connect", "agents.disconnect"] {
            let answer = host.handle(action, json!({"id":id})).await;
            assert_eq!(
                answer.pointer("/error/key"),
                Some(&json!("agents.invalid")),
                "{action} {id}: {answer}"
            );
        }
    }
    for (id, file, bytes) in &files {
        assert_eq!(&fs::read(file).unwrap(), bytes, "{id}");
    }
    for home in [
        fixture.profile.agent_home("CLAUDE_CONFIG_DIR"),
        fixture.profile.agent_home("CODEX_HOME"),
    ] {
        let calls = fs::read_to_string(home.join("calls")).unwrap_or_default();
        assert!(
            !calls.contains("mcp add") && !calls.contains("mcp remove"),
            "an agent's CLI was asked to write over unreadable settings: {calls}"
        );
    }
    host.shutdown().await;
}

/// Claude Code reads its user configuration from `~/.claude.json` unless the person's environment sets
/// `CLAUDE_CONFIG_DIR`, and from `$CLAUDE_CONFIG_DIR/.claude.json` when it does: that file is the one judged and named.
#[tokio::test]
async fn claude_settings_are_judged_where_claude_code_reads_them() {
    let _turn = agent_env().await;
    let mut fixture = Fixture::new();
    let claude_home = fixture.profile.home.join(".claude");
    set_agent_home(
        &mut fixture.profile,
        "CLAUDE_CONFIG_DIR",
        &claude_home,
        false,
    );
    private_mkdir(&claude_home);
    let claude = fake_claude(&fixture);
    let _claude = EnvGuard::set("SIDEVOICE_CLAUDE_BIN", claude.as_os_str());
    let in_home = fixture.profile.home.join(".claude.json");
    let in_config_dir = claude_home.join(".claude.json");
    let file = |listed: &Value| row(listed, "claude").pointer("/instructions/file").cloned();

    // Unset: `~/.claude.json`. A broken file inside `~/.claude` is not Claude Code's and is not read.
    private_write(&in_config_dir, b"{\"mcpServers\": {");
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let listed = host.handle("agents.list", json!({"rescan":true})).await;
    assert_eq!(
        row(&listed, "claude")["registration"],
        "not-connected",
        "{listed}"
    );
    assert_eq!(file(&listed), Some(json!(in_home.to_string_lossy())));
    private_write(&in_home, b"{\"mcpServers\": {");
    let listed = host.handle("agents.list", json!({"rescan":true})).await;
    assert_eq!(
        row(&listed, "claude")["registration"],
        "invalid",
        "{listed}"
    );
    host.shutdown().await;

    // Set: `$CLAUDE_CONFIG_DIR/.claude.json`, whatever `~/.claude.json` holds.
    set_agent_home(
        &mut fixture.profile,
        "CLAUDE_CONFIG_DIR",
        &claude_home,
        true,
    );
    write_json(&in_config_dir, &json!({}));
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let listed = host.handle("agents.list", json!({"rescan":true})).await;
    assert_eq!(
        row(&listed, "claude")["registration"],
        "not-connected",
        "{listed}"
    );
    assert_eq!(file(&listed), Some(json!(in_config_dir.to_string_lossy())));
    private_write(&in_config_dir, b"{\"mcpServers\": {");
    let listed = host.handle("agents.list", json!({"rescan":true})).await;
    assert_eq!(
        row(&listed, "claude")["registration"],
        "invalid",
        "{listed}"
    );
    host.shutdown().await;
}

/// Without its data directory (`~/.claude`, the variable unset), Claude Code's unparsable `~/.claude.json` is still
/// `invalid`: Connect and Disconnect refuse, its CLI is never asked, and the file is left as it was.
#[tokio::test]
async fn unparsable_claude_settings_are_left_alone_without_its_data_directory() {
    let _turn = agent_env().await;
    let mut fixture = Fixture::new();
    let claude_home = fixture.profile.home.join(".claude");
    set_agent_home(
        &mut fixture.profile,
        "CLAUDE_CONFIG_DIR",
        &claude_home,
        false,
    );
    let claude = fake_claude(&fixture);
    let _claude = EnvGuard::set("SIDEVOICE_CLAUDE_BIN", claude.as_os_str());
    let file = fixture.profile.home.join(".claude.json");
    let bytes = b"{\"mcpServers\": {";
    private_write(&file, bytes);
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());

    let listed = host.handle("agents.list", json!({"rescan":true})).await;
    assert_eq!(
        row(&listed, "claude")["registration"],
        "invalid",
        "{listed}"
    );
    for action in ["agents.connect", "agents.disconnect"] {
        let answer = host.handle(action, json!({"id":"claude"})).await;
        assert_eq!(
            answer.pointer("/error/key"),
            Some(&json!("agents.invalid")),
            "{action}: {answer}"
        );
    }
    assert_eq!(fs::read(&file).unwrap(), bytes);
    assert!(!claude_home.exists(), "Claude Code's CLI was run");
    host.shutdown().await;
}
#[tokio::test]
async fn complete_request_gate_orders_scans_mutations_and_external_dismissals() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let binary = fixture.root.join("bin/codex");
    private_mkdir(binary.parent().unwrap());
    private_write(
        &binary,
        br##"#!/bin/sh
home="$CODEX_HOME"
state_file="$home/fake-entry.json"
config_file="$home/config.toml"
quote() { printf '"%s"' "$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g')"; }
if [ "$#" -eq 1 ] && [ "$1" = "--version" ]; then
    echo 'codex fixture 1'
    exit 0
fi
if [ "$1 $2 $3" = "mcp get sidevoice" ]; then
    if [ -e "$home/timeout-next-get" ]; then
        rm -f "$home/timeout-next-get"
        sleep 4
    fi
    if [ -e "$home/slow-next-get" ]; then
        rm -f "$home/slow-next-get"
        printf '%s' "$$" > "$home/slow-get-started"
        sleep 0.8
    fi
    if [ ! -e "$state_file" ]; then
        echo 'No such server: sidevoice' >&2
        exit 1
    fi
    cat "$state_file"
    exit 0
fi
if [ "$1 $2 $3" = "mcp remove sidevoice" ]; then
    rm -f "$state_file" "$config_file"
    if [ -e "$home/timeout-after-remove" ]; then
        rm -f "$home/timeout-after-remove"
        printf '1' > "$home/timeout-next-get"
    fi
    exit 0
fi
if [ "$1 $2 $3" = "mcp add sidevoice" ]; then
    shift 3
    env=""
    while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do
        if [ "$1" = "--env" ]; then
            shift
            if [ -n "$env" ]; then env="$env, "; fi
            env="$env$(quote "${1%%=*}"): $(quote "${1#*=}")"
        fi
        shift
    done
    if [ "$#" -lt 2 ]; then
        echo 'unsupported fixture command' >&2
        exit 2
    fi
    shift
    command="$1"
    shift
    args=""
    for arg in "$@"; do
        if [ -n "$args" ]; then args="$args, "; fi
        args="$args$(quote "$arg")"
    done
    if [ -e "$home/leave-unregistered-after-add" ]; then
        rm -f "$home/leave-unregistered-after-add"
        exit 0
    fi
    if [ -e "$home/slow-next-add" ]; then
        rm -f "$home/slow-next-add"
        printf '%s' "$$" > "$home/slow-add-started"
        sleep 0.8
    fi
    printf '{"name": "sidevoice", "transport": {"type": "stdio", "command": %s, "args": [%s], "env": {%s}}, "enabled": true}' \
        "$(quote "$command")" "$args" "$env" > "$state_file"
    printf '[mcp_servers.sidevoice]\ncommand = %s\nargs = [%s]\n' "$(quote "$command")" "$args" > "$config_file"
    exit 0
fi
echo 'unsupported fixture command' >&2
exit 2
"##,
    );
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let _codex_override = EnvGuard::set("SIDEVOICE_CODEX_BIN", binary.as_os_str());
    let mut initial = empty_state();
    initial["login_path"] = json!("/usr/bin");
    write_json(&fixture.profile.data.join("agents.json"), &initial);
    let host = HostAgents::new(fixture.profile.clone()).unwrap();

    let started = fixture
        .profile
        .agent_home("CODEX_HOME")
        .join("slow-get-started");
    private_write(
        &fixture
            .profile
            .agent_home("CODEX_HOME")
            .join("slow-next-get"),
        b"1",
    );
    let listing_host = host.clone();
    let listing = tokio::spawn(async move {
        listing_host
            .handle("agents.list", json!({"watch":"codex","rescan":true}))
            .await
    });
    wait_for_file(&started).await;
    let connecting_host = host.clone();
    let connecting = tokio::spawn(async move {
        connecting_host
            .handle("agents.connect", json!({"id":"codex"}))
            .await
    });
    let listed = listing.await.unwrap();
    let connected = connecting.await.unwrap();
    assert_eq!(row(&listed, "codex")["registration"], "not-connected");
    assert_eq!(row(&connected, "codex")["registration"], "connected");
    let cached = host.handle("agents.list", json!({})).await;
    assert_eq!(row(&cached, "codex")["registration"], "connected");

    // A cross-process JS writer can dismiss while the CLI is being inspected. The short file lock
    // and reload-before-merge must preserve that newer generation dismissal.
    let slow_scan = fixture
        .profile
        .agent_home("CODEX_HOME")
        .join("slow-get-started");
    let state_path = fixture.profile.data.join("agents.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    let generation = state["seen"]["codex"]["generation"].clone();
    private_write(
        &fixture
            .profile
            .agent_home("CODEX_HOME")
            .join("slow-next-get"),
        b"1",
    );
    let scanning_host = host.clone();
    let scanning = tokio::spawn(async move {
        scanning_host
            .handle("agents.list", json!({"watch":"codex","rescan":true}))
            .await
    });
    wait_for_file(&slow_scan).await;
    let lock_path = fixture.profile.data.join("agents.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(lock_path)
        .unwrap();
    lock.lock_exclusive().unwrap();
    state["dismissed"]["codex"] = generation;
    atomic_json(&state_path, &state).unwrap();
    FileExt::unlock(&lock).unwrap();
    let scanned = scanning.await.unwrap();
    assert_eq!(row(&scanned, "codex")["dismissed"], true);

    let disconnected = host
        .handle("agents.disconnect", json!({"id":"codex"}))
        .await;
    assert_eq!(row(&disconnected, "codex")["registration"], "not-connected");
    private_write(
        &fixture
            .profile
            .agent_home("CODEX_HOME")
            .join("slow-next-add"),
        b"1",
    );
    let add_started = fixture
        .profile
        .agent_home("CODEX_HOME")
        .join("slow-add-started");
    let connect_host = host.clone();
    let connect = tokio::spawn(async move {
        connect_host
            .handle("agents.connect", json!({"id":"codex"}))
            .await
    });
    wait_for_file(&add_started).await;
    let disconnect_host = host.clone();
    let disconnect = tokio::spawn(async move {
        disconnect_host
            .handle("agents.disconnect", json!({"id":"codex"}))
            .await
    });
    let connected = connect.await.unwrap();
    let disconnected = disconnect.await.unwrap();
    assert_eq!(row(&connected, "codex")["registration"], "connected");
    assert_eq!(row(&disconnected, "codex")["registration"], "not-connected");
    let final_state = host.handle("agents.list", json!({})).await;
    assert_eq!(row(&final_state, "codex")["registration"], "not-connected");

    private_write(
        &fixture
            .profile
            .agent_home("CODEX_HOME")
            .join("leave-unregistered-after-add"),
        b"1",
    );
    let unconfirmed_connect = host.handle("agents.connect", json!({"id":"codex"})).await;
    assert_eq!(
        unconfirmed_connect.pointer("/error/key"),
        Some(&json!("agents.registration-not-confirmed"))
    );
    assert!(!fixture
        .profile
        .agent_home("CODEX_HOME")
        .join("fake-entry.json")
        .exists());

    let restored = host.handle("agents.connect", json!({"id":"codex"})).await;
    assert_eq!(row(&restored, "codex")["registration"], "connected");
    private_write(
        &fixture
            .profile
            .agent_home("CODEX_HOME")
            .join("timeout-after-remove"),
        b"1",
    );
    let uncertain_disconnect = host
        .handle("agents.disconnect", json!({"id":"codex"}))
        .await;
    assert_eq!(
        uncertain_disconnect.pointer("/error/key"),
        Some(&json!("agents.registration-unknown"))
    );
    assert!(!fixture
        .profile
        .agent_home("CODEX_HOME")
        .join("fake-entry.json")
        .exists());

    // Dropping the link-side waiter cancels the owned operation. The delayed add is killed before
    // it can write, and a subsequent request re-inspects the actual entry before connecting.
    fs::remove_file(&add_started).unwrap();
    private_write(
        &fixture
            .profile
            .agent_home("CODEX_HOME")
            .join("slow-next-add"),
        b"1",
    );
    let pid_file = fixture
        .profile
        .agent_home("CODEX_HOME")
        .join("slow-add-started");
    let abandoned_host = host.clone();
    let abandoned = tokio::spawn(async move {
        abandoned_host
            .handle("agents.connect", json!({"id":"codex"}))
            .await
    });
    wait_for_file(&pid_file).await;
    let pid: i32 = fs::read_to_string(&pid_file).unwrap().parse().unwrap();
    abandoned.abort();
    let _ = abandoned.await;
    tokio::time::sleep(StdDuration::from_millis(100)).await;
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "cancelled CLI process was reaped"
    );
    assert!(!fixture
        .profile
        .agent_home("CODEX_HOME")
        .join("fake-entry.json")
        .exists());
    let recovered = host.handle("agents.connect", json!({"id":"codex"})).await;
    assert_eq!(row(&recovered, "codex")["registration"], "connected");

    host.shutdown().await;
}

async fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + StdDuration::from_secs(5);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        sleep(StdDuration::from_millis(10)).await;
    }
    panic!("timed out waiting for fixture marker {}", path.display());
}

#[tokio::test]
async fn the_installed_command_is_registered_and_owns_every_release_of_it() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let profile = &fixture.profile;
    let releases = profile.xdg_data.join("sidevoice");
    let binary = releases.join("releases/1.0.0/bin/sidevoice-connector");
    private_mkdir(binary.parent().unwrap());
    private_write(&binary, b"#!/bin/sh\n");
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    symlink("releases/1.0.0", releases.join("current")).unwrap();
    let stable = releases.join("current/bin/sidevoice-connector");
    assert!(
        InstalledCommand::installed(profile).is_none(),
        "nothing recorded"
    );
    write_json(
        &profile.data.join("install.json"),
        &json!({"command":[stable],"releases":releases}),
    );
    let installed = InstalledCommand::installed(profile).unwrap();
    assert_eq!(installed.command, stable.to_string_lossy());
    assert_eq!(installed.args, ["mcp"]);
    let mcp = ["mcp".to_owned()];
    assert!(installed.owns(&stable.to_string_lossy(), &mcp));
    assert!(
        installed.owns(&binary.to_string_lossy(), &mcp),
        "a release's own binary"
    );
    assert!(!installed.owns(&stable.to_string_lossy(), &["connector".into()]));
    assert!(!installed.owns("/usr/local/bin/sidevoice-connector", &mcp));
    let elsewhere = fixture.root.join("elsewhere/bin/sidevoice-connector");
    private_mkdir(elsewhere.parent().unwrap());
    private_write(&elsewhere, b"#!/bin/sh\n");
    assert!(!installed.owns(&elsewhere.to_string_lossy(), &mcp));

    // The installer registers while a person's stop holds (uninstall writes one first); the CLI does not.
    write_json(&profile.data.join("node-stopped.json"), &json!({}));
    let stopped = HostAgents::new(profile.clone()).unwrap();
    let refused = stopped
        .handle("agents.connect", json!({"id":"cursor"}))
        .await;
    assert!(refused.get("error").is_some(), "{refused}");
    let installer = HostAgents::for_installer(profile.clone()).unwrap();
    let connected = installer
        .handle("agents.connect", json!({"id":"cursor"}))
        .await;
    assert!(connected.get("error").is_none(), "{connected}");
    let config: Value = serde_json::from_slice(
        &fs::read(profile.agent_home("CURSOR_CONFIG_DIR").join("mcp.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(config["mcpServers"]["sidevoice"]["command"], json!(stable));
    assert_eq!(config["mcpServers"]["sidevoice"]["args"], json!(["mcp"]));
    let removed = installer
        .handle("agents.disconnect", json!({"id":"cursor"}))
        .await;
    assert!(removed.get("error").is_none(), "{removed}");
    let config: Value = serde_json::from_slice(
        &fs::read(profile.agent_home("CURSOR_CONFIG_DIR").join("mcp.json")).unwrap(),
    )
    .unwrap();
    assert!(config["mcpServers"].get("sidevoice").is_none(), "{config}");
}

// Delivery runs the Codex the scan found, with the login shell's PATH, not whatever `codex` the daemon's own PATH (a
// service manager's) reaches; a launcher that is a script finds its interpreter there.
#[tokio::test]
async fn codex_delivery_does_not_use_a_cached_binary_without_login_path() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let codex = fake_codex(&fixture);
    let mut state = empty_state();
    state["binaries"]["codex"] = json!(codex);
    write_json(&fixture.profile.data.join("agents.json"), &state);
    assert!(agent_command(&fixture.profile, "codex").program.is_none());
    let result = crate::adapters::codex::deliver(
        &json!({"kind":"codex-queue","thread":"t-1"}),
        &json!({"channel":"voice","session_id":"s","revision":1,"message_id":"m","text":"hello"}),
        &fixture.profile,
    )
    .await;
    assert!(result.is_err());
    assert!(!fixture
        .profile
        .agent_home("CODEX_HOME")
        .join("calls")
        .exists());
}

#[tokio::test]
async fn codex_delivery_runs_the_scanned_codex_with_the_login_path() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let bin = fixture.root.join("login-bin");
    private_mkdir(&bin);
    let marker = fixture.root.join("queued");
    let codex = bin.join("codex");
    private_write(
        &codex,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$PATH\" \"$@\" > '{}'\n",
            marker.display()
        )
        .as_bytes(),
    );
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o700)).unwrap();
    let login = format!("{}:/usr/bin:/bin", bin.display());
    let shell = fixture.root.join("bench-login-shell");
    private_write(&shell, format!("#!/bin/sh\nprintf '\\036SIDEVOICE_PATH_BEGIN\\036%s\\036SIDEVOICE_PATH_END\\036' '{login}'\n").as_bytes());
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
    let _shell = EnvGuard::set("SHELL", shell.as_os_str());
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let discovered = host.handle("agents.list", json!({"rescan":true})).await;
    assert!(
        listed_ids(&discovered).contains(&"codex".into()),
        "{discovered}"
    );
    let state: Value =
        serde_json::from_slice(&fs::read(fixture.profile.data.join("agents.json")).unwrap())
            .unwrap();
    assert_eq!(state["binaries"]["codex"], json!(codex));
    host.shutdown().await;

    let answer = crate::adapters::codex::deliver(
        &json!({"kind":"codex-queue","thread":"t-1"}),
        &json!({"channel":"voice","session_id":"s","revision":1,"message_id":"m","text":"hola"}),
        &fixture.profile,
    )
    .await
    .unwrap();
    assert_eq!(answer["status"], "accepted");
    let seen = fs::read_to_string(&marker).unwrap();
    let lines: Vec<&str> = seen.lines().collect();
    assert_eq!(lines[0], login);
    assert_eq!(lines[1..4], ["queue", "--thread", "t-1"]);
}

/// A Codex CLI that keeps one `sidevoice` entry in `$CODEX_HOME` as the real one shows it (`mcp get --json`, with
/// `transport.env`), and logs every call.
fn fake_codex(fixture: &Fixture) -> PathBuf {
    let binary = fixture.root.join("bin/codex");
    private_mkdir(binary.parent().unwrap());
    private_write(
        &binary,
        br##"#!/bin/sh
home="${CODEX_HOME:-$HOME/.codex}"
entry="$home/fake-entry.json"
printf '%s\n' "$*" >> "$home/calls"
quote() { printf '"%s"' "$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g')"; }
case "$1 $2 $3" in
"mcp get sidevoice")
    if [ ! -e "$entry" ]; then echo 'No such server: sidevoice' >&2; exit 1; fi
    cat "$entry"; exit 0 ;;
"mcp remove sidevoice")
    rm -f "$entry" "$home/config.toml"; exit 0 ;;
"mcp add sidevoice")
    shift 3
    env=""
    while [ "$1" != "--" ]; do
        if [ "$1" = "--env" ]; then
            shift
            if [ -n "$env" ]; then env="$env, "; fi
            env="$env$(quote "${1%%=*}"): $(quote "${1#*=}")"
        fi
        shift
    done
    shift
    command="$1"; shift
    printf '{"name":"sidevoice","enabled":true,"transport":{"type":"stdio","command":%s,"args":["%s"],"env":{%s}}}' \
        "$(quote "$command")" "$*" "$env" > "$entry"
    printf '[mcp_servers.sidevoice]\ncommand = %s\nargs = ["%s"]\n' "$(quote "$command")" "$*" > "$home/config.toml"
    exit 0 ;;
esac
echo 'unsupported fixture command' >&2
exit 2
"##,
    );
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    binary
}

/// An entry of ours in the fake Codex: `enabled`, with `env` as its environment.
fn codex_entry(fixture: &Fixture, selected: &InstalledCommand, enabled: bool, env: Value) {
    let codex = &fixture.profile.agent_home("CODEX_HOME");
    write_json(
        &codex.join("fake-entry.json"),
        &json!({"name":"sidevoice","enabled":enabled,"transport":{"type":"stdio",
            "command":selected.command,"args":selected.args,"env":env}}),
    );
    private_write(
        &codex.join("config.toml"),
        format!(
            "[mcp_servers.sidevoice]\ncommand = {}\nargs = {}\n",
            serde_json::to_string(&selected.command).unwrap(),
            serde_json::to_string(&selected.args).unwrap()
        )
        .as_bytes(),
    );
}

fn installation_env(fixture: &Fixture) -> Value {
    json!({"SIDEVOICE_DATA_DIR": fixture.profile.data, "XDG_DATA_HOME": fixture.profile.xdg_data})
}

#[tokio::test]
async fn codex_registration_carries_the_installation_environment() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let binary = fake_codex(&fixture);
    let _codex = EnvGuard::set("SIDEVOICE_CODEX_BIN", binary.as_os_str());
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());

    let connected = host.handle("agents.connect", json!({"id":"codex"})).await;
    assert_eq!(
        row(&connected, "codex")["registration"],
        "connected",
        "{connected}"
    );
    let calls = fs::read_to_string(fixture.profile.agent_home("CODEX_HOME").join("calls")).unwrap();
    let add = calls
        .lines()
        .find(|line| line.starts_with("mcp add"))
        .unwrap();
    assert!(
        add.contains(&format!(
            "--env SIDEVOICE_DATA_DIR={}",
            fixture.profile.data.display()
        )),
        "{add}"
    );
    assert!(
        add.contains(&format!(
            "--env XDG_DATA_HOME={}",
            fixture.profile.xdg_data.display()
        )),
        "{add}"
    );

    // A registration of ours without this installation's environment (an agent opened from another shell would reach
    // another data directory) is not current: connecting writes it again.
    codex_entry(&fixture, &fixture.selected(), true, json!({}));
    let listed = host.handle("agents.list", json!({"rescan":true})).await;
    assert_eq!(row(&listed, "codex")["registration"], "outdated");
    assert_eq!(row(&listed, "codex")["owned"], true);
    let again = host.handle("agents.connect", json!({"id":"codex"})).await;
    assert_eq!(row(&again, "codex")["registration"], "connected", "{again}");
}

#[tokio::test]
async fn codex_disconnect_removes_a_disabled_or_outdated_entry_of_ours() {
    let _turn = agent_env().await;
    for (enabled, env) in [(false, None), (true, Some(json!({})))] {
        let fixture = Fixture::new();
        let binary = fake_codex(&fixture);
        let _codex = EnvGuard::set("SIDEVOICE_CODEX_BIN", binary.as_os_str());
        let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
        codex_entry(
            &fixture,
            &fixture.selected(),
            enabled,
            env.unwrap_or_else(|| installation_env(&fixture)),
        );
        let listed = host.handle("agents.list", json!({"rescan":true})).await;
        assert_eq!(row(&listed, "codex")["registration"], "outdated");
        assert_eq!(row(&listed, "codex")["owned"], true);

        let answer = host
            .handle("agents.disconnect", json!({"id":"codex"}))
            .await;
        assert!(answer.get("error").is_none(), "{answer}");
        assert!(
            !fixture
                .profile
                .agent_home("CODEX_HOME")
                .join("fake-entry.json")
                .exists(),
            "the entry of ours stayed"
        );
        assert_eq!(row(&answer, "codex")["owned"], false);
    }
}

/// A Claude Code CLI that keeps its user-scope MCP servers where the real one does: `$CLAUDE_CONFIG_DIR/.claude.json`
/// when the variable is set, else `~/.claude.json`. It logs every call to its data directory (`$CLAUDE_CONFIG_DIR`,
/// else `~/.claude`). Like the real one, `mcp get` starts the server the entry names (its command is kept in the data
/// directory) to report whether it connects.
fn fake_claude(fixture: &Fixture) -> PathBuf {
    let binary = fixture.root.join("bin/claude");
    private_mkdir(binary.parent().unwrap());
    private_write(
        &binary,
        br##"#!/bin/sh
home="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
settings="${CLAUDE_CONFIG_DIR:-$HOME}/.claude.json"
printf '%s\n' "$*" >> "$home/calls"
quote() { printf '"%s"' "$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g')"; }
case "$1 $2" in
"mcp get")
    if [ -e "$home/fake-command" ]; then "$(cat "$home/fake-command")" </dev/null >/dev/null 2>&1; fi
    if [ ! -e "$settings" ]; then echo 'No MCP server named "sidevoice" found.' >&2; exit 1; fi
    cat "$settings"; exit 0 ;;
"mcp remove")
    rm -f "$home/fake-command"
    printf '{"mcpServers":{}}\n' > "$settings"; exit 0 ;;
"mcp add")
    if grep -q '"sidevoice"' "$settings" 2>/dev/null; then echo 'MCP server sidevoice already exists in user config' >&2; exit 1; fi
    if [ -e "$home/fail-add" ]; then echo 'add failed' >&2; exit 1; fi
    shift 5
    env=""
    while [ "$1" != "--" ]; do
        if [ "$1" = "-e" ]; then
            shift
            if [ -n "$env" ]; then env="$env, "; fi
            env="$env$(quote "${1%%=*}"): $(quote "${1#*=}")"
        fi
        shift
    done
    shift
    command="$1"; shift
    printf '%s' "$command" > "$home/fake-command"
    printf '{"numStartups":3,"mcpServers":{"sidevoice":{"type":"stdio","command":%s,"args":["%s"],"env":{%s}}}}\n' \
        "$(quote "$command")" "$*" "$env" > "$settings"
    chmod 600 "$settings"
    exit 0 ;;
esac
echo 'unsupported fixture command' >&2
exit 2
"##,
    );
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    binary
}

#[tokio::test]
async fn claude_registration_carries_the_installation_environment_and_an_older_one_is_removed() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let binary = fake_claude(&fixture);
    let _claude = EnvGuard::set("SIDEVOICE_CLAUDE_BIN", binary.as_os_str());
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());

    let connected = host.handle("agents.connect", json!({"id":"claude"})).await;
    assert_eq!(
        row(&connected, "claude")["registration"],
        "connected",
        "{connected}"
    );
    let calls = fs::read_to_string(
        fixture
            .profile
            .agent_home("CLAUDE_CONFIG_DIR")
            .join("calls"),
    )
    .unwrap();
    let add = calls
        .lines()
        .find(|line| line.starts_with("mcp add"))
        .unwrap();
    assert!(
        add.starts_with(&format!(
            "mcp add --scope user sidevoice -e SIDEVOICE_DATA_DIR={} -e XDG_DATA_HOME={} --",
            fixture.profile.data.display(),
            fixture.profile.xdg_data.display()
        )),
        "{add}"
    );

    // The same command without the environment: ours, not current, and removed on disconnect.
    let selected = fixture.selected();
    let settings = fixture
        .profile
        .agent_home("CLAUDE_CONFIG_DIR")
        .join(".claude.json");
    write_json(
        &settings,
        &json!({"mcpServers":{"sidevoice":{"type":"stdio","command":selected.command,"args":selected.args}}}),
    );
    let listed = host.handle("agents.list", json!({"rescan":true})).await;
    assert_eq!(row(&listed, "claude")["registration"], "outdated");
    assert_eq!(row(&listed, "claude")["owned"], true);
    let answer = host
        .handle("agents.disconnect", json!({"id":"claude"}))
        .await;
    assert!(answer.get("error").is_none(), "{answer}");
    let saved: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    assert!(
        saved.pointer("/mcpServers/sidevoice").is_none(),
        "the entry of ours stayed"
    );
}

/// Claude Code's `mcp get` starts the server an entry names. Reading the entry must not: a foreign command never runs,
/// on a scan or a check, and a foreign entry has no health.
#[tokio::test]
async fn a_foreign_claude_entry_is_never_started() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let binary = fake_claude(&fixture);
    let _claude = EnvGuard::set("SIDEVOICE_CLAUDE_BIN", binary.as_os_str());
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let ran = fixture.root.join("foreign-ran");
    let foreign = fixture.root.join("bin/foreign");
    private_write(
        &foreign,
        format!("#!/bin/sh\ntouch '{}'\n", ran.display()).as_bytes(),
    );
    fs::set_permissions(&foreign, fs::Permissions::from_mode(0o700)).unwrap();
    write_json(
        &fixture
            .profile
            .agent_home("CLAUDE_CONFIG_DIR")
            .join(".claude.json"),
        &json!({"mcpServers":{"sidevoice":{"type":"stdio","command":foreign,"args":["mcp"]}}}),
    );
    private_write(
        &fixture
            .profile
            .agent_home("CLAUDE_CONFIG_DIR")
            .join("fake-command"),
        foreign.to_string_lossy().as_bytes(),
    );

    let listed = host.handle("agents.list", json!({"rescan":true})).await;
    assert_eq!(
        row(&listed, "claude")["registration"],
        "foreign",
        "{listed}"
    );
    let checked = host.handle("agents.check", json!({"id":"claude"})).await;
    assert_eq!(row(&checked, "claude")["registration"], "foreign");
    assert_eq!(row(&checked, "claude")["health"], Value::Null);
    let refused = host.handle("agents.connect", json!({"id":"claude"})).await;
    assert_eq!(
        refused.pointer("/error/key"),
        Some(&json!("agents.foreign"))
    );
    assert!(!ran.exists(), "the foreign command ran");
    let calls = fs::read_to_string(
        fixture
            .profile
            .agent_home("CLAUDE_CONFIG_DIR")
            .join("calls"),
    )
    .unwrap_or_default();
    assert!(!calls.contains("mcp get"), "{calls}");
    host.shutdown().await;
}

/// Claude Code reads its user-scope MCP servers from `~/.claude.json` unless the person's environment sets
/// `CLAUDE_CONFIG_DIR`, and from `$CLAUDE_CONFIG_DIR/.claude.json` when it does: the entry is read from that file.
#[tokio::test]
async fn the_claude_entry_is_read_where_claude_code_reads_it() {
    let _turn = agent_env().await;
    let mut fixture = Fixture::new();
    let claude_home = fixture.profile.home.join(".claude");
    set_agent_home(
        &mut fixture.profile,
        "CLAUDE_CONFIG_DIR",
        &claude_home,
        false,
    );
    private_mkdir(&claude_home);
    let binary = fake_claude(&fixture);
    let _claude = EnvGuard::set("SIDEVOICE_CLAUDE_BIN", binary.as_os_str());
    let in_home = fixture.profile.home.join(".claude.json");
    let in_config_dir = claude_home.join(".claude.json");
    let foreign =
        json!({"mcpServers":{"sidevoice":{"type":"stdio","command":"/elsewhere/server"}}});

    // Unset: connecting writes `~/.claude.json`, and that is the file read back; another one in `~/.claude` is not
    // Claude Code's.
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let connected = host.handle("agents.connect", json!({"id":"claude"})).await;
    assert_eq!(
        row(&connected, "claude")["registration"],
        "connected",
        "{connected}"
    );
    let saved: Value = serde_json::from_slice(&fs::read(&in_home).unwrap()).unwrap();
    assert!(saved.pointer("/mcpServers/sidevoice").is_some(), "{saved}");
    write_json(&in_config_dir, &foreign);
    let listed = host.handle("agents.list", json!({"rescan":true})).await;
    assert_eq!(
        row(&listed, "claude")["registration"],
        "connected",
        "{listed}"
    );
    host.shutdown().await;

    // Set: `$CLAUDE_CONFIG_DIR/.claude.json`, whatever `~/.claude.json` holds.
    set_agent_home(
        &mut fixture.profile,
        "CLAUDE_CONFIG_DIR",
        &claude_home,
        true,
    );
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let listed = host.handle("agents.list", json!({"rescan":true})).await;
    assert_eq!(
        row(&listed, "claude")["registration"],
        "foreign",
        "{listed}"
    );
    host.shutdown().await;
}

/// A stand-in for this installation's `mcp` server at the selected command: it answers the handshake and logs, to
/// the data directory its entry names, the health-check marker and its arguments.
fn fake_server(fixture: &Fixture) {
    let server = fixture.root.join("selected/current/dist/sidevoice");
    private_mkdir(server.parent().unwrap());
    private_write(
        &server,
        br##"#!/bin/sh
read -r initialize
printf '%s\n' "$SIDEVOICE_HEALTH_CHECK $*" >> "$SIDEVOICE_DATA_DIR/health-calls"
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"1"}}}'
read -r initialized
read -r list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}'
sleep 30
"##,
    );
    fs::set_permissions(&server, fs::Permissions::from_mode(0o700)).unwrap();
}

#[tokio::test]
async fn a_connected_agent_is_checked_on_connect_scan_and_demand_and_its_health_is_kept() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let binary = fake_claude(&fixture);
    let _claude = EnvGuard::set("SIDEVOICE_CLAUDE_BIN", binary.as_os_str());
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    fake_server(&fixture);
    let calls = fixture.profile.data.join("health-calls");

    // Not connected: no health.
    let listed = host.handle("agents.list", json!({"rescan":true})).await;
    assert_eq!(row(&listed, "claude")["registration"], "not-connected");
    assert_eq!(row(&listed, "claude")["health"], Value::Null);
    assert!(!calls.exists());

    // Connecting checks it, starting the entry as Claude Code would: its command, its arguments and its environment,
    // with the marker that keeps our server from starting the daemon.
    let connected = host.handle("agents.connect", json!({"id":"claude"})).await;
    let health = &row(&connected, "claude")["health"];
    assert_eq!(health["state"], "working", "{connected}");
    assert_eq!(health["error"], Value::Null);
    let checked_at = health["checked_at"].as_str().unwrap().to_owned();
    assert_eq!(fs::read_to_string(&calls).unwrap(), "1 mcp\n");

    // A listing from the cache keeps it; a rescan checks again.
    let cached = host.handle("agents.list", json!({})).await;
    assert_eq!(row(&cached, "claude")["health"]["checked_at"], checked_at);
    let rescanned = host.handle("agents.list", json!({"rescan":true})).await;
    assert_eq!(row(&rescanned, "claude")["health"]["state"], "working");
    assert_eq!(fs::read_to_string(&calls).unwrap().lines().count(), 2);

    // On demand: a server that no longer starts says why, and the store keeps it.
    fs::remove_file(fixture.root.join("selected/current/dist/sidevoice")).unwrap();
    let checked = host.handle("agents.check", json!({"id":"claude"})).await;
    assert_eq!(
        row(&checked, "claude")["health"],
        json!({"state":"not-starting","error":"command not found",
            "checked_at":row(&checked, "claude")["health"]["checked_at"]}),
        "{checked}"
    );
    assert_eq!(row(&checked, "claude")["registration"], "connected");
    let store: Value =
        serde_json::from_slice(&fs::read(fixture.profile.data.join("agents.json")).unwrap())
            .unwrap();
    assert_eq!(
        store.pointer("/seen/claude/agent/health/state"),
        Some(&json!("not-starting"))
    );

    // An unknown agent is refused like any other request; a disconnected one has no health again.
    let unknown = host.handle("agents.check", json!({"id":"nope"})).await;
    assert_eq!(
        unknown.pointer("/error/key"),
        Some(&json!("agents.unknown"))
    );
    let disconnected = host
        .handle("agents.disconnect", json!({"id":"claude"}))
        .await;
    assert_eq!(row(&disconnected, "claude")["health"], Value::Null);
    host.shutdown().await;
}

/// Codex starts its server in the entry's `cwd`: the check does too, and one that does not exist does not work.
#[tokio::test]
async fn a_codex_entry_is_checked_in_its_working_directory() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let binary = fake_codex(&fixture);
    let _codex = EnvGuard::set("SIDEVOICE_CODEX_BIN", binary.as_os_str());
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    fake_server(&fixture);
    let selected = fixture.selected();
    codex_entry(&fixture, &selected, true, installation_env(&fixture));
    let entry_path = fixture
        .profile
        .agent_home("CODEX_HOME")
        .join("fake-entry.json");
    for (cwd, state, error) in [
        (fixture.root.join("home"), "working", Value::Null),
        (
            fixture.root.join("missing"),
            "not-starting",
            json!("working directory not found"),
        ),
    ] {
        let mut entry: Value = serde_json::from_slice(&fs::read(&entry_path).unwrap()).unwrap();
        entry["transport"]["cwd"] = json!(cwd);
        write_json(&entry_path, &entry);
        let checked = host.handle("agents.check", json!({"id":"codex"})).await;
        let row = row(&checked, "codex");
        assert_eq!(row["registration"], "connected", "{checked}");
        assert_eq!(row["health"]["state"], state, "{checked}");
        assert_eq!(row["health"]["error"], error, "{checked}");
    }
    host.shutdown().await;
}

#[tokio::test]
async fn each_agent_says_what_the_connector_does_itself_and_how_to_do_it_by_hand() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let claude = fake_claude(&fixture);
    let codex = fake_codex(&fixture);
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let selected = fixture.selected();
    let env = [
        format!("SIDEVOICE_DATA_DIR={}", fixture.profile.data.display()),
        format!("XDG_DATA_HOME={}", fixture.profile.xdg_data.display()),
    ];
    let quoted = |words: Vec<String>| {
        words
            .iter()
            .map(|word| format!("'{word}'"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let ours = || {
        std::iter::once("--".to_owned())
            .chain(std::iter::once(selected.command.clone()))
            .chain(selected.args.iter().cloned())
    };

    // Not found: Claude Code and Codex are not listed (an_agent_is_listed_when_installed_not_for_its_home_alone);
    // Cursor's file is edited all the same.
    let listed = host.handle("agents.list", json!({"rescan":true})).await;
    assert_eq!(
        row(&listed, "cursor")["can"],
        json!({"connect":true,"update":true,"disconnect":true})
    );

    let _claude = EnvGuard::set("SIDEVOICE_CLAUDE_BIN", claude.as_os_str());
    let _codex = EnvGuard::set("SIDEVOICE_CODEX_BIN", codex.as_os_str());
    let listed = host.handle("agents.list", json!({"rescan":true})).await;
    for id in ["claude", "codex", "cursor"] {
        let agent = row(&listed, id);
        assert_eq!(agent["connect"], "auto", "{agent}");
        assert_eq!(
            agent["can"],
            json!({"connect":true,"update":true,"disconnect":true})
        );
        // The older fields hold connecting and disconnecting as one step each: here, their only step.
        assert_eq!(agent["instructions"], agent["manual"]["connect"][0]);
        assert_eq!(agent["removal"], agent["manual"]["disconnect"][0]);
    }

    // Each command names the agent's home, as the connector runs its CLI (the fixture's environment sets
    // `CLAUDE_CONFIG_DIR` and `CODEX_HOME`; without them, see `steps_name_an_agents_home_only_when_the_person_sets_it`).
    let claude_home = fixture
        .profile
        .agent_home("CLAUDE_CONFIG_DIR")
        .display()
        .to_string();
    let codex_home = fixture
        .profile
        .agent_home("CODEX_HOME")
        .display()
        .to_string();
    let words = |cli: &Path, words: &[&str]| {
        std::iter::once(cli.to_string_lossy().into_owned())
            .chain(words.iter().map(|word| (*word).to_owned()))
            .collect::<Vec<_>>()
    };

    let mut add = words(&claude, &["mcp", "add", "--scope", "user", "sidevoice"]);
    for pair in &env {
        add.extend(["-e".to_owned(), pair.clone()]);
    }
    add.extend(ours());
    let file = fixture
        .profile
        .agent_home("CLAUDE_CONFIG_DIR")
        .join(".claude.json");
    let file = file.to_string_lossy();
    let step = |words: Vec<String>| json!({"command":format!("CLAUDE_CONFIG_DIR='{claude_home}' {}", quoted(words)),"file":file});
    let add = step(add);
    let remove = step(words(
        &claude,
        &["mcp", "remove", "--scope", "user", "sidevoice"],
    ));
    // Another entry may be in any scope: removed from the one it is in.
    let remove_other = step(words(&claude, &["mcp", "remove", "sidevoice"]));
    assert_eq!(
        row(&listed, "claude")["manual"],
        json!({"connect":[add],"update":[remove,add],"disconnect":[remove],"replace":[remove_other,add]})
    );

    let file = fixture.profile.agent_home("CODEX_HOME").join("config.toml");
    let file = file.to_string_lossy();
    let mut add = words(&codex, &["mcp", "add", "sidevoice"]);
    for pair in &env {
        add.extend(["--env".to_owned(), pair.clone()]);
    }
    add.extend(ours());
    let step = |words: Vec<String>| json!({"command":format!("CODEX_HOME='{codex_home}' {}", quoted(words)),"file":file});
    let add = step(add);
    let remove = step(words(&codex, &["mcp", "remove", "sidevoice"]));
    assert_eq!(
        row(&listed, "codex")["manual"],
        json!({"connect":[add],"update":[remove,add],"disconnect":[remove],"replace":[remove,add]})
    );

    let file = fixture
        .profile
        .agent_home("CURSOR_CONFIG_DIR")
        .join("mcp.json");
    let file = file.to_string_lossy();
    let snippet = mcp_servers_json(&selected.command, &selected.args, host.mcp_env()).unwrap();
    let add = json!({"snippet":snippet,"file":file});
    assert_eq!(
        row(&listed, "cursor")["manual"],
        json!({
            "connect":[add],
            "update":[{"snippet":"Delete the \"sidevoice\" entry under \"mcpServers\".","file":file},add],
            "disconnect":[{"snippet":"Delete the \"sidevoice\" entry under \"mcpServers\".","file":file}],
            "replace":[{"snippet":"Delete the other \"sidevoice\" entry under \"mcpServers\".","file":file},add],
        })
    );
    host.shutdown().await;
}

#[tokio::test]
async fn the_manual_steps_run_by_hand_update_an_older_entry_in_the_agents_own_home() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let claude = fake_claude(&fixture);
    let codex = fake_codex(&fixture);
    let _claude = EnvGuard::set("SIDEVOICE_CLAUDE_BIN", claude.as_os_str());
    let _codex = EnvGuard::set("SIDEVOICE_CODEX_BIN", codex.as_os_str());
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let selected = fixture.selected();
    // `connect` (on an outdated entry), `update`, and the older `instructions` (connecting as one step) each rewrite
    // an older entry: each runs from one.
    for kind in ["connect", "update", "instructions"] {
        // Older entries of ours: this installation's command without its environment.
        claude_older_entry(
            &fixture
                .profile
                .agent_home("CLAUDE_CONFIG_DIR")
                .join(".claude.json"),
            &selected,
        );
        codex_entry(&fixture, &selected, true, json!({}));

        let listed = host.handle("agents.list", json!({"rescan":true})).await;
        for id in ["claude", "codex"] {
            let agent = row(&listed, id);
            assert_eq!(agent["owned"], true, "{agent}");
            let steps = if kind == "instructions" {
                let command = agent["instructions"]["command"].as_str().unwrap();
                assert!(command.contains(" && "), "removes, then adds: {agent}");
                vec![agent["instructions"].clone()]
            } else {
                let steps = agent["manual"][kind].as_array().unwrap().clone();
                assert_eq!(
                    steps.len(),
                    2,
                    "an older entry is removed first: {kind} {agent}"
                );
                steps
            };
            // Run by hand from a shell that names no agent home: the steps name it themselves.
            for step in steps {
                let command = step["command"].as_str().unwrap();
                let status = std::process::Command::new("/bin/sh")
                    .arg("-c")
                    .arg(command)
                    .env_remove("CLAUDE_CONFIG_DIR")
                    .env_remove("CODEX_HOME")
                    .env("HOME", fixture.root.join("elsewhere"))
                    .status()
                    .unwrap();
                assert!(status.success(), "{kind}: {command}");
            }
        }

        let listed = host.handle("agents.list", json!({"rescan":true})).await;
        for id in ["claude", "codex"] {
            let agent = row(&listed, id);
            assert_eq!(agent["registration"], "connected", "{kind}: {agent}");
            assert_eq!(agent["manual"]["connect"].as_array().unwrap().len(), 1);
            assert_eq!(agent["manual"]["update"].as_array().unwrap().len(), 2);
        }
    }
    host.shutdown().await;
}

/// Without an agent's home variable in the person's environment, its steps name none and change the file in the
/// agent's default home (`~/.claude.json` for Claude Code, `~/.codex/config.toml` for Codex): run by hand from the
/// person's shell, they change what the connector reads.
#[tokio::test]
async fn steps_name_an_agents_home_only_when_the_person_sets_it() {
    let _turn = agent_env().await;
    let mut fixture = Fixture::new();
    let home = fixture.profile.home.clone();
    for (var, default) in [("CLAUDE_CONFIG_DIR", ".claude"), ("CODEX_HOME", ".codex")] {
        set_agent_home(&mut fixture.profile, var, &home.join(default), false);
        private_mkdir(&home.join(default));
    }
    let claude = fake_claude(&fixture);
    let codex = fake_codex(&fixture);
    let _claude = EnvGuard::set("SIDEVOICE_CLAUDE_BIN", claude.as_os_str());
    let _codex = EnvGuard::set("SIDEVOICE_CODEX_BIN", codex.as_os_str());
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let selected = fixture.selected();
    // Older entries of ours: this installation's command without its environment.
    claude_older_entry(&home.join(".claude.json"), &selected);
    codex_entry(&fixture, &selected, true, json!({}));

    let listed = host.handle("agents.list", json!({"rescan":true})).await;
    for (id, cli, file) in [
        ("claude", &claude, home.join(".claude.json")),
        ("codex", &codex, home.join(".codex/config.toml")),
    ] {
        let agent = row(&listed, id);
        assert_eq!(agent["owned"], true, "{agent}");
        let manual = &agent["manual"];
        for operation in ["connect", "update", "disconnect", "replace"] {
            for step in manual[operation].as_array().unwrap() {
                assert_eq!(step["file"], json!(file.to_string_lossy()), "{step}");
                let command = step["command"].as_str().unwrap();
                assert!(
                    command.starts_with(&format!("'{}' ", cli.display())),
                    "{command}"
                );
            }
        }
        // The person's shell: their HOME, no agent home named.
        for step in manual["connect"].as_array().unwrap() {
            let command = step["command"].as_str().unwrap();
            let status = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(command)
                .env_remove("CLAUDE_CONFIG_DIR")
                .env_remove("CODEX_HOME")
                .env("HOME", &home)
                .status()
                .unwrap();
            assert!(status.success(), "{command}");
        }
    }
    let listed = host.handle("agents.list", json!({"rescan":true})).await;
    for id in ["claude", "codex"] {
        assert_eq!(row(&listed, id)["registration"], "connected", "{listed}");
    }
    host.shutdown().await;
}

/// An entry of ours in the fake Claude Code that names this installation's command without its environment.
fn claude_outdated_entry(fixture: &Fixture) {
    claude_older_entry(
        &fixture
            .profile
            .agent_home("CLAUDE_CONFIG_DIR")
            .join(".claude.json"),
        &fixture.selected(),
    );
}

/// An older entry of ours in Claude Code's user configuration `file`: this installation's command, without its
/// environment.
fn claude_older_entry(file: &Path, selected: &InstalledCommand) {
    private_write(
        file,
        json!({"mcpServers": {"sidevoice": {"type": "stdio", "command": selected.command,
            "args": selected.args, "env": {}}}})
        .to_string()
        .as_bytes(),
    );
}

#[tokio::test]
async fn connect_rewrites_an_outdated_entry_and_a_failed_rewrite_is_not_success() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let binary = fake_claude(&fixture);
    let _claude = EnvGuard::set("SIDEVOICE_CLAUDE_BIN", binary.as_os_str());
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let calls = fixture
        .profile
        .agent_home("CLAUDE_CONFIG_DIR")
        .join("calls");
    let entry = fixture
        .profile
        .agent_home("CLAUDE_CONFIG_DIR")
        .join(".claude.json");

    claude_outdated_entry(&fixture);
    let listed = host.handle("agents.list", json!({"rescan":true})).await;
    assert_eq!(row(&listed, "claude")["registration"], "outdated");
    let _ = fs::remove_file(&calls);
    let connected = host.handle("agents.connect", json!({"id":"claude"})).await;
    assert!(connected.get("error").is_none(), "{connected}");
    assert_eq!(row(&connected, "claude")["registration"], "connected");
    assert_eq!(row(&connected, "claude")["owned"], true);
    // The old entry is removed, then this installation's is added, with its environment.
    let mutations: Vec<String> = fs::read_to_string(&calls)
        .unwrap()
        .lines()
        .filter(|line| line.starts_with("mcp remove") || line.starts_with("mcp add"))
        .map(str::to_owned)
        .collect();
    assert_eq!(mutations.len(), 2, "{mutations:?}");
    assert_eq!(mutations[0], "mcp remove --scope user sidevoice");
    assert!(
        mutations[1].starts_with(&format!(
            "mcp add --scope user sidevoice -e SIDEVOICE_DATA_DIR={} -e XDG_DATA_HOME={} --",
            fixture.profile.data.display(),
            fixture.profile.xdg_data.display()
        )),
        "{mutations:?}"
    );
    let written: Value = serde_json::from_slice(&fs::read(&entry).unwrap()).unwrap();
    assert_eq!(
        written.pointer("/mcpServers/sidevoice/env/SIDEVOICE_DATA_DIR"),
        Some(&json!(fixture.profile.data.to_string_lossy())),
        "{written}"
    );

    // The removal succeeds but the add fails: a failure, never reported as connected.
    claude_outdated_entry(&fixture);
    private_write(
        &fixture
            .profile
            .agent_home("CLAUDE_CONFIG_DIR")
            .join("fail-add"),
        b"1",
    );
    let failed = host.handle("agents.connect", json!({"id":"claude"})).await;
    assert_eq!(
        failed.pointer("/error/key"),
        Some(&json!("agents.action-failed")),
        "{failed}"
    );
    let listed = host.handle("agents.list", json!({"rescan":true})).await;
    assert_ne!(row(&listed, "claude")["registration"], "connected");
    host.shutdown().await;
}

/// `agents.*` from a newer core carry fields this connector does not know, at the top and nested
/// (sidevoice/sidevoice-core#112): each request is answered for what it knows.
#[tokio::test]
async fn agents_requests_with_unknown_fields_are_answered_for_what_they_know() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let binary = fake_claude(&fixture);
    let _claude = EnvGuard::set("SIDEVOICE_CLAUDE_BIN", binary.as_os_str());
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    fake_server(&fixture);

    let listed = host
        .handle(
            "agents.list",
            json!({"rescan":true,"future_field":1,"future_filter":{"nested":["x"]}}),
        )
        .await;
    assert_eq!(
        row(&listed, "claude")["registration"],
        "not-connected",
        "{listed}"
    );
    let connected = host
        .handle(
            "agents.connect",
            json!({"id":"claude","future_field":{"nested":{"deeper":true}}}),
        )
        .await;
    assert_eq!(
        row(&connected, "claude")["registration"],
        "connected",
        "{connected}"
    );
    let checked = host
        .handle("agents.check", json!({"id":"claude","future_field":[1, 2]}))
        .await;
    assert_eq!(
        row(&checked, "claude")["health"]["state"],
        "working",
        "{checked}"
    );
    host.shutdown().await;
}
