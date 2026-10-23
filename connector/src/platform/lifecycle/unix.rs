//! Unix: `SIGTERM` and `SIGINT`, through tokio.

// The raw calls the rest of the crate is not allowed to make (`clippy.toml`) are made here.
#![allow(clippy::disallowed_methods)]

use std::future::Future;
use std::io;
use tokio::signal::unix::{signal, SignalKind};

/// This OS's lifecycle.
pub struct Os;

impl super::Lifecycle for Os {
    fn stop_requests() -> io::Result<impl Future<Output = ()> + Send> {
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        Ok(async move {
            tokio::select! {
                _ = terminate.recv() => {}
                _ = interrupt.recv() => {}
            }
        })
    }
}
