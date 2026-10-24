//! The operating-system code Sidevoice's crates share: this user ([`user`]), where their data lives ([`data_dir`]), the private directories and files that
//! user keeps ([`fs`]), the local channel between this user's processes ([`ipc`]) and how one of them asks another to
//! leave ([`stop`]), and child execution ([`process`]). The core depends on it by path; the connector (and later other crates) by the git tag of a core
//! release. Only what two or more crates use lives here; what one crate alone needs stays in that crate's own
//! `platform` module, which re-exports these concerns so its callers keep one door to the OS.
//!
//! Each concern has one shape: `<concern>.rs` declares a `pub` trait, documented once, with the tests written
//! against it; one `pub` type per OS in `<concern>/{unix,windows}.rs` implements it, and `<concern>.rs` re-exports
//! the one compiled in. Callers import the trait and the type (`use sidevoice_platform::user::{User, UserId};`).
//! Data-only concerns ([`data_dir`], [`target`]) need no trait or per-OS files.

pub mod data_dir;
pub mod fs;
pub mod ipc;
pub mod process;
pub mod stop;
pub mod target;
pub mod user;
