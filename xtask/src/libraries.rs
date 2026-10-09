//! The native libraries the binary may link: on Linux, only those every glibc system has.

use std::path::Path;

use crate::Result;

/// The libraries every glibc system has: a Linux binary may name these and nothing else.
pub(crate) const LINUX_SYSTEM: [&str; 9] = [
    "libc.so.6",
    "libm.so.6",
    "libdl.so.2",
    "libpthread.so.0",
    "librt.so.1",
    "libgcc_s.so.1",
    "libstdc++.so.6",
    "ld-linux-x86-64.so.2",
    "ld-linux-aarch64.so.1",
];

/// The libraries the Linux binary names, all of them the system's; anything else is an error.
pub(crate) fn linux_libraries(binary: &Path) -> Result<String> {
    let needed = crate::glibc::needed_libraries(binary.to_str().ok_or("path")?)?;
    if let Some(name) = needed
        .iter()
        .find(|name| !LINUX_SYSTEM.contains(&name.as_str()))
    {
        return Err(format!("unexpected native dependency {name}: {needed:?}"));
    }
    Ok(needed.join(" "))
}
