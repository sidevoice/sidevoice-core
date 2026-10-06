//! Helpers shared by every command: files, JSON, processes, downloads, temporary directories.

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::Result;

pub(crate) fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the repository")
        .to_path_buf()
}

pub(crate) fn cache_dir() -> Result<PathBuf> {
    env::var_os("RUSTVANI_CACHE_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| "RUSTVANI_CACHE_DIR is not set".into())
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn read(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).map_err(|error| format!("{}: {error}", path.display()))
}

pub(crate) fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::write(path, bytes).map_err(|error| format!("{}: {error}", path.display()))
}

pub(crate) fn mkdir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|error| format!("{}: {error}", path.display()))
}

pub(crate) fn chmod(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// Compact JSON with sorted keys and a final newline: the one byte form of every JSON file in an archive.
pub(crate) fn canonical(value: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(value).expect("JSON values serialize");
    bytes.push(b'\n');
    bytes
}

pub(crate) fn parse_json(bytes: &[u8], what: &str) -> Result<Value> {
    serde_json::from_slice(bytes).map_err(|error| format!("{what}: {error}"))
}

/// Runs a program to completion and returns its standard output; a failure is an error with its standard error.
pub(crate) fn output(program: &str, args: &[&str], dir: Option<&Path>) -> Result<String> {
    let mut command = Command::new(program);
    command.args(args);
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    let result = command
        .output()
        .map_err(|error| format!("{program}: {error}"))?;
    if !result.status.success() {
        return Err(format!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    String::from_utf8(result.stdout).map_err(|_| format!("{program}: output is not UTF-8"))
}

pub(crate) fn run(program: &str, args: &[&str]) -> Result<()> {
    output(program, args, None).map(|_| ())
}

pub(crate) fn download(url: &str) -> Result<Vec<u8>> {
    let result = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--retry",
            "3",
            url,
        ])
        .output()
        .map_err(|error| format!("curl: {error}"))?;
    if !result.status.success() {
        return Err(format!(
            "download {url}: {}",
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    Ok(result.stdout)
}

pub(crate) fn host_target() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("linux-x86_64"),
        ("linux", "aarch64") => Ok("linux-aarch64"),
        ("macos", "aarch64") => Ok("macos-aarch64"),
        (os, arch) => Err(format!("unsupported build host {os}-{arch}")),
    }
}

pub(crate) fn git(args: &[&str]) -> Result<String> {
    Ok(output("git", args, Some(&repo()))?.trim().to_string())
}

pub(crate) fn is_commit(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub(crate) fn file_record(path: &Path, name: &str) -> Result<Value> {
    let bytes = read(path)?;
    Ok(json!({"name": name, "size": bytes.len(), "sha256": sha256(&bytes)}))
}

/// Every entry below `root`, as paths relative to it, sorted.
pub(crate) fn walk(root: &Path) -> Result<Vec<(String, bool)>> {
    fn visit(root: &Path, dir: &Path, found: &mut Vec<(String, bool)>) -> Result<()> {
        for entry in fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))? {
            let path = entry.map_err(|error| error.to_string())?.path();
            let relative = path
                .strip_prefix(root)
                .expect("below root")
                .to_string_lossy()
                .into_owned();
            let is_dir = path.is_dir();
            found.push((relative, is_dir));
            if is_dir {
                visit(root, &path, found)?;
            }
        }
        Ok(())
    }
    let mut found = Vec::new();
    visit(root, root, &mut found)?;
    found.sort();
    Ok(found)
}

pub(crate) struct TempDir(pub(crate) PathBuf);

impl TempDir {
    pub(crate) fn new(label: &str) -> Result<Self> {
        Self::new_in(&env::temp_dir(), label)
    }

    pub(crate) fn new_in(base: &Path, label: &str) -> Result<Self> {
        let path = base.join(format!("{label} {}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        mkdir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
