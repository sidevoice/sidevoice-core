//! `cargo xtask manifest DIR [--tag vX.Y.Z]`: `native-core-manifest.json` and `SHA256SUMS` for every target.

use std::env;
use std::path::Path;

use serde_json::json;

use crate::archive::unpack_checked;
use crate::util::*;
use crate::{Result, ENTRYPOINT, KIND, ROOT_NAME, TARGETS};

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
    let mut bundles = serde_json::Map::new();
    let mut sums = Vec::new();
    for target in TARGETS {
        let name = format!("{ROOT_NAME}-{source_sha}-{target}.tar.zst");
        let work = TempDir::new("sidevoice-manifest-check")?;
        let (_, inventory) = unpack_checked(&dir.join(&name), &work.0)?;
        if inventory["target"] != target || inventory["source_sha"] != source_sha.as_str() {
            return Err(format!("wrong archive identity: {name}"));
        }
        let record = file_record(&dir.join(&name), &name)?;
        sums.push(format!(
            "{}  {name}\n",
            record["sha256"].as_str().unwrap_or("")
        ));
        bundles.insert(target.into(), record);
    }
    let value = json!({"schema": 1, "kind": KIND, "source_sha": source_sha,
                       "cargo_lock_sha256": sha256(&read(&repo.join("Cargo.lock"))?),
                       "entrypoint": ENTRYPOINT, "bundles": bundles});
    let bytes = canonical(&value);
    write(&dir.join("native-core-manifest.json"), &bytes)?;
    sums.push(format!("{}  native-core-manifest.json\n", sha256(&bytes)));
    sums.sort_by(|left, right| left[66..].cmp(&right[66..]));
    write(&dir.join("SHA256SUMS"), sums.concat().as_bytes())?;
    print!("{}", String::from_utf8_lossy(&bytes));
    Ok(())
}
