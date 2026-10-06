//! Unpacking an archive with every bound and identity check: shared by `verify` and `manifest`.

use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::glibc;
use crate::util::*;
use crate::{Result, ENTRYPOINT, KIND, ROOT_NAME, TARGETS};

/// Unpacks an archive into `destination` with every bound and identity check, and returns its root and inventory.
pub(crate) fn unpack_checked(archive: &Path, destination: &Path) -> Result<(PathBuf, Value)> {
    if read(archive)?.len() > 250_000_000 {
        return Err("compressed archive exceeds bound".into());
    }
    let raw = destination.join("bundle.tar");
    run(
        "zstd",
        &[
            "-q",
            "-d",
            "-f",
            archive.to_str().ok_or("path")?,
            "-o",
            raw.to_str().ok_or("path")?,
        ],
    )?;
    if fs::metadata(&raw).map_err(|error| error.to_string())?.len() > 1_000_000_000 {
        return Err("decompressed archive exceeds bound".into());
    }
    let mut seen = BTreeSet::new();
    let mut directories = BTreeSet::new();
    let mut total: u64 = 0;
    let mut tar = tar::Archive::new(fs::File::open(&raw).map_err(|error| error.to_string())?);
    for entry in tar.entries().map_err(|error| error.to_string())? {
        let mut entry = entry.map_err(|error| error.to_string())?;
        let name = entry
            .path()
            .map_err(|error| error.to_string())?
            .to_string_lossy()
            .trim_end_matches('/')
            .to_string();
        if name.len() > 240 || seen.len() >= 2_000 {
            return Err("archive path or entry count exceeds bound".into());
        }
        let kind = entry.header().entry_type();
        if name == ROOT_NAME {
            if !kind.is_dir() {
                return Err("archive root must be a directory".into());
            }
        } else {
            validate_name(&name)?;
        }
        if !seen.insert(name.clone()) || !(kind.is_dir() || kind.is_file()) {
            return Err(format!("duplicate or forbidden tar entry: {name}"));
        }
        let path = destination.join(&name);
        if kind.is_dir() {
            directories.insert(name);
            mkdir(&path)?;
        } else {
            let size = entry.header().size().map_err(|error| error.to_string())?;
            total += size;
            if size > 500_000_000 || total > 1_000_000_000 {
                return Err("archive size exceeds bound".into());
            }
            mkdir(path.parent().expect("below root"))?;
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .map_err(|error| error.to_string())?;
            write(&path, &bytes)?;
            chmod(
                &path,
                entry.header().mode().map_err(|error| error.to_string())? & 0o777,
            )?;
        }
    }
    fs::remove_file(&raw).map_err(|error| error.to_string())?;

    let root = destination.join(ROOT_NAME);
    let bytes = read(&root.join("native-core.json"))?;
    let inventory = parse_json(&bytes, "native-core.json")?;
    if canonical(&inventory) != bytes {
        return Err("native-core.json is not canonical".into());
    }
    let fields: BTreeSet<&str> = inventory
        .as_object()
        .ok_or("inventory")?
        .keys()
        .map(String::as_str)
        .collect();
    let target = inventory["target"].as_str().unwrap_or("");
    let mut expected = BTreeSet::from([
        "schema",
        "kind",
        "target",
        "source_sha",
        "entrypoint",
        "files",
    ]);
    // A Linux archive's glibc floor: the oldest C library it runs on (crate::glibc).
    if target.starts_with("linux-") {
        expected.insert("glibc");
        if glibc::parse(inventory["glibc"].as_str().unwrap_or("")).is_none() {
            return Err("wrong glibc floor".into());
        }
    }
    if fields != expected {
        return Err("wrong inventory fields".into());
    }
    if inventory["schema"] != 1
        || inventory["kind"] != KIND
        || inventory["entrypoint"] != ENTRYPOINT
    {
        return Err("wrong native kind or entrypoint".into());
    }
    if !TARGETS.contains(&target) || !is_commit(inventory["source_sha"].as_str().unwrap_or("")) {
        return Err("wrong target or source commit".into());
    }
    let records = inventory["files"].as_array().ok_or("inventory files")?;
    let names: Vec<&str> = records
        .iter()
        .map(|record| record["name"].as_str().unwrap_or(""))
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    if names != sorted {
        return Err("unsorted or duplicated inventory".into());
    }
    let mut expected_files: BTreeSet<String> = names.iter().map(|name| name.to_string()).collect();
    expected_files.insert("native-core.json".into());
    let mut actual_files = BTreeSet::new();
    let mut expected_directories = BTreeSet::from([ROOT_NAME.to_string()]);
    for (name, is_dir) in walk(&root)? {
        if !is_dir {
            let parts: Vec<&str> = name.split('/').collect();
            for index in 1..parts.len() {
                expected_directories.insert(format!("{ROOT_NAME}/{}", parts[..index].join("/")));
            }
            actual_files.insert(name);
        }
    }
    if actual_files != expected_files {
        return Err(format!(
            "unexpected or missing files: {:?}",
            actual_files.symmetric_difference(&expected_files)
        ));
    }
    if directories != expected_directories {
        return Err(format!(
            "unexpected or missing directories: {:?}",
            directories.symmetric_difference(&expected_directories)
        ));
    }
    for record in records {
        let name = record["name"].as_str().unwrap_or("");
        validate_name(&format!("{ROOT_NAME}/{name}"))?;
        if record.as_object().map(|object| object.len()) != Some(3)
            || *record != file_record(&root.join(name), name)?
        {
            return Err(format!("inventory mismatch: {name}"));
        }
    }
    Ok((root, inventory))
}

pub(crate) fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(format!("unsafe member path: {name}"));
    }
    if !name.starts_with(&format!("{ROOT_NAME}/")) {
        return Err(format!("wrong archive root: {name}"));
    }
    Ok(())
}
