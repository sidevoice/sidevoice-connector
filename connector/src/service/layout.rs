//! Where an installation and its data are, resolved once from the environment (or from a running connector's
//! profile), so every service command, the launcher and `node.status` name the same files.
//!
//! | What | Where |
//! |---|---|
//! | `D`, the data directory | `$SIDEVOICE_DATA_DIR`, else `~/.sidevoice` |
//! | `D/install.json` | the installation record: `command` (the absolute argv prefix that runs this binary), `releases` |
//! | `R`, the releases | `install.json`'s `releases`, else `$XDG_DATA_HOME/sidevoice` |
//! | `R/current` | a link to the selected release; its core at `core/bin/sidevoice-core-rust`, with `core/models` |
//! | `D/core` | the core's data: `local.sock`, `core.json` (ready), `core-failure.json` |
//! | `D/connector.sock`, `D/connector.lock` | the connector daemon's socket and lock |
//! | `D/install.lock`, `D/agents.lock` | the install/service lock and the agents lock |
//! | `D/node-stopped.json` | a person's stop |
//!
//! The release layout below `R/current` is the installer's; it is named here once (`CORE_ENTRYPOINT`).

use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

/// The core's program inside a release, and the directory its models are in (`core/models`).
pub const CORE_ENTRYPOINT: &str = "core/bin/sidevoice-core-rust";
pub const CORE_DIRECTORY: &str = "core";

/// The core's port when `SIDEVOICE_CORE_PORT` does not name one.
pub const DEFAULT_CORE_PORT: &str = "8768";

/// Settings that are never written into a service definition: credentials of a core somebody else runs, test hooks,
/// what only says where to install from, and what a definition sets itself.
const NOT_SETTINGS: &[&str] = &[
    "SIDEVOICE_URL",
    "SIDEVOICE_CONNECTOR_ID",
    "SIDEVOICE_CONNECTOR_TOKEN",
    "SIDEVOICE_SERVICE",
    "SIDEVOICE_SERVICE_MANAGER",
    "SIDEVOICE_TEST_HOOKS",
    "SIDEVOICE_CORE_BIN",
    "SIDEVOICE_CORE_SPEC",
    "SIDEVOICE_CORE_WHEEL_DIR",
    "SIDEVOICE_UV",
    "SIDEVOICE_INSTALL_FROM_SOURCE",
    "SIDEVOICE_INSTALLED_BY",
    "SIDEVOICE_DATA_DIR",
];

/// The agents' configuration directories a definition passes on when the installing environment names them.
const AGENT_DIRECTORIES: &[&str] = &[
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "CURSOR_CONFIG_DIR",
    "CURSOR_DATA_DIR",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub home: PathBuf,
    /// `D`.
    pub data: PathBuf,
    /// `R`.
    pub releases: PathBuf,
    pub config_home: PathBuf,
    pub data_home: PathBuf,
    /// `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `CURSOR_CONFIG_DIR`, `CURSOR_DATA_DIR`: only those that are set.
    pub agent_dirs: BTreeMap<String, PathBuf>,
    /// The `SIDEVOICE_*` settings this installation is made with.
    pub settings: BTreeMap<String, String>,
}

fn absolute(
    name: &str,
    value: Option<OsString>,
    fallback: impl FnOnce() -> PathBuf,
) -> anyhow::Result<PathBuf> {
    let path = value
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(fallback);
    if !path.is_absolute() {
        anyhow::bail!("{name} must be an absolute path");
    }
    Ok(path)
}

impl Layout {
    /// From this process's environment.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_vars(|name| std::env::var_os(name), std::env::vars_os())
    }

    /// From named variables (`var`) and all of them (`vars`, for the `SIDEVOICE_*` settings).
    pub fn from_vars(
        var: impl Fn(&str) -> Option<OsString>,
        vars: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> anyhow::Result<Self> {
        let home = absolute("HOME", var("HOME"), PathBuf::new)?;
        let data = absolute("SIDEVOICE_DATA_DIR", var("SIDEVOICE_DATA_DIR"), || {
            home.join(".sidevoice")
        })?;
        let data_home = absolute("XDG_DATA_HOME", var("XDG_DATA_HOME"), || {
            home.join(".local/share")
        })?;
        let config_home = absolute("XDG_CONFIG_HOME", var("XDG_CONFIG_HOME"), || {
            home.join(".config")
        })?;
        let mut agent_dirs = BTreeMap::new();
        for name in AGENT_DIRECTORIES {
            if let Some(value) = var(name).filter(|value| !value.is_empty()) {
                agent_dirs.insert(
                    (*name).to_owned(),
                    absolute(name, Some(value), PathBuf::new)?,
                );
            }
        }
        let settings = vars
            .into_iter()
            .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
            .filter(|(name, _)| {
                name.starts_with("SIDEVOICE_") && !NOT_SETTINGS.contains(&name.as_str())
            })
            .collect();
        let mut layout = Self {
            releases: data_home.join("sidevoice"),
            home,
            data,
            config_home,
            data_home,
            agent_dirs,
            settings,
        };
        if let Some(recorded) = layout
            .install_record()
            .and_then(|record| {
                record
                    .get("releases")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
            })
            .filter(|path| {
                path.is_absolute() && path.file_name().is_some_and(|name| name == "sidevoice")
            })
        {
            layout.releases = recorded;
        }
        Ok(layout)
    }

    /// The paths a running connector's profile names, with this process's `SIDEVOICE_*` settings.
    pub fn from_profile(profile: &crate::profile::Profile) -> Self {
        let mut vars: BTreeMap<String, OsString> = BTreeMap::new();
        for (name, path) in [
            ("HOME", &profile.home),
            ("SIDEVOICE_DATA_DIR", &profile.data),
            ("XDG_DATA_HOME", &profile.xdg_data),
            ("XDG_CONFIG_HOME", &profile.xdg_config),
            ("CLAUDE_CONFIG_DIR", &profile.claude),
            ("CODEX_HOME", &profile.codex),
            ("CURSOR_CONFIG_DIR", &profile.cursor),
            ("CURSOR_DATA_DIR", &profile.cursor_data),
        ] {
            vars.insert(name.into(), path.clone().into_os_string());
        }
        Self::from_vars(|name| vars.get(name).cloned(), std::env::vars_os())
            .expect("a validated profile names absolute paths")
    }

    pub fn current(&self) -> PathBuf {
        self.releases.join("current")
    }

    pub fn core_program(&self) -> PathBuf {
        self.current().join(CORE_ENTRYPOINT)
    }

    pub fn core_data(&self) -> PathBuf {
        self.data.join("core")
    }

    pub fn core_socket(&self) -> PathBuf {
        self.core_data().join("local.sock")
    }

    pub fn core_ready(&self) -> PathBuf {
        self.core_data().join("core.json")
    }

    pub fn core_failure(&self) -> PathBuf {
        self.core_data().join("core-failure.json")
    }

    /// The core's own log (it writes it), and where a launcher appends the core's stdout and stderr.
    pub fn core_log(&self) -> PathBuf {
        self.data.join("core.log")
    }

    pub fn core_output(&self) -> PathBuf {
        self.data.join("core.stderr.log")
    }

    pub fn connector_log(&self) -> PathBuf {
        self.data.join("connector.log")
    }

    pub fn connector_socket(&self) -> PathBuf {
        self.data.join("connector.sock")
    }

    pub fn connector_lock(&self) -> PathBuf {
        self.data.join("connector.lock")
    }

    pub fn install_lock(&self) -> PathBuf {
        self.data.join("install.lock")
    }

    pub fn stop_marker(&self) -> PathBuf {
        self.data.join("node-stopped.json")
    }

    pub fn room_credential(&self) -> PathBuf {
        self.data.join("credentials.json")
    }

    pub fn install_file(&self) -> PathBuf {
        self.data.join("install.json")
    }

    /// The core's port: `SIDEVOICE_CORE_PORT`, else the default.
    pub fn core_port(&self) -> String {
        self.settings
            .get("SIDEVOICE_CORE_PORT")
            .filter(|port| port.parse::<u16>().is_ok())
            .cloned()
            .unwrap_or_else(|| DEFAULT_CORE_PORT.to_owned())
    }

    /// Whether a release is selected: `R/current` is a link to a directory.
    pub fn installed(&self) -> bool {
        fs::symlink_metadata(self.current()).is_ok_and(|metadata| metadata.file_type().is_symlink())
            && self.current().is_dir()
    }

    /// `D/install.json`, read only when it is this user's regular file and nobody else can change it.
    pub fn install_record(&self) -> Option<Value> {
        let bytes = crate::secure_fs::read_trusted(&self.install_file(), 64 * 1024).ok()??;
        serde_json::from_slice(&bytes).ok()
    }

    /// The argv prefix that runs this installation's connector (`install.json`'s `command`): absolute paths for
    /// the program, plain words after it.
    pub fn connector_command(&self) -> Option<Vec<String>> {
        let record = self.install_record()?;
        let command: Vec<String> = record
            .get("command")?
            .as_array()?
            .iter()
            .map(|word| word.as_str().map(str::to_owned))
            .collect::<Option<_>>()?;
        let program = command.first()?;
        Path::new(program).is_absolute().then_some(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(vars: &[(&str, &str)]) -> anyhow::Result<Layout> {
        let map: BTreeMap<String, OsString> = vars
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
            .collect();
        Layout::from_vars(
            |name| map.get(name).cloned(),
            map.iter()
                .map(|(name, value)| (OsString::from(name), value.clone())),
        )
    }

    #[test]
    fn paths_follow_the_environment_with_the_documented_defaults() {
        let plain = layout(&[("HOME", "/home/u")]).unwrap();
        assert_eq!(plain.data, Path::new("/home/u/.sidevoice"));
        assert_eq!(plain.releases, Path::new("/home/u/.local/share/sidevoice"));
        assert_eq!(plain.config_home, Path::new("/home/u/.config"));
        assert_eq!(
            plain.core_program(),
            Path::new("/home/u/.local/share/sidevoice/current/core/bin/sidevoice-core-rust")
        );
        assert!(plain.agent_dirs.is_empty());
        assert_eq!(plain.core_port(), "8768");

        let named = layout(&[
            ("HOME", "/home/u"),
            ("SIDEVOICE_DATA_DIR", "/d"),
            ("XDG_DATA_HOME", "/x"),
            ("CODEX_HOME", "/c"),
            ("SIDEVOICE_CORE_PORT", "0"),
            ("SIDEVOICE_CONNECTOR_TOKEN", "secret"),
            ("SIDEVOICE_SERVICE", "launchd"),
        ])
        .unwrap();
        assert_eq!(named.core_socket(), Path::new("/d/core/local.sock"));
        assert_eq!(named.releases, Path::new("/x/sidevoice"));
        assert_eq!(named.agent_dirs["CODEX_HOME"], Path::new("/c"));
        assert_eq!(named.core_port(), "0");
        assert_eq!(
            named.settings.keys().collect::<Vec<_>>(),
            ["SIDEVOICE_CORE_PORT"],
            "credentials and what a definition sets itself are never settings"
        );
    }

    #[test]
    fn relative_paths_are_refused() {
        assert!(layout(&[("HOME", "relative")]).is_err());
        assert!(layout(&[("HOME", "/h"), ("SIDEVOICE_DATA_DIR", "d")]).is_err());
        assert!(layout(&[]).is_err(), "no HOME, no layout");
    }
}
