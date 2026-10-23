//! The only place in this crate that touches the operating system: `cfg(target_os)`, `cfg(unix)`/`cfg(windows)`,
//! `libc`, `rustix`, `windows-sys`, `std::os::*` and Unix sockets live here and nowhere else. Everything outside
//! answers in the crate's own terms and stays the same on every host; what differs per OS is chosen inside a
//! submodule, one per concern. Clippy refuses the raw calls outside this module (`clippy.toml`).
//!
//! What the core shares with the connector, this user ([`user`]), where data lives ([`data_dir`]) and the local channel ([`ipc`]), is the
//! `sidevoice-platform` crate of this workspace (`platform/`), re-exported here so callers keep this one door:
//! `use crate::platform::user::{User, UserId};`. What only the core needs is its own submodule, in the same shape:
//! `<concern>.rs` declares a `pub` trait, documented once, with the tests written against it; one `pub` type per OS
//! in `<concern>/{unix,windows}.rs` implements it, and `<concern>.rs` re-exports the one compiled in.

pub use sidevoice_platform::{data_dir, ipc, user};
pub mod signal;
