//! `cargo xtask manifest DIR [--tag vX.Y.Z]`: `native-core-manifest.json` and `SHA256SUMS` for every target.

use std::env;
use std::fs;
use std::path::Path;

use serde_json::json;
use sidevoice_platform::target::TARGETS;

use crate::archive::unpack_checked;
use crate::util::*;
use crate::{Result, ENTRYPOINT_STEM, KIND};

pub(crate) fn manifest(dir: &Path, tag: Option<&str>) -> Result<()> {
    let repo = repo();
    let source_sha = git(&["rev-parse", "HEAD"])?;
    if let Ok(expected) = env::var("GITHUB_SHA") {
        // The attestation names GITHUB_SHA as its source: the archives must come from that very commit.
        if expected != source_sha {
            return Err(format!(
                "checked out {source_sha}, but this run is for {expected}"
            ));
        }
    }
    if let Some(tag) = tag {
        let metadata = parse_json(
            output(
                "cargo",
                &["metadata", "--no-deps", "--locked", "--format-version", "1"],
                Some(&repo),
            )?
            .as_bytes(),
            "cargo metadata",
        )?;
        let version = metadata["packages"]
            .as_array()
            .and_then(|packages| {
                packages
                    .iter()
                    .find(|package| package["name"] == "sidevoice-core")
            })
            .and_then(|package| package["version"].as_str())
            .ok_or("no sidevoice-core version")?;
        if format!("v{version}") != tag {
            return Err(format!("Cargo.toml says {version}, the release is {tag}"));
        }
    }
    // Fixed names for the nightly, so its download URLs never change; the version for a release. The commit is
    // in the manifest and in every archive's inventory.
    let label = tag
        .map(|tag| tag.trim_start_matches('v'))
        .unwrap_or("nightly");
    let bytes = write_manifest(dir, label, &source_sha, &read(&repo.join("Cargo.lock"))?)?;
    print!("{}", String::from_utf8_lossy(&bytes));
    Ok(())
}

/// Checks and publishes exactly the targets in the shared release table.
fn write_manifest(dir: &Path, label: &str, source_sha: &str, cargo_lock: &[u8]) -> Result<Vec<u8>> {
    let mut bundles = serde_json::Map::new();
    let mut sums = Vec::new();
    for target in TARGETS {
        let built = dir.join(target.archive_name("sidevoice-core", None));
        let name = target.archive_name("sidevoice-core", Some(label));
        if built.exists() {
            fs::rename(&built, dir.join(&name)).map_err(|error| format!("{name}: {error}"))?;
        }
        let work = TempDir::new("sidevoice-manifest-check")?;
        let (_, inventory) = unpack_checked(&dir.join(&name), &work.0)?;
        if inventory["target"] != target.name || inventory["source_sha"] != source_sha {
            return Err(format!("wrong archive identity: {name}"));
        }
        let record = file_record(&dir.join(&name), &name)?;
        sums.push(format!(
            "{}  {name}\n",
            record["sha256"].as_str().unwrap_or("")
        ));
        bundles.insert(target.name.into(), record);
    }
    let value = json!({"schema": 1, "kind": KIND, "source_sha": source_sha,
                       "cargo_lock_sha256": sha256(cargo_lock),
                       "entrypoint": ENTRYPOINT_STEM, "bundles": bundles});
    let bytes = canonical(&value);
    write(&dir.join("native-core-manifest.json"), &bytes)?;
    sums.push(format!("{}  native-core-manifest.json\n", sha256(&bytes)));
    sums.sort_by(|left, right| left[66..].cmp(&right[66..]));
    write(&dir.join("SHA256SUMS"), sums.concat().as_bytes())?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{dist::write_tar, glibc, ROOT_NAME};

    #[test]
    fn manifest_publishes_every_shared_target_and_requires_all_archives() {
        let dir = TempDir::new("shared-target-manifest").unwrap();
        let source_sha = "a".repeat(40);
        for target in TARGETS {
            let work = TempDir::new("shared-target-archive").unwrap();
            let root = work.0.join(ROOT_NAME);
            mkdir(&root.join("bin")).unwrap();
            let entrypoint = target.exe_name(ENTRYPOINT_STEM);
            write(&root.join(&entrypoint), b"fixture core").unwrap();
            let mut inventory = json!({
                "schema": 1, "kind": KIND, "target": target.name, "source_sha": source_sha,
                "entrypoint": entrypoint,
                "files": [file_record(&root.join(&entrypoint), &entrypoint).unwrap()],
            });
            if target.os == "linux" {
                inventory["glibc"] = glibc::FLOOR.into();
            }
            write(&root.join("native-core.json"), &canonical(&inventory)).unwrap();
            let raw = work.0.join("bundle.tar");
            write_tar(&work.0, &raw, 0, &entrypoint).unwrap();
            let archive = dir.0.join(target.archive_name("sidevoice-core", None));
            run(
                "zstd",
                &["-q", raw.to_str().unwrap(), "-o", archive.to_str().unwrap()],
            )
            .unwrap();
        }
        let bytes = write_manifest(&dir.0, "nightly", &source_sha, b"lockfile").unwrap();
        let manifest = parse_json(&bytes, "manifest").unwrap();
        let bundles = manifest["bundles"].as_object().unwrap();
        assert_eq!(bundles.len(), TARGETS.len());
        assert_eq!(manifest["cargo_lock_sha256"], sha256(b"lockfile"));
        let sums = String::from_utf8(read(&dir.0.join("SHA256SUMS")).unwrap()).unwrap();
        for target in TARGETS {
            let name = target.archive_name("sidevoice-core", Some("nightly"));
            assert_eq!(
                bundles[target.name],
                file_record(&dir.0.join(&name), &name).unwrap()
            );
            assert!(!dir
                .0
                .join(target.archive_name("sidevoice-core", None))
                .exists());
            assert!(sums.contains(&format!("  {name}\n")));
        }
        // A partial set must never produce a successful release manifest.
        let missing = TARGETS
            .last()
            .unwrap()
            .archive_name("sidevoice-core", Some("nightly"));
        fs::remove_file(dir.0.join(&missing)).unwrap();
        let error = write_manifest(&dir.0, "nightly", &source_sha, b"lockfile").unwrap_err();
        assert!(error.contains(&missing), "{error}");
    }
}
