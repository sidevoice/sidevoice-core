//! `cargo xtask sentry-upload`: the release `sidevoice-core@<version>` and the debug files of the release build `dist` just
//! made (a split `.dwp` on Linux, a `.dSYM` on macOS, a `.pdb` on Windows, and the binary itself, whose symbol table
//! `strip = "debuginfo"` keeps), to Sentry, so that a panic's addresses become function names and lines (rust/crash.rs).
//! `sentry-cli` does the upload; this only says what and where.
//!
//! Reads `SENTRY_AUTH_TOKEN` (the organisation's secret, in the environment of the workflow step that runs this and no
//! other; never printed, never on a command line). The organisation, the project and the EU region are fixed here: org
//! `twentynine-tech-sl`, project `sidevoice`, `https://de.sentry.io`. Only the release and nightly workflows run it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use sidevoice_platform::target;

use crate::dist::built_dir;
use crate::util::version;
use crate::Result;

const URL: &str = "https://de.sentry.io";
const ORG: &str = "twentynine-tech-sl";
const PROJECT: &str = "sidevoice";
/// The `service.name` a release carries, as the reporter in the core names it.
const SERVICE: &str = "sidevoice-core";

/// The release a build reports itself as: `<service.name>@<version>`.
fn release_name(version: &str) -> String {
    format!("{SERVICE}@{version}")
}

/// Whether `name`, a file in the build directory, is a debug file of the core: its binary, or what was split off it.
fn is_debug_file(name: &str, binary: &str) -> bool {
    let ours = name.starts_with("sidevoice-core") || name.starts_with("sidevoice_core");
    name == binary
        || (ours
            && [".dwp", ".pdb", ".dSYM"]
                .iter()
                .any(|ext| name.ends_with(ext)))
}

fn debug_files(dir: &Path, binary: &str) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))? {
        let entry = entry.map_err(|error| error.to_string())?;
        if is_debug_file(&entry.file_name().to_string_lossy(), binary) {
            found.push(entry.path());
        }
    }
    found.sort();
    if found.is_empty() {
        return Err(format!("no debug files in {}", dir.display()));
    }
    Ok(found)
}

fn sentry(args: &[&str]) -> Result<()> {
    if std::env::var_os("SENTRY_AUTH_TOKEN").is_none_or(|token| token.is_empty()) {
        return Err("SENTRY_AUTH_TOKEN is not set".into());
    }
    // npm's global shim on Windows is a batch file.
    let program = if cfg!(windows) {
        "sentry-cli.cmd"
    } else {
        "sentry-cli"
    };
    let status = Command::new(program)
        .args(args)
        .env("SENTRY_URL", URL)
        .env("SENTRY_ORG", ORG)
        .env("SENTRY_PROJECT", PROJECT)
        .status()
        .map_err(|error| format!("sentry-cli: {error}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("sentry-cli {} failed", args.join(" ")))
}

pub(crate) fn upload() -> Result<()> {
    let target = target::build_target().ok_or("unsupported build target")?;
    let release = release_name(&version()?);
    let files = debug_files(&built_dir(target), &target.exe_name("sidevoice-core"))?;
    let files: Vec<String> = files
        .iter()
        .map(|file| file.to_string_lossy().into_owned())
        .collect();
    sentry(&["releases", "new", &release])?;
    let mut upload = vec!["debug-files", "upload", "--wait"];
    upload.extend(files.iter().map(String::as_str));
    sentry(&upload)?;
    sentry(&["releases", "finalize", &release])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_is_named_after_its_service() {
        assert_eq!(release_name("0.18.0"), "sidevoice-core@0.18.0");
    }

    #[test]
    fn the_binary_and_what_was_split_off_it_are_debug_files_and_nothing_else_is() {
        for name in [
            "sidevoice-core",
            "sidevoice-core.dwp",
            "sidevoice-core.dSYM",
            "sidevoice_core.pdb",
        ] {
            assert!(is_debug_file(name, "sidevoice-core"), "{name}");
        }
        assert!(is_debug_file("sidevoice-core.exe", "sidevoice-core.exe"));
        for name in [
            "sidevoice-core.d",
            "libsidevoice_core.rlib",
            "libsidevoice_core.d",
            "deps",
            "build",
            "incremental",
            ".fingerprint",
        ] {
            assert!(!is_debug_file(name, "sidevoice-core"), "{name}");
        }
    }
}
