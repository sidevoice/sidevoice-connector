#![allow(clippy::disallowed_methods)]

use super::{Ended, GRACE};
use crate::platform::process::Child;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use tokio::signal::unix::{signal, Signal, SignalKind};

pub struct Signals {
    term: Signal,
    int: Signal,
}

fn ended(status: ExitStatus) -> Ended {
    match (status.code(), status.signal()) {
        (Some(code), _) => Ended::Code(code),
        (None, Some(signal)) => Ended::Signal(signal),
        (None, None) => Ended::Code(1),
    }
}

impl Signals {
    pub fn install() -> io::Result<Signals> {
        Ok(Signals {
            term: signal(SignalKind::terminate())?,
            int: signal(SignalKind::interrupt())?,
        })
    }

    pub async fn supervise(&mut self, child: &mut Child) -> io::Result<Ended> {
        let (received, name) = tokio::select! {
            status = child.wait() => return Ok(ended(status?)),
            _ = self.term.recv() => (rustix::process::Signal::TERM, libc::SIGTERM),
            _ = self.int.recv() => (rustix::process::Signal::INT, libc::SIGINT),
        };
        // The child leads its own group: its pid is the group's.
        if let Some(group) = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process_group(group, received);
        }
        match tokio::time::timeout(GRACE, child.wait()).await {
            Ok(status) => Ok(ended(status?)),
            Err(_) => {
                child.kill_tree().await;
                // It had to be ended: the guard ends by the signal it was given.
                Ok(Ended::Signal(name))
            }
        }
    }
}

pub fn raise(signal: i32) {
    // SAFETY: `signal` and `raise` with a signal number and the default disposition: no pointer, no callback.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}
