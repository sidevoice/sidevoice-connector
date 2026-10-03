use super::*;
use std::os::unix::fs::{symlink, PermissionsExt};

struct Fixture {
    root: PathBuf,
    profile: Profile,
}

struct EnvGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &std::ffi::OsStr) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
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
        for name in ["home", "claude", "codex", "cursor", "sidevoice"] {
            private_mkdir(&root.join(name));
        }
        private_mkdir(&root.join("sidevoice/core"));
        let data = root.join("sidevoice");
        let profile = Profile {
            root: root.clone(),
            home: root.join("home"),
            claude: root.join("claude"),
            codex: root.join("codex"),
            cursor: root.join("cursor"),
            socket: data.join("connector.sock"),
            core_socket: data.join("core/local.sock"),
            core_ready: data.join("core/core.json"),
            data,
        };
        Self { root, profile }
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

fn row<'a>(answer: &'a Value, id: &str) -> &'a Value {
    answer["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["id"] == id)
        .unwrap()
}

fn identity_signature(id: &str, version: Option<&str>, evidence: &[Evidence<'_>]) -> String {
    let identity = format!(
        "{{\"id\":{},\"version\":{},\"evidence\":{}}}",
        serde_json::to_string(id).unwrap(),
        serde_json::to_string(&version).unwrap(),
        serde_json::to_string(evidence).unwrap(),
    );
    hex::encode(Sha256::digest(identity.as_bytes()))
}

#[test]
fn agent_refusals_labels_and_manual_text_use_the_shared_english_bundle() {
    assert_eq!(
        agent_message("agents.invalid", &json!({"agent":"Codex"})),
        "Codex has an unreadable or malformed Sidevoice registration; it was left unchanged."
    );
    assert_eq!(AgentId::Claude.label(), "Claude Code");
    assert_eq!(
        agent_message(
            "agents.manual.codex.replace-existing",
            &json!({"remove":"codex mcp remove sidevoice","add":"codex mcp add sidevoice"})
        ),
        "# Replace the existing Sidevoice entry by running these Codex commands:\n\
         codex mcp remove sidevoice\n\
         codex mcp add sidevoice"
    );
}

#[test]
fn proof_ownership_is_exact_and_js_release_fixtures_stay_narrow() {
    let fixture = Fixture::new();
    assert_eq!(
        Profile::from_root(&fixture.root).unwrap().root,
        fixture.profile.root
    );
    let proof = InstalledCommand::proof(&fixture.profile).unwrap();
    let selected_args = proof.args.clone();
    for id in [AgentId::Claude, AgentId::Codex, AgentId::Cursor] {
        assert!(proof.owns(id, &proof.command, &selected_args, None));
    }
    assert!(!proof.owns(
        AgentId::Codex,
        &proof.command,
        &["mcp".into(), "--profile-root".into(), "/tmp/other".into()],
        None
    ));
    assert!(!proof.owns(AgentId::Claude, &proof.command, &["mcp".into()], None));
    let old_codex_env = json!({
        "SIDEVOICE_DATA_DIR":fixture.profile.data,
        "CODEX_HOME":fixture.profile.codex,
    });
    assert!(proof.owns(
        AgentId::Codex,
        &proof.command,
        &["mcp".into()],
        Some(&old_codex_env)
    ));
    assert!(!proof.owns(
        AgentId::Cursor,
        &proof.command,
        &["mcp".into()],
        Some(&old_codex_env)
    ));
    assert!(!proof.owns(
        AgentId::Codex,
        &proof.command,
        &["mcp".into()],
        Some(&json!({"SIDEVOICE_DATA_DIR":"/tmp/other","CODEX_HOME":"/tmp/other"}))
    ));
    assert!(!proof.owns(AgentId::Codex, "/tmp/other/sidevoice", &selected_args, None));

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
        AgentId::Codex,
        "/usr/local/bin/node",
        &[node_cli.to_string_lossy().into_owned(), "mcp".into()],
        None
    ));
    let old_native = releases.join("releases/1.2.3/dist/sidevoice");
    let native = InstalledCommand::selected(
        vec![releases
            .join("current/dist/sidevoice")
            .to_string_lossy()
            .into_owned()],
        releases.clone(),
    );
    assert!(native.owns(
        AgentId::Claude,
        &old_native.to_string_lossy(),
        &["mcp".into()],
        None
    ));
    assert!(!native.owns(
        AgentId::Claude,
        "/tmp/other/dist/sidevoice",
        &["mcp".into()],
        None
    ));
}

#[tokio::test]
async fn cursor_actions_preserve_js_state_and_refuse_foreign_invalid_and_escaped_files() {
    let fixture = Fixture::new();
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let old = fixture.root.join("selected/releases/1.2.3/dist/sidevoice");
    let current = fixture.root.join("selected/current/dist/sidevoice");
    let config_path = fixture.profile.cursor.join("mcp.json");
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
    let evidence = vec![Evidence {
        kind: "config-dir",
        path: fixture.profile.cursor.to_string_lossy().into_owned(),
    }];
    let signature = identity_signature("cursor", None, &evidence);
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
    assert_eq!(row(&listed, "cursor")["registration"], "not-connected");
    assert_eq!(row(&listed, "cursor")["dismissed"], false);
    assert_eq!(row(&listed, "cursor")["actionable"], true);
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

#[tokio::test]
async fn disappeared_and_reappeared_config_gets_a_new_generation() {
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
    fs::remove_dir_all(&fixture.profile.cursor).unwrap();
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
    private_mkdir(&fixture.profile.cursor);
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
    fs::remove_dir_all(&fixture.profile.cursor).unwrap();
    symlink(&external, &fixture.profile.cursor).unwrap();

    let answer = host
        .handle("agents.connect", json!({"id":"cursor"}))
        .await;
    assert_eq!(
        answer.pointer("/error/key"),
        Some(&json!("agents.invalid"))
    );
    assert_eq!(fs::read(&config).unwrap(), before);
    host.shutdown().await;
    let _ = fs::remove_dir_all(external);
}

#[tokio::test]
async fn replacing_codex_profile_with_symlink_refuses_connect_before_cli_spawn() {
    let fixture = Fixture::new();
    let executable = fixture.root.join("bin/codex");
    private_mkdir(executable.parent().unwrap());
    let marker = fixture.root.join("codex-was-started");
    private_write(
        &executable,
        format!("#!/bin/sh\nprintf started > '{}'\n", marker.display()).as_bytes(),
    );
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
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
    fs::remove_dir_all(&fixture.profile.codex).unwrap();
    symlink(&external, &fixture.profile.codex).unwrap();

    let answer = host
        .handle("agents.connect", json!({"id":"codex"}))
        .await;
    assert_eq!(
        answer.pointer("/error/key"),
        Some(&json!("agents.invalid"))
    );
    assert!(!marker.exists(), "Codex CLI ran with a replaced private profile");
    assert_eq!(fs::read(&config).unwrap(), before);

    // The spawn boundary also rejects replacement after binary resolution by an earlier request.
    let direct = run_command(
        &fixture.profile,
        Some(&fixture.profile.codex),
        executable.to_str().unwrap(),
        &["mcp".into(), "get".into(), "sidevoice".into()],
        &Cancellation::new(),
        Instant::now() + StdDuration::from_secs(1),
    )
    .await;
    assert!(matches!(direct, Err(CommandFailure::Start)));
    assert!(!marker.exists(), "CLI spawn boundary accepted a replaced profile");
    host.shutdown().await;
    let _ = fs::remove_dir_all(external);
}

#[tokio::test]
async fn dangling_codex_config_symlink_is_invalid_not_absent() {
    let fixture = Fixture::new();
    let host = HostAgents::with_selected(fixture.profile.clone(), fixture.selected());
    let missing = fixture.root.join("missing-config-target.toml");
    symlink(missing, fixture.profile.codex.join("config.toml")).unwrap();
    let answer = host
        .handle("agents.connect", json!({"id":"codex"}))
        .await;
    assert_eq!(
        answer.pointer("/error/key"),
        Some(&json!("agents.invalid"))
    );
    host.shutdown().await;
}

#[tokio::test]
async fn complete_request_gate_orders_scans_mutations_and_external_dismissals() {
    let fixture = Fixture::new();
    let binary = fixture.root.join("bin/codex");
    private_mkdir(binary.parent().unwrap());
    private_write(
        &binary,
        br##"#!/usr/bin/python3
import json, os, pathlib, sys, time
home = pathlib.Path(os.environ['CODEX_HOME'])
state_file = home / 'fake-entry.json'
config_file = home / 'config.toml'
args = sys.argv[1:]
if args == ['--version']:
    print('codex fixture 1')
    raise SystemExit(0)
if args[:3] == ['mcp', 'get', 'sidevoice']:
    state = json.loads(state_file.read_text()) if state_file.exists() else None
    timeout_next = home / 'timeout-next-get'
    if timeout_next.exists():
        timeout_next.unlink()
        time.sleep(4)
    slow = home / 'slow-next-get'
    if slow.exists():
        slow.unlink()
        (home / 'slow-get-started').write_text(str(os.getpid()))
        time.sleep(.8)
    if state is None:
        print('No such server: sidevoice', file=sys.stderr)
        raise SystemExit(1)
    print(json.dumps(state))
    raise SystemExit(0)
if args[:3] == ['mcp', 'remove', 'sidevoice']:
    state_file.unlink(missing_ok=True)
    config_file.unlink(missing_ok=True)
    uncertain = home / 'timeout-after-remove'
    if uncertain.exists():
        uncertain.unlink()
        (home / 'timeout-next-get').write_text('1')
    raise SystemExit(0)
if args[:3] == ['mcp', 'add', 'sidevoice'] and '--' in args:
    command = args[args.index('--') + 1]
    command_args = args[args.index('--') + 2:]
    unregistered = home / 'leave-unregistered-after-add'
    if unregistered.exists():
        unregistered.unlink()
        raise SystemExit(0)
    slow = home / 'slow-next-add'
    if slow.exists():
        slow.unlink()
        (home / 'slow-add-started').write_text(str(os.getpid()))
        time.sleep(.8)
    state = {'name':'sidevoice','transport':{'type':'stdio','command':command,'args':command_args},'enabled':True}
    state_file.write_text(json.dumps(state))
    config_file.write_text('[mcp_servers.sidevoice]\ncommand = ' + json.dumps(command) + '\nargs = ' + json.dumps(command_args) + '\n')
    raise SystemExit(0)
print('unsupported fixture command', file=sys.stderr)
raise SystemExit(2)
"##,
    );
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let _codex_override = EnvGuard::set("SIDEVOICE_CODEX_BIN", binary.as_os_str());
    let mut initial = empty_state();
    initial["login_path"] = json!("/usr/bin");
    write_json(&fixture.profile.data.join("agents.json"), &initial);
    let host = HostAgents::new(fixture.profile.clone()).unwrap();

    let started = fixture.profile.codex.join("slow-get-started");
    private_write(&fixture.profile.codex.join("slow-next-get"), b"1");
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
    let slow_scan = fixture.profile.codex.join("slow-get-started");
    let state_path = fixture.profile.data.join("agents.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    let generation = state["seen"]["codex"]["generation"].clone();
    private_write(&fixture.profile.codex.join("slow-next-get"), b"1");
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
    private_write(&fixture.profile.codex.join("slow-next-add"), b"1");
    let add_started = fixture.profile.codex.join("slow-add-started");
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

    private_write(&fixture.profile.codex.join("leave-unregistered-after-add"), b"1");
    let unconfirmed_connect = host
        .handle("agents.connect", json!({"id":"codex"}))
        .await;
    assert_eq!(
        unconfirmed_connect.pointer("/error/key"),
        Some(&json!("agents.registration-not-confirmed"))
    );
    assert!(!fixture.profile.codex.join("fake-entry.json").exists());

    let restored = host.handle("agents.connect", json!({"id":"codex"})).await;
    assert_eq!(row(&restored, "codex")["registration"], "connected");
    private_write(&fixture.profile.codex.join("timeout-after-remove"), b"1");
    let uncertain_disconnect = host
        .handle("agents.disconnect", json!({"id":"codex"}))
        .await;
    assert_eq!(
        uncertain_disconnect.pointer("/error/key"),
        Some(&json!("agents.registration-unknown"))
    );
    assert!(!fixture.profile.codex.join("fake-entry.json").exists());

    // Dropping the link-side waiter cancels the owned operation. The delayed add is killed before
    // it can write, and a subsequent request re-inspects the actual entry before connecting.
    fs::remove_file(&add_started).unwrap();
    private_write(&fixture.profile.codex.join("slow-next-add"), b"1");
    let pid_file = fixture.profile.codex.join("slow-add-started");
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
    assert!(!fixture.profile.codex.join("fake-entry.json").exists());
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
