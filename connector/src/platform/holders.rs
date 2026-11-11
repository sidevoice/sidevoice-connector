//! Who has the installed files open: the processes that may stop them being deleted or replaced.
//!
//! Windows cannot delete or replace a program that is running, and an agent app or editor keeps a
//! `sidevoice-connector mcp` server running from the release for as long as a conversation is open. When removing
//! the installed files fails, the caller asks this module which processes hold them so that the error can name them.
//! This module only observes: what to say, and whether to say it, is the caller's.
//!
//! Windows asks the Restart Manager (`RmGetList`), the OS's own account of which applications use a file; it refuses
//! directories, so a lock on a directory alone is not seen. Unix can
//! unlink a running program, so nothing there holds an installed file in a way that matters: it reports no holders,
//! but it still ends the servers an agent app left running from a release.

use std::path::Path;

#[cfg(unix)]
#[path = "holders/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "holders/windows.rs"]
mod imp;
pub use imp::Os as Holders;

/// A process that has a file open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    pub pid: u32,
    /// What the OS calls the process: its application name, else its program.
    pub name: String,
}

/// The processes holding installed files.
pub trait Holding {
    /// The processes that have a file open that is `root` or under it, in no particular order and without repeats. Only
    /// files are asked about: Windows cannot say who holds a directory, so a lock on a directory alone is not seen.
    /// Empty when there are none, when `root` is not there, and when the OS cannot tell: an observation that fails
    /// is no holder.
    fn of(root: &Path) -> Vec<Holder>;

    /// Kills every process that has a file open under one of `roots` and whose program is a file inside `program_root`
    /// (its full path, links resolved: the name a process gives itself proves nothing). The process is opened once;
    /// its start time (as the OS reported it holding the file) and its program are checked on that same handle, and it
    /// is ended through it, so a pid reused meanwhile is never touched. Never this process. Returns the pids ended.
    /// Unix has no holders to ask, so it looks at every process's program instead (`sysinfo`): the same test, a program
    /// inside `program_root`; `roots` plays no part there.
    fn stop_running_from(roots: &[&Path], program_root: &Path) -> Vec<u32>;

    /// The processes listening on TCP `port` here, as far as this user can see them, without repeats: what to name
    /// when a program finds its port taken. Empty when none is seen and when the OS cannot tell; looking is bounded in
    /// time. Unix asks `lsof` (macOS has it; a Linux without it sees nothing). Windows does not look.
    fn listening(port: u16) -> Vec<Holder>;
}

/// `lsof -F pc` read: one holder per `p<pid>` line, named by the `c<command>` line after it, control characters
/// dropped.
#[cfg(any(unix, test))]
fn lsof_listeners(output: &str) -> Vec<Holder> {
    let mut found: Vec<Holder> = Vec::new();
    for line in output.lines() {
        if let Some(pid) = line.strip_prefix('p') {
            if let Ok(pid) = pid.trim().parse::<u32>() {
                if !found.iter().any(|holder| holder.pid == pid) {
                    found.push(Holder {
                        pid,
                        name: String::new(),
                    });
                }
            }
        } else if let (Some(name), Some(last)) = (line.strip_prefix('c'), found.last_mut()) {
            if last.name.is_empty() {
                last.name = name
                    .chars()
                    .filter(|ch| !ch.is_control())
                    .take(80)
                    .collect();
            }
        }
    }
    found
}

/// Whether the program at `image` is a file inside `root`, both as the file system resolves them. A program whose file
/// is gone (Linux shows `/path/prog (deleted)` for one whose release was pruned while it ran) is judged by the path
/// it was started from, against the resolved `root`: it is still a program of that installation.
fn runs_from(image: &Path, root: &Path) -> bool {
    let Ok(root) = std::fs::canonicalize(root) else {
        return false;
    };
    let root = dunce::simplified(&root);
    match std::fs::canonicalize(image) {
        Ok(image) => dunce::simplified(&image).starts_with(root),
        Err(_) => image
            .to_str()
            .and_then(|text| text.strip_suffix(" (deleted)"))
            .is_some_and(|text| Path::new(text).starts_with(root)),
    }
}

/// Whether `root` may be walked: neither it nor any directory above it is a link or junction, as `is_link` says. A
/// path the remover refused for that reason must never be enumerated, or its target would be searched for holders.
/// Not being able to tell is a link.
#[cfg(any(windows, test))]
fn walkable(root: &Path, is_link: impl Fn(&Path) -> Option<bool>) -> bool {
    root.ancestors().all(|path| is_link(path) == Some(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn a_program_belongs_to_an_installation_by_its_path_not_its_name() {
        let scratch = crate::test_support::temp_dir("holders-runs-from");
        let root = scratch.path().join("install");
        let other = scratch.path().join("install2");
        for dir in [&root, &other] {
            std::fs::create_dir_all(dir.join("bin")).unwrap();
            std::fs::write(dir.join("bin/sidevoice-connector"), "").unwrap();
        }
        assert!(runs_from(&root.join("bin/sidevoice-connector"), &root));
        // The same name from another installation, or from a folder that only shares the prefix, is not ours.
        assert!(!runs_from(&other.join("bin/sidevoice-connector"), &root));
        assert!(!runs_from(&root.join("missing"), &root));
        // A program whose file was pruned while it ran, as Linux names it.
        let gone = format!(
            "{}/bin/gone (deleted)",
            std::fs::canonicalize(&root).unwrap().display()
        );
        assert!(runs_from(Path::new(&gone), &root));
        assert!(!runs_from(Path::new(&gone), &other));
    }

    #[test]
    fn lsof_answers_read_as_the_listening_processes() {
        let output = "p4242\ncsidevoice-core\nf7\np99\ncnode\x07\np4242\ncagain\n";
        assert_eq!(
            lsof_listeners(output),
            vec![
                Holder {
                    pid: 4242,
                    name: "sidevoice-core".into()
                },
                Holder {
                    pid: 99,
                    name: "node".into()
                },
            ]
        );
        assert!(lsof_listeners("").is_empty());
        assert!(lsof_listeners("pnot-a-pid\ncx\n").is_empty());
    }

    /// A port this test listens on is seen held by this process, where the OS can tell (macOS always: `lsof` is part of
    /// it; Linux when `lsof` is installed).
    #[cfg(unix)]
    #[test]
    fn a_listening_port_is_held_by_its_process() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let held = Holders::listening(port);
        if held.is_empty() && cfg!(not(target_os = "macos")) {
            eprintln!("skipped: no lsof here");
            return;
        }
        assert!(
            held.iter().any(|holder| holder.pid == std::process::id()),
            "{held:?}"
        );
    }

    /// A program running from inside the releases is ended, whatever its name, and one outside them is left alone.
    #[cfg(unix)]
    #[test]
    fn a_server_running_from_a_release_is_stopped_and_another_program_is_not() {
        use std::os::unix::process::CommandExt;
        let scratch = crate::test_support::temp_dir("holders-stop-unix");
        let releases = scratch.path().join("releases");
        let outside = scratch.path().join("elsewhere");
        for dir in [&releases, &outside] {
            std::fs::create_dir_all(dir.join("1.0.0/bin")).unwrap();
        }
        let sleep = ["/bin/sleep", "/usr/bin/sleep"]
            .into_iter()
            .find(|path| Path::new(path).is_file())
            .expect("a sleep program");
        let program = |dir: &Path| {
            let copy = dir.join("1.0.0/bin/sidevoice-connector");
            std::fs::copy(sleep, &copy).unwrap();
            std::process::Command::new(&copy)
                .arg0("mcp")
                .arg("30")
                .spawn()
                .unwrap()
        };
        let (mut ours, mut theirs) = (program(&releases), program(&outside));
        let ended = Holders::stop_running_from(&[releases.as_path()], &releases);
        assert_eq!(ended, vec![ours.id()]);
        assert!(ours.wait().is_ok_and(|status| !status.success()));
        assert!(theirs.try_wait().unwrap().is_none(), "left running");
        theirs.kill().unwrap();
        let _ = theirs.wait();
    }

    /// What the child below runs: several threads, asleep.
    #[cfg(unix)]
    #[test]
    #[ignore = "a child of the test below, not a test"]
    fn threaded_sleeper() {
        let threads: Vec<_> = (0..3)
            .map(|_| std::thread::spawn(|| std::thread::sleep(std::time::Duration::from_secs(30))))
            .collect();
        for thread in threads {
            let _ = thread.join();
        }
    }

    /// A server with several threads is ended once, as the process: no thread is signalled by itself (Linux lists
    /// threads as processes of the same program), and the caller's own threads are never touched.
    #[cfg(unix)]
    #[test]
    fn a_multi_threaded_server_is_stopped_as_one_process_and_this_one_survives() {
        let scratch = crate::test_support::temp_dir("holders-stop-threads");
        let releases = scratch.path().join("releases");
        std::fs::create_dir_all(releases.join("1.0.0/bin")).unwrap();
        let copy = releases.join("1.0.0/bin/sidevoice-connector");
        let exe = std::env::current_exe().unwrap();
        if std::fs::hard_link(&exe, &copy).is_err() {
            std::fs::copy(&exe, &copy).unwrap();
        }
        // This process has threads of its own, running the same program as the child (from another path).
        let keep: Vec<_> = (0..2)
            .map(|_| {
                std::thread::spawn(|| std::thread::sleep(std::time::Duration::from_millis(500)))
            })
            .collect();
        let mut child = std::process::Command::new(&copy)
            .args([
                "--ignored",
                "--exact",
                "platform::holders::tests::threaded_sleeper",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        // Let its threads start.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let ended = Holders::stop_running_from(&[releases.as_path()], &releases);
        assert_eq!(ended, vec![child.id()]);
        assert!(child.wait().is_ok_and(|status| !status.success()));
        for thread in keep {
            thread.join().unwrap();
        }
    }

    #[test]
    fn a_linked_root_or_a_linked_ancestor_is_never_walked() {
        let linked = PathBuf::from("/data/link");
        let looks = |path: &Path| Some(path == linked);
        assert!(walkable(Path::new("/data/real/install"), looks));
        assert!(!walkable(&linked, looks));
        assert!(!walkable(&linked.join("deeper/install"), looks));
        assert!(!walkable(Path::new("/data/unknown"), |_| None));
    }
}
