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
        login_path: "/usr/bin".into(),
    };
    let mcp = profile.mcp_env();
    assert_eq!(
        Path::new(&mcp["SIDEVOICE_DATA_DIR"]),
        Path::new("/p/home").join(
            sidevoice_connector::platform::data_dir::standard()
                .unwrap()
                .file_name()
                .unwrap()
        )
    );
    assert_eq!(mcp["XDG_DATA_HOME"], "/p/home/.local/share");
    assert_eq!(mcp["SIDEVOICE_SERVICE_MANAGER"], "none");
    assert_eq!(mcp["SHELL"], "/p/bench/login-shell");
    assert!(!mcp.contains_key("SIDEVOICE_CODEX_BIN"));
    let agent = profile.agent_env();
    assert!(agent.keys().all(|name| !name.starts_with("XDG_")));
    assert_eq!(agent["SOME_AGENT_HOME"], "/p/agents/some_agent_home");
}

#[test]
fn bench_login_shell_exposes_captured_cli_path_without_binary_override() {
    let root = std::env::temp_dir().join(format!(
        "sidevoice-testbench-shell-{}",
        uuid::Uuid::new_v4()
    ));
    let bin = root.join("person bin");
    fs::create_dir_all(root.join("bench")).unwrap();
    fs::create_dir_all(&bin).unwrap();
    fs::write(bin.join("codex"), b"#!/bin/sh\nexit 0\n").unwrap();
    let path = format!("{}:/usr/bin", bin.display());
    write_bench_login_shell(&root, &path).unwrap();
    let output = Command::new(root.join("bench/login-shell"))
        .args(["-lc", "ignored"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        stdout,
        format!("\u{1e}SIDEVOICE_PATH_BEGIN\u{1e}{path}\u{1e}SIDEVOICE_PATH_END\u{1e}")
    );
    assert!(bin.join("codex").is_file());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bench_captures_process_path_when_runner_has_no_login_shell() {
    let old = std::env::var_os("SHELL");
    std::env::remove_var("SHELL");
    let captured = capture_person_login_path();
    if let Some(old) = old {
        std::env::set_var("SHELL", old);
    }
    assert_eq!(captured.unwrap(), std::env::var("PATH").unwrap());
    assert!(checked_bench_path("relative:/usr/bin".into()).is_err());
}
