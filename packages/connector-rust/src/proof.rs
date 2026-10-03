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
        let cursor = child("cursor")?;
        let data = child("sidevoice")?;
        let core = data.join("core");
        private_dir(&core)?;
        if data.join("install.json").exists() {
            bail!("selected installation data is forbidden in proof profile");
        }
        Ok(Self {
            root,
            home,
            claude,
            socket: data.join("connector.sock"),
            core_socket: core.join("local.sock"),
            core_ready: core.join("core.json"),
            data,
            codex,
            cursor,
        })
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
            .env("SIDEVOICE_DATA_DIR", &self.data)
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
        Ok(ready)
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
