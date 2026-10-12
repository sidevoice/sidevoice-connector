//! Sidevoice's entry in Claude Code (`claude mcp`, user scope).

use super::Claude;
use crate::adapters::registration::{
    looks_absent, run_required, shell_command, strings, AgentError, AgentHost, BenchSetup,
    CliError, Registration,
};
use crate::profile::Profile;
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

const REMOVE: [&str; 5] = ["mcp", "remove", "--scope", "user", "sidevoice"];

impl Claude {
    async fn entry_state(
        &self,
        host: &dyn AgentHost,
        binary: Option<&str>,
    ) -> Result<String, AgentError> {
        // Settings Claude Code cannot parse are left alone, whatever else is there: its CLI is not even asked.
        if super::read_user_config(host.profile()).is_err() {
            return Ok("invalid".into());
        }
        match fs::symlink_metadata(host.profile().agent_home(super::HOME.var)) {
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok("absent".into()),
            Err(_) => return Ok("invalid".into()),
            Ok(_) => {}
        }
        let Some(binary) = binary else {
            return Ok("unknown".into());
        };
        let output = match host
            .run(binary, strings(&["mcp", "get", "sidevoice"]))
            .await
        {
            Ok(output) => output,
            Err(CliError::Cancelled) => return Err(AgentError::Cancelled),
            Err(_) => return Ok("unknown".into()),
        };
        if !output.status.success() {
            let detail = format!("{}\n{}", output.stdout, output.stderr);
            if (output.status.code() == Some(1) && detail.trim().is_empty())
                || looks_absent(&detail)
            {
                return Ok("absent".into());
            }
            return Ok("unknown".into());
        }
        let detail = format!("{}\n{}", output.stdout, output.stderr);
        let field = |name: &str| -> Option<String> {
            detail.lines().find_map(|line| {
                let (key, value) = line.trim().split_once(':')?;
                key.eq_ignore_ascii_case(name)
                    .then(|| value.trim().to_owned())
            })
        };
        let Some(command) = field("Command").filter(|value| !value.is_empty()) else {
            return Ok(if looks_absent(&detail) {
                "absent"
            } else {
                "unknown"
            }
            .into());
        };
        let args = field("Args")
            .map(|value| {
                value
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let user_scope =
            field("Scope").is_some_and(|value| value.to_ascii_lowercase().contains("user"));
        if !user_scope {
            return Ok("foreign".into());
        }
        // The block under `Environment:`, one `KEY=value` per line, indented below the field.
        let env = detail
            .lines()
            .skip_while(|line| !line.trim().eq_ignore_ascii_case("Environment:"))
            .skip(1)
            .take_while(|line| line.starts_with("    ") && line.contains('='))
            .filter_map(|line| line.trim().split_once('='))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect::<BTreeMap<_, _>>();
        if host.owns(&command, &args) {
            Ok(
                if command == host.command() && args == host.args() && env == host.mcp_env() {
                    "connected"
                } else {
                    "owned-old"
                }
                .into(),
            )
        } else {
            Ok("foreign".into())
        }
    }

    async fn registration_state(
        &self,
        host: &dyn AgentHost,
        binary: Option<&str>,
    ) -> Result<String, AgentError> {
        let state = self.entry_state(host, binary).await?;
        Ok(match state.as_str() {
            "absent" => "not-connected".into(),
            "owned-old" => "outdated".into(),
            _ => state,
        })
    }

    async fn connect_entry(
        &self,
        host: &dyn AgentHost,
        binary: Option<&str>,
    ) -> Result<(), AgentError> {
        let binary = binary.ok_or(AgentError::Key("agents.manual-required"))?;
        let state = self.entry_state(host, Some(binary)).await?;
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
            let again = self.entry_state(host, Some(binary)).await?;
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
        let state = self.registration_state(host, Some(binary)).await?;
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
        Some(format!(
            "Claude Code (log in once inside the profile with /login):\n  . {} && claude --mcp-config {} --strict-mcp-config",
            setup.env_script, setup.mcp_config
        ))
    }

    fn after_install(&self, profile: &Profile) -> Vec<String> {
        let mut notes = vec!["In a conversation, ask to join the voice room. Sessions already open need a restart before they \
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

    fn known_paths(&self, home: &Path) -> Vec<PathBuf> {
        vec![
            home.join(".claude/local/claude"),
            home.join(".local/bin/claude"),
            PathBuf::from("/usr/local/bin/claude"),
            PathBuf::from("/opt/homebrew/bin/claude"),
        ]
    }

    fn state<'a>(
        &'a self,
        host: &'a dyn AgentHost,
        binary: Option<&'a str>,
    ) -> BoxFuture<'a, Result<String, AgentError>> {
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

    fn instructions(
        &self,
        host: &dyn AgentHost,
        _registration: &str,
        binary: Option<&str>,
    ) -> Value {
        let command = shell_command(binary.unwrap_or("claude"), &add_args(host));
        json!({
            "command":command.clone(),
            "file":super::user_config(host.profile()).to_string_lossy(),
            "snippet":command,
        })
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
        "This machine runs Claude Code sessions in bypassPermissions, and those hold what the room sends instead of \
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
    use crate::secure_fs::tests::Scratch;
    use std::os::unix::fs::PermissionsExt;

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
    fn profile(scratch: &Scratch, set: bool, exists: bool) -> (Profile, PathBuf) {
        let mut profile =
            Profile::for_test(&scratch.0, &crate::adapters::Registry::standard().homes());
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
            fs::create_dir_all(&home.path).unwrap();
            fs::set_permissions(&home.path, fs::Permissions::from_mode(0o700)).unwrap();
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
                let scratch = Scratch::new("claude-unparsable");
                let (profile, file) = profile(&scratch, set, exists);
                fs::write(&file, b"{\"mcpServers\": {").unwrap();
                fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
                assert_eq!(
                    state(&Host(profile), None).await,
                    "invalid",
                    "set {set}, home there {exists}"
                );
                assert_eq!(fs::read(&file).unwrap(), b"{\"mcpServers\": {");
            }
        }
    }

    /// A FIFO where the user configuration belongs is refused at once, never waited on.
    #[tokio::test]
    async fn a_fifo_for_the_user_configuration_is_refused_without_waiting() {
        let scratch = Scratch::new("claude-fifo");
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
    fn linked(scratch: &Scratch, file: &Path, name: &str) -> PathBuf {
        let dotfiles = scratch.0.join("dotfiles");
        fs::create_dir_all(&dotfiles).unwrap();
        let target = dotfiles.join(name);
        std::os::unix::fs::symlink(&target, file).unwrap();
        target
    }

    fn write(path: &Path, bytes: &[u8], mode: u32) {
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Dotfiles managers link `~/.claude.json` from their own directory: the link is followed, and what it leads to
    /// is read, or judged unparsable, as the file itself would be.
    #[tokio::test]
    async fn a_linked_user_configuration_is_read_where_the_link_leads() {
        let scratch = Scratch::new("claude-linked");
        let (profile, file) = profile(&scratch, false, true);
        let target = linked(&scratch, &file, "claude.json");
        write(&target, b"{\"mcpServers\": {}}", 0o600);
        assert_eq!(
            super::super::read_user_config(&profile),
            Ok(Some(json!({"mcpServers": {}})))
        );
        write(&target, b"{\"mcpServers\": {", 0o600);
        assert_eq!(state(&Host(profile), None).await, "invalid");
        assert_eq!(fs::read(&target).unwrap(), b"{\"mcpServers\": {");
    }

    /// A link to a FIFO is refused at once, never waited on.
    #[tokio::test]
    async fn a_link_to_a_fifo_is_refused_without_waiting() {
        let scratch = Scratch::new("claude-linked-fifo");
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
    /// refused by the same check of the opened file, which a test cannot make without root), and a link that leads
    /// nowhere is unreadable, so the CLI is not run over it.
    #[tokio::test]
    async fn a_link_to_a_file_others_may_write_or_to_nothing_is_refused() {
        let scratch = Scratch::new("claude-linked-mode");
        let (profile, file) = profile(&scratch, false, true);
        let target = linked(&scratch, &file, "claude.json");
        write(&target, b"{}", 0o622);
        assert_eq!(
            super::super::read_user_config(&profile),
            Err(super::super::Unreadable)
        );
        assert_eq!(state(&Host(profile.clone()), None).await, "invalid");
        fs::remove_file(&target).unwrap();
        assert_eq!(
            super::super::read_user_config(&profile),
            Err(super::super::Unreadable)
        );
        assert_eq!(state(&Host(profile), None).await, "invalid");
    }

    async fn state(host: &Host, binary: Option<&str>) -> String {
        Claude.entry_state(host, binary).await.unwrap()
    }
}
