use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::{timeout, Duration};

#[derive(Clone, Debug)]
pub struct Profile {
    pub root: PathBuf,
    pub home: PathBuf,
    pub claude: PathBuf,
    pub data: PathBuf,
    pub codex: PathBuf,
    pub cursor: PathBuf,
    pub cursor_data: PathBuf,
    pub xdg_config: PathBuf,
    pub xdg_data: PathBuf,
    pub socket: PathBuf,
    pub core_socket: PathBuf,
    pub core_ready: PathBuf,
    pub installed: Option<InstalledRelease>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct InstalledRelease {
    pub id: String,
    pub connector: String,
    pub format: Option<String>,
    pub core_kind: Option<String>,
    pub core_build: Option<String>,
    pub core_target: Option<String>,
    pub core_source_sha: Option<String>,
    pub core_cargo_lock_sha256: Option<String>,
    pub core_manifest_sha256: Option<String>,
    pub core_archive_sha256: Option<String>,
    pub core_archive_size: Option<u64>,
    pub core_entrypoint: Option<String>,
    pub runtime_kind: String,
    pub runtime_build_sha: Option<String>,
    pub runtime_target: Option<String>,
    pub runtime_sha256: Option<String>,
    pub runtime_size: Option<u64>,
    pub distributor_sha256: Option<String>,
    pub distributor_size: Option<u64>,
    pub pair_id: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Ready {
    pub pid: u32,
    pub launch_id: String,
    pub socket: PathBuf,
    pub connector_id: String,
    pub token: String,
    pub connector_protocols: Option<Vec<u8>>,
}

#[derive(Serialize)]
pub struct Evidence<'a> {
    pub pid: u32,
    pub executable: String,
    pub executable_sha256: String,
    pub core_launch_id: &'a str,
    pub core_pid: u32,
    pub protocol: u8,
    pub socket: String,
}

fn uid() -> u32 {
    unsafe { libc::geteuid() }
}

fn validate_profile_child(root: &Path, path: &Path) -> Result<()> {
    private_dir(path)?;
    let canonical = path.canonicalize()?;
    if canonical != path || !canonical.starts_with(root) {
        bail!("proof profile directory escaped its root");
    }
    Ok(())
}

fn digest_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut block = [0u8; 65536];
    loop {
        let read = file.read(&mut block)?;
        if read == 0 {
            break;
        }
        hasher.update(&block[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn safe_executable(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != uid()
        || metadata.mode() & 0o022 != 0
        || metadata.mode() & 0o111 == 0
    {
        bail!("unsafe installed executable {}", path.display());
    }
    Ok(metadata)
}

fn compiled_target() -> Option<&'static str> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return Some("macos-aarch64");
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        return Some("linux-x86_64");
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        return Some("linux-aarch64");
    }
    #[allow(unreachable_code)]
    None
}

fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn private_dir(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)
        .with_context(|| format!("missing private directory {}", path.display()))?;
    if !m.is_dir() || m.file_type().is_symlink() || m.uid() != uid() || m.mode() & 0o077 != 0 {
        bail!("unsafe private directory {}", path.display());
    }
    Ok(())
}

pub fn private_file(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_file() || m.file_type().is_symlink() || m.uid() != uid() || m.mode() & 0o077 != 0 {
        bail!("unsafe private file {}", path.display());
    }
    Ok(())
}

pub fn validate_user_directory(path: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("missing user directory {}", path.display()))?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != uid()
        || metadata.mode() & 0o022 != 0
    {
        bail!("unsafe user directory {}", path.display());
    }
    path.canonicalize().map_err(Into::into)
}

pub fn verify_socket(path: &Path) -> Result<()> {
    private_dir(path.parent().context("socket parent")?)?;
    let m = fs::symlink_metadata(path)?;
    if !m.file_type().is_socket() || m.uid() != uid() || m.mode() & 0o077 != 0 {
        bail!("unsafe socket {}", path.display());
    }
    Ok(())
}

impl Profile {
    pub fn from_root(path: &Path) -> Result<Self> {
        if !path.is_absolute() {
            bail!("proof root must be absolute");
        }
        let root_meta = fs::symlink_metadata(path).context("missing proof root")?;
        if root_meta.file_type().is_symlink() {
            bail!("proof root cannot be a symlink");
        }
        private_dir(path)?;
        let root = path.canonicalize()?;
        let child = |name: &str| -> Result<PathBuf> {
            let path = root.join(name);
            private_dir(&path)?;
            let canonical = path.canonicalize()?;
            if canonical != path || !canonical.starts_with(&root) {
                bail!("proof profile directory escaped its root");
            }
            Ok(canonical)
        };
        let home = child("home")?;
        let claude = child("claude")?;
        let codex = child("codex")?;
        let cursor_root = child("cursor")?;
        let cursor = cursor_root.join("config");
        let cursor_data = cursor_root.join("data");
        private_dir(&cursor)?;
        private_dir(&cursor_data)?;
        let xdg_root = child("xdg")?;
        let xdg_config = xdg_root.join("config");
        let xdg_data = xdg_root.join("data");
        private_dir(&xdg_config)?;
        private_dir(&xdg_data)?;
        let data = child("sidevoice")?;
        let core = data.join("core");
        private_dir(&core)?;
        if data.join("install.json").exists() {
            bail!("selected installation data is forbidden in proof profile");
        }
        let profile = Self {
            root,
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
            installed: None,
        };
        profile.validate_private()?;
        Ok(profile)
    }

    pub fn from_env() -> Result<Self> {
        let data = PathBuf::from(
            std::env::var_os("SIDEVOICE_DATA_DIR")
                .context("SIDEVOICE_DATA_DIR required for proof")?,
        );
        let codex =
            PathBuf::from(std::env::var_os("CODEX_HOME").context("CODEX_HOME required for proof")?);
        let root = data.parent().context("proof root")?;
        if data != root.join("sidevoice") || codex != root.join("codex") {
            bail!("proof profile paths must be fixed children of one private root");
        }
        let profile = Self::from_root(root)?;
        for (key, expected) in [
            ("HOME", &profile.home),
            ("CLAUDE_CONFIG_DIR", &profile.claude),
            ("CODEX_HOME", &profile.codex),
            ("CURSOR_CONFIG_DIR", &profile.cursor),
            ("CURSOR_DATA_DIR", &profile.cursor_data),
            ("XDG_CONFIG_HOME", &profile.xdg_config),
            ("XDG_DATA_HOME", &profile.xdg_data),
            ("SIDEVOICE_DATA_DIR", &profile.data),
        ] {
            let supplied =
                std::env::var_os(key).context("isolated proof environment is incomplete")?;
            if Path::new(&supplied) != expected.as_path() {
                bail!("isolated proof environment does not match its root");
            }
        }
        Ok(profile)
    }

    pub fn from_installed_env() -> Result<Self> {
        let executable = std::env::current_exe()?.canonicalize()?;
        let dist = executable.parent().context("installed dist directory")?;
        let release_dir = dist
            .parent()
            .context("installed release directory")?
            .to_path_buf();
        if executable.file_name().and_then(|name| name.to_str()) != Some("sidevoice-rust")
            || dist.file_name().and_then(|name| name.to_str()) != Some("dist")
            || release_dir
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                != Some("releases")
        {
            bail!("Rust Connector is not inside a selected Sidevoice release");
        }
        let releases_dir = release_dir
            .parent()
            .context("release collection")?
            .to_path_buf();
        let root = releases_dir
            .parent()
            .context("Sidevoice release root")?
            .to_path_buf();
        private_dir(&root)?;
        private_dir(&releases_dir)?;
        private_dir(&release_dir)?;
        private_dir(dist)?;
        let current_link = root.join("current");
        if !fs::symlink_metadata(&current_link)?
            .file_type()
            .is_symlink()
            || current_link.canonicalize()? != release_dir
        {
            bail!("Rust Connector is not the current selected release");
        }
        let release_file = release_dir.join("release.json");
        private_file(&release_file)?;
        if fs::metadata(&release_file)?.len() > 65536 {
            bail!("selected release record is oversized");
        }
        let selected: InstalledRelease = serde_json::from_slice(&fs::read(&release_file)?)?;
        let target = compiled_target().context("unsupported installed Rust Connector target")?;
        let runtime_sha = selected
            .runtime_sha256
            .as_deref()
            .context("selected runtime digest missing")?;
        let runtime_source = selected
            .runtime_build_sha
            .as_deref()
            .context("selected Rust Connector source missing")?;
        let core_target = selected
            .core_target
            .as_deref()
            .context("selected Core target missing")?;
        let core_source = selected
            .core_source_sha
            .as_deref()
            .context("selected Core source missing")?;
        let core_archive_sha = selected
            .core_archive_sha256
            .as_deref()
            .context("selected Core archive digest missing")?;
        let core_id = format!("rust-native-v1-{core_target}-{core_source}-{core_archive_sha}");
        if selected.id
            != release_dir
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
            || selected.format.as_deref() != Some("sea")
            || selected.runtime_kind != "rust-native-v1"
            || selected.runtime_target.as_deref() != Some(target)
            || runtime_source != env!("SIDEVOICE_CONNECTOR_BUILD_SHA")
            || selected.connector != env!("SIDEVOICE_CONNECTOR_VERSION")
            || env!("SIDEVOICE_CONNECTOR_TARGET") != target
            || selected.core_kind.as_deref() != Some("rust-native-v1")
            || core_target != target
            || !lower_hex(runtime_sha, 64)
            || !lower_hex(runtime_source, 40)
            || !lower_hex(core_source, 40)
            || !lower_hex(core_archive_sha, 64)
            || !selected
                .core_cargo_lock_sha256
                .as_deref()
                .is_some_and(|value| lower_hex(value, 64))
            || !selected
                .core_manifest_sha256
                .as_deref()
                .is_some_and(|value| lower_hex(value, 64))
            || selected.core_entrypoint.as_deref() != Some("bin/sidevoice-core-rust")
            || selected.core_build.as_deref() != Some(core_id.as_str())
            || selected.pair_id != format!("pair-v1:rust-native-v1:{runtime_sha}:core:{core_id}")
        {
            bail!("selected Rust Connector and Core release identity is invalid");
        }
        let runtime_size = selected
            .runtime_size
            .context("selected runtime size missing")?;
        let runtime_info = safe_executable(&executable)?;
        if runtime_info.len() != runtime_size || digest_file(&executable)? != runtime_sha {
            bail!("selected Rust Connector bytes differ from the release record");
        }
        let control = dist.join("sidevoice");
        let control_info = safe_executable(&control)?;
        if control_info.len()
            != selected
                .distributor_size
                .context("selected control SEA size missing")?
            || digest_file(&control)?
                != selected
                    .distributor_sha256
                    .as_deref()
                    .context("selected control SEA digest missing")?
        {
            bail!("selected Sidevoice control executable differs from the release record");
        }
        let core_binary = release_dir.join("core/bin/sidevoice-core-rust");
        let _ = safe_executable(&core_binary)?;
        let home = PathBuf::from(
            std::env::var_os("HOME").context("HOME is required for installed Sidevoice")?,
        );
        if !home.is_absolute() {
            bail!("HOME must be absolute");
        }
        let data = std::env::var_os("SIDEVOICE_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".sidevoice"));
        let claude = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        let codex = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        let cursor = std::env::var_os("CURSOR_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cursor"));
        let cursor_data = std::env::var_os("CURSOR_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| cursor.join("data"));
        let xdg_config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let xdg_data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"));
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
        let core_data = data.join("core");
        let profile = Self {
            root,
            home,
            claude,
            data: data.clone(),
            codex,
            cursor,
            cursor_data,
            xdg_config,
            xdg_data,
            socket: data.join("connector.sock"),
            core_socket: core_data.join("local.sock"),
            core_ready: core_data.join("core.json"),
            installed: Some(selected),
        };
        profile.validate_existing_private()?;
        Ok(profile)
    }

    pub fn is_installed(&self) -> bool {
        self.installed.is_some()
    }

    pub fn connector_version(&self) -> &str {
        self.installed
            .as_ref()
            .map(|selected| selected.connector.as_str())
            .unwrap_or(env!("CARGO_PKG_VERSION"))
    }

    pub fn validate_installed_service_environment(&self) -> Result<()> {
        if self.installed.is_none() {
            bail!("installed service environment requires an installed release");
        }
        let service = std::env::var("SIDEVOICE_SERVICE")
            .context("installed service manager is not identified")?;
        if !matches!(service.as_str(), "launchd" | "systemd") {
            bail!("unsupported installed service manager");
        }
        for (key, expected) in [
            ("HOME", &self.home),
            ("CLAUDE_CONFIG_DIR", &self.claude),
            ("CODEX_HOME", &self.codex),
            ("CURSOR_CONFIG_DIR", &self.cursor),
            ("CURSOR_DATA_DIR", &self.cursor_data),
            ("XDG_CONFIG_HOME", &self.xdg_config),
            ("XDG_DATA_HOME", &self.xdg_data),
            ("SIDEVOICE_DATA_DIR", &self.data),
        ] {
            let required = matches!(
                key,
                "HOME" | "XDG_CONFIG_HOME" | "XDG_DATA_HOME" | "SIDEVOICE_DATA_DIR"
            );
            let supplied = match std::env::var_os(key) {
                Some(supplied) => supplied,
                None if required => bail!("installed service environment is incomplete"),
                // The existing JS service also derives these agent paths from HOME unless explicitly set.
                None => continue,
            };
            if Path::new(&supplied) != expected.as_path() {
                bail!("installed service environment differs from its profile");
            }
        }
        Ok(())
    }

    pub fn runtime_identity(&self) -> Value {
        if let Some(selected) = &self.installed {
            json!({"runtime_kind":selected.runtime_kind,"runtime_build_sha":selected.runtime_build_sha,
                "runtime_sha256":selected.runtime_sha256,"runtime_target":selected.runtime_target,"release_id":selected.id})
        } else {
            json!({"runtime_kind":"rust-proof","runtime_build_sha":env!("SIDEVOICE_CONNECTOR_BUILD_SHA"),
                "runtime_target":env!("SIDEVOICE_CONNECTOR_TARGET")})
        }
    }

    pub fn control_executable(&self) -> Result<PathBuf> {
        if self.installed.is_none() {
            bail!("the proof profile has no selected control executable");
        }
        Ok(self.root.join("current/dist/sidevoice"))
    }

    pub fn command_env<'a>(
        &self,
        command: &'a mut tokio::process::Command,
    ) -> &'a mut tokio::process::Command {
        command
            .env("HOME", &self.home)
            .env("CLAUDE_CONFIG_DIR", &self.claude)
            .env("CODEX_HOME", &self.codex)
            .env("CURSOR_CONFIG_DIR", &self.cursor)
            .env("CURSOR_DATA_DIR", &self.cursor_data)
            .env("XDG_CONFIG_HOME", &self.xdg_config)
            .env("XDG_DATA_HOME", &self.xdg_data)
            .env("SIDEVOICE_DATA_DIR", &self.data)
    }

    #[cfg(target_os = "macos")]
    pub fn validate_service_environment(&self, manager: &str) -> Result<()> {
        for (key, expected) in [
            ("HOME", &self.home),
            ("CLAUDE_CONFIG_DIR", &self.claude),
            ("CODEX_HOME", &self.codex),
            ("CURSOR_CONFIG_DIR", &self.cursor),
            ("CURSOR_DATA_DIR", &self.cursor_data),
            ("XDG_CONFIG_HOME", &self.xdg_config),
            ("XDG_DATA_HOME", &self.xdg_data),
            ("SIDEVOICE_DATA_DIR", &self.data),
        ] {
            let supplied =
                std::env::var_os(key).context("managed profile environment is incomplete")?;
            if Path::new(&supplied) != expected.as_path() {
                bail!("managed profile environment does not match its root");
            }
        }
        if std::env::var("SIDEVOICE_SERVICE").ok().as_deref() != Some(manager) {
            bail!("managed service invocation is not identified by its manager");
        }
        Ok(())
    }

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

    /// Reject profile paths that were replaced after this process opened the isolated profile.
    /// All of these roots are passed to host CLIs or used for profile writes, so check them again
    /// at each operation boundary rather than trusting startup-time validation.
    pub fn validate_existing_private(&self) -> Result<()> {
        if self.installed.is_some() {
            private_dir(&self.root)?;
            private_dir(&self.root.join("releases"))?;
            private_dir(&self.data)?;
            private_dir(&self.data.join("core"))?;
            if self.root.canonicalize()? != self.root {
                bail!("installed release root changed after startup");
            }
            for path in [
                &self.home,
                &self.claude,
                &self.codex,
                self.cursor.parent().context("Cursor config root")?,
                &self.cursor_data,
                self.xdg_config.parent().context("XDG config root")?,
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
            return Ok(());
        }
        private_dir(&self.root)?;
        let root = self.root.canonicalize()?;
        if root != self.root {
            bail!("proof root changed after startup");
        }
        validate_profile_child(&root, &self.home)?;
        validate_profile_child(&root, &self.data)?;
        let core = self.data.join("core");
        validate_profile_child(&root, &core)?;
        validate_profile_child(&root, self.cursor.parent().context("Cursor profile root")?)?;
        validate_profile_child(&root, &self.cursor_data)?;
        validate_profile_child(&root, self.xdg_config.parent().context("XDG profile root")?)?;
        validate_profile_child(&root, &self.xdg_config)?;
        validate_profile_child(&root, &self.xdg_data)?;
        for path in [&self.claude, &self.codex, &self.cursor] {
            match fs::symlink_metadata(path) {
                Ok(_) => validate_profile_child(&root, path)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub fn validate_for_agent(&self, config_root: &Path) -> Result<()> {
        self.validate_existing_private()?;
        if config_root != self.claude.as_path()
            && config_root != self.codex.as_path()
            && config_root != self.cursor.as_path()
        {
            bail!("unknown proof agent configuration directory");
        }
        if self.installed.is_some() {
            validate_user_directory(config_root)?;
            return Ok(());
        }
        validate_profile_child(&self.root, config_root)
    }

    pub fn agent_root(&self, config_root: &Path) -> Result<PathBuf> {
        self.validate_for_agent(config_root)?;
        if self.installed.is_some() {
            config_root.canonicalize().map_err(Into::into)
        } else {
            Ok(self.root.clone())
        }
    }

    pub fn validate_private(&self) -> Result<()> {
        self.validate_existing_private()?;
        if self.installed.is_some() {
            return Ok(());
        }
        for path in [&self.claude, &self.codex, &self.cursor] {
            validate_profile_child(&self.root, path)?;
        }
        Ok(())
    }

    pub fn try_connector_lock(&self) -> Result<File> {
        let path = self.data.join("connector.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        private_file(&path)?;
        lock.try_lock_exclusive()
            .context("the isolated connector already owns this profile")?;
        Ok(lock)
    }

    pub async fn ready(&self) -> Result<Ready> {
        self.health().await.map(|(ready, _)| ready)
    }

    pub fn read_ready(&self) -> Result<Ready> {
        private_file(&self.core_ready)?;
        let bytes = fs::read(&self.core_ready)?;
        if bytes.len() > 65536 {
            bail!("Core ready file too large");
        }
        let ready: Ready = serde_json::from_slice(&bytes)?;
        if ready.socket != self.core_socket || ready.launch_id.is_empty() || ready.pid == 0 {
            bail!("Core ready identity mismatch");
        }
        if !ready
            .connector_protocols
            .as_ref()
            .is_some_and(|v| v.contains(&3))
        {
            bail!("Core v3 upgrade required");
        }
        verify_socket(&ready.socket)?;
        Ok(ready)
    }

    pub async fn health_ready(&self, ready: &Ready) -> Result<Value> {
        if ready.socket != self.core_socket || ready.launch_id.is_empty() || ready.pid == 0 {
            bail!("Core ready identity mismatch");
        }
        if !ready
            .connector_protocols
            .as_ref()
            .is_some_and(|versions| versions.contains(&3))
        {
            bail!("Core v3 upgrade required");
        }
        verify_socket(&ready.socket)?;
        let mut stream =
            timeout(Duration::from_secs(2), UnixStream::connect(&ready.socket)).await??;
        stream
            .write_all(
                b"GET /api/local/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .await?;
        let mut body = Vec::new();
        timeout(
            Duration::from_secs(2),
            stream.take(65536).read_to_end(&mut body),
        )
        .await??;
        let response = String::from_utf8(body)?;
        if !response.starts_with("HTTP/1.1 200 ") && !response.starts_with("HTTP/1.0 200 ") {
            bail!("Core health refused");
        }
        let (_, payload) = response
            .split_once("\r\n\r\n")
            .context("Core health body missing")?;
        let health: Value = serde_json::from_str(payload)?;
        if health.get("launch_id").and_then(Value::as_str) != Some(&ready.launch_id)
            || health.get("pid").and_then(Value::as_u64) != Some(ready.pid as u64)
        {
            bail!("Core health identity mismatch");
        }
        Ok(health)
    }

    pub async fn health(&self) -> Result<(Ready, Value)> {
        let ready = self.read_ready()?;
        let health = self.health_ready(&ready).await?;
        Ok((ready, health))
    }

    pub fn write_evidence(&self, ready: &Ready) -> Result<()> {
        let exe = std::env::current_exe()?.canonicalize()?;
        let mut input = File::open(&exe)?;
        let mut hasher = Sha256::new();
        let mut block = [0u8; 65536];
        loop {
            let n = input.read(&mut block)?;
            if n == 0 {
                break;
            }
            hasher.update(&block[..n]);
        }
        let evidence = Evidence {
            pid: std::process::id(),
            executable: exe.display().to_string(),
            executable_sha256: hex::encode(hasher.finalize()),
            core_launch_id: &ready.launch_id,
            core_pid: ready.pid,
            protocol: 3,
            socket: self.socket.display().to_string(),
        };
        atomic_json(&self.data.join("proof.json"), &evidence)
    }
}

pub fn atomic_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("file parent")?;
    private_dir(parent)?;
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap().to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        serde_json::to_writer(&mut file, value)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}
