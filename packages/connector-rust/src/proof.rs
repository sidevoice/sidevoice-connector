use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
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
        validate_profile_child(&self.root, config_root)
    }

    pub fn validate_private(&self) -> Result<()> {
        self.validate_existing_private()?;
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
