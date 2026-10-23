//! The only place in this crate that touches the operating system: `cfg(target_os)`, `cfg(unix)`/`cfg(windows)`,
//! `libc` and `std::os::*` live here and nowhere else. Everything outside answers in the crate's own terms and stays
//! the same on every host; what differs per OS is chosen inside a submodule. Maintained crates come first, our own
//! `unsafe` last. Clippy refuses the raw calls outside this module (`clippy.toml`).
//!
//! One concern per module, laid out the same way: `<concern>.rs` declares the whole interface, documented once, and
//! `<concern>/unix.rs` and `<concern>/windows.rs` only implement it, behind a private `imp`. A side that lacks an
//! operation does not compile; a host that has nothing to do implements it as a no-op, so the gap is a decision.

pub mod env;
pub mod fs;
pub mod process;
pub mod service;
pub mod uninstall;

// This user and the local channel, shared with the core: `sidevoice-platform`, from the core's repository at the tag
// of a core release. Re-exported so callers keep their paths (`crate::platform::ipc`, `crate::platform::user`).
pub use sidevoice_platform::{ipc, user};
