//! The only place in this crate that touches the operating system: `cfg(target_os)`, `cfg(unix)`/`cfg(windows)`,
//! `libc` and `std::os::*` live here and nowhere else. Everything outside answers in the crate's own terms and stays
//! the same on every host; what differs per OS is chosen inside a submodule. Maintained crates come first, our own
//! `unsafe` last. Clippy refuses the raw calls outside this module (`clippy.toml`).

pub mod fs;
pub mod user;
