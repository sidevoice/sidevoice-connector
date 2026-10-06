//! Processes as the service sees them: alive, what command line they run, how long they have run, and a signal
//! sent only to a process proven to be the one meant (pids are reused). Linux reads `/proc`; macOS asks `ps`.

use std::io;
#[cfg(not(target_os = "linux"))]
use std::process::{Command, Stdio};

pub fn alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    unsafe {
        libc::kill(pid, 0) == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(target_os = "linux")]
pub fn command_line(pid: u32) -> Option<String> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let words: Vec<String> = bytes
        .split(|byte| *byte == 0)
        .filter(|word| !word.is_empty())
        .map(|word| String::from_utf8_lossy(word).into_owned())
        .collect();
    (!words.is_empty()).then(|| words.join(" "))
}

#[cfg(not(target_os = "linux"))]
pub fn command_line(pid: u32) -> Option<String> {
    let output = ps(&["-ww", "-o", "command=", "-p", &pid.to_string()])?;
    let line = output.trim();
    (!line.is_empty()).then(|| line.to_owned())
}

#[cfg(not(target_os = "linux"))]
fn ps(args: &[&str]) -> Option<String> {
    let output = Command::new("/bin/ps")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Every process whose command line `matches`.
#[cfg(target_os = "linux")]
pub fn find(matches: impl Fn(&str) -> bool) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| *pid != std::process::id())
        .filter(|pid| command_line(*pid).is_some_and(|line| matches(&line)))
        .collect()
}

#[cfg(not(target_os = "linux"))]
pub fn find(matches: impl Fn(&str) -> bool) -> Vec<u32> {
    let Some(listing) = ps(&["-ww", "-axo", "pid=,command="]) else {
        return Vec::new();
    };
    listing
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (pid, command) = line.split_once(char::is_whitespace)?;
            Some((pid.parse::<u32>().ok()?, command.trim()))
        })
        .filter(|(pid, command)| *pid != std::process::id() && matches(command))
        .map(|(pid, _)| pid)
        .collect()
}

/// Seconds since `pid` started.
#[cfg(target_os = "linux")]
pub fn age(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is in parentheses and may hold spaces: the fields after it are counted from its end.
    let fields: Vec<&str> = stat
        .get(stat.rfind(')')? + 2..)?
        .split_whitespace()
        .collect();
    let started_ticks: u64 = fields.get(19)?.parse().ok()?;
    let uptime: f64 = std::fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks <= 0 {
        return None;
    }
    Some((uptime - started_ticks as f64 / ticks as f64).max(0.0) as u64)
}

#[cfg(not(target_os = "linux"))]
pub fn age(pid: u32) -> Option<u64> {
    parse_elapsed(&ps(&["-o", "etime=", "-p", &pid.to_string()])?)
}

/// `ps`'s `etime`: `[[dd-]hh:]mm:ss`.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub fn parse_elapsed(value: &str) -> Option<u64> {
    let value = value.trim();
    let (days, clock) = match value.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, value),
    };
    let parts = clock
        .split(':')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let seconds = match parts.as_slice() {
        [minutes, seconds] => minutes * 60 + seconds,
        [hours, minutes, seconds] => hours * 3600 + minutes * 60 + seconds,
        _ => return None,
    };
    Some(days * 86_400 + seconds)
}

/// Send `signal` to `pid` only while its command line still `matches`; whether it was sent.
pub fn signal_verified(pid: u32, signal: i32, matches: impl Fn(&str) -> bool) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    if raw <= 1 || !command_line(pid).is_some_and(|line| matches(&line)) {
        return false;
    }
    unsafe { libc::kill(raw, signal) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn elapsed_times_parse_in_every_ps_form() {
        assert_eq!(parse_elapsed("00:07"), Some(7));
        assert_eq!(parse_elapsed(" 01:02:03\n"), Some(3723));
        assert_eq!(parse_elapsed("2-00:00:01"), Some(172_801));
        assert_eq!(parse_elapsed("soon"), None);
    }

    #[test]
    fn this_process_is_found_alive_with_its_command_line() {
        let me = std::process::id();
        assert!(alive(me));
        assert!(command_line(me).is_some());
        assert!(age(me).is_some_and(|age| age < 3600));
        assert!(!alive(0));
    }

    #[test]
    fn a_signal_goes_only_to_the_process_it_was_meant_for() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        assert!(find(|line| line.starts_with("sleep 30")).contains(&pid));
        assert!(!signal_verified(pid, libc::SIGTERM, |line| line.contains("someone-else")));
        assert!(alive(pid));
        assert!(signal_verified(pid, libc::SIGTERM, |line| line.starts_with("sleep 30")));
        let _ = child.wait();
    }
}
