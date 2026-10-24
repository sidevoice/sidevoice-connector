//! `sidevoice-background`: Windows runs the service jobs through it (`service::background`). A GUI program, so Task
//! Scheduler opens no console window for it. On other systems the program is a stub that is never released or run.
#![cfg_attr(windows, windows_subsystem = "windows")]

use sidevoice_connector::service::background;

#[tokio::main]
async fn main() {
    let code = match background::connector() {
        Ok(connector) => background::run(&connector, std::env::args_os().skip(1).collect())
            .await
            .unwrap_or(1),
        Err(_) => 1,
    };
    std::process::exit(code);
}
