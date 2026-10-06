//! `cargo xtask publish DIR TAG`: attach, read back, verify and publish a release.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::util::*;
use crate::Result;

/// The signer every asset must carry: the reusable build workflow on main, for nightlies and releases alike.
pub(crate) fn signer() -> Result<String> {
    let repository = env::var("GH_REPO").map_err(|_| "GH_REPO is not set")?;
    Ok(format!(
        "https://github.com/{repository}/.github/workflows/build.yml@refs/heads/main"
    ))
}

pub(crate) fn gh(args: &[&str]) -> Result<String> {
    output("gh", args, None)
}

pub(crate) fn publish(dir: &Path, tag: &str) -> Result<()> {
    let signer = signer()?;
    let mut assets: Vec<String> = Vec::new();
    for entry in fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))? {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.is_file() {
            assets.push(path.to_str().ok_or("path")?.to_string());
        }
    }
    assets.sort();
    let files: Vec<&str> = assets.iter().map(String::as_str).collect();

    if tag == "nightly" {
        let sha = env::var("GITHUB_SHA").map_err(|_| "GITHUB_SHA is not set")?;
        let repository = env::var("GH_REPO").map_err(|_| "GH_REPO is not set")?;
        if gh(&["api", &format!("repos/{repository}/git/ref/tags/nightly")]).is_ok() {
            gh(&[
                "api",
                "-X",
                "PATCH",
                &format!("repos/{repository}/git/refs/tags/nightly"),
                "-f",
                &format!("sha={sha}"),
                "-F",
                "force=true",
            ])?;
        } else {
            gh(&[
                "api",
                "-X",
                "POST",
                &format!("repos/{repository}/git/refs"),
                "-f",
                "ref=refs/tags/nightly",
                "-f",
                &format!("sha={sha}"),
            ])?;
        }
        let notes = format!(
            "Snapshot of `main` at {sha}. Not a version: the `nightly` tag moves to every commit on `main` whose \
             build passes, and these assets are replaced each time. Pin a `vX.Y.Z` release instead."
        );
        if gh(&["release", "view", "nightly"]).is_ok() {
            gh(&[
                "release",
                "edit",
                "nightly",
                "--title",
                "Nightly (main)",
                "--notes",
                &notes,
                "--prerelease",
                "--latest=false",
            ])?;
            let mut upload = vec!["release", "upload", "nightly"];
            upload.extend(&files);
            upload.push("--clobber");
            gh(&upload)?;
            let names: BTreeSet<String> = files
                .iter()
                .filter_map(|file| {
                    Path::new(file)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .collect();
            for old in gh(&[
                "release",
                "view",
                "nightly",
                "--json",
                "assets",
                "-q",
                ".assets[].name",
            ])?
            .lines()
            {
                if !names.contains(old) {
                    gh(&["release", "delete-asset", "nightly", old, "--yes"])?;
                }
            }
        } else {
            let mut create = vec!["release", "create", "nightly"];
            create.extend(&files);
            create.extend([
                "--verify-tag",
                "--title",
                "Nightly (main)",
                "--notes",
                &notes,
                "--draft",
                "--prerelease",
                "--latest=false",
            ]);
            gh(&create)?;
        }
    } else {
        let mut upload = vec!["release", "upload", tag];
        upload.extend(&files);
        upload.push("--clobber");
        gh(&upload)?;
    }

    // Read every asset back from the release: same bytes, listed in SHA256SUMS, signed by the build workflow.
    let check = TempDir::new("sidevoice-release-check")?;
    let check_dir = check.0.to_str().ok_or("path")?;
    gh(&["release", "download", tag, "--dir", check_dir])?;
    for line in String::from_utf8_lossy(&read(&check.0.join("SHA256SUMS"))?).lines() {
        let (digest, name) = line.split_once("  ").ok_or("malformed SHA256SUMS")?;
        let downloaded = read(&check.0.join(name))?;
        if sha256(&downloaded) != digest || downloaded != read(&dir.join(name))? {
            return Err(format!(
                "{name}: the release holds other bytes than this build"
            ));
        }
        let status = Command::new("gh")
            .args([
                "attestation",
                "verify",
                check.0.join(name).to_str().ok_or("path")?,
                "--repo",
                &env::var("GH_REPO").unwrap_or_default(),
                "--bundle",
                check
                    .0
                    .join("attestation.sigstore.json")
                    .to_str()
                    .ok_or("path")?,
                "--cert-identity",
                &signer,
                "--deny-self-hosted-runners",
            ])
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .status()
            .map_err(|error| format!("gh attestation verify: {error}"))?;
        if !status.success() {
            return Err(format!("{name}: attestation does not verify"));
        }
    }

    if tag == "nightly" || tag.contains('-') {
        gh(&[
            "release",
            "edit",
            tag,
            "--draft=false",
            "--prerelease",
            "--latest=false",
        ])?;
    } else {
        gh(&[
            "release",
            "edit",
            tag,
            "--draft=false",
            "--prerelease=false",
            "--latest",
        ])?;
    }
    println!("published {tag}");
    Ok(())
}
