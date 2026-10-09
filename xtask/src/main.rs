//! Build tooling for the core, run as `cargo xtask <command>` (alias in `.cargo/config.toml`).
//!
//! - `dist`: build the release binary for this host (Linux: against glibc `glibc::FLOOR`, with cargo-zigbuild) and
//!   package it, with its licence notices, as the relocatable archive
//!   `native/sidevoice-core-<target>.tar.zst`; then `verify` it.
//! - `verify ARCHIVE`: unpack it somewhere else, check its inventory and (Linux) that nothing in it needs a glibc
//!   newer than the floor the inventory records, and start the core from there.
//! - `verify-floor ARCHIVE` (Linux, needs Docker and cargo-zigbuild): `verify`, starting the core in a container of
//!   the oldest distribution it supports, whose glibc is the floor (`glibc::FLOOR_IMAGE`); it runs `verify-tree ROOT`
//!   there, the start alone of an unpacked tree.
//! - `manifest DIR [--tag vX.Y.Z]`: check every target's archive in DIR and write `native-core-manifest.json` and
//!   `SHA256SUMS`; with a tag, the crate version must be that release.
//! - `publish DIR TAG`: attach every file in DIR to the release TAG (for `nightly`, move the tag here first and drop
//!   older assets), download them back, check them against `SHA256SUMS` and the attestation, and publish.
//! - `compat`: run the cross-repository contract tests against the latest published release of the connector and of
//!   the web client; `compat-report` opens or updates the `compat` issue after a failed run in Actions.

mod archive;
mod compat;
mod dist;
mod glibc;
mod libraries;
mod manifest;
mod notices;
mod publish;
mod util;
mod verify;

use std::env;
use std::path::Path;

use compat::{compat, report};
use dist::dist;
use manifest::manifest;
use publish::publish;
use verify::{verify, verify_tree};

pub(crate) type Result<T> = std::result::Result<T, String>;

pub(crate) const TARGETS: [&str; 3] = ["linux-aarch64", "linux-x86_64", "macos-aarch64"];
pub(crate) const ROOT_NAME: &str = "sidevoice-core-rust";
pub(crate) const ENTRYPOINT: &str = "bin/sidevoice-core-rust";
/// The archive's layout: `bin/` and `notices/` (v1 also carried the voice pipeline's models, `checks/` and `lib/`).
pub(crate) const KIND: &str = "rust-native-v2";

const USAGE: &str =
    "usage: cargo xtask dist | verify ARCHIVE | verify-floor ARCHIVE | manifest DIR [--tag vX.Y.Z] \
     | publish DIR TAG | compat | compat-report";

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["dist"] => dist(),
        ["verify", archive] => verify(Path::new(archive), None),
        ["verify-floor", archive] => verify(Path::new(archive), Some(glibc::FLOOR_IMAGE)),
        ["verify-tree", root] => verify_tree(Path::new(root)),
        ["manifest", dir] => manifest(Path::new(dir), None),
        ["manifest", dir, "--tag", tag] => manifest(Path::new(dir), Some(tag)),
        ["publish", dir, tag] => publish(Path::new(dir), tag),
        ["compat"] => compat(),
        ["compat-report"] => report(),
        _ => Err(USAGE.into()),
    };
    if let Err(error) = result {
        eprintln!("xtask: {error}");
        std::process::exit(1);
    }
}
