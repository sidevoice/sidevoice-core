//! The only place in this crate that touches the operating system: `cfg(target_os)`, `cfg(unix)`/`cfg(windows)`,
//! `libc`, `rustix`, `windows-sys`, `std::os::*` and Unix sockets live here and nowhere else. Everything outside
//! answers in the crate's own terms and stays the same on every host; what differs per OS is chosen inside a
//! submodule, one per concern. Clippy refuses the raw calls outside this module (`clippy.toml`).

pub mod ipc;
pub mod user;
