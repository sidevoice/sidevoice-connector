//! Where an installation and its data are, resolved once from the environment (or from a running connector's
//! profile), so every service command, the launcher and `node.status` name the same files.
//!
//! | What | Where |
//! |---|---|
//! | `D`, the data directory | `$SIDEVOICE_DATA_DIR`, else the OS's standard place (`data_dir`) |
//! | `D/install.json` | the installation record: `command` (the absolute argv prefix that runs this binary), `releases` |
//! | `R`, the releases | `D/install` |
//! | `R/current.json` | the pointer record `{"release": "<id>"}`: the selected release, `R/releases/<id>`, with its core at `core/bin/sidevoice-core-rust` and `core/models` |
//! | `D/core` | the core's data: `local.sock`, `core.json` (ready), `core-failure.json` |
//! | `D/connector.sock`, `D/connector.lock` | the connector daemon's socket and lock |
//! | `D/install.lock`, `D/agents.lock` | the install/service lock and the agents lock |
//! | `D/<lock>.holder` | who holds that lock (`lock.rs`) |
//! | `D/node-stopped.json` | a person's stop |
//!
//! The release layout below `R/releases/<id>` is the installer's; it is named here once (`CORE_ENTRYPOINT`).

use crate::data_dir::{data_dir, releases_dir};
use crate::platform::env::{Env, Environment};
use crate::platform::fs::{Files, Fs};
use crate::platform::ipc::{Address, Endpoint};
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// The core's program inside a release, and the directory its models are in (`core/models`).
pub const CORE_ENTRYPOINT: &str = "core/bin/sidevoice-core-rust";
pub const CORE_DIRECTORY: &str = "core";

/// The core's port when `SIDEVOICE_CORE_PORT` does not name one.
pub const DEFAULT_CORE_PORT: &str = "10829";

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub home: PathBuf,
    /// `D`.
    pub data: PathBuf,
    /// `R`.
    pub releases: PathBuf,
    pub config_home: PathBuf,
    /// The agents' homes the installing environment names (only those that are set), by variable.
    pub agent_dirs: BTreeMap<String, PathBuf>,
    /// Every agent home's variable the harnesses declare: a job is given those named, and none of the others.
    pub agent_vars: Vec<&'static str>,
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
    /// From this process's environment, with the agents' homes named by `agent_vars`.
    pub fn from_env(agent_vars: &[&'static str]) -> anyhow::Result<Self> {
        Self::from_vars(
            |name| std::env::var_os(name),
            std::env::vars_os(),
            agent_vars,
        )
    }

    /// From named variables (`var`) and all of them (`vars`, for the `SIDEVOICE_*` settings).
    pub fn from_vars(
        var: impl Fn(&str) -> Option<OsString>,
        vars: impl IntoIterator<Item = (OsString, OsString)>,
        agent_vars: &[&'static str],
    ) -> anyhow::Result<Self> {
        let home = absolute(Environment::HOME, var(Environment::HOME), PathBuf::new)?;
        let data = data_dir(&var)?;
        let config_home = absolute("XDG_CONFIG_HOME", var("XDG_CONFIG_HOME"), || {
            home.join(".config")
        })?;
        let mut agent_dirs = BTreeMap::new();
        for name in agent_vars {
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
        Ok(Self {
            releases: releases_dir(&data),
            home,
            data,
            config_home,
            agent_dirs,
            agent_vars: agent_vars.to_vec(),
            settings,
        })
    }

    /// The paths a running connector's profile names, with this process's `SIDEVOICE_*` settings.
    pub fn from_profile(profile: &crate::profile::Profile) -> Self {
        let vars: BTreeMap<String, OsString> = profile
            .variables()
            .into_iter()
            .map(|(name, path)| (name.to_owned(), path.as_os_str().to_owned()))
            .collect();
        let agent_vars: Vec<&'static str> = profile
            .agent_homes
            .iter()
            .map(|resolved| resolved.spec.var)
            .collect();
        Self::from_vars(
            |name| vars.get(name).cloned(),
            std::env::vars_os(),
            &agent_vars,
        )
        .expect("a validated profile names absolute paths")
    }

    /// The agents' directories the installing environment leaves unset: a job must not inherit them from its manager
    /// either, so the agents use their own defaults there too.
    pub fn agent_dirs_unset(&self) -> Vec<&'static str> {
        self.agent_vars
            .iter()
            .copied()
            .filter(|name| !self.agent_dirs.contains_key(*name))
            .collect()
    }

    /// Where the service manager keeps this installation's job definitions.
    pub fn places(&self) -> crate::platform::service::Places {
        crate::platform::service::Places {
            home: self.home.clone(),
            config_home: self.config_home.clone(),
            data: self.data.clone(),
        }
    }

    /// `R/current.json`, the pointer record naming the selected release.
    pub fn current_record(&self) -> PathBuf {
        self.releases.join("current.json")
    }

    /// `R/releases`, where each release is a directory named by its id.
    pub fn releases_dir(&self) -> PathBuf {
        self.releases.join("releases")
    }

    /// The selected release's id: what `R/current.json` names, when it is the name of a directory of `R/releases`.
    /// The record is read as a trusted file (this user's, nobody else able to change it); anything else is none.
    pub fn selected_id(&self) -> Option<String> {
        let bytes = Files::read_trusted(&self.current_record(), 4096).ok()??;
        let id = serde_json::from_slice::<Value>(&bytes)
            .ok()?
            .get("release")?
            .as_str()?
            .to_owned();
        let mut parts = Path::new(&id).components();
        let one_name = matches!(
            (parts.next(), parts.next()),
            (Some(Component::Normal(name)), None) if name == OsStr::new(&id)
        );
        (one_name
            && !id.starts_with('.')
            && fs::symlink_metadata(self.releases_dir().join(&id)).is_ok_and(|m| m.is_dir()))
        .then_some(id)
    }

    /// The selected release's directory, `R/releases/<id>`.
    pub fn selected(&self) -> Option<PathBuf> {
        Some(self.releases_dir().join(self.selected_id()?))
    }

    /// The core's program in the selected release.
    pub fn core_program(&self) -> Option<PathBuf> {
        Some(self.selected()?.join(CORE_ENTRYPOINT))
    }

    pub fn core_data(&self) -> PathBuf {
        self.data.join("core")
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

    pub fn connector_endpoint(&self) -> Endpoint {
        Endpoint::new(&self.data, "connector")
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

    /// Whether a release is selected: `R/current.json` names a release directory.
    pub fn installed(&self) -> bool {
        self.selected().is_some()
    }

    /// `D/install.json`, read only when it is this user's regular file and nobody else can change it.
    pub fn install_record(&self) -> Option<Value> {
        let bytes = Files::read_trusted(&self.install_file(), 64 * 1024).ok()??;
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
            &["ALPHA_HOME"],
        )
    }

    /// An absolute path on this OS (not necessarily there): `name` under the temporary directory.
    fn absolute(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    #[test]
    fn paths_follow_the_environment_with_the_documented_defaults() {
        let home = absolute("u");
        let home_var = home.to_str().unwrap();
        let standard = absolute("s");
        let plain = layout(&[
            (Environment::HOME, home_var),
            ("SIDEVOICE_DATA_DIR", standard.to_str().unwrap()),
        ])
        .unwrap();
        let releases = standard.join("install");
        assert_eq!(plain.data, standard);
        assert_eq!(plain.releases, releases);
        assert_eq!(plain.config_home, home.join(".config"));
        assert_eq!(plain.current_record(), releases.join("current.json"));
        assert_eq!(plain.core_program(), None, "nothing selected");
        assert!(plain.agent_dirs.is_empty());
        assert_eq!(plain.core_port(), "10829");

        let (data, alpha) = (absolute("d"), absolute("c"));
        let named = layout(&[
            (Environment::HOME, home_var),
            ("SIDEVOICE_DATA_DIR", data.to_str().unwrap()),
            ("ALPHA_HOME", alpha.to_str().unwrap()),
            ("SIDEVOICE_CORE_PORT", "0"),
            ("SIDEVOICE_CONNECTOR_TOKEN", "secret"),
            ("SIDEVOICE_SERVICE", "launchd"),
        ])
        .unwrap();
        assert_eq!(named.data, data);
        assert_eq!(named.releases, data.join("install"));
        assert_eq!(named.agent_dirs["ALPHA_HOME"], alpha);
        assert_eq!(named.core_port(), "0");
        assert_eq!(
            named.settings.keys().collect::<Vec<_>>(),
            ["SIDEVOICE_CORE_PORT"],
            "credentials and what a definition sets itself are never settings"
        );
    }

    #[test]
    fn relative_paths_are_refused() {
        assert!(layout(&[(Environment::HOME, "relative")]).is_err());
        let home = absolute("h");
        assert!(layout(&[
            (Environment::HOME, home.to_str().unwrap()),
            ("SIDEVOICE_DATA_DIR", "d")
        ])
        .is_err());
        assert!(layout(&[]).is_err(), "no HOME, no layout");
    }

    /// A layout whose releases are in a private scratch directory, with release directories `ids` made in it.
    fn with_releases(ids: &[&str]) -> (tempfile::TempDir, Layout) {
        let scratch = crate::test_support::temp_dir("selected");
        let data = scratch.path().join("data");
        let home = scratch.path().join("home");
        let layout = layout(&[
            (Environment::HOME, home.to_str().unwrap()),
            ("SIDEVOICE_DATA_DIR", data.to_str().unwrap()),
        ])
        .unwrap();
        Files::ensure_private_dir(&layout.releases).unwrap();
        for id in ids {
            fs::create_dir_all(layout.releases_dir().join(id)).unwrap();
        }
        (scratch, layout)
    }

    #[test]
    fn the_pointer_record_selects_a_release_that_is_there() {
        let (_scratch, layout) = with_releases(&["1.0.0", "2.0.0+abc"]);
        assert_eq!(layout.selected_id(), None, "no record, nothing selected");
        assert!(!layout.installed());
        Files::write_json(
            &layout.current_record(),
            &serde_json::json!({"release": "1.0.0"}),
        )
        .unwrap();
        assert_eq!(layout.selected_id().as_deref(), Some("1.0.0"));
        assert_eq!(layout.selected(), Some(layout.releases_dir().join("1.0.0")));
        assert_eq!(
            layout.core_program(),
            Some(layout.releases_dir().join("1.0.0").join(CORE_ENTRYPOINT))
        );
        assert!(layout.installed());
        Files::write_json(
            &layout.current_record(),
            &serde_json::json!({"release": "2.0.0+abc"}),
        )
        .unwrap();
        assert_eq!(layout.selected_id().as_deref(), Some("2.0.0+abc"));
    }

    #[test]
    fn a_pointer_record_that_names_anything_else_selects_nothing() {
        let (_scratch, layout) = with_releases(&["1.0.0", ".1.0.0.partial-x"]);
        for record in [
            serde_json::json!({"release": "3.0.0"}),
            serde_json::json!({"release": "../releases/1.0.0"}),
            serde_json::json!({"release": "1.0.0/"}),
            serde_json::json!({"release": ".1.0.0.partial-x"}),
            serde_json::json!({"release": ""}),
            serde_json::json!({"release": 1}),
            serde_json::json!(["1.0.0"]),
        ] {
            Files::write_json(&layout.current_record(), &record).unwrap();
            assert_eq!(layout.selected_id(), None, "{record}");
        }
        Files::write_private(&layout.current_record(), b"not json").unwrap();
        assert!(!layout.installed());
    }

    #[test]
    fn a_pointer_record_is_not_read_through_a_link() {
        let (_scratch, layout) = with_releases(&["1.0.0"]);
        let outside = layout.releases.join("outside.json");
        Files::write_json(&outside, &serde_json::json!({"release": "1.0.0"})).unwrap();
        crate::test_support::symlink(&outside, layout.current_record());
        assert_eq!(layout.selected_id(), None);
    }
}
