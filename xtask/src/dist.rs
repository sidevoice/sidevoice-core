//! `cargo xtask dist`: build the release binary for this host and package it as the relocatable archive.
//!
//! On Linux the binary is linked against glibc [`glibc::FLOOR`] (`cargo zigbuild`, zig's glibc stubs), not against
//! the build machine's, and needs no library beyond the system's; the inventory records the floor and `verify` checks
//! nothing in the archive needs a newer glibc.

use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use serde_json::json;

use crate::glibc;
use crate::libraries::linux_libraries;
use crate::notices::stage_notices;
use crate::util::*;
use crate::verify::verify;
use crate::{Result, ENTRYPOINT, KIND, ROOT_NAME};

pub(crate) fn dist() -> Result<()> {
    let repo = repo();
    let target = host_target()?;
    let source_sha = git(&["rev-parse", "HEAD"])?;
    let epoch: u64 = git(&["show", "-s", "--format=%ct", "HEAD"])?
        .parse()
        .map_err(|_| "commit time")?;
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let linux = target.starts_with("linux-");
    let mut build = Command::new(&cargo);
    let built = if linux {
        let triple = glibc::host_triple()?;
            build
                .args([
                    "zigbuild",
                    "--locked",
                    "--release",
                    "--bin",
                    "sidevoice-core-rust",
                ])
                .args(["--target", &format!("{triple}.{}", glibc::FLOOR)]);
        repo.join("target").join(triple).join("release")
    } else {
        build.args([
            "build",
            "--locked",
            "--release",
            "--bin",
            "sidevoice-core-rust",
        ]);
        repo.join("target/release")
    };
    let status = build
        .current_dir(&repo)
        .status()
        .map_err(|error| format!("cargo build: {error}"))?;
    if !status.success() {
        return Err("cargo build failed".into());
    }

    let work = TempDir::new("sidevoice-dist")?;
    let stage = work.0.join(ROOT_NAME);
    for name in ["bin", "notices"] {
        mkdir(&stage.join(name))?;
    }
    let binary = stage.join(ENTRYPOINT);
    write(&binary, &read(&built.join("sidevoice-core-rust"))?)?;
    chmod(&binary, 0o755)?;
    stage_notices(&stage.join("notices"), target)?;
    let links = if linux {
        linux_libraries(&binary)?
    } else {
        output("otool", &["-L", binary.to_str().ok_or("path")?], None)?
    };

    let mut files = Vec::new();
    for (name, is_dir) in walk(&stage)? {
        if !is_dir {
            files.push(file_record(&stage.join(&name), &name)?);
        }
    }
    let count = files.len();
    let mut inventory = json!({"schema": 1, "kind": KIND, "target": target, "source_sha": source_sha,
                               "entrypoint": ENTRYPOINT, "files": files});
    if linux {
        inventory["glibc"] = glibc::FLOOR.into();
    }
    write(&stage.join("native-core.json"), &canonical(&inventory))?;

    let raw = work.0.join("bundle.tar");
    write_tar(&work.0, &raw, epoch)?;
    let archive = repo
        .join("native")
        .join(format!("sidevoice-core-{target}.tar.zst"));
    mkdir(archive.parent().expect("has a parent"))?;
    run(
        "zstd",
        &[
            "-q",
            "-f",
            "-19",
            raw.to_str().ok_or("path")?,
            "-o",
            archive.to_str().ok_or("path")?,
        ],
    )?;
    let bytes = read(&archive)?;
    println!(
        "{}",
        json!({"target": target, "name": archive.file_name().map(|name| name.to_string_lossy()),
               "size": bytes.len(), "sha256": sha256(&bytes), "files": count, "links_before_relocation": links})
    );
    verify(&archive, None)
}

/// A reproducible tar of `<work>/sidevoice-core-rust`: owner root, fixed time, 0755 for directories and the
/// entrypoint, 0644 for everything else.
pub(crate) fn write_tar(work: &Path, raw: &Path, epoch: u64) -> Result<()> {
    let file = fs::File::create(raw).map_err(|error| error.to_string())?;
    let mut builder = tar::Builder::new(file);
    let stage = work.join(ROOT_NAME);
    let mut entries = vec![(String::new(), true)];
    entries.extend(walk(&stage)?);
    for (relative, is_dir) in entries {
        let name = if relative.is_empty() {
            ROOT_NAME.to_string()
        } else {
            format!("{ROOT_NAME}/{relative}")
        };
        let mut header = tar::Header::new_ustar();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(epoch);
        if is_dir {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            builder.append_data(&mut header, format!("{name}/"), std::io::empty())
        } else {
            let bytes = read(&work.join(&name))?;
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(if relative == ENTRYPOINT { 0o755 } else { 0o644 });
            header.set_size(bytes.len() as u64);
            builder.append_data(&mut header, &name, bytes.as_slice())
        }
        .map_err(|error| format!("tar {name}: {error}"))?;
    }
    builder
        .into_inner()
        .and_then(|mut file| file.flush())
        .map_err(|error| error.to_string())
}
