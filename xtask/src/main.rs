//! Build tooling for the core, run as `cargo xtask <command>` (alias in `.cargo/config.toml`).
//!
//! - `models [DIR]`: stage the detector models pinned in `assets/rust-models.json` (default `$RUSTVANI_CACHE_DIR`).
//! - `dist`: build the release binary for this host and package it, with its models, native libraries and licence
//!   notices, as the relocatable archive `native/sidevoice-core-rust-<commit>-<target>.tar.zst`; then `verify` it.
//! - `verify ARCHIVE`: unpack it somewhere else, check its inventory, and start the core from there.
//! - `manifest DIR [--tag vX.Y.Z]`: check every target's archive in DIR and write `native-core-manifest.json` and
//!   `SHA256SUMS`; with a tag, the crate version must be that release.
//! - `publish DIR TAG`: attach every file in DIR to the release TAG (for `nightly`, move the tag here first and drop
//!   older assets), download them back, check them against `SHA256SUMS` and the attestation, and publish.

mod archive;
mod dist;
mod libraries;
mod manifest;
mod models;
mod notices;
mod publish;
mod util;
mod verify;

use std::env;
use std::path::Path;

use dist::dist;
use manifest::manifest;
use models::models;
use publish::publish;
use util::cache_dir;
use verify::verify;

pub(crate) type Result<T> = std::result::Result<T, String>;

pub(crate) const TARGETS: [&str; 3] = ["linux-aarch64", "linux-x86_64", "macos-aarch64"];
pub(crate) const ROOT_NAME: &str = "sidevoice-core-rust";
pub(crate) const ENTRYPOINT: &str = "bin/sidevoice-core-rust";
pub(crate) const KIND: &str = "rust-native-v1";

const USAGE: &str =
    "usage: cargo xtask models [DIR] | dist | verify ARCHIVE | manifest DIR [--tag vX.Y.Z] | publish DIR TAG";

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["models"] => cache_dir().and_then(|dir| models(&dir)),
        ["models", dir] => models(Path::new(dir)),
        ["dist"] => dist(),
        ["verify", archive] => verify(Path::new(archive)),
        ["manifest", dir] => manifest(Path::new(dir), None),
        ["manifest", dir, "--tag", tag] => manifest(Path::new(dir), Some(tag)),
        ["publish", dir, tag] => publish(Path::new(dir), tag),
        _ => Err(USAGE.into()),
    };
    if let Err(error) = result {
        eprintln!("xtask: {error}");
        std::process::exit(1);
    }
}
