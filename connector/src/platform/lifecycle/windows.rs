//! Windows: another process's request to leave (this process's stop event, `sidevoice_platform::stop`), which also
//! reaches a process without a console (a Task Scheduler task), and the console events a process can be sent,
//! through tokio.

// The raw calls the rest of the crate is not allowed to make (`clippy.toml`) are made here.
#![allow(clippy::disallowed_methods)]

use sidevoice_platform::stop::{StopRequest, StopRequests};
use std::future::Future;
use std::io;
use tokio::signal::windows::{ctrl_c, ctrl_close, ctrl_logoff, ctrl_shutdown};

/// This OS's lifecycle.
pub struct Os;

impl super::Lifecycle for Os {
    fn stop_requests() -> io::Result<impl Future<Output = ()> + Send> {
        let mut requests = StopRequests::listen()?;
        let mut interrupt = ctrl_c()?;
        let mut close = ctrl_close()?;
        let mut shutdown = ctrl_shutdown()?;
        let mut logoff = ctrl_logoff()?;
        Ok(async move {
            tokio::select! {
                _ = requests.received() => {}
                _ = interrupt.recv() => {}
                _ = close.recv() => {}
                _ = shutdown.recv() => {}
                _ = logoff.recv() => {}
            }
        })
    }
}
