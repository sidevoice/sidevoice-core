//! `cargo xtask compat`: this core against the latest published release of the connector and of the web client.
//!
//! "Latest published" is GitHub's `releases/latest` of each repository: the newest `vX.Y.Z` release that is
//! neither a draft nor a pre-release, so never `nightly`. Each release's assets are downloaded, checked against
//! its `SHA256SUMS` and unpacked; then `tests/compat.rs` runs with the unpacked artifact named in its
//! environment. A repository with no such release yet is reported and skipped: there is nothing published to be
//! compatible with. `SIDEVOICE_COMPAT_CONNECTOR_TAG` and `SIDEVOICE_COMPAT_WEB_TAG` name another release to check
//! instead (a candidate, or `nightly`).
//!
//! `cargo xtask compat-report`: on a failed run in Actions, open an issue labelled `compat`, or comment on the open
//! one.

use std::path::Path;
use std::process::Command;

use crate::util::*;
use crate::Result;

const CONNECTOR: &str = "sidevoice/sidevoice-connector";
const WEB: &str = "sidevoice/sidevoice-web";
const LABEL: &str = "compat";

pub(crate) fn compat() -> Result<()> {
    let dir = TempDir::new("sidevoice-compat")?;
    let mut checks: Vec<(&str, String)> = Vec::new();
    if let Some(tag) = release(CONNECTOR, "SIDEVOICE_COMPAT_CONNECTOR_TAG")? {
        let assets = dir.0.join("connector");
        fetch(CONNECTOR, &tag, &assets)?;
        let package = asset(&assets, "sidevoice-uplink-", ".tgz")?;
        unpack(&package, &assets)?;
        let cli = assets.join("package/dist/cli.mjs");
        if !cli.is_file() {
            return Err(format!(
                "{CONNECTOR} {tag}: no package/dist/cli.mjs in {}",
                package.display()
            ));
        }
        println!("{CONNECTOR} {tag}");
        checks.push(("SIDEVOICE_COMPAT_CONNECTOR", cli.display().to_string()));
    }
    if let Some(tag) = release(WEB, "SIDEVOICE_COMPAT_WEB_TAG")? {
        let assets = dir.0.join("web");
        fetch(WEB, &tag, &assets)?;
        let site = assets.join("site");
        mkdir(&site)?;
        unpack(&asset(&assets, "sidevoice-web-", ".tar.gz")?, &site)?;
        println!("{WEB} {tag}");
        checks.push(("SIDEVOICE_COMPAT_WEB", site.display().to_string()));
    }
    if checks.is_empty() {
        println!("nothing published to check against yet");
        return Ok(());
    }
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .current_dir(repo())
        .args([
            "test",
            "--locked",
            "--test",
            "compat",
            "--",
            "--ignored",
            "--test-threads=1",
        ])
        .envs(checks)
        .status()
        .map_err(|error| format!("cargo: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(
            "this core is not compatible with a published release (see the test output above)"
                .into(),
        )
    }
}

/// The release to check: the tag in `override_variable` if set (a candidate, or `nightly`, checked before it is
/// published), otherwise the latest published one.
fn release(repository: &str, override_variable: &str) -> Result<Option<String>> {
    match std::env::var(override_variable) {
        Ok(tag) if !tag.is_empty() => Ok(Some(tag)),
        _ => latest(repository),
    }
}

/// The tag of the repository's latest published release, or `None` while it has none.
fn latest(repository: &str) -> Result<Option<String>> {
    let result = Command::new("gh")
        .args([
            "api",
            &format!("repos/{repository}/releases/latest"),
            "--jq",
            ".tag_name",
        ])
        .output()
        .map_err(|error| format!("gh: {error}"))?;
    let stderr = String::from_utf8_lossy(&result.stderr);
    if result.status.success() {
        let tag = String::from_utf8_lossy(&result.stdout).trim().to_owned();
        return Ok(Some(tag).filter(|tag| !tag.is_empty()));
    }
    if stderr.contains("HTTP 404") {
        println!("{repository} has no published release yet: skipped");
        return Ok(None);
    }
    Err(format!("{repository}: latest release: {stderr}"))
}

/// Downloads every asset of the release into `dir` and checks each file `SHA256SUMS` lists.
fn fetch(repository: &str, tag: &str, dir: &Path) -> Result<()> {
    mkdir(dir)?;
    run(
        "gh",
        &[
            "release",
            "download",
            tag,
            "--repo",
            repository,
            "--dir",
            dir.to_str().ok_or("path")?,
        ],
    )?;
    let sums =
        String::from_utf8(read(&dir.join("SHA256SUMS"))?).map_err(|_| "SHA256SUMS is not UTF-8")?;
    let mut checked = 0;
    for line in sums.lines().filter(|line| !line.trim().is_empty()) {
        let (digest, name) = line
            .split_once(char::is_whitespace)
            .ok_or_else(|| format!("{repository} {tag}: SHA256SUMS line {line:?}"))?;
        let name = name.trim().trim_start_matches('*');
        if name.contains('/') {
            return Err(format!(
                "{repository} {tag}: SHA256SUMS names a path: {name}"
            ));
        }
        let actual = sha256(&read(&dir.join(name))?);
        if actual != digest {
            return Err(format!(
                "{repository} {tag}: {name}: SHA-256 {actual}, SHA256SUMS says {digest}"
            ));
        }
        checked += 1;
    }
    if checked == 0 {
        return Err(format!("{repository} {tag}: SHA256SUMS lists nothing"));
    }
    Ok(())
}

/// The one asset in `dir` named `prefix…suffix`.
fn asset(dir: &Path, prefix: &str, suffix: &str) -> Result<std::path::PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))? {
        let path = entry.map_err(|error| error.to_string())?.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if name.starts_with(prefix) && name.ends_with(suffix) {
            found.push(path);
        }
    }
    match found.as_slice() {
        [one] => Ok(one.clone()),
        _ => Err(format!(
            "{}: expected one {prefix}*{suffix}, found {found:?}",
            dir.display()
        )),
    }
}

fn unpack(archive: &Path, into: &Path) -> Result<()> {
    run(
        "tar",
        &[
            "-xzf",
            archive.to_str().ok_or("path")?,
            "-C",
            into.to_str().ok_or("path")?,
        ],
    )
}

pub(crate) fn report() -> Result<()> {
    let variable = |name: &str| std::env::var(name).map_err(|_| format!("{name} is not set"));
    let run_url = format!(
        "{}/{}/actions/runs/{}",
        variable("GITHUB_SERVER_URL")?,
        variable("GITHUB_REPOSITORY")?,
        variable("GITHUB_RUN_ID")?
    );
    let body = format!(
        "The weekly compatibility check failed: this core against the latest published release of \
         {CONNECTOR} and {WEB} (`cargo xtask compat`).\n\nRun: {run_url}"
    );
    let open = output(
        "gh",
        &[
            "issue",
            "list",
            "--label",
            LABEL,
            "--state",
            "open",
            "--json",
            "number",
            "--jq",
            ".[0].number",
        ],
        None,
    )?;
    let open = open.trim();
    if open.is_empty() {
        // The label may not exist yet; creating it again is refused, which is fine.
        let _ = output(
            "gh",
            &[
                "label",
                "create",
                LABEL,
                "--description",
                "The core against released clients",
                "--color",
                "d93f0b",
            ],
            None,
        );
        run(
            "gh",
            &[
                "issue",
                "create",
                "--title",
                "Compatibility with released clients",
                "--label",
                LABEL,
                "--body",
                &body,
            ],
        )
    } else {
        run("gh", &["issue", "comment", open, "--body", &body])
    }
}
