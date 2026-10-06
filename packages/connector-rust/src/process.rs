//! Which process a pid is, before anything is done to it. A pid read from a file is only a hint: pids are reused, and
//! a stale lock or ready file can name somebody else's work. Nothing is signalled on a pid alone.
//!
//! A process is identified by its owner, its start time and its command line: on Linux from `/proc`, elsewhere from
//! `ps`. When those cannot be read the answer is "unknown", and unknown never authorises a signal. A zombie (dead,
//! not yet reaped) is no process: it will never do anything again.

#[cfg(not(target_os = "linux"))]
use std::process::{Command, Stdio};
use std::sync::OnceLock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub uid: u32,
    /// Opaque start time: comparable only with another reading on the same machine.
    pub start: String,
    pub command: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// No such process (or a zombie).
    Gone,
    /// There is a process, but who it is cannot be told.
    Unknown,
    Alive(Identity),
}

pub fn valid_pid(pid: u32) -> bool {
    pid > 1 && pid <= i32::MAX as u32
}

/// What `pid` is now.
pub fn lookup(pid: u32) -> Lookup {
    if !valid_pid(pid) {
        return Lookup::Gone;
    }
    if unsafe { libc::kill(pid as i32, 0) } != 0 {
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => return Lookup::Gone,
            Some(libc::EPERM) => {}
            _ => return Lookup::Unknown,
        }
    }
    read_identity(pid)
}

#[cfg(target_os = "linux")]
fn read_identity(pid: u32) -> Lookup {
    let read = |name: &str| std::fs::read(format!("/proc/{pid}/{name}"));
    let missing = |error: std::io::Error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Lookup::Gone
        } else {
            Lookup::Unknown
        }
    };
    let stat = match read("stat") {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => return missing(error),
    };
    // The command name (field 2) may hold spaces and parentheses: fields resume after the last ')'.
    let Some(close) = stat.rfind(')') else {
        return Lookup::Unknown;
    };
    let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    // fields[0] is field 3 (state); fields[19] is field 22 (start time in clock ticks since boot).
    match fields.first() {
        Some(&("Z" | "X" | "x")) => return Lookup::Gone,
        None => return Lookup::Unknown,
        _ => {}
    }
    let Some(start) = fields.get(19) else {
        return Lookup::Unknown;
    };
    let status = match read("status") {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => return missing(error),
    };
    let Some(uid) = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
    else {
        return Lookup::Unknown;
    };
    let command = match read("cmdline") {
        Ok(bytes) => bytes
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect::<Vec<_>>()
            .join(" "),
        Err(error) => return missing(error),
    };
    Lookup::Alive(Identity {
        uid,
        start: (*start).to_owned(),
        command,
    })
}

#[cfg(not(target_os = "linux"))]
fn read_identity(pid: u32) -> Lookup {
    let output = Command::new("/bin/ps")
        .args([
            "-o", "stat=", "-o", "uid=", "-o", "lstart=", "-o", "command=",
        ])
        .args(["-p", &pid.to_string()])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else {
        return Lookup::Unknown;
    };
    if output.status.code() == Some(1) && output.stdout.iter().all(u8::is_ascii_whitespace) {
        return Lookup::Gone;
    }
    if !output.status.success() {
        return Lookup::Unknown;
    }
    parse_ps(&String::from_utf8_lossy(&output.stdout))
}

/// One `ps -o stat= -o uid= -o lstart= -o command=` line: `S 501 Mon Oct  6 10:00:00 2026 /bin/sleep 30`.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn parse_ps(line: &str) -> Lookup {
    let mut words = line.split_whitespace();
    let (Some(state), Some(uid)) = (words.next(), words.next()) else {
        return Lookup::Unknown;
    };
    let Ok(uid) = uid.parse() else {
        return Lookup::Unknown;
    };
    let start: Vec<&str> = words.by_ref().take(5).collect();
    if start.len() != 5 {
        return Lookup::Unknown;
    }
    if state.contains('Z') {
        return Lookup::Gone;
    }
    Lookup::Alive(Identity {
        uid,
        start: start.join(" "),
        command: words.collect::<Vec<_>>().join(" "),
    })
}

/// This process, as a lock records it.
pub fn self_identity() -> Option<Identity> {
    static SELF: OnceLock<Option<Identity>> = OnceLock::new();
    SELF.get_or_init(|| match lookup(std::process::id()) {
        Lookup::Alive(identity) => Some(identity),
        _ => None,
    })
    .clone()
}

// Signalling a recorded pid (a core started on demand, a stale connector) goes through these.
/// What a pid was recorded as: its start time and, optionally, something its command line must contain.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, Default)]
pub struct Expected<'a> {
    pub start: Option<&'a str>,
    pub command_contains: Option<&'a str>,
}

/// Whether `pid` is still the process recorded: alive, this user's, the same start time and command when given.
/// False when it is gone, somebody else's, another process now, or cannot be told.
#[cfg_attr(not(test), allow(dead_code))]
pub fn is_process(pid: u32, expected: &Expected<'_>) -> bool {
    let Lookup::Alive(found) = lookup(pid) else {
        return false;
    };
    found.uid == crate::secure_fs::uid()
        && expected.start.is_none_or(|start| found.start == start)
        && expected
            .command_contains
            .is_none_or(|part| found.command.contains(part))
}

/// Signals `pid` only if it is still the process described; returns whether it was signalled.
#[cfg_attr(not(test), allow(dead_code))]
pub fn signal_verified(pid: u32, signal: i32, expected: &Expected<'_>) -> bool {
    is_process(pid, expected) && unsafe { libc::kill(pid as i32, signal) } == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn sleeper() -> std::process::Child {
        Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    fn this_process_is_identified_by_owner_start_and_command() {
        let me = self_identity().expect("own identity");
        assert_eq!(me.uid, crate::secure_fs::uid());
        assert!(!me.start.is_empty());
        assert!(is_process(
            std::process::id(),
            &Expected {
                start: Some(&me.start),
                command_contains: None
            }
        ));
        assert!(!is_process(
            std::process::id(),
            &Expected {
                start: Some("another start"),
                command_contains: None
            }
        ));
    }

    #[test]
    fn invalid_pids_are_never_a_process() {
        for pid in [0, 1, u32::MAX] {
            assert_eq!(lookup(pid), Lookup::Gone, "{pid}");
            assert!(!signal_verified(pid, 0, &Expected::default()));
        }
    }

    #[test]
    fn a_child_is_signalled_only_as_its_recorded_self_and_is_gone_once_dead() {
        let mut child = sleeper();
        let pid = child.id();
        let Lookup::Alive(found) = lookup(pid) else {
            panic!("the child is not alive");
        };
        assert!(found.command.contains("sleep"), "{found:?}");
        let wrong_command = Expected {
            start: Some(&found.start),
            command_contains: Some("not-this-program"),
        };
        assert!(!signal_verified(pid, libc::SIGTERM, &wrong_command));
        let wrong_start = Expected {
            start: Some("0"),
            command_contains: None,
        };
        assert!(!signal_verified(pid, libc::SIGTERM, &wrong_start));
        assert!(child.try_wait().unwrap().is_none(), "nothing was signalled");
        // Dead but not reaped yet: a zombie is no process.
        let exact = Expected {
            start: Some(&found.start),
            command_contains: Some("sleep"),
        };
        assert!(signal_verified(pid, libc::SIGKILL, &exact));
        let deadline = Instant::now() + Duration::from_secs(5);
        while lookup(pid) != Lookup::Gone {
            assert!(Instant::now() < deadline, "a killed child stayed alive");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!is_process(pid, &exact));
        child.wait().unwrap();
        assert_eq!(lookup(pid), Lookup::Gone);
    }

    #[test]
    fn ps_lines_parse_to_an_identity_and_zombies_are_gone() {
        assert_eq!(
            parse_ps("Ss   501 Mon Oct  6 10:00:00 2026 /bin/sleep 30\n"),
            Lookup::Alive(Identity {
                uid: 501,
                start: "Mon Oct 6 10:00:00 2026".into(),
                command: "/bin/sleep 30".into(),
            })
        );
        assert_eq!(
            parse_ps("Z+ 501 Mon Oct  6 10:00:00 2026 (sleep)"),
            Lookup::Gone
        );
        assert_eq!(parse_ps("garbage"), Lookup::Unknown);
    }
}
