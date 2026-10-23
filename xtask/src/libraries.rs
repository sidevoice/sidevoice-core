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

/// The Windows system DLLs a binary may import, by name (lower case). A fixed baseline, not whatever the build runner's
/// `System32` happens to hold: a newer runner image could carry a DLL that a supported older Windows does not. Every
/// one is part of the Windows 10 and later system itself and documented as a Win32 API library on
/// <https://learn.microsoft.com/en-us/windows/win32/api/> (the header's "Library" row names the DLL). A binary that
/// imports another DLL fails `dist` and `verify` on purpose: add it here once it is known to be on every supported
/// Windows. `pdh.dll` (Performance Data Helper, `pdh.h`) is there for `sysinfo`, which the core links.
pub(crate) const WINDOWS_SYSTEM: [&str; 25] = [
    "advapi32.dll",
    "bcrypt.dll",
    "bcryptprimitives.dll",
    "combase.dll",
    "crypt32.dll",
    "gdi32.dll",
    "iphlpapi.dll",
    "kernel32.dll",
    "kernelbase.dll",
    "mswsock.dll",
    "ncrypt.dll",
    "netapi32.dll",
    "ntdll.dll",
    "ole32.dll",
    "oleaut32.dll",
    "pdh.dll",
    "powrprof.dll",
    "psapi.dll",
    "rpcrt4.dll",
    "secur32.dll",
    "shell32.dll",
    "user32.dll",
    "userenv.dll",
    "winmm.dll",
    "ws2_32.dll",
];

/// Whether a Windows DLL is on every machine: one of [`WINDOWS_SYSTEM`], or an API set (`api-ms-win-*`, `ext-ms-win-*`:
/// virtual names the loader resolves on the machine itself,
/// <https://learn.microsoft.com/en-us/windows/win32/apiindex/windows-apisets>). Never the Visual C++ runtime, which
/// only some machines have (the binary links it statically, `+crt-static`, `.cargo/config.toml`).
pub(crate) fn is_windows_system_dll(library: &str) -> bool {
    let library = library.to_ascii_lowercase();
    library.starts_with("api-ms-win-")
        || library.starts_with("ext-ms-win-")
        || WINDOWS_SYSTEM.contains(&library.as_str())
}

/// The DLLs a PE file imports (its import table, delay-loaded ones included), lower case, sorted.
fn imported_dlls(binary: &Path) -> Result<Vec<String>> {
    let data = crate::util::read(binary)?;
    let file = object::read::pe::PeFile64::parse(data.as_slice())
        .map_err(|error| format!("{}: {error}", binary.display()))?;
    let mut names = Vec::new();
    for import in
        object::Object::imports(&file).map_err(|error| format!("{}: {error}", binary.display()))?
    {
        let import = import.map_err(|error| format!("{}: {error}", binary.display()))?;
        names.push(String::from_utf8_lossy(import.library()).to_ascii_lowercase());
    }
    names.sort();
    names.dedup();
    Ok(names)
}

/// The DLLs the Windows binary imports, all of them the system's; anything else is an error.
pub(crate) fn windows_libraries(binary: &Path) -> Result<String> {
    let imported = imported_dlls(binary)?;
    if let Some(name) = imported.iter().find(|name| !is_windows_system_dll(name)) {
        return Err(format!("unexpected native dependency {name}: {imported:?}"));
    }
    Ok(imported.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_windows_binary_may_load_api_sets_and_system_dlls_but_not_the_visual_c_runtime() {
        assert!(is_windows_system_dll("KERNEL32.dll"));
        assert!(is_windows_system_dll("api-ms-win-core-synch-l1-2-0.dll"));
        assert!(!is_windows_system_dll("vcruntime140.dll"));
        assert!(!is_windows_system_dll("MSVCP140.dll"));
        assert!(!is_windows_system_dll("libzstd.dll"));
    }
}
