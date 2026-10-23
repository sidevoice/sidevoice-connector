//! Sidevoice's entry in Claude Code: written with `claude mcp`, user scope; read back from its user configuration
//! (`adapters::claude::user_config`).

use super::Claude;
use crate::adapters::registration::{
    cli_step, run_required, strings, AgentError, AgentHost, BenchSetup, Entry, Launch, Manual,
    Registration,
};
use crate::profile::Profile;
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

const REMOVE: [&str; 5] = ["mcp", "remove", "--scope", "user", "sidevoice"];

impl Claude {
    /// The `sidevoice` entry of Claude Code's user scope, read from its configuration file and never through `claude
    /// mcp get`, which starts the server it names: a foreign command must not run, and ours would start the daemon.
    async fn entry_state(
        &self,
        host: &dyn AgentHost,
        binary: Option<&str>,
    ) -> Result<Entry, AgentError> {
        let profile = host.profile();
        // Settings Claude Code cannot parse are left alone, whatever else is there: nothing else is judged.
        let Ok(config) = super::read_user_config(profile) else {
            return Ok(Entry::bare("invalid"));
        };
        match fs::symlink_metadata(profile.agent_home(super::HOME.var)) {
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Entry::bare("absent")),
            Err(_) => return Ok(Entry::bare("invalid")),
            Ok(_) => {}
        }
        // Registering goes through its CLI: without one, what the file says cannot be acted on.
        if binary.is_none() {
            return Ok(Entry::bare("unknown"));
        }
        let Some(config) = config else {
            return Ok(Entry::bare("absent"));
        };
        let Some(config) = config.as_object() else {
            return Ok(Entry::bare("invalid"));
        };
        let Some(servers) = config.get("mcpServers") else {
            return Ok(Entry::bare("absent"));
        };
        let Some(entry) = servers
            .as_object()
            .and_then(|servers| servers.get("sidevoice"))
        else {
            return Ok(Entry::bare(if servers.is_object() {
                "absent"
            } else {
                "invalid"
            }));
        };
        // Another transport (`http`, `sse`) or no command: not an entry Sidevoice writes.
        let stdio = entry
            .get("type")
            .and_then(Value::as_str)
            .is_none_or(|kind| kind == "stdio");
        let Some(command) = entry
            .get("command")
            .and_then(Value::as_str)
            .filter(|command| stdio && !command.is_empty())
            .map(str::to_owned)
        else {
            return Ok(Entry::bare("foreign"));
        };
        let Some(args) = strings_of(entry.get("args")) else {
            return Ok(Entry::bare("invalid"));
        };
        let env = entry
            .get("env")
            .and_then(Value::as_object)
            .map(|env| {
                env.iter()
                    .map(|(key, value)| (key.clone(), value.as_str().unwrap_or("").to_owned()))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        let state = if !host.owns(&command, &args) {
            "foreign"
        } else if command == host.command() && args == host.args() && env == host.mcp_env() {
            "connected"
        } else {
            "owned-old"
        };
        Ok(Entry::with(
            state,
            Launch {
                command,
                args,
                env,
                cwd: None,
            },
        ))
    }

    async fn registration_state(
        &self,
        host: &dyn AgentHost,
        binary: Option<&str>,
    ) -> Result<Entry, AgentError> {
        let mut entry = self.entry_state(host, binary).await?;
        match entry.registration.as_str() {
            "absent" => entry.registration = "not-connected".into(),
            "owned-old" => entry.registration = "outdated".into(),
            _ => {}
        }
        Ok(entry)
    }

    async fn connect_entry(
        &self,
        host: &dyn AgentHost,
        binary: Option<&str>,
    ) -> Result<(), AgentError> {
        let binary = binary.ok_or(AgentError::Key("agents.manual-required"))?;
        let state = self.entry_state(host, Some(binary)).await?.registration;
        if state == "connected" {
            return Ok(());
        }
        if state == "foreign" {
            return Err(AgentError::Key("agents.foreign"));
        }
        if state == "invalid" {
            return Err(AgentError::Key("agents.invalid"));
        }
        if state == "unknown" {
            return Err(AgentError::Key("agents.registration-unknown"));
        }
        host.live()?;
        if state == "absent" || state == "owned-old" {
            // A second probe just before mutation closes the gap with an external CLI writer.
            let again = self.entry_state(host, Some(binary)).await?.registration;
            if again == "foreign" {
                return Err(AgentError::Key("agents.foreign"));
            }
            if again == "invalid" {
                return Err(AgentError::Key("agents.invalid"));
            }
            if again == "unknown" {
                return Err(AgentError::Key("agents.registration-unknown"));
            }
            if again == "connected" {
                return Ok(());
            }
            if again == "owned-old" {
                run_required(host, binary, &strings(&REMOVE)).await?;
            }
        }
        run_required(host, binary, &add_args(host)).await
    }

    async fn disconnect_entry(
        &self,
        host: &dyn AgentHost,
        binary: Option<&str>,
    ) -> Result<(), AgentError> {
        let binary = binary.ok_or(AgentError::Key("agents.registration-unknown"))?;
        let state = self
            .registration_state(host, Some(binary))
            .await?
            .registration;
        match state.as_str() {
            "not-connected" => Ok(()),
            // Ours, current or not (an older release, another environment): removed.
            "connected" | "outdated" => run_required(host, binary, &strings(&REMOVE)).await,
            "foreign" => Err(AgentError::Key("agents.foreign")),
            "invalid" => Err(AgentError::Key("agents.invalid")),
            _ => Err(AgentError::Key("agents.registration-unknown")),
        }
    }
}

/// `mcp add` for this installation: its command, and the environment it runs with. `-e` takes every value up
/// to the next option, so the server's name comes before it.
fn add_args(host: &dyn AgentHost) -> Vec<String> {
    let mut args = strings(&["mcp", "add", "--scope", "user", "sidevoice"]);
    for (key, value) in host.mcp_env() {
        args.push("-e".into());
        args.push(format!("{key}={value}"));
    }
    args.push("--".into());
    args.push(host.command().to_owned());
    args.extend(host.args().iter().cloned());
    args
}

impl Registration for Claude {
    fn bench_steps(&self, setup: &BenchSetup) -> Option<String> {
        let start = setup.line(
            "claude",
            &strings(&["--mcp-config", setup.mcp_config, "--strict-mcp-config"]),
            None,
        );
        Some(format!(
            "Claude Code (log in once inside the profile with /login):\n  {start}"
        ))
    }

    fn after_install(&self, profile: &Profile) -> Vec<String> {
        let mut notes = vec!["In a conversation, ask to join voice. Sessions already open need a restart before they \
             see the server."
            .to_owned()];
        notes.extend(inbound_warning(profile));
        notes
    }

    fn config_root<'a>(&self, profile: &'a Profile) -> &'a Path {
        profile.agent_home(super::HOME.var)
    }

    fn binary_names(&self) -> &'static [&'static str] {
        &["claude"]
    }

    fn install_markers(&self) -> &'static [crate::adapters::markers::Marker] {
        use crate::adapters::markers::{Marker, Root};
        &[
            Marker {
                os: "macos",
                kind: "install_file",
                root: Root::Home,
                path: ".local/bin/claude",
            },
            Marker {
                os: "macos",
                kind: "install_file",
                root: Root::Absolute,
                path: "/opt/homebrew/bin/claude",
            },
            Marker {
                os: "macos",
                kind: "install_file",
                root: Root::Absolute,
                path: "/usr/local/bin/claude",
            },
            Marker {
                os: "linux",
                kind: "install_file",
                root: Root::Home,
                path: ".local/bin/claude",
            },
            Marker {
                os: "windows",
                kind: "install_file",
                root: Root::Home,
                path: ".local/bin/claude.exe",
            },
        ]
    }

    fn state<'a>(
        &'a self,
        host: &'a dyn AgentHost,
        binary: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Entry, AgentError>> {
        Box::pin(self.registration_state(host, binary))
    }

    fn connect<'a>(
        &'a self,
        host: &'a dyn AgentHost,
        binary: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), AgentError>> {
        Box::pin(self.connect_entry(host, binary))
    }

    fn disconnect<'a>(
        &'a self,
        host: &'a dyn AgentHost,
        binary: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), AgentError>> {
        Box::pin(self.disconnect_entry(host, binary))
    }

    fn manual(&self, host: &dyn AgentHost, registration: &str, binary: Option<&str>) -> Manual {
        let cli = binary.unwrap_or("claude");
        let profile = host.profile();
        // `claude mcp` writes Claude Code's user configuration: the file each step changes. A step names
        // `CLAUDE_CONFIG_DIR` only when the person's environment sets it, as the connector runs the CLI.
        let file = super::user_config(profile);
        let step = |args: &[String]| cli_step(profile, super::HOME.var, cli, args, &file);
        // Ours is always in the user scope. Another entry may be in any scope, a project's or a local one relative to
        // the directory the person runs it in: without `--scope`, Claude Code removes it from the scope it is in.
        Manual::by_cli(
            registration,
            step(&add_args(host)),
            step(&strings(&REMOVE)),
            step(&strings(&["mcp", "remove", "sidevoice"])),
        )
    }
}

/// The strings of a JSON array; none at all is no arguments, anything but strings is not an entry.
fn strings_of(value: Option<&Value>) -> Option<Vec<String>> {
    match value {
        None => Some(Vec::new()),
        Some(value) => value
            .as_array()?
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect(),
    }
}

/// Claude Code holds what other local processes send to a session that bypasses permission prompts.
fn inbound_warning(profile: &Profile) -> Option<String> {
    let settings = profile.agent_home(super::HOME.var).join("settings.json");
    let parsed: Value = serde_json::from_slice(&fs::read(&settings).ok()?).ok()?;
    if parsed
        .get("crossSessionInbound")
        .is_some_and(|value| !value.is_null())
        || parsed
            .pointer("/permissions/defaultMode")
            .and_then(Value::as_str)
            != Some("bypassPermissions")
    {
        return None;
    }
    Some(format!(
        "This machine runs Claude Code sessions in bypassPermissions, and those hold what Sidevoice sends instead of \
         delivering it: voice looks sent and never arrives. Either start a session with\n  --settings \
         '{{\"crossSessionInbound\":\"accept\"}}'\nor add \"crossSessionInbound\": \"accept\" to {}. That second one \
         lets any local process post into every Claude session on this machine, which is the safeguard it removes: \
         your call, not ours.",
        settings.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::registration::{CliError, CliOutput};
    use crate::platform::fs::TestFiles;
    use crate::platform::fs::{Files, Fs};
    use serde_json::json;

    /// A host with no CLI to run: a state judged here never needs one.
    struct Host(Profile);

    impl AgentHost for Host {
        fn profile(&self) -> &Profile {
            &self.0
        }
        fn command(&self) -> &str {
            "/nowhere/sidevoice"
        }
        fn args(&self) -> &[String] {
            &[]
        }
        fn owns(&self, command: &str, args: &[String]) -> bool {
            command == self.command() && args.is_empty()
        }
        fn mcp_env(&self) -> BTreeMap<String, String> {
            BTreeMap::new()
        }
        fn run<'a>(
            &'a self,
            _binary: &'a str,
            _args: Vec<String>,
        ) -> BoxFuture<'a, Result<CliOutput, CliError>> {
            panic!("the CLI was asked")
        }
        fn live(&self) -> Result<(), AgentError> {
            Ok(())
        }
    }

    /// A profile whose Claude home is `home/.claude` (left to the default) or its own directory (`set`), created
    /// only when `exists`; and the user configuration file Claude Code reads there.
    fn profile(scratch: &tempfile::TempDir, set: bool, exists: bool) -> (Profile, PathBuf) {
        let mut profile = Profile::for_test(
            scratch.path(),
            &crate::adapters::Registry::standard().homes(),
        );
        let home = profile
            .agent_homes
            .iter_mut()
            .find(|home| home.spec.var == super::super::HOME.var)
            .unwrap();
        let _ = fs::remove_dir(&home.path);
        if !set {
            home.path = profile.home.join(".claude");
        }
        home.set = set;
        if exists {
            Files::ensure_private_dir(&home.path).unwrap();
        } else if set {
            // Its parent stays, so the file can be there without the home.
            fs::create_dir_all(home.path.parent().unwrap()).unwrap();
        }
        let file = super::super::user_config(&profile);
        (profile, file)
    }

    /// Settings Claude Code cannot parse are `invalid` before anything else is judged: whether or not the person's
    /// environment names its home, whether or not its data directory is there, and with no CLI found.
    #[tokio::test]
    async fn unparsable_settings_are_invalid_before_the_home_and_the_cli() {
        for set in [false, true] {
            for exists in [false, true] {
                if set && !exists {
                    continue; // The file lives in the home then: no home, no file.
                }
                let scratch = crate::test_support::temp_dir("claude-unparsable");
                let (profile, file) = profile(&scratch, set, exists);
                write(&file, b"{\"mcpServers\": {");
                assert_eq!(
                    state(&Host(profile), None).await,
                    "invalid",
                    "set {set}, home there {exists}"
                );
                assert_eq!(fs::read(&file).unwrap(), b"{\"mcpServers\": {");
            }
        }
    }

    /// A FIFO where the user configuration belongs is refused at once, never waited on. Unix only: a FIFO is a file
    /// there; a Windows pipe has no place among files.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_fifo_for_the_user_configuration_is_refused_without_waiting() {
        let scratch = crate::test_support::temp_dir("claude-fifo");
        let (profile, file) = profile(&scratch, true, true);
        let path = std::ffi::CString::new(file.to_string_lossy().into_owned()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let (sent, received) = std::sync::mpsc::channel();
        let reader = profile.clone();
        std::thread::spawn(move || {
            let _ = sent.send(super::super::read_user_config(&reader).is_err());
        });
        assert_eq!(
            received.recv_timeout(std::time::Duration::from_secs(10)),
            Ok(true),
            "the read waited on the FIFO, or did not refuse it"
        );
        assert_eq!(state(&Host(profile), None).await, "invalid");
    }

    /// Links `file` to `name` in a dotfiles directory (outside Claude's and the home's), and returns where it leads.
    fn linked(scratch: &tempfile::TempDir, file: &Path, name: &str) -> PathBuf {
        let dotfiles = scratch.path().join("dotfiles");
        Files::ensure_private_dir(&dotfiles).unwrap();
        let target = dotfiles.join(name);
        Files::symlink(&target, file);
        target
    }

    /// `bytes` as the whole of a file of the person's, open to nobody else.
    fn write(path: &Path, bytes: &[u8]) {
        Files::write_private(path, bytes).unwrap();
    }

    /// Dotfiles managers link `~/.claude.json` from their own directory: the link is followed, and what it leads to
    /// is read, or judged unparsable, as the file itself would be.
    #[tokio::test]
    async fn a_linked_user_configuration_is_read_where_the_link_leads() {
        let scratch = crate::test_support::temp_dir("claude-linked");
        let (profile, file) = profile(&scratch, false, true);
        let target = linked(&scratch, &file, "claude.json");
        write(&target, b"{\"mcpServers\": {}}");
        assert_eq!(
            super::super::read_user_config(&profile),
            Ok(Some(json!({"mcpServers": {}})))
        );
        write(&target, b"{\"mcpServers\": {");
        assert_eq!(state(&Host(profile), None).await, "invalid");
        assert_eq!(fs::read(&target).unwrap(), b"{\"mcpServers\": {");
    }

    /// A link to a FIFO is refused at once, never waited on. Unix only, as above.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_link_to_a_fifo_is_refused_without_waiting() {
        let scratch = crate::test_support::temp_dir("claude-linked-fifo");
        let (profile, file) = profile(&scratch, false, true);
        let target = linked(&scratch, &file, "fifo");
        let path = std::ffi::CString::new(target.to_string_lossy().into_owned()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let (sent, received) = std::sync::mpsc::channel();
        let reader = profile.clone();
        std::thread::spawn(move || {
            let _ = sent.send(super::super::read_user_config(&reader).is_err());
        });
        assert_eq!(
            received.recv_timeout(std::time::Duration::from_secs(10)),
            Ok(true),
            "the read waited on the FIFO, or did not refuse it"
        );
        assert_eq!(state(&Host(profile), None).await, "invalid");
    }

    /// What a link leads to is judged as the file itself: one others may write is refused (one another user owns is
    /// refused by the same check of the opened file, which a test cannot make without root). Unix only: the file is
    /// opened to others with mode bits, which the interface does not offer (Windows access is a DACL).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_link_to_a_file_others_may_write_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = crate::test_support::temp_dir("claude-linked-mode");
        let (profile, file) = profile(&scratch, false, true);
        let target = linked(&scratch, &file, "claude.json");
        write(&target, b"{}");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o622)).unwrap();
        assert_eq!(
            super::super::read_user_config(&profile),
            Err(super::super::Unreadable)
        );
        assert_eq!(state(&Host(profile), None).await, "invalid");
    }

    /// A link that leads nowhere is unreadable, so the CLI is not run over it.
    #[tokio::test]
    async fn a_link_to_nothing_is_refused() {
        let scratch = crate::test_support::temp_dir("claude-linked-nothing");
        let (profile, file) = profile(&scratch, false, true);
        linked(&scratch, &file, "claude.json");
        assert_eq!(
            super::super::read_user_config(&profile),
            Err(super::super::Unreadable)
        );
        assert_eq!(state(&Host(profile), None).await, "invalid");
    }

    /// Through a dotfiles link, the entry is read as from the file: this installation's is `connected`, and one an
    /// older Sidevoice wrote (another environment) is `owned-old`.
    #[tokio::test]
    async fn entries_are_read_through_a_link() {
        let scratch = crate::test_support::temp_dir("claude-linked-entry");
        let (profile, file) = profile(&scratch, false, true);
        let target = linked(&scratch, &file, "claude.json");
        let host = Host(profile);
        for (env, expected) in [(json!({}), "connected"), (json!({"OLD": "1"}), "owned-old")] {
            let config = json!({"mcpServers": {"sidevoice": {"type": "stdio",
                "command": "/nowhere/sidevoice", "args": [], "env": env}}});
            write(&target, config.to_string().as_bytes());
            assert_eq!(state(&host, Some("/nowhere/claude")).await, expected);
        }
    }

    async fn state(host: &Host, binary: Option<&str>) -> String {
        Claude.entry_state(host, binary).await.unwrap().registration
    }
}
