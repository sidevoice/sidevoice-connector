//! Where everything is: this user's directories, read from the environment, and the connector's data directory
//! layout.
//!
//! - `D`, the data directory: `$SIDEVOICE_DATA_DIR`, else `~/.sidevoice`. Private (0700). The connector is the
//!   one writer of its records there, each written whole (`secure_fs.rs`); `D/core/` is the core's.
//!   `connector.sock`, `connector.lock`, `connector.log`, `agents.json`, `agents.lock`, `install.lock`,
//!   `node-stopped.json` (a person's stop), `credentials.json` (the room pairing), `outbox.json`, `core/core.json`
//!   and `core/local.sock`.
//! - `R`, the release root: `$XDG_DATA_HOME/sidevoice` (else `~/.local/share/sidevoice`), as `service/layout.rs` names it.
//! - The agents' homes: `$CLAUDE_CONFIG_DIR` (`~/.claude`), `$CODEX_HOME` (`~/.codex`), `$CURSOR_CONFIG_DIR`
//!   (`~/.cursor`) and `$CURSOR_DATA_DIR` (`<cursor>/data`).
//!
//! Every path is absolute. The directories are checked again at each operation boundary, not trusted from startup:
//! they are passed to agents' command lines and written into.

use crate::secure_fs::{private_dir, private_file, validate_user_directory};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Profile {
    pub home: PathBuf,
    pub claude: PathBuf,
    /// `D`, the data directory.
    pub data: PathBuf,
    pub codex: PathBuf,
    pub cursor: PathBuf,
    pub cursor_data: PathBuf,
    pub xdg_config: PathBuf,
    pub xdg_data: PathBuf,
    pub socket: PathBuf,
    pub core_socket: PathBuf,
    pub core_ready: PathBuf,
}

impl Profile {
    /// This user's profile from the environment.
    pub fn from_env() -> Result<Self> {
        Self::from_vars(|key| std::env::var_os(key))
    }

    fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> Result<Self> {
        let home = PathBuf::from(var("HOME").context("HOME is not set")?);
        if !home.is_absolute() {
            bail!("HOME must be absolute");
        }
        let or = |key: &str, fallback: PathBuf| var(key).map(PathBuf::from).unwrap_or(fallback);
        let data = or("SIDEVOICE_DATA_DIR", home.join(".sidevoice"));
        let claude = or("CLAUDE_CONFIG_DIR", home.join(".claude"));
        let codex = or("CODEX_HOME", home.join(".codex"));
        let cursor = or("CURSOR_CONFIG_DIR", home.join(".cursor"));
        let cursor_data = or("CURSOR_DATA_DIR", cursor.join("data"));
        let xdg_config = or("XDG_CONFIG_HOME", home.join(".config"));
        let xdg_data = or("XDG_DATA_HOME", home.join(".local/share"));
        for (name, path) in [
            ("SIDEVOICE_DATA_DIR", &data),
            ("CLAUDE_CONFIG_DIR", &claude),
            ("CODEX_HOME", &codex),
            ("CURSOR_CONFIG_DIR", &cursor),
            ("CURSOR_DATA_DIR", &cursor_data),
            ("XDG_CONFIG_HOME", &xdg_config),
            ("XDG_DATA_HOME", &xdg_data),
        ] {
            if !path.is_absolute() {
                bail!("{name} must be absolute");
            }
        }
        let core = data.join("core");
        let profile = Self {
            home,
            claude,
            socket: data.join("connector.sock"),
            core_socket: core.join("local.sock"),
            core_ready: core.join("core.json"),
            data,
            codex,
            cursor,
            cursor_data,
            xdg_config,
            xdg_data,
        };
        profile.validate_existing_private()?;
        Ok(profile)
    }

    /// The version this connector reports: the build's.
    pub fn connector_version(&self) -> &str {
        crate::identity::VERSION
    }

    /// What identifies the running build to a client that checks it (`identity`).
    pub fn runtime_identity(&self) -> Value {
        json!({"runtime_build_sha":env!("SIDEVOICE_CONNECTOR_BUILD_SHA"),
            "runtime_target":env!("SIDEVOICE_CONNECTOR_TARGET")})
    }

    /// Passes this profile on to a child, whatever the child's own environment was cleared to.
    pub fn command_env<'a>(
        &self,
        command: &'a mut tokio::process::Command,
    ) -> &'a mut tokio::process::Command {
        for (key, value) in self.variables() {
            command.env(key, value);
        }
        command
    }

    fn variables(&self) -> [(&'static str, &Path); 8] {
        [
            ("HOME", &self.home),
            ("CLAUDE_CONFIG_DIR", &self.claude),
            ("CODEX_HOME", &self.codex),
            ("CURSOR_CONFIG_DIR", &self.cursor),
            ("CURSOR_DATA_DIR", &self.cursor_data),
            ("XDG_CONFIG_HOME", &self.xdg_config),
            ("XDG_DATA_HOME", &self.xdg_data),
            ("SIDEVOICE_DATA_DIR", &self.data),
        ]
    }

    /// A process its service manager started must have been given this very profile: the paths a service job
    /// names, never ones inherited by accident. The agents' homes may be left to derive from `HOME`.
    pub fn validate_service_environment(&self) -> Result<()> {
        let service = std::env::var("SIDEVOICE_SERVICE")
            .context("the service manager is not identified (SIDEVOICE_SERVICE)")?;
        if !matches!(service.as_str(), "launchd" | "systemd") {
            bail!("unsupported service manager {service:?}");
        }
        for (key, expected) in self.variables() {
            let required = matches!(
                key,
                "HOME" | "XDG_CONFIG_HOME" | "XDG_DATA_HOME" | "SIDEVOICE_DATA_DIR"
            );
            let supplied = match std::env::var_os(key) {
                Some(supplied) => supplied,
                None if required => bail!("the service environment does not name {key}"),
                None => continue,
            };
            if Path::new(&supplied) != expected {
                bail!("the service environment's {key} differs from its profile");
            }
        }
        Ok(())
    }

    /// Whether a person stopped Sidevoice (`node-stopped.json`), which no automatic start overrides.
    pub fn service_stopped(&self) -> Result<bool> {
        let marker = self.data.join("node-stopped.json");
        match fs::symlink_metadata(&marker) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
            Ok(_) => {
                private_file(&marker)?;
                Ok(true)
            }
        }
    }

    /// Refuses directories that were replaced or opened up after this process started: the data directory and the
    /// core's are private when they exist (they are created private by whoever needs them first), and every
    /// directory of the person's we read or write is theirs and writable by nobody else.
    pub fn validate_existing_private(&self) -> Result<()> {
        for dir in [&self.data, &self.data.join("core")] {
            match fs::symlink_metadata(dir) {
                Ok(_) => private_dir(dir)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        for path in [
            &self.home,
            &self.claude,
            &self.codex,
            self.cursor.parent().context("Cursor config parent")?,
            &self.cursor_data,
            self.xdg_config.parent().context("XDG config parent")?,
            &self.xdg_config,
            &self.xdg_data,
        ] {
            match fs::symlink_metadata(path) {
                Ok(_) => {
                    validate_user_directory(path)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    /// Before an agent's configuration is read or changed: it is one of the three this profile names, and a
    /// directory of the person's.
    pub fn validate_for_agent(&self, config_root: &Path) -> Result<()> {
        self.validate_existing_private()?;
        if config_root != self.claude.as_path()
            && config_root != self.codex.as_path()
            && config_root != self.cursor.as_path()
        {
            bail!("unknown agent configuration directory");
        }
        validate_user_directory(config_root)?;
        Ok(())
    }

    /// The directory an agent's configuration files must stay inside: its own, canonical.
    pub fn agent_root(&self, config_root: &Path) -> Result<PathBuf> {
        self.validate_for_agent(config_root)?;
        config_root.canonicalize().map_err(Into::into)
    }

    /// The connector's own lock, `D/connector.lock`: one connector serves the socket.
    pub fn try_connector_lock(&self) -> Result<crate::lock::Lock> {
        let path = self.data.join("connector.lock");
        crate::lock::try_lock(&path, "connector")?.with_context(|| {
            let pid = crate::lock::holder(&path)
                .and_then(|record| record["pid"].as_u64())
                .map_or_else(String::new, |pid| format!(" (pid {pid})"));
            format!("another Sidevoice connector{pid} already serves this data directory")
        })
    }

    /// A profile entirely inside `root`, with every directory made private: what unit tests run in.
    #[cfg(test)]
    pub fn for_test(root: &Path) -> Self {
        use std::os::unix::fs::DirBuilderExt;
        for child in [
            "home",
            "claude",
            "codex",
            "cursor/config",
            "cursor/data",
            "xdg/config",
            "xdg/data",
            "sidevoice/core",
        ] {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(root.join(child))
                .unwrap();
        }
        let root = root.canonicalize().unwrap();
        let path = |child: &str| -> OsString { root.join(child).into_os_string() };
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
        Self::from_vars(|key| {
            variables
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.clone())
        })
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure_fs::tests::Scratch;
    use std::os::unix::fs::PermissionsExt;

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let pairs: Vec<(String, OsString)> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), OsString::from(value)))
            .collect();
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn the_layout_derives_from_home_unless_each_place_is_named() {
        let scratch = Scratch::new("layout");
        let home = scratch.0.to_string_lossy().into_owned();
        let profile = Profile::from_vars(vars(&[("HOME", &home)])).unwrap();
        assert_eq!(profile.data, scratch.0.join(".sidevoice"));
        assert_eq!(profile.socket, scratch.0.join(".sidevoice/connector.sock"));
        assert_eq!(
            profile.core_socket,
            scratch.0.join(".sidevoice/core/local.sock")
        );
        assert_eq!(
            profile.core_ready,
            scratch.0.join(".sidevoice/core/core.json")
        );
        assert_eq!(profile.claude, scratch.0.join(".claude"));
        assert_eq!(profile.codex, scratch.0.join(".codex"));
        assert_eq!(profile.cursor, scratch.0.join(".cursor"));
        assert_eq!(profile.cursor_data, scratch.0.join(".cursor/data"));
        assert_eq!(profile.connector_version(), crate::identity::VERSION);

        let data = scratch.0.join("elsewhere");
        let named = Profile::from_vars(vars(&[
            ("HOME", &home),
            ("SIDEVOICE_DATA_DIR", data.to_str().unwrap()),
            ("XDG_DATA_HOME", "/xdg-data"),
        ]))
        .unwrap();
        assert_eq!(named.data, data);
    }

    #[test]
    fn relative_or_missing_places_are_refused() {
        assert!(Profile::from_vars(vars(&[])).is_err());
        assert!(Profile::from_vars(vars(&[("HOME", "relative")])).is_err());
        assert!(
            Profile::from_vars(vars(&[("HOME", "/tmp"), ("SIDEVOICE_DATA_DIR", "data")])).is_err()
        );
    }

    #[test]
    fn a_data_directory_others_can_enter_or_a_replaced_agent_home_is_refused() {
        let scratch = Scratch::new("trust");
        let profile = Profile::for_test(&scratch.0);
        profile.validate_existing_private().unwrap();
        fs::set_permissions(&profile.data, fs::Permissions::from_mode(0o750)).unwrap();
        assert!(profile.validate_existing_private().is_err());
        fs::set_permissions(&profile.data, fs::Permissions::from_mode(0o700)).unwrap();

        fs::set_permissions(&profile.codex, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(profile.validate_for_agent(&profile.codex).is_err());
        fs::set_permissions(&profile.codex, fs::Permissions::from_mode(0o700)).unwrap();
        profile.validate_for_agent(&profile.codex).unwrap();
        assert!(profile.validate_for_agent(&profile.home).is_err());

        fs::remove_dir(&profile.claude).unwrap();
        std::os::unix::fs::symlink(&profile.codex, &profile.claude).unwrap();
        assert!(profile.validate_existing_private().is_err());
    }

    #[test]
    fn missing_directories_are_not_unsafe_and_one_connector_holds_the_data_directory() {
        let scratch = Scratch::new("fresh");
        let home = scratch.0.to_string_lossy().into_owned();
        let profile = Profile::from_vars(vars(&[("HOME", &home)])).unwrap();
        assert!(!profile.data.exists());
        crate::secure_fs::ensure_private_dir(&profile.data).unwrap();
        let held = profile.try_connector_lock().unwrap();
        assert!(profile.try_connector_lock().is_err());
        drop(held);
        profile.try_connector_lock().unwrap();
    }

    #[test]
    fn a_person_stop_marker_is_honoured_and_must_be_private() {
        let scratch = Scratch::new("stop");
        let profile = Profile::for_test(&scratch.0);
        assert!(!profile.service_stopped().unwrap());
        let marker = profile.data.join("node-stopped.json");
        crate::secure_fs::atomic_json(&marker, &json!({"at":"now"})).unwrap();
        assert!(profile.service_stopped().unwrap());
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(profile.service_stopped().is_err());
    }
}
