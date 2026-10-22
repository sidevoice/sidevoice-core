//! The only place in this crate that touches the operating system: `cfg(target_os)`, `cfg(unix)`/`cfg(windows)`,
//! `libc`, `rustix`, `windows-sys`, `std::os::*` and Unix sockets live here and nowhere else. Everything outside
//! answers in the crate's own terms and stays the same on every host; what differs per OS is chosen inside a
//! submodule, one per concern. Clippy refuses the raw calls outside this module (`clippy.toml`).
//!
//! Each concern has the same shape as the connector's `platform` (sidevoice-connector), so that a later shared crate
//! is a move: `<concern>.rs` declares a `pub` trait, documented once, with the tests written against it; one `pub`
//! type per OS in `<concern>/{unix,windows}.rs` implements it, and `<concern>.rs` re-exports the one compiled in.

pub mod ipc;
pub mod signal;
pub mod user;
