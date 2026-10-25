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

/// Cancel only after the stand-in is actually running, then prove its owned child did not survive.
pub(crate) async fn assert_cancellation_stops_child<T>(
    task: tokio::task::JoinHandle<T>,
    pid_file: &std::path::Path,
) {
    use crate::platform::process::{ChildProcess, Processes};
    use std::time::Duration;
    use tokio::time::{sleep, Instant};
    let deadline = Instant::now() + Duration::from_secs(10);
    let pid = loop {
        if let Some(pid) = std::fs::read_to_string(pid_file)
            .ok()
            .and_then(|text| text.parse::<u32>().ok())
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "child never reported its pid");
        sleep(Duration::from_millis(20)).await;
    };
    task.abort();
    assert!(task.await.err().is_some_and(|error| error.is_cancelled()));
    let deadline = Instant::now() + Duration::from_secs(10);
    while Processes::identity(pid).is_some() {
        assert!(
            Instant::now() < deadline,
            "cancelled delivery left pid {pid} running"
        );
        sleep(Duration::from_millis(20)).await;
    }
}
