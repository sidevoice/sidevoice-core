//! Native libraries the binary links besides the system's: copied into `lib/` and the binary pointed there.
//!
//! On Linux that is Microsoft's ONNX Runtime release build alone ([`onnxruntime`]); libopus is compiled from the
//! source audiopus_sys bundles and linked statically, so it follows the glibc floor like the rest (`crate::glibc`).

use std::path::{Path, PathBuf};

use crate::util::*;
use crate::Result;

/// The libraries every glibc system has: a Linux binary or bundled library may name these and what is in `lib/`.
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

/// Microsoft's ONNX Runtime 1.22.0 release build per Linux target (the version ort-sys 2.0.0-rc.10 binds), with its
/// archive digest. Built on manylinux 2_28, it needs glibc 2.27 and GLIBCXX 3.4.22; the static library ort-sys
/// downloads by itself needs glibc 2.32 and the build machine's libstdc++, so the Linux binary links this one.
pub(crate) const ONNXRUNTIME_LINUX: [(&str, &str, &str); 2] = [
    (
        "linux-x86_64",
        "https://github.com/microsoft/onnxruntime/releases/download/v1.22.0/onnxruntime-linux-x64-1.22.0.tgz",
        "8344d55f93d5bc5021ce342db50f62079daf39aaafb5d311a451846228be49b3",
    ),
    (
        "linux-aarch64",
        "https://github.com/microsoft/onnxruntime/releases/download/v1.22.0/onnxruntime-linux-aarch64-1.22.0.tgz",
        "bb76395092d150b52c7092dc6b8f2fe4d80f0f3bf0416d2f269193e347e24702",
    ),
];

/// The library the binary loads ONNX Runtime from, by its soname.
const ONNXRUNTIME_SONAME: &str = "libonnxruntime.so.1";

/// The pinned ONNX Runtime release for a Linux target, unpacked once under `target/onnxruntime/<digest>` (with
/// `lib/`, `LICENSE` and `ThirdPartyNotices.txt`): its directory, URL and digest.
pub(crate) fn onnxruntime(target: &str) -> Result<(PathBuf, &'static str, &'static str)> {
    let (_, url, digest) = ONNXRUNTIME_LINUX
        .iter()
        .find(|(name, _, _)| *name == target)
        .ok_or_else(|| format!("no ONNX Runtime build pinned for {target}"))?;
    let dir = repo().join("target/onnxruntime").join(digest);
    let complete = dir.join(".complete");
    if !complete.exists() {
        let bytes = download(url)?;
        if sha256(&bytes) != *digest {
            return Err(format!("pinned ONNX Runtime digest changed: {url}"));
        }
        let _ = std::fs::remove_dir_all(&dir);
        mkdir(&dir)?;
        let archive = dir.join("onnxruntime.tgz");
        write(&archive, &bytes)?;
        let (archive_path, dir_path) =
            (archive.to_str().ok_or("path")?, dir.to_str().ok_or("path")?);
        run(
            "tar",
            &["-xzf", archive_path, "-C", dir_path, "--strip-components=1"],
        )?;
        write(&complete, b"")?;
    }
    Ok((dir, url, digest))
}

/// Copies the ONNX Runtime library the binary loads into `lib/` and points the binary there; anything else it names
/// besides the system's is an error.
pub(crate) fn linux_libraries(binary: &Path, lib_dir: &Path, onnxruntime: &Path) -> Result<String> {
    let binary_path = binary.to_str().ok_or("path")?;
    let needed = crate::glibc::needed_libraries(binary_path)?;
    if !needed.iter().any(|name| name == ONNXRUNTIME_SONAME) {
        return Err(format!(
            "the binary does not load {ONNXRUNTIME_SONAME}: {needed:?}"
        ));
    }
    for name in &needed {
        if name == ONNXRUNTIME_SONAME {
            let source = onnxruntime.join("lib").join(name);
            let resolved = source
                .canonicalize()
                .map_err(|error| format!("{}: {error}", source.display()))?;
            write(&lib_dir.join(name), &read(&resolved)?)?;
        } else if !LINUX_SYSTEM.contains(&name.as_str()) {
            return Err(format!("unexpected native dependency {name}: {needed:?}"));
        }
    }
    run("patchelf", &["--set-rpath", "$ORIGIN/../lib", binary_path])?;
    for (name, _) in walk(lib_dir)? {
        run(
            "patchelf",
            &[
                "--set-rpath",
                "$ORIGIN",
                lib_dir.join(name).to_str().ok_or("path")?,
            ],
        )?;
    }
    Ok(needed.join(" "))
}

/// The same on macOS: copy libopus / libonnxruntime next to the binary, rewrite their install names, re-sign.
pub(crate) fn mac_libraries(binary: &Path, lib_dir: &Path) -> Result<String> {
    let binary_path = binary.to_str().ok_or("path")?;
    let links = output("otool", &["-L", binary_path], None)?;
    let libraries: Vec<String> = links
        .lines()
        .skip(1)
        .map(|line| line.trim().split(" (").next().unwrap_or("").to_string())
        .collect();
    if libraries
        .iter()
        .filter(|name| name.contains("libopus"))
        .count()
        > 1
    {
        return Err(format!("unexpected multiple Opus dependencies: {links}"));
    }
    for original in &libraries {
        let name = if original.contains("libopus") {
            "libopus.0.dylib"
        } else if original.contains("libonnxruntime") {
            "libonnxruntime.dylib"
        } else {
            continue;
        };
        let mut source = PathBuf::from(original);
        if !source.exists() {
            source = binary.with_file_name(source.file_name().ok_or("library name")?);
            if !source.exists() {
                return Err(format!("missing native library {original}"));
            }
        }
        let destination = lib_dir.join(name);
        write(
            &destination,
            &read(&source.canonicalize().map_err(|error| error.to_string())?)?,
        )?;
        let destination_path = destination.to_str().ok_or("path")?;
        run(
            "install_name_tool",
            &[
                "-change",
                original,
                &format!("@executable_path/../lib/{name}"),
                binary_path,
            ],
        )?;
        run(
            "install_name_tool",
            &["-id", &format!("@loader_path/{name}"), destination_path],
        )?;
        run("codesign", &["--force", "--sign", "-", destination_path])?;
    }
    run("codesign", &["--force", "--sign", "-", binary_path])?;
    Ok(links)
}
