//! The oldest C library a Linux archive runs on.
//!
//! `dist` links the binary against [`FLOOR`] (`cargo zigbuild --target <triple>.<FLOOR>`) and records it in the
//! inventory; `verify` reads the GLIBC symbol versions the binary and every bundled library need and refuses one newer
//! than the inventory's floor; `verify-floor` starts the core in [`FLOOR_IMAGE`], a distribution whose C library is
//! exactly the floor.

use std::cmp::Ordering;
use std::env;

use crate::util::output;
use crate::Result;

/// The glibc release every Linux archive runs on, and every newer one. 2.28: Debian 10, Ubuntu 20.04 (2.31), RHEL
/// and AlmaLinux 8, Amazon Linux 2023 and every later release of each. What it rests on: Microsoft's ONNX Runtime
/// 1.22.0 build needs 2.27, the rest is compiled here against the floor.
pub(crate) const FLOOR: &str = "2.28";

/// A distribution whose C library is glibc [`FLOOR`]: AlmaLinux 8, pinned by its multi-architecture index.
pub(crate) const FLOOR_IMAGE: &str =
    "almalinux:8@sha256:8b469a3a78515e8a18ea8fc727e6a3679e1d0c5ba6f58d5b30be3d0d9b86cffe";

/// This machine's Rust target triple on Linux, the one `cargo zigbuild` builds for.
pub(crate) fn host_triple() -> Result<&'static str> {
    match env::consts::ARCH {
        "x86_64" => Ok("x86_64-unknown-linux-gnu"),
        "aarch64" => Ok("aarch64-unknown-linux-gnu"),
        arch => Err(format!("no Linux triple for {arch}")),
    }
}

/// "2.28" as (2, 28, 0), so versions compare by number, not by text ("2.9" is older than "2.28").
pub(crate) fn parse(version: &str) -> Option<Vec<u32>> {
    let parts: Option<Vec<u32>> = version.split('.').map(|part| part.parse().ok()).collect();
    parts.filter(|parts| (2..=3).contains(&parts.len()))
}

fn compare(left: &str, right: &str) -> Ordering {
    let pad = |version: &str| {
        let mut parts = parse(version).unwrap_or_default();
        parts.resize(3, 0);
        parts
    };
    pad(left).cmp(&pad(right))
}

/// The newest `GLIBC_x.y[.z]` version a `readelf --version-info` listing requires, if any.
fn newest_in(listing: &str) -> Option<String> {
    listing
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .filter_map(|word| word.strip_prefix("GLIBC_"))
        .filter(|version| parse(version).is_some())
        .max_by(|left, right| compare(left, right))
        .map(str::to_string)
}

/// The newest glibc version an ELF file needs a symbol of, from its version requirements (`readelf`).
pub(crate) fn needed(elf: &str) -> Result<String> {
    let listing = output("readelf", &["--version-info", "--wide", elf], None)?;
    newest_in(&listing)
        .ok_or_else(|| format!("{elf} needs no GLIBC symbol version: not a glibc binary"))
}

/// The newest of two versions.
pub(crate) fn newest(left: String, right: String) -> String {
    if compare(&left, &right) == Ordering::Less {
        right
    } else {
        left
    }
}

/// Refuses a file that needs a glibc newer than `floor`.
pub(crate) fn check(what: &str, needed: &str, floor: &str) -> Result<()> {
    if compare(needed, floor) == Ordering::Greater {
        return Err(format!(
            "{what} needs glibc {needed}, newer than the floor {floor}: it would not start on older systems"
        ));
    }
    Ok(())
}

/// The shared libraries an ELF file names (`DT_NEEDED`, from `readelf --dynamic`).
pub(crate) fn needed_libraries(elf: &str) -> Result<Vec<String>> {
    let listing = output("readelf", &["--dynamic", "--wide", elf], None)?;
    Ok(listing
        .lines()
        .filter(|line| line.contains("(NEEDED)"))
        .filter_map(|line| {
            let start = line.find('[')? + 1;
            let end = line[start..].find(']')? + start;
            Some(line[start..end].to_string())
        })
        .collect())
}
