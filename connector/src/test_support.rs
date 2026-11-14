//! What the crate's unit tests share. What they do to files (links, programs, access) is the platform's:
//! `crate::platform::fs::TestFiles`.

/// A scratch directory for one test, removed when dropped (`tempfile`). Under the temporary directory's canonical
/// path (macOS: `/private/var/...`, not the `/var` link), so paths built from it equal what the code canonicalizes.
/// Not private: a test that needs a private directory makes one inside with `Files::create_private_dir`.
pub(crate) fn temp_dir(label: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("sv-{label}-"))
        .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
        .unwrap()
}

pub(crate) mod stand_in;

/// Cancel only after the leader and descendants are running, then prove none survived.
pub(crate) async fn assert_cancellation_stops_children<T>(
    task: tokio::task::JoinHandle<T>,
    pid_files: &[&std::path::Path],
) {
    use crate::platform::process::{ChildProcess, Processes};
    use std::time::Duration;
    use tokio::time::{sleep, Instant};
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut pids = Vec::new();
    for pid_file in pid_files {
        let pid = loop {
            if let Some(pid) = std::fs::read_to_string(pid_file)
                .ok()
                .and_then(|text| text.parse::<u32>().ok())
            {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "child never reported its pid: {}",
                pid_file.display()
            );
            sleep(Duration::from_millis(20)).await;
        };
        pids.push(pid);
    }
    task.abort();
    assert!(task.await.err().is_some_and(|error| error.is_cancelled()));
    let deadline = Instant::now() + Duration::from_secs(10);
    for pid in pids {
        while Processes::identity(pid).is_some() {
            assert!(
                Instant::now() < deadline,
                "cancelled delivery left pid {pid} running"
            );
            sleep(Duration::from_millis(20)).await;
        }
    }
}

/// The log lines every thread's events produce from [`capture`] on, as the daemon would write them: one global
/// subscriber for the whole test process, so events from `spawn_blocking` and other threads are seen too. Tests run in
/// parallel, so others' lines are in it as well: assert on values unique to the test.
pub(crate) struct Captured {
    from: usize,
}

static LINES: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());

impl Captured {
    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&LINES.lock().unwrap()[self.from..]).into_owned()
    }
}

#[derive(Clone)]
struct Shared;

impl std::io::Write for Shared {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        LINES.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Shared {
    type Writer = Shared;
    fn make_writer(&'a self) -> Shared {
        Shared
    }
}

/// Captures the events of every thread at every level from now on.
pub(crate) fn capture() -> Captured {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(Shared)
            .finish();
        tracing::subscriber::set_global_default(subscriber).unwrap();
    });
    Captured {
        from: LINES.lock().unwrap().len(),
    }
}
