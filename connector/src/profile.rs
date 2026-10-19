//! Where everything is: this user's directories, read from the environment, and the connector's data directory
//! layout.
//!
//! - `D`, the data directory: `$SIDEVOICE_DATA_DIR`, else `~/.sidevoice`. Private (0700). The connector is the
//!   one writer of its records there, each written whole (`secure_fs.rs`); `D/core/` is the core's.
//!   `connector.sock`, `connector.lock`, `connector.log`, `agents.json`, `agents.lock`, `install.lock`,
//!   `node-stopped.json` (a person's stop), `credentials.json` (the room pairing), `outbox.json`, `core/core.json`
//!   and `core/local.sock`.
//! - `R`, the release root: `$XDG_DATA_HOME/sidevoice` (else `~/.local/share/sidevoice`), as `service/layout.rs` names it.
//! - The agents' homes, each named by a variable its harness declares ([`AgentHome`]): the variable's value, else
//!   its default below `HOME` (or below another home of the same agent). A home that is not always passed on reaches
//!   a child only when the person's environment sets it, so the agent uses its own default there as it does for
//!   the person.
//!
//! Every path is absolute. The directories are checked again at each operation boundary, not trusted from startup:
//! they are passed to agents' command lines and written into.

use crate::secure_fs::{private_dir, private_file, validate_user_directory};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

/// Where an agent's home is when its variable is unset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HomeDefault {
    /// Below `HOME`.
    Home(&'static str),
    /// Below another home (named by its variable), declared before it.
    Under(&'static str, &'static str),
}

/// A directory an agent keeps, named by an environment variable: what a harness declares of its homes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentHome {
    pub var: &'static str,
    pub default: HomeDefault,
    /// Passed to a child even when the environment leaves the variable unset (the agent reads that very default).
    pub always_passed: bool,
    /// Its configuration is what Sidevoice's registration reads and writes (`validate_for_agent`).
    pub config: bool,
    /// Checked through its parent rather than itself.
    pub check_parent: bool,
}

/// An agent's home as this profile resolves it.
#[derive(Clone, Debug)]
pub struct ResolvedHome {
    pub spec: AgentHome,
    pub path: PathBuf,
    /// Whether the environment sets its variable.
    pub set: bool,
}

#[derive(Clone, Debug)]
pub struct Profile {
    pub home: PathBuf,
    /// `D`, the data directory.
    pub data: PathBuf,
    /// The agents' homes, as their harnesses declare them.
    pub agent_homes: Vec<ResolvedHome>,
    pub xdg_config: PathBuf,
    pub xdg_data: PathBuf,
    pub socket: PathBuf,
    pub core_socket: PathBuf,
    pub core_ready: PathBuf,
}

impl Profile {
    /// This user's profile from the environment, with the agents' homes `homes` declares.
    pub fn from_env(homes: &[AgentHome]) -> Result<Self> {
        Self::from_vars(|key| std::env::var_os(key), homes)
    }

    pub fn from_vars(var: impl Fn(&str) -> Option<OsString>, homes: &[AgentHome]) -> Result<Self> {
        let home = PathBuf::from(var("HOME").context("HOME is not set")?);
        if !home.is_absolute() {
            bail!("HOME must be absolute");
        }
        let or = |key: &str, fallback: PathBuf| var(key).map(PathBuf::from).unwrap_or(fallback);
        let data = or("SIDEVOICE_DATA_DIR", home.join(".sidevoice"));
        let xdg_config = or("XDG_CONFIG_HOME", home.join(".config"));
        let xdg_data = or("XDG_DATA_HOME", home.join(".local/share"));
        for (name, path) in [
            ("SIDEVOICE_DATA_DIR", &data),
            ("XDG_CONFIG_HOME", &xdg_config),
            ("XDG_DATA_HOME", &xdg_data),
        ] {
            if !path.is_absolute() {
                bail!("{name} must be absolute");
            }
        }
        let mut agent_homes: Vec<ResolvedHome> = Vec::new();
        for spec in homes {
            let set = var(spec.var).is_some();
            let fallback = match spec.default {
                HomeDefault::Home(relative) => home.join(relative),
                HomeDefault::Under(parent, relative) => agent_homes
                    .iter()
                    .find(|resolved| resolved.spec.var == parent)
                    .with_context(|| format!("{} is declared before {parent}", spec.var))?
                    .path
                    .join(relative),
            };
            let path = or(spec.var, fallback);
            if !path.is_absolute() {
                bail!("{} must be absolute", spec.var);
            }
            agent_homes.push(ResolvedHome {
                spec: *spec,
                path,
                set,
            });
        }
        let core = data.join("core");
        let profile = Self {
            home,
            socket: data.join("connector.sock"),
            core_socket: core.join("local.sock"),
            core_ready: core.join("core.json"),
            data,
            agent_homes,
            xdg_config,
            xdg_data,
        };
        profile.validate_existing_private()?;
        Ok(profile)
    }

    /// The home named by `var`, as this profile resolves it. A harness asks only for homes it declares.
    pub fn agent_home(&self, var: &str) -> &Path {
        self.agent_homes
            .iter()
            .find(|resolved| resolved.spec.var == var)
            .map(|resolved| resolved.path.as_path())
            .unwrap_or_else(|| panic!("{var} is not a declared agent home"))
    }

    /// Whether the person's environment sets the home named by `var`. A harness asks only for homes it declares.
    pub fn agent_home_set(&self, var: &str) -> bool {
        self.agent_homes
            .iter()
            .find(|resolved| resolved.spec.var == var)
            .map(|resolved| resolved.set)
            .unwrap_or_else(|| panic!("{var} is not a declared agent home"))
    }

    /// The version this connector reports: the build's.
    pub fn connector_version(&self) -> &str {
        env!("SIDEVOICE_CONNECTOR_VERSION")
    }

    /// What identifies the running build to a client that checks it (`identity`).
    pub fn runtime_identity(&self) -> Value {
        json!({"runtime_build_sha":env!("SIDEVOICE_CONNECTOR_BUILD_SHA"),
            "runtime_target":env!("SIDEVOICE_CONNECTOR_TARGET")})
    }

    /// Passes this profile on to a child, whatever the child's own environment was cleared to. A home that is
    /// passed on only when set is removed from the child's environment when it is not.
    pub fn command_env<'a>(
        &self,
        command: &'a mut tokio::process::Command,
    ) -> &'a mut tokio::process::Command {
        for resolved in &self.agent_homes {
            if !resolved.spec.always_passed && !resolved.set {
                command.env_remove(resolved.spec.var);
            }
        }
        for (key, value) in self.variables() {
            command.env(key, value);
        }
        command
    }

    /// The variables a child is given: every directory, except an agent's home its harness passes on only when the
    /// environment sets it, and it does not (the agent then uses its own default, as it does for the person).
    pub fn variables(&self) -> Vec<(&'static str, &Path)> {
        let mut variables = vec![("HOME", self.home.as_path())];
        variables.extend(
            self.agent_homes
                .iter()
                .filter(|resolved| resolved.spec.always_passed || resolved.set)
                .map(|resolved| (resolved.spec.var, resolved.path.as_path())),
        );
        variables.extend([
            ("XDG_CONFIG_HOME", self.xdg_config.as_path()),
            ("XDG_DATA_HOME", self.xdg_data.as_path()),
            ("SIDEVOICE_DATA_DIR", self.data.as_path()),
        ]);
        variables
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
        let mut paths = vec![self.home.as_path()];
        for resolved in &self.agent_homes {
            paths.push(if resolved.spec.check_parent {
                resolved
                    .path
                    .parent()
                    .with_context(|| format!("{} parent", resolved.spec.var))?
            } else {
                resolved.path.as_path()
            });
        }
        paths.extend([
            self.xdg_config.parent().context("XDG config parent")?,
            &self.xdg_config,
            &self.xdg_data,
        ]);
        for path in paths {
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

    /// Before an agent's configuration is read or changed: it is a configuration home this profile names, and a
    /// directory of the person's.
    pub fn validate_for_agent(&self, config_root: &Path) -> Result<()> {
        self.validate_existing_private()?;
        if !self
            .agent_homes
            .iter()
            .any(|resolved| resolved.spec.config && resolved.path == config_root)
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
    pub fn for_test(root: &Path, homes: &[AgentHome]) -> Self {
        use std::os::unix::fs::DirBuilderExt;
        let mut children = vec![
            "home".to_owned(),
            "xdg/config".into(),
            "xdg/data".into(),
            "sidevoice/core".into(),
        ];
        children.extend(
            homes
                .iter()
                .map(|home| format!("agents/{}", home.var.to_ascii_lowercase())),
        );
        for child in &children {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(root.join(child))
                .unwrap();
        }
        let root = root.canonicalize().unwrap();
        let path = |child: &str| -> OsString { root.join(child).into_os_string() };
        let mut variables = vec![
            ("HOME".to_owned(), path("home")),
            ("XDG_CONFIG_HOME".into(), path("xdg/config")),
            ("XDG_DATA_HOME".into(), path("xdg/data")),
            ("SIDEVOICE_DATA_DIR".into(), path("sidevoice")),
        ];
        variables.extend(homes.iter().map(|home| {
            (
                home.var.to_owned(),
                path(&format!("agents/{}", home.var.to_ascii_lowercase())),
            )
        }));
        Self::from_vars(
            |key| {
                variables
                    .iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value.clone())
            },
            homes,
        )
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

    /// Two agents' homes as a harness would declare them: one passed on always, one only when set, one below it.
    const HOMES: [AgentHome; 3] = [
        AgentHome {
            var: "ALPHA_HOME",
            default: HomeDefault::Home(".alpha"),
            always_passed: true,
            config: true,
            check_parent: false,
        },
        AgentHome {
            var: "BETA_HOME",
            default: HomeDefault::Home(".beta"),
            always_passed: false,
            config: true,
            check_parent: true,
        },
        AgentHome {
            var: "BETA_DATA",
            default: HomeDefault::Under("BETA_HOME", "data"),
            always_passed: true,
            config: false,
            check_parent: false,
        },
    ];

    #[test]
    fn the_layout_derives_from_home_unless_each_place_is_named() {
        let scratch = Scratch::new("layout");
        let home = scratch.0.to_string_lossy().into_owned();
        let profile = Profile::from_vars(vars(&[("HOME", &home)]), &HOMES).unwrap();
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
        assert_eq!(profile.agent_home("ALPHA_HOME"), scratch.0.join(".alpha"));
        assert_eq!(profile.agent_home("BETA_HOME"), scratch.0.join(".beta"));
        assert_eq!(
            profile.agent_home("BETA_DATA"),
            scratch.0.join(".beta/data")
        );
        assert_eq!(profile.connector_version(), crate::identity::VERSION);

        let data = scratch.0.join("elsewhere");
        let beta = scratch.0.join("beta");
        let named = Profile::from_vars(
            vars(&[
                ("HOME", &home),
                ("SIDEVOICE_DATA_DIR", data.to_str().unwrap()),
                ("XDG_DATA_HOME", "/xdg-data"),
                ("BETA_HOME", beta.to_str().unwrap()),
            ]),
            &HOMES,
        )
        .unwrap();
        assert_eq!(named.data, data);
        assert_eq!(named.agent_home("BETA_DATA"), beta.join("data"));
    }

    /// A home its harness passes on only when set reaches a child only when the environment sets it, and a stray
    /// inherited value is removed; one passed always reaches it either way.
    #[test]
    fn a_home_passed_only_when_set_reaches_a_child_only_then() {
        let scratch = Scratch::new("passed-when-set");
        let home = scratch.0.to_string_lossy().into_owned();
        let passed = |profile: &Profile, key: &str| {
            let mut command = tokio::process::Command::new("true");
            profile.command_env(&mut command);
            command
                .as_std()
                .get_envs()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.map(|value| value.to_owned()))
        };

        let unset = Profile::from_vars(vars(&[("HOME", &home)]), &HOMES).unwrap();
        assert!(!unset.variables().iter().any(|(key, _)| *key == "BETA_HOME"));
        assert_eq!(
            passed(&unset, "BETA_HOME"),
            Some(None),
            "an inherited value is removed"
        );
        assert_eq!(
            passed(&unset, "ALPHA_HOME"),
            Some(Some(scratch.0.join(".alpha").into_os_string()))
        );
        let layout = crate::service::layout::Layout::from_profile(&unset);
        assert!(!layout.agent_dirs.contains_key("BETA_HOME"));
        assert!(layout.agent_dirs_unset().contains(&"BETA_HOME"));

        let named = scratch.0.join("beta-home");
        fs::create_dir(&named).unwrap();
        fs::set_permissions(&named, fs::Permissions::from_mode(0o700)).unwrap();
        let set = Profile::from_vars(
            vars(&[("HOME", &home), ("BETA_HOME", named.to_str().unwrap())]),
            &HOMES,
        )
        .unwrap();
        assert_eq!(
            passed(&set, "BETA_HOME"),
            Some(Some(named.clone().into_os_string()))
        );
        let layout = crate::service::layout::Layout::from_profile(&set);
        assert_eq!(layout.agent_dirs.get("BETA_HOME"), Some(&named));
    }

    #[test]
    fn relative_or_missing_places_are_refused() {
        assert!(Profile::from_vars(vars(&[]), &HOMES).is_err());
        assert!(Profile::from_vars(vars(&[("HOME", "relative")]), &HOMES).is_err());
        assert!(Profile::from_vars(
            vars(&[("HOME", "/tmp"), ("SIDEVOICE_DATA_DIR", "data")]),
            &HOMES
        )
        .is_err());
        assert!(
            Profile::from_vars(vars(&[("HOME", "/tmp"), ("ALPHA_HOME", "alpha")]), &HOMES).is_err()
        );
    }

    #[test]
    fn a_data_directory_others_can_enter_or_a_replaced_agent_home_is_refused() {
        let scratch = Scratch::new("trust");
        let profile = Profile::for_test(&scratch.0, &HOMES);
        let config = profile
            .agent_homes
            .iter()
            .filter(|home| home.spec.config)
            .map(|home| home.path.clone())
            .collect::<Vec<_>>();
        let (first, second) = (&config[0], &config[1]);
        profile.validate_existing_private().unwrap();
        fs::set_permissions(&profile.data, fs::Permissions::from_mode(0o750)).unwrap();
        assert!(profile.validate_existing_private().is_err());
        fs::set_permissions(&profile.data, fs::Permissions::from_mode(0o700)).unwrap();

        fs::set_permissions(second, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(profile.validate_for_agent(second).is_err());
        fs::set_permissions(second, fs::Permissions::from_mode(0o700)).unwrap();
        profile.validate_for_agent(second).unwrap();
        assert!(profile.validate_for_agent(&profile.home).is_err());

        fs::remove_dir(first).unwrap();
        std::os::unix::fs::symlink(second, first).unwrap();
        assert!(profile.validate_existing_private().is_err());
    }

    #[test]
    fn missing_directories_are_not_unsafe_and_one_connector_holds_the_data_directory() {
        let scratch = Scratch::new("fresh");
        let home = scratch.0.to_string_lossy().into_owned();
        let profile = Profile::from_vars(vars(&[("HOME", &home)]), &HOMES).unwrap();
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
        let profile = Profile::for_test(&scratch.0, &HOMES);
        assert!(!profile.service_stopped().unwrap());
        let marker = profile.data.join("node-stopped.json");
        crate::secure_fs::atomic_json(&marker, &json!({"at":"now"})).unwrap();
        assert!(profile.service_stopped().unwrap());
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(profile.service_stopped().is_err());
    }
}
