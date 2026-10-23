//! Windows: the console events a process can be sent, through tokio. A process with no console (a Task Scheduler
//! task, the core) gets none of them; it is asked in band (see the module above).

// The raw calls the rest of the crate is not allowed to make (`clippy.toml`) are made here.
#![allow(clippy::disallowed_methods)]

use std::future::Future;
use std::io;
use tokio::signal::windows::{ctrl_c, ctrl_close, ctrl_logoff, ctrl_shutdown};

/// This OS's lifecycle.
pub struct Os;

impl super::Lifecycle for Os {
    fn stop_requests() -> io::Result<impl Future<Output = ()> + Send> {
        let mut interrupt = ctrl_c()?;
        let mut close = ctrl_close()?;
        let mut shutdown = ctrl_shutdown()?;
        let mut logoff = ctrl_logoff()?;
        Ok(async move {
            tokio::select! {
                _ = interrupt.recv() => {}
                _ = close.recv() => {}
                _ = shutdown.recv() => {}
                _ = logoff.recv() => {}
            }
        })
    }
}
