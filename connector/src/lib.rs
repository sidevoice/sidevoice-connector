//! The connector as a library: its modules, shared by the two binaries (`sidevoice-connector`, `src/main.rs`; and
//! `sidevoice-bench`, `src/bench/main.rs`) and by what tests need of its OS-facing layer. Only what a binary uses is
//! `pub`.

pub mod adapters;
pub mod agents;
pub mod core_package;
pub mod core_ready;
pub mod daemon;
pub mod data_dir;
mod frame;
pub mod identity;
pub mod install;
mod link;
mod lock;
pub mod logfile;
pub mod mcp;
pub mod messages;
pub mod pairing;
pub mod platform;
pub mod profile;
pub mod service;
#[cfg(test)]
mod test_support;
