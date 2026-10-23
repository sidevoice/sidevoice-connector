//! Unix: another process's request to leave (`SIGTERM`, `sidevoice_platform::stop`) and `SIGINT`, through tokio.

// The raw calls the rest of the crate is not allowed to make (`clippy.toml`) are made here.
#![allow(clippy::disallowed_methods)]

use sidevoice_platform::stop::{StopRequest, StopRequests};
use std::future::Future;
use std::io;
use tokio::signal::unix::{signal, SignalKind};

/// This OS's lifecycle.
pub struct Os;

impl super::Lifecycle for Os {
    fn stop_requests() -> io::Result<impl Future<Output = ()> + Send> {
        let mut requests = StopRequests::listen()?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        Ok(async move {
            tokio::select! {
                _ = requests.received() => {}
                _ = interrupt.recv() => {}
            }
        })
    }
}
