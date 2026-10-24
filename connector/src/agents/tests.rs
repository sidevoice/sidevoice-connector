use super::*;
use crate::adapters::registration::{aggregate_readiness, ReadinessCheck};
use crate::platform::data_dir;
use crate::platform::fs::TestFiles;
use crate::platform::fs::{Create, Files, Fs};
use crate::platform::target::Host;
use crate::test_support::stand_in::Script;

mod stand_ins;

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
#[cfg_attr(
    windows,
    ignore = "the login PATH is asked of a login shell (`$SHELL`) on Unix; Windows reads it from the registry, so no shell can fail or change it"
)]
async fn failed_watched_capture_keeps_the_last_scan_and_recovery_reconciles_all() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let _no_cursor_override = EnvGuard::set("SIDEVOICE_CURSOR_BIN", std::ffi::OsStr::new(""));
    let bin = fixture.root.join("login-bin");
    private_mkdir(&bin);
    for name in ["claude", "cursor-agent"] {
        Script::default().make(&bin.join(Host::exe_name(name)));
    }
    let shell = fixture.root.join(Host::exe_name("login-shell"));
    login_shell(&shell, &bin.to_string_lossy());
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
    failing(&shell);
    let failed = host.handle("agents.list", json!({"watch":"cursor"})).await;
    assert_eq!(failed["scan_failed"], true, "{failed}");
    assert_eq!(listed_ids(&failed), listed_ids(&first), "{failed}");
    let cached = host.handle("agents.list", json!({})).await;
    assert_eq!(cached["scan_failed"], true, "{cached}");
    assert!(cached["scanned_at"].is_string(), "{cached}");
    assert_eq!(listed_ids(&cached), listed_ids(&first), "{cached}");
    login_shell(&shell, &bin.to_string_lossy());
    // What is installed changed while the capture was failing: recovery must look at every agent again.
    fs::remove_file(bin.join(Host::exe_name("claude"))).unwrap();
    let recovered = host.handle("agents.list", json!({"watch":"cursor"})).await;
    assert!(
        !listed_ids(&recovered).contains(&"claude".into()),
        "{recovered}"
    );
    assert!(
        listed_ids(&recovered).contains(&"cursor".into()),
        "{recovered}"
    );
    assert_eq!(recovered["scan_failed"], false, "{recovered}");
    host.shutdown().await;
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "the login PATH is asked of a login shell (`$SHELL`) on Unix; Windows reads it from the registry, so no shell can fail or change it"
)]
async fn actions_refuse_while_the_last_scan_failed_instead_of_trusting_the_cache() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let bin = fixture.root.join("bin");
    private_mkdir(&bin);
    let cursor = bin.join(Host::exe_name("cursor"));
    prints(&cursor, "Cursor 3.22.12\n");
    let _cursor_bin = EnvGuard::set("SIDEVOICE_CURSOR_BIN", cursor.as_os_str());
    write_json(
        &fixture.profile.data.join("agents.json"),
        &json!({
            "version":1,"scanned_at":null,"login_path":bin,"binaries":{},"seen":{},"dismissed":{}
        }),
    );
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let first = host.handle("agents.list", json!({"rescan":true})).await;
    assert_eq!(first["scan_failed"], false, "{first}");
    let broken = fixture.root.join(Host::exe_name("broken-login-shell"));
    failing(&broken);
    let _shell = EnvGuard::set("SHELL", broken.as_os_str());
    for action in ["agents.connect", "agents.disconnect"] {
        let refused = host.handle(action, json!({"id":"cursor"})).await;
        assert_eq!(
            refused.pointer("/error/key"),
            Some(&json!("agents.scan-failed")),
            "{refused}"
        );
    }
    host.shutdown().await;
}

#[tokio::test]
async fn only_a_scan_of_every_agent_clears_a_recorded_failure() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    write_json(
        &fixture.profile.data.join("agents.json"),
        &json!({
            "version":1,"scanned_at":"old","login_path":"/usr/bin","scan_failed":true,
            "binaries":{},"seen":{},"dismissed":{}
        }),
    );
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let base = json!({
        "version":1,"scanned_at":"old","login_path":"/usr/bin","scan_failed":false,
        "binaries":{},"seen":{},"dismissed":{}
    });
    let deadline = Instant::now() + StdDuration::from_secs(5);
    let watched = host
        .merge_scan(
            base.clone(),
            Vec::new(),
            Some("/usr/bin".into()),
            false,
            &Cancellation::new(),
            deadline,
        )
        .await
        .unwrap()
        .0;
    assert_eq!(watched["scan_failed"], true, "{watched}");
    let full = host
        .merge_scan(
            base,
            Vec::new(),
            Some("/usr/bin".into()),
            true,
            &Cancellation::new(),
            deadline,
        )
        .await
        .unwrap()
        .0;
    assert_eq!(full["scan_failed"], false, "{full}");
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
    assert_eq!(
        row(&listed, "cursor")["evidence"],
        json!([{"kind":"install_dir"}])
    );
    assert!(!row(&listed, "cursor")["evidence"]
        .to_string()
        .contains(&fixture.profile.home.to_string_lossy().to_string()));
    host.shutdown().await;
}

#[tokio::test]
async fn cursor_cli_without_a_configuration_directory_is_not_connected_rather_than_unreadable() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    fs::remove_dir_all(crate::adapters::cursor::config_home(&fixture.profile)).ok();
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let listed = host.handle("agents.list", json!({"rescan":true})).await;
    assert_eq!(row(&listed, "cursor")["registration"], "not-connected");
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
        // macOS's per-user temp dir can be too long for the Codex daemon socket's SUN_LEN: `/tmp` where that is an
        // absolute path (Unix), the temporary directory elsewhere (Windows, where the socket is a named pipe).
        let short = Path::new("/tmp");
        let base = if short.is_absolute() {
            short.to_path_buf()
        } else {
            std::env::temp_dir()
        };
        let root = base.join(format!(
            "sv-agent-{}-{}",
            std::process::id(),
            &Uuid::new_v4().simple().to_string()[..8]
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
            (Environment::HOME, path("home")),
            ("CLAUDE_CONFIG_DIR", path("claude")),
            ("CODEX_HOME", path("codex")),
            ("CURSOR_CONFIG_DIR", path("cursor/config")),
            ("CURSOR_DATA_DIR", path("cursor/data")),
            ("XDG_CONFIG_HOME", path("xdg/config")),
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
        let cursor = root.join("cursor-bin").join(Host::exe_name("cursor-agent"));
        private_mkdir(cursor.parent().unwrap());
        Script::default().make(&cursor);
        let _cursor = EnvGuard::set("SIDEVOICE_CURSOR_BIN", cursor.as_os_str());
        let shell = root.join(Host::exe_name("fixture-login-shell"));
        login_shell(&shell, "/usr/bin");
        let _shell = EnvGuard::set("SHELL", shell.as_os_str());
        Self {
            root,
            profile,
            _cursor,
            _shell,
        }
    }

    fn cursor(&self) -> PathBuf {
        self.root
            .join("cursor-bin")
            .join(Host::exe_name("cursor-agent"))
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
    Files::shut_others_out(path);
}

fn private_write(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    Files::shut_others_out(path);
}

fn write_json(path: &Path, value: &Value) {
    private_write(path, &serde_json::to_vec_pretty(value).unwrap());
}

/// A program that prints `text` and succeeds.
fn prints(program: &Path, text: &str) {
    Script {
        print: text.into(),
        ..Script::default()
    }
    .make(program);
}

/// A program that fails.
fn failing(program: &Path) {
    Script {
        exit: 1,
        ..Script::default()
    }
    .make(program);
}

/// A login shell (`$SHELL`, where the login `PATH` is asked of one) that says `path` is its `PATH`.
fn login_shell(program: &Path, path: &str) {
    prints(
        program,
        &format!("{}{path}{}", LoginPath::START, LoginPath::END),
    );
}

/// Runs `command`, a line a person pastes in this OS's shell, with `home` as their home and no agent home named.
fn run_by_hand(command: &str, home: &Path) -> std::process::ExitStatus {
    Environment::hand_shell(command)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env(Environment::HOME, home)
        .status()
        .unwrap()
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

const BRIDGE_MESSAGE: &str = "Voice in the Cursor editor needs Cursor's Desktop Bridge: in Cursor, open Settings › Beta, enable “Allow CLI to access desktop agents”, then restart Cursor.";
const UNSUPPORTED_MESSAGE: &str = "Cursor editor voice is not supported on this computer.";

/// Asks for the Cursor row until its readiness is `state`: the bridge is looked for in the background, so the first
/// answer is provisional.
async fn cursor_readiness_settles(host: &Arc<HostAgents>, state: &str) -> Value {
    tokio::time::timeout(StdDuration::from_secs(6), async {
        loop {
            let answer = host.handle("agents.list", json!({"watch":"cursor"})).await;
            if row(&answer, "cursor")["readiness"]["state"] == state {
                break row(&answer, "cursor")["readiness"].clone();
            }
            tokio::time::sleep(StdDuration::from_millis(20)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn cursor_agent_row_exposes_readiness_codes_and_fallbacks_without_private_data() {
    use crate::platform::ipc::{Address, Channel, Channels, Endpoint};
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let bin = fixture.root.join("bin");
    private_mkdir(&bin);
    let cursor = bin.join(Host::exe_name("cursor"));
    prints(&cursor, "Cursor 3.22.12\n");
    let _cursor_bin = EnvGuard::set("SIDEVOICE_CURSOR_BIN", cursor.as_os_str());
    write_json(
        &fixture.profile.data.join("agents.json"),
        &json!({
            "version":1,"scanned_at":null,"login_path":bin,"binaries":{},"seen":{},"dismissed":{}
        }),
    );
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    // Until the first look at the bridge has finished, the answer is provisional.
    let missing = host.handle("agents.list", json!({"watch":"cursor"})).await;
    assert_eq!(
        &row(&missing, "cursor")["readiness"],
        &json!({"state":"needs_attention","reasons":[{"code":"cursor.bridge_not_running","message":BRIDGE_MESSAGE}]})
    );
    // Neither the app nor a bridge: not supported here.
    assert_eq!(
        cursor_readiness_settles(&host, "unsupported").await,
        json!({"state":"unsupported","reasons":[{"code":"cursor.unsupported","message":UNSUPPORTED_MESSAGE}]})
    );

    let dir = fixture.profile.home.join(".cursor/desktop-bridge");
    private_mkdir(&fixture.profile.home.join(".cursor"));
    private_mkdir(&dir);
    // The route this OS publishes for the bridge (a Unix socket path, a `\\.\pipe\…` name).
    let endpoint = Endpoint::new(&fixture.root, "bridge");
    let _listener = Channels::listen(&endpoint).unwrap();
    let socket = endpoint.to_string();
    write_json(
        &dir.join("one.json"),
        &json!({
            "protocolVersion":1,"pid":std::process::id(),"socketPath":socket,
            "token":"secret-token","userDataDir":fixture.root.join("private-cursor-data")
        }),
    );
    let bridge = cursor_readiness_settles(&host, "ready").await;
    assert_eq!(bridge, json!({"state":"ready","reasons":[]}));
    let serialized = bridge.to_string();
    assert!(!serialized.contains(&fixture.root.to_string_lossy().to_string()));
    assert!(!serialized.contains("secret-token"));
}

#[tokio::test]
async fn cursor_readiness_distinguishes_cli_only_from_editor() {
    use crate::adapters::markers::Root;
    use crate::adapters::registration::Registration;
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let bin = fixture.root.join("bin");
    private_mkdir(&bin);
    let _path = EnvGuard::set("PATH", bin.as_os_str());
    let cli = bin.join(Host::exe_name("cursor-agent"));
    prints(&cli, "cursor-agent 1.0.0\n");
    write_json(
        &fixture.profile.data.join("agents.json"),
        &json!({
            "version":1,"scanned_at":null,"login_path":bin,"binaries":{},"seen":{},"dismissed":{}
        }),
    );
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    assert_eq!(
        cursor_readiness_settles(&host, "unsupported").await,
        json!({"state":"unsupported","reasons":[{"code":"cursor.unsupported","message":UNSUPPORTED_MESSAGE}]})
    );

    // A home-root install makes the missing bridge something to fix. Windows uses
    // SHGetKnownFolderPath via dirs::data_local_dir(), which ignores LOCALAPPDATA overrides.
    // Its installed-editor case is covered with injected Roots in bridge_setup's probe tests.
    let Some(marker) = crate::adapters::cursor::Cursor
        .install_markers()
        .iter()
        .find(|marker| marker.os == std::env::consts::OS && marker.root == Root::Home)
    else {
        return;
    };
    std::fs::create_dir_all(fixture.profile.home.join(marker.path)).unwrap();
    assert_eq!(
        cursor_readiness_settles(&host, "needs_attention").await,
        json!({"state":"needs_attention","reasons":[{"code":"cursor.bridge_not_running","message":BRIDGE_MESSAGE}]})
    );
}

#[test]
fn readiness_aggregation_prioritizes_unsupported_then_attention_then_ready() {
    use ReadinessCheck::{NeedsAttention, Ok, Unsupported};
    for (checks, expected) in [
        (vec![], None),
        (vec![Ok], Some(json!({"state":"ready","reasons":[]}))),
        (
            vec![
                Ok,
                NeedsAttention("cursor.bridge_not_running", BRIDGE_MESSAGE),
            ],
            Some(
                json!({"state":"needs_attention","reasons":[{"code":"cursor.bridge_not_running","message":BRIDGE_MESSAGE}]}),
            ),
        ),
        (
            vec![
                NeedsAttention("cursor.bridge_not_running", BRIDGE_MESSAGE),
                Unsupported("cursor.unsupported", UNSUPPORTED_MESSAGE),
            ],
            Some(
                json!({"state":"unsupported","reasons":[{"code":"cursor.bridge_not_running","message":BRIDGE_MESSAGE},{"code":"cursor.unsupported","message":UNSUPPORTED_MESSAGE}]}),
            ),
        ),
    ] {
        assert_eq!(aggregate_readiness(&checks), expected);
    }
}

#[test]
fn readiness_row_helper_attaches_only_nonempty_check_sets() {
    assert_eq!(
        with_readiness(
            json!({"id":"cursor"}),
            vec![ReadinessCheck::NeedsAttention(
                "cursor.bridge_not_running",
                BRIDGE_MESSAGE
            )]
        ),
        json!({"id":"cursor","readiness":{"state":"needs_attention","reasons":[{"code":"cursor.bridge_not_running","message":BRIDGE_MESSAGE}]}})
    );
    assert_eq!(
        with_readiness(json!({"id":"claude"}), vec![]),
        json!({"id":"claude"})
    );
}

#[tokio::test]
async fn profile_only_readiness_hook_defaults_empty_for_other_agents() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let claude = crate::adapters::claude::Claude.readiness_checks(&fixture.profile);
    let codex = crate::adapters::codex::Codex.readiness_checks(&fixture.profile);
    assert_eq!(aggregate_readiness(&claude), None);
    assert_eq!(aggregate_readiness(&codex), None);
    let cursor: &dyn Registration = &crate::adapters::cursor::Cursor;
    assert_eq!(cursor.readiness_checks(&fixture.profile).len(), 1);
}

fn identity_signature(
    id: &str,
    version: Option<&str>,
    evidence: &[Evidence<'_>],
    binary: Option<&str>,
) -> String {
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

/// This program is registered by its canonical path in plain form (`dunce`: not Windows' `\\?\` form where a plain
/// one names it), and an entry naming it in either spelling is ours.
#[test]
fn this_program_is_registered_by_its_plain_canonical_path() {
    let own = InstalledCommand::executable().unwrap();
    let canonical = std::env::current_exe().unwrap().canonicalize().unwrap();
    assert_eq!(Path::new(&own.command), dunce::simplified(&canonical));
    assert!(own.owns(&own.command, &own.args));
    assert!(own.owns(&canonical.to_string_lossy(), &own.args));
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
    Files::symlink(&escaped, &config_path);
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
    Files::make_executable(&fixture.cursor());
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
    Files::symlink(&external, fixture.profile.agent_home("CURSOR_CONFIG_DIR"));

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
    let executable = fixture.root.join("bin").join(Host::exe_name("codex"));
    private_mkdir(executable.parent().unwrap());
    let marker = fixture.root.join("codex-was-started");
    Script {
        write: Some((marker.clone(), "started".into())),
        ..Script::default()
    }
    .make(&executable);
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
    Files::symlink(&external, fixture.profile.agent_home("CODEX_HOME"));

    // Where the login PATH is asked of a login shell run with the profile, a symlinked agent home also stops that
    // capture, so the scan fails; elsewhere the connect itself refuses the profile. Either way the action is refused.
    let answer = host.handle("agents.connect", json!({"id":"codex"})).await;
    let refusal = match Environment::login_path() {
        Some(LoginPath::Run { .. }) => "agents.scan-failed",
        _ => "agents.invalid",
    };
    assert_eq!(answer.pointer("/error/key"), Some(&json!(refusal)));
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
    Files::symlink(
        missing,
        fixture.profile.agent_home("CODEX_HOME").join("config.toml"),
    );
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
    Files::symlink(&target, &config);
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
    Files::let_others_change(&target);
    let listed = list(host.clone()).await;
    assert_eq!(
        row(&listed, "codex")["registration"],
        "invalid",
        "group-writable: {listed}"
    );

    // A FIFO is a Unix file type: Windows has none to refuse.
    #[cfg(unix)]
    {
        fs::remove_file(&target).unwrap();
        let fifo = std::ffi::CString::new(target.to_string_lossy().into_owned()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let listed = list(host.clone()).await;
        assert_eq!(
            row(&listed, "codex")["registration"],
            "invalid",
            "FIFO: {listed}"
        );
    }
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
    let binary = fixture.root.join("bin").join(Host::exe_name("codex"));
    private_mkdir(binary.parent().unwrap());
    stand_ins::make(&binary, "codex-gate");
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
    let lock = Files::open_private(&lock_path, Create::ReadWrite).unwrap();
    lock.lock().unwrap();
    state["dismissed"]["codex"] = generation;
    Files::write_json(&state_path, &state).unwrap();
    lock.unlock().unwrap();
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
    let pid = wait_for_pid(&pid_file).await;
    abandoned.abort();
    let _ = abandoned.await;
    tokio::time::sleep(StdDuration::from_millis(100)).await;
    assert!(
        Processes::identity(pid as u32).is_none(),
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

/// Waits until the fixture has written a whole pid: the file exists a moment before its content does.
async fn wait_for_pid(path: &Path) -> i32 {
    let deadline = Instant::now() + StdDuration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(pid) = fs::read_to_string(path)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            return pid;
        }
        sleep(StdDuration::from_millis(10)).await;
    }
    panic!("timed out waiting for a pid in {}", path.display());
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
    let releases = data_dir::releases_dir(&profile.data);
    let binary = releases.join(format!("releases/1.0.0/{}", crate::install::binary()));
    private_mkdir(binary.parent().unwrap());
    private_write(&binary, b"#!/bin/sh\n");
    Files::make_executable(&binary);
    let stable = binary.clone();
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
    let other = releases.join(format!("releases/2.0.0/{}", crate::install::binary()));
    private_mkdir(other.parent().unwrap());
    private_write(&other, b"#!/bin/sh\n");
    assert!(
        installed.owns(&other.to_string_lossy(), &mcp),
        "another release's own binary"
    );
    // One whose path is too long for a plain form (Windows keeps it `\\?\…`, while the releases' is plain) is ours
    // all the same.
    let long = releases
        .join("releases")
        .join(format!("2.0.0-{}", "l".repeat(240)))
        .join(crate::install::binary());
    fs::create_dir_all(long.parent().unwrap()).unwrap();
    fs::write(&long, b"").unwrap();
    assert!(
        installed.owns(&long.to_string_lossy(), &mcp),
        "a release with a long path"
    );
    assert!(!installed.owns(&stable.to_string_lossy(), &["connector".into()]));
    assert!(!installed.owns("/usr/local/bin/sidevoice-connector", &mcp));
    let elsewhere = fixture
        .root
        .join(format!("elsewhere/{}", crate::install::binary()));
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
        &json!({"channel":"voice","session_id":"s","revision":1,"epoch":0,"message_id":"m","text":"hello"}),
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
#[cfg_attr(
    windows,
    ignore = "the test puts Codex on the login PATH through a login shell (`$SHELL`); Windows reads that PATH from the registry"
)]
async fn codex_delivery_runs_the_scanned_codex_with_the_login_path() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let bin = fixture.root.join("login-bin");
    private_mkdir(&bin);
    let marker = fixture.root.join("queued");
    let codex = bin.join(Host::exe_name("codex"));
    Script {
        record: Some(marker.clone()),
        record_path: true,
        ..Script::default()
    }
    .make(&codex);
    let login = login_path(&bin);
    let shell = fixture.root.join(Host::exe_name("bench-login-shell"));
    login_shell(&shell, &login);
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
        &json!({"channel":"voice","session_id":"s","revision":1,"epoch":1,"message_id":"m","text":"hola"}),
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
    let binary = fixture.root.join("bin").join(Host::exe_name("codex"));
    private_mkdir(binary.parent().unwrap());
    stand_ins::make(&binary, "codex");
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
    json!({"SIDEVOICE_DATA_DIR": fixture.profile.data})
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
    let binary = fixture.root.join("bin").join(Host::exe_name("claude"));
    private_mkdir(binary.parent().unwrap());
    stand_ins::make(&binary, "claude");
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
            "mcp add --scope user sidevoice -e SIDEVOICE_DATA_DIR={} --",
            fixture.profile.data.display()
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
    let foreign = fixture.root.join("bin").join(Host::exe_name("foreign"));
    Script {
        write: Some((ran.clone(), String::new())),
        ..Script::default()
    }
    .make(&foreign);
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
    stand_ins::make(&server, "mcp-server");
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
    let env = [format!(
        "SIDEVOICE_DATA_DIR={}",
        fixture.profile.data.display()
    )];
    // A command line as this OS's shell reads it: the call, then each word quoted.
    let quoted = |words: Vec<String>| {
        let words = words
            .iter()
            .map(|word| Environment::quote(word))
            .collect::<Vec<_>>()
            .join(" ");
        format!("{}{words}", Environment::CALL)
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
    let step = |words: Vec<String>| json!({"command":format!("{}{}", Environment::set("CLAUDE_CONFIG_DIR", &Environment::quote(&claude_home)), quoted(words)),"file":file});
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
    let step = |words: Vec<String>| json!({"command":format!("{}{}", Environment::set("CODEX_HOME", &Environment::quote(&codex_home)), quoted(words)),"file":file});
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

/// `connect` (on an outdated entry) and `update` each rewrite an older entry: each runs from one.
#[tokio::test]
async fn the_manual_steps_run_by_hand_update_an_older_entry_in_the_agents_own_home() {
    manual_steps_by_hand(&["connect", "update"]).await;
}

/// The older `instructions` (connecting as one step) rewrite an older entry too.
#[tokio::test]
async fn the_joined_instructions_run_by_hand_update_an_older_entry() {
    manual_steps_by_hand(&["instructions"]).await;
}

/// Runs each of `kinds` by hand from an older entry of ours, in the agents' own homes, and checks it connects.
async fn manual_steps_by_hand(kinds: &[&str]) {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let claude = fake_claude(&fixture);
    let codex = fake_codex(&fixture);
    let _claude = EnvGuard::set("SIDEVOICE_CLAUDE_BIN", claude.as_os_str());
    let _codex = EnvGuard::set("SIDEVOICE_CODEX_BIN", codex.as_os_str());
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let selected = fixture.selected();
    for kind in kinds {
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
            let steps = if *kind == "instructions" {
                let command = agent["instructions"]["command"].as_str().unwrap();
                let update: Vec<&str> = agent["manual"]["update"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|step| step["command"].as_str().unwrap())
                    .collect();
                assert_eq!(
                    command,
                    Environment::in_sequence(&update),
                    "removes, then adds: {agent}"
                );
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
                let status = run_by_hand(command, &fixture.root.join("elsewhere"));
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
                    command.starts_with(&format!(
                        "{}{} ",
                        Environment::CALL,
                        Environment::quote(&cli.to_string_lossy())
                    )),
                    "{command}"
                );
            }
        }
        // The person's shell: their HOME, no agent home named.
        for step in manual["connect"].as_array().unwrap() {
            let command = step["command"].as_str().unwrap();
            let status = run_by_hand(command, &home);
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
            "mcp add --scope user sidevoice -e SIDEVOICE_DATA_DIR={} --",
            fixture.profile.data.display()
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

/// A shell whose PATH (Homebrew included) is set only by its rc file and the files that rc sources dynamically, with
/// a noisy prompt line and nothing in the login files: what the capture must still see.
async fn capture_with_rc_only_path(shell_name: &str, real_shell: &str, rc: &str) {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    // The child's HOME is the profile's, whatever this process's is.
    let home = fixture.profile.home.clone();
    fs::create_dir_all(home.join(".config/sh")).unwrap();
    private_write(
        &home.join(".config/sh/01-path.sh"),
        b"export PATH=\"/opt/homebrew/bin:$PATH\"\n",
    );
    private_write(
        &home.join(rc),
        b"echo 'prompt noise'\nfor f in \"$HOME\"/.config/sh/*.sh; do . \"$f\"; done\n",
    );
    let shell = fixture.root.join(shell_name);
    private_write(
        &shell,
        format!("#!/bin/sh\nexec {real_shell} \"$@\"\n").as_bytes(),
    );
    Files::make_executable(&shell);
    let _shell = EnvGuard::set("SHELL", shell.as_os_str());
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let path = host
        .capture_login_path(
            &Cancellation::new(),
            Instant::now() + StdDuration::from_secs(5),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(path.starts_with("/opt/homebrew/bin:"), "{path}");
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "the login PATH is asked of a login shell (`$SHELL`) on Unix; Windows reads it from the registry, so no shell can fail or change it"
)]
async fn login_path_comes_from_a_bashrc_only_path() {
    capture_with_rc_only_path("bash", "/bin/bash", ".bashrc").await;
}

#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "the login PATH is asked of a login shell (`$SHELL`) on Unix; Windows reads it from the registry, so no shell can fail or change it"
)]
async fn login_path_comes_from_a_zshrc_only_path() {
    if let Some(zsh) = ["/bin/zsh", "/usr/bin/zsh"]
        .into_iter()
        .find(|path| Path::new(path).exists())
    {
        capture_with_rc_only_path("zsh", zsh, ".zshrc").await;
    }
}

/// A scanned fake `codex` that reports `version` and records every call it gets in `calls`, in a profile whose Codex
/// home may hold a daemon socket.
fn steer_fixture(version: &str) -> (Fixture, PathBuf) {
    let fixture = Fixture::new();
    let bin = fixture.root.join("login-bin");
    private_mkdir(&bin);
    let calls = fixture.root.join("calls");
    Script {
        version: Some(format!("codex-cli {version}")),
        record: Some(calls.clone()),
        append: true,
        ..Script::default()
    }
    .make(&bin.join(Host::exe_name("codex")));
    let mut state = empty_state();
    state["login_path"] = json!(login_path(&bin));
    write_json(&fixture.profile.data.join("agents.json"), &state);
    (fixture, calls)
}

/// A login `PATH` with `bin` first, then the system's directories, as this OS writes a list.
fn login_path(bin: &Path) -> String {
    std::env::join_paths([bin, Path::new("/usr/bin"), Path::new("/bin")])
        .unwrap()
        .into_string()
        .unwrap()
}

fn voice_event(id: &str) -> Value {
    json!({"channel":"voice","session_id":"s","revision":1,"epoch":1,"message_id":id,"text":"hola"})
}

#[cfg(unix)]
fn running_turn_answers(steer: Value) -> Vec<Value> {
    vec![
        json!({"userAgent":"x"}),
        json!({"data":[{"id":"turn-1","status":"inProgress"}]}),
        steer,
    ]
}

async fn deliver_to(fixture: &Fixture, id: &str) -> Value {
    crate::adapters::codex::deliver(
        &json!({"kind":"codex-queue","thread":"t-1"}),
        &voice_event(id),
        &fixture.profile,
    )
    .await
    .unwrap()
}

#[tokio::test]
#[cfg(unix)]
async fn codex_delivery_steers_the_running_turn_when_the_version_allows() {
    let _turn = agent_env().await;
    let (fixture, calls) = steer_fixture("0.157.0");
    let socket =
        crate::adapters::codex::steer::socket_path(fixture.profile.agent_home("CODEX_HOME"));
    let daemon = crate::adapters::codex::steer::daemon_at(
        socket,
        running_turn_answers(json!({"turnId":"turn-1"})),
    )
    .await;
    let answer = deliver_to(&fixture, "m-steer").await;
    assert_eq!(answer["status"], "accepted");
    assert!(
        answer["detail"].as_str().unwrap().contains("steered"),
        "{answer}"
    );
    assert!(!calls.exists(), "the queue was not used");
    let seen = daemon.await.unwrap();
    let steer = seen
        .iter()
        .find(|frame| frame["method"] == "turn/steer")
        .unwrap();
    assert_eq!(steer["params"]["clientUserMessageId"], "m-steer");
    let text = steer["params"]["input"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("\"message_id\":\"m-steer\"") && text.contains("hola"),
        "{text}"
    );
}

#[tokio::test]
#[cfg(unix)]
async fn codex_delivery_queues_when_the_version_is_too_old() {
    let _turn = agent_env().await;
    let (fixture, calls) = steer_fixture("0.156.0");
    let socket =
        crate::adapters::codex::steer::socket_path(fixture.profile.agent_home("CODEX_HOME"));
    let daemon = crate::adapters::codex::steer::daemon_at(socket, vec![]).await;
    let answer = deliver_to(&fixture, "m-old").await;
    assert_eq!(answer["detail"], "codex queue confirmed the thread");
    assert!(fs::read_to_string(calls)
        .unwrap()
        .starts_with("queue\n--thread\nt-1\n--message\n"));
    daemon.abort();
}

#[tokio::test]
async fn codex_delivery_queues_when_no_daemon_socket_exists() {
    let _turn = agent_env().await;
    let (fixture, calls) = steer_fixture("0.163.0");
    let answer = deliver_to(&fixture, "m-nosock").await;
    assert_eq!(answer["detail"], "codex queue confirmed the thread");
    assert!(fs::read_to_string(calls).unwrap().starts_with("queue\n"));
}

#[tokio::test]
#[cfg(unix)]
async fn codex_delivery_queues_when_the_steer_is_refused() {
    let _turn = agent_env().await;
    let (fixture, calls) = steer_fixture("0.163.0");
    let socket =
        crate::adapters::codex::steer::socket_path(fixture.profile.agent_home("CODEX_HOME"));
    let daemon = crate::adapters::codex::steer::daemon_at(
        socket,
        running_turn_answers(json!({"code":-32600,"message":"active turn not steerable"})),
    )
    .await;
    let answer = deliver_to(&fixture, "m-refused").await;
    assert_eq!(answer["detail"], "codex queue confirmed the thread");
    let queued = fs::read_to_string(calls).unwrap();
    assert!(queued.starts_with("queue\n--thread\nt-1\n--message\n"));
    assert!(
        queued.contains("\"message_id\":\"m-refused\""),
        "same message, same id"
    );
    daemon.await.unwrap();
}

#[test]
fn a_scan_that_fails_after_an_action_says_the_change_was_applied() {
    let failed = json!({"scan_failed":true});
    let key = |result: std::result::Result<(), Failure>| match result {
        Err(Failure::Key(key, _)) => key,
        _ => "",
    };
    assert_eq!(key(require_scanned(&failed, "codex")), "agents.scan-failed");
    assert_eq!(
        key(require_verified(&failed, "codex")),
        "agents.scan-failed-after"
    );
    assert!(require_verified(&json!({"scan_failed":false}), "codex").is_ok());
}

/// sidevoice/sidevoice-connector#266: Claude Code installed but never started has no home yet. Connecting it runs its
/// CLI, which makes the home as its first start would, instead of refusing for a directory that is not there.
#[tokio::test]
async fn claude_never_started_is_connected_and_its_cli_makes_its_home() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let binary = fake_claude(&fixture);
    let _claude = EnvGuard::set("SIDEVOICE_CLAUDE_BIN", binary.as_os_str());
    let home = fixture
        .profile
        .agent_home("CLAUDE_CONFIG_DIR")
        .to_path_buf();
    fs::remove_dir_all(&home).unwrap();
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());

    let connected = host.handle("agents.connect", json!({"id":"claude"})).await;
    assert!(connected.get("error").is_none(), "{connected}");
    assert_eq!(
        row(&connected, "claude")["registration"],
        "connected",
        "{connected}"
    );
    assert!(home.is_dir(), "the CLI did not make {}", home.display());
    host.shutdown().await;
}

/// Claude Code on the login PATH `$SHELL` says (`login-bin`), and that shell overtaken by another scan of the same data
/// directory the first `times` times it runs: the other scan records its own login PATH, where nothing is found.
fn overtaken_scans(fixture: &Fixture, times: u64) -> PathBuf {
    let bin = fixture.root.join("login-bin");
    private_mkdir(&bin);
    Script::default().make(&bin.join(Host::exe_name("claude")));
    let shell = fixture.root.join(Host::exe_name("login-shell"));
    crate::test_support::stand_in::make(
        &shell,
        &json!({
            "play": "overtaken-login-shell",
            "path": bin.to_string_lossy(),
            "times": times,
            "marker": fixture.root.join("overtaken"),
            "file": fixture.profile.data.join("agents.json"),
            "state": {
                "version": 1,
                "login_path": fixture.root.join("service-bin").to_string_lossy(),
                "scanned_at": "2026-10-24T00:14:52.000Z",
                "scan_failed": false,
                "binaries": {},
                "seen": {"claude": {"present": false}},
                "dismissed": {},
            },
        }),
    );
    shell
}

/// Two scans of one data directory overlap with different login PATHs (the daemon's at its start, from the service's
/// environment, while `install` scans from the person's): the one overtaken scans again from what the other recorded,
/// so it answers with what it found, never with the other's listing or with nothing.
#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "the login PATH is asked of a login shell (`$SHELL`) on Unix; Windows reads it from the registry, so no shell can be overtaken"
)]
async fn a_scan_overtaken_by_another_scans_again_and_answers_what_it_found() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let _no_cursor_override = EnvGuard::set("SIDEVOICE_CURSOR_BIN", std::ffi::OsStr::new(""));
    let shell = overtaken_scans(&fixture, 1);
    let _shell = EnvGuard::set("SHELL", shell.as_os_str());
    let host = HostAgents::for_installer(fixture.profile.clone()).unwrap();

    let listed = host.handle("agents.list", json!({"rescan":true})).await;
    assert!(
        fixture.root.join("overtaken").exists(),
        "the other scan never wrote"
    );
    assert!(listed.get("error").is_none(), "{listed}");
    assert_eq!(listed["scan_failed"], false, "{listed}");
    assert!(listed_ids(&listed).contains(&"claude".into()), "{listed}");
    host.shutdown().await;
}

/// Overtaken on every attempt, a scan says it failed rather than answer with the other scan's listing as its own; what
/// the other scan recorded is left as it recorded it.
#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "the login PATH is asked of a login shell (`$SHELL`) on Unix; Windows reads it from the registry, so no shell can be overtaken"
)]
async fn a_scan_overtaken_on_every_attempt_says_it_failed() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let _no_cursor_override = EnvGuard::set("SIDEVOICE_CURSOR_BIN", std::ffi::OsStr::new(""));
    let shell = overtaken_scans(&fixture, SCAN_ATTEMPTS as u64);
    let _shell = EnvGuard::set("SHELL", shell.as_os_str());
    let host = HostAgents::for_installer(fixture.profile.clone()).unwrap();

    let listed = host.handle("agents.list", json!({"rescan":true})).await;
    assert_eq!(listed["scan_failed"], true, "{listed}");
    let runs = fs::read_to_string(fixture.root.join("overtaken")).unwrap();
    assert_eq!(
        runs.trim(),
        SCAN_ATTEMPTS.to_string(),
        "one capture per attempt"
    );
    let recorded: Value =
        serde_json::from_slice(&fs::read(fixture.profile.data.join("agents.json")).unwrap())
            .unwrap();
    assert_eq!(recorded["scan_failed"], false, "{recorded}");
    host.shutdown().await;
}

/// The reverse order: a scan from the person's environment found Claude Code; a later one from the service's, whose
/// login PATH does not have it (the daemon's start or its six-hourly rescan), records it absent, and a cached listing
/// then drops it. Which source wins is a decision for the operator (sidevoice/sidevoice-connector#269, "Decision for
/// the operator"): kept here as what the chosen design must make pass.
#[tokio::test]
#[ignore = "TODO(sidevoice/sidevoice-connector#269): presence across scan sources is undecided (operator decision)"]
async fn a_scan_from_the_service_environment_does_not_drop_what_the_persons_found() {
    let _turn = agent_env().await;
    let fixture = Fixture::new();
    let _no_cursor_override = EnvGuard::set("SIDEVOICE_CURSOR_BIN", std::ffi::OsStr::new(""));
    let shell = overtaken_scans(&fixture, 0);
    let host = HostAgents::for_installer(fixture.profile.clone()).unwrap();
    {
        let _shell = EnvGuard::set("SHELL", shell.as_os_str());
        let found = host.handle("agents.list", json!({"rescan":true})).await;
        assert!(listed_ids(&found).contains(&"claude".into()), "{found}");
    }
    let service = fixture.root.join(Host::exe_name("service-shell"));
    login_shell(
        &service,
        &fixture.root.join("service-bin").to_string_lossy(),
    );
    {
        let _shell = EnvGuard::set("SHELL", service.as_os_str());
        let daemon = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
        daemon.handle("agents.list", json!({"rescan":true})).await;
        daemon.shutdown().await;
    }
    let cached = host.handle("agents.list", json!({})).await;
    assert!(listed_ids(&cached).contains(&"claude".into()), "{cached}");
    host.shutdown().await;
}

#[test]
fn native_resolution_errors_and_unrunnable_results_keep_the_launcher() {
    let dir = crate::test_support::temp_dir("agent-resolver-fallback");
    let launcher = dir.path().join(Host::exe_name("launcher"));
    Files::open_private(&launcher, Create::NewProgram).unwrap();
    let found = launcher.to_str().unwrap().to_owned();
    assert_eq!(
        resolved_program(found.clone(), Err(std::io::Error::other("resolver failed"))),
        Some(found.clone())
    );
    assert_eq!(
        resolved_program(found.clone(), Ok(Some(dir.path().join("missing-native")))),
        Some(found.clone())
    );
    // If the launcher disappears too, resolution returns None so find_binary can try fallback_binaries.
    std::fs::remove_file(&launcher).unwrap();
    assert_eq!(
        resolved_program(found, Err(std::io::Error::other("resolver failed"))),
        None
    );
}
