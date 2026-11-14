//! The connector as a library: its modules, shared by the two binaries (`sidevoice-connector`, `src/main.rs`; and
//! `sidevoice-testbench`, the test bench package) and by what tests need of its OS-facing layer. Only what a binary uses is
//! `pub`.

// Unit tests print for their own sake (stand-in programs, timings): that is not logging.
#![cfg_attr(test, allow(clippy::disallowed_macros))]

pub mod adapters;
pub mod agents;
pub mod core_package;
pub mod core_ready;
pub mod daemon;
mod frame;
pub mod identity;
pub mod install;
mod link;
mod link_error;
mod lock;
pub mod log_safe;
pub mod logfile;
pub mod mcp;
pub mod messages;
pub mod pairing;
pub mod platform;
pub mod private_dir;
pub mod profile;
pub mod service;
pub mod step;
#[cfg(test)]
mod test_support;
