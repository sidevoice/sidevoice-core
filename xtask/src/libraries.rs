//! Native libraries the binary links besides the system's: copied into `lib/` and the binary pointed there.

use std::path::{Path, PathBuf};

use crate::util::*;
use crate::Result;

/// Copies the shared libraries the binary needs besides the system's into `lib/` and points the binary there.
pub(crate) fn linux_libraries(binary: &Path, lib_dir: &Path) -> Result<String> {
    let binary_path = binary.to_str().ok_or("path")?;
    let links = output("ldd", &[binary_path], None)?;
    let needed: Vec<(String, PathBuf)> = links
        .lines()
        .filter_map(|line| {
            let (name, rest) = line.trim().split_once(" => ")?;
            Some((
                name.to_string(),
                PathBuf::from(rest.split_whitespace().next()?),
            ))
        })
        .collect();
    if !needed
        .iter()
        .any(|(name, path)| name == "libopus.so.0" && path.is_file())
    {
        return Err(format!("missing native dependency libopus.so.0: {links}"));
    }
    for (name, source) in &needed {
        if [
            "libopus.so",
            "libonnxruntime.so",
            "libssl.so",
            "libcrypto.so",
        ]
        .iter()
        .any(|prefix| name.starts_with(prefix))
        {
            let resolved = source
                .canonicalize()
                .map_err(|error| format!("unresolved {name}: {error}"))?;
            write(&lib_dir.join(name), &read(&resolved)?)?;
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
    Ok(links)
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
