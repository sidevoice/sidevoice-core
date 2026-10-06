//! Build tooling for the core, run as `cargo xtask <command>` (alias in `.cargo/config.toml`).
//!
//! - `models [DIR]`: stage the detector models pinned in `assets/rust-models.json` (default `$RUSTVANI_CACHE_DIR`).
//! - `dist`: build the release binary for this host and package it, with its models, native libraries and licence
//!   notices, as the relocatable archive `native/sidevoice-core-rust-<commit>-<target>.tar.zst`; then `verify` it.
//! - `verify ARCHIVE`: unpack it somewhere else, check its inventory, and start the core from there.
//! - `manifest DIR [--tag vX.Y.Z]`: check every target's archive in DIR and write `native-core-manifest.json` and
//!   `SHA256SUMS`; with a tag, the crate version must be that release.
//! - `publish DIR TAG`: attach every file in DIR to the release TAG (for `nightly`, move the tag here first and drop
//!   older assets), download them back, check them against `SHA256SUMS` and the attestation, and publish.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

type Result<T> = std::result::Result<T, String>;

const TARGETS: [&str; 3] = ["linux-aarch64", "linux-x86_64", "macos-aarch64"];
const ROOT_NAME: &str = "sidevoice-core-rust";
const ENTRYPOINT: &str = "bin/sidevoice-core-rust";
const KIND: &str = "rust-native-v1";
/// Upstream licence texts the archive carries, pinned by digest.
const LICENSES: [(&str, &str, &str); 4] = [
    (
        "onnxruntime-license.txt",
        "https://raw.githubusercontent.com/microsoft/onnxruntime/v1.22.0/LICENSE",
        "2f07c72751aed99790b8a4869cf2311df85a860b22ded05fa22803587a48922c",
    ),
    (
        "silero-vad-license.txt",
        "https://raw.githubusercontent.com/snakers4/silero-vad/master/LICENSE",
        "2e63e9a38b6e8fc0c7bc37ce174caca1862870856c6daf5697cfb785e925520b",
    ),
    (
        "smart-turn-license.txt",
        "https://raw.githubusercontent.com/pipecat-ai/smart-turn/main/LICENSE",
        "0d66364067f678c08586ebb60a16a2aed4fa081ec11057df35585759ce0e774f",
    ),
    (
        "opus-license.txt",
        "https://raw.githubusercontent.com/xiph/opus/v1.5.2/COPYING",
        "01e1167d54a096d123cf6dfbbeb19587278845c6481d2d66d545669846079551",
    ),
];
const ORT_SYS_VERSION: &str = "2.0.0-rc.10";
const RUSTVANI_REVISION: &str = "d01f33e671f7a4d8a128e7bfe55dbf0e8963cb21";

const USAGE: &str =
    "usage: cargo xtask models [DIR] | dist | verify ARCHIVE | manifest DIR [--tag vX.Y.Z] | publish DIR TAG";

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["models"] => cache_dir().and_then(|dir| models(&dir)),
        ["models", dir] => models(Path::new(dir)),
        ["dist"] => dist(),
        ["verify", archive] => verify(Path::new(archive)),
        ["manifest", dir] => manifest(Path::new(dir), None),
        ["manifest", dir, "--tag", tag] => manifest(Path::new(dir), Some(tag)),
        ["publish", dir, tag] => publish(Path::new(dir), tag),
        _ => Err(USAGE.into()),
    };
    if let Err(error) = result {
        eprintln!("xtask: {error}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Small helpers

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the repository")
        .to_path_buf()
}

fn cache_dir() -> Result<PathBuf> {
    env::var_os("RUSTVANI_CACHE_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| "RUSTVANI_CACHE_DIR is not set".into())
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn read(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).map_err(|error| format!("{}: {error}", path.display()))
}

fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::write(path, bytes).map_err(|error| format!("{}: {error}", path.display()))
}

fn mkdir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|error| format!("{}: {error}", path.display()))
}

fn chmod(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// Compact JSON with sorted keys and a final newline: the one byte form of every JSON file in an archive.
fn canonical(value: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(value).expect("JSON values serialize");
    bytes.push(b'\n');
    bytes
}

fn parse_json(bytes: &[u8], what: &str) -> Result<Value> {
    serde_json::from_slice(bytes).map_err(|error| format!("{what}: {error}"))
}

/// Runs a program to completion and returns its standard output; a failure is an error with its standard error.
fn output(program: &str, args: &[&str], dir: Option<&Path>) -> Result<String> {
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

fn run(program: &str, args: &[&str]) -> Result<()> {
    output(program, args, None).map(|_| ())
}

fn download(url: &str) -> Result<Vec<u8>> {
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

fn host_target() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("linux-x86_64"),
        ("linux", "aarch64") => Ok("linux-aarch64"),
        ("macos", "aarch64") => Ok("macos-aarch64"),
        (os, arch) => Err(format!("unsupported build host {os}-{arch}")),
    }
}

fn git(args: &[&str]) -> Result<String> {
    Ok(output("git", args, Some(&repo()))?.trim().to_string())
}

fn is_commit(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn file_record(path: &Path, name: &str) -> Result<Value> {
    let bytes = read(path)?;
    Ok(json!({"name": name, "size": bytes.len(), "sha256": sha256(&bytes)}))
}

/// Every entry below `root`, as paths relative to it, sorted.
fn walk(root: &Path) -> Result<Vec<(String, bool)>> {
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

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Result<Self> {
        let path = env::temp_dir().join(format!("{label} {}", std::process::id()));
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

// ---------------------------------------------------------------------------------------------------------------
// models

fn models(dir: &Path) -> Result<()> {
    let pins = parse_json(
        &read(&repo().join("assets/rust-models.json"))?,
        "assets/rust-models.json",
    )?;
    let revision = pins["source_commit"]
        .as_str()
        .ok_or("rust-models.json: no source_commit")?;
    mkdir(dir)?;
    for model in pins["models"]
        .as_array()
        .ok_or("rust-models.json: no models")?
    {
        let name = model["name"].as_str().ok_or("model without name")?;
        let expected = model["sha256"].as_str().ok_or("model without sha256")?;
        let target = dir.join(name);
        if target.is_file() && sha256(&read(&target)?) == expected {
            continue;
        }
        let source = model["source_path"]
            .as_str()
            .ok_or("model without source_path")?;
        let bytes = download(&format!(
            "https://raw.githubusercontent.com/Allenmylath/rustvani/{revision}/{source}"
        ))?;
        let actual = sha256(&bytes);
        if actual != expected {
            return Err(format!("{name}: SHA-256 mismatch: {actual}"));
        }
        let staged = dir.join(format!(".{name}.partial"));
        write(&staged, &bytes)?;
        fs::rename(&staged, &target).map_err(|error| format!("{}: {error}", target.display()))?;
        println!("staged {name} {actual}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// dist

fn dist() -> Result<()> {
    let repo = repo();
    let target = host_target()?;
    let source_sha = git(&["rev-parse", "HEAD"])?;
    let epoch: u64 = git(&["show", "-s", "--format=%ct", "HEAD"])?
        .parse()
        .map_err(|_| "commit time")?;
    let models_dir = cache_dir()?;
    models(&models_dir)?;
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(&cargo)
        .args([
            "build",
            "--locked",
            "--release",
            "--bin",
            "sidevoice-core-rust",
        ])
        .current_dir(&repo)
        .status()
        .map_err(|error| format!("cargo build: {error}"))?;
    if !status.success() {
        return Err("cargo build failed".into());
    }

    let work = TempDir::new("sidevoice-dist")?;
    let stage = work.0.join(ROOT_NAME);
    for name in ["bin", "lib", "models", "checks", "notices"] {
        mkdir(&stage.join(name))?;
    }
    let binary = stage.join(ENTRYPOINT);
    write(
        &binary,
        &read(&repo.join("target/release/sidevoice-core-rust"))?,
    )?;
    chmod(&binary, 0o755)?;
    let pins = parse_json(
        &read(&repo.join("assets/rust-models.json"))?,
        "assets/rust-models.json",
    )?;
    for model in pins["models"]
        .as_array()
        .ok_or("rust-models.json: no models")?
    {
        let name = model["name"].as_str().ok_or("model without name")?;
        let bytes = read(&models_dir.join(name))?;
        if Some(sha256(&bytes).as_str()) != model["sha256"].as_str() {
            return Err(format!("model digest mismatch: {name}"));
        }
        write(&stage.join("models").join(name), &bytes)?;
    }
    write(
        &stage.join("checks/detector-16k.wav"),
        &read(&repo.join("tests/fixtures/hola-sala-16k.wav"))?,
    )?;
    stage_notices(&stage.join("notices"), target, &pins)?;
    let links = if target.starts_with("linux-") {
        linux_libraries(&binary, &stage.join("lib"))?
    } else {
        mac_libraries(&binary, &stage.join("lib"))?
    };
    if fs::read_dir(stage.join("lib"))
        .map_err(|error| error.to_string())?
        .next()
        .is_none()
    {
        fs::remove_dir(stage.join("lib")).map_err(|error| error.to_string())?;
    }

    let mut files = Vec::new();
    for (name, is_dir) in walk(&stage)? {
        if !is_dir {
            files.push(file_record(&stage.join(&name), &name)?);
        }
    }
    let count = files.len();
    let inventory = json!({"schema": 1, "kind": KIND, "target": target, "source_sha": source_sha,
                           "entrypoint": ENTRYPOINT, "files": files});
    write(&stage.join("native-core.json"), &canonical(&inventory))?;

    let raw = work.0.join("bundle.tar");
    write_tar(&work.0, &raw, epoch)?;
    let archive = repo
        .join("native")
        .join(format!("{ROOT_NAME}-{source_sha}-{target}.tar.zst"));
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
    verify(&archive)
}

/// A reproducible tar of `<work>/sidevoice-core-rust`: owner root, fixed time, 0755 for directories and the
/// entrypoint, 0644 for everything else.
fn write_tar(work: &Path, raw: &Path, epoch: u64) -> Result<()> {
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

fn stage_notices(notices: &Path, target: &str, pins: &Value) -> Result<()> {
    let metadata = parse_json(
        output(
            "cargo",
            &["metadata", "--locked", "--format-version", "1"],
            Some(&repo()),
        )?
        .as_bytes(),
        "cargo metadata",
    )?;
    let packages_meta = metadata["packages"]
        .as_array()
        .ok_or("cargo metadata: no packages")?;
    let licenses = notices.join("licenses");
    mkdir(&licenses)?;
    let mut packages = Vec::new();
    for package in packages_meta {
        let manifest = package["manifest_path"]
            .as_str()
            .ok_or("package without manifest_path")?;
        let source = Path::new(manifest)
            .parent()
            .ok_or("manifest without directory")?;
        let mut names: Vec<_> = fs::read_dir(source)
            .map_err(|error| format!("{}: {error}", source.display()))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_file())
            .collect();
        names.sort();
        let mut texts = Vec::new();
        for path in names {
            let file_name = path
                .file_name()
                .expect("a file")
                .to_string_lossy()
                .into_owned();
            let lower = file_name.to_lowercase();
            if !["license", "copying", "notice"]
                .iter()
                .any(|prefix| lower.starts_with(prefix))
            {
                continue;
            }
            let bytes = read(&path)?;
            if bytes.len() > 1_000_000 {
                return Err(format!(
                    "unexpectedly large license: {} {file_name}",
                    package["name"]
                ));
            }
            let digest = sha256(&bytes);
            let destination = licenses.join(format!("{digest}.txt"));
            if !destination.exists() {
                write(&destination, &bytes)?;
            }
            texts.push(json!({"name": file_name, "sha256": digest}));
        }
        packages.push(json!({"name": package["name"], "version": package["version"], "license": package["license"],
                             "source": package["source"], "license_texts": texts}));
    }
    packages.sort_by_key(|package| {
        let field = |key: &str| package[key].as_str().unwrap_or("").to_string();
        (field("name"), field("version"), field("source"))
    });
    write(
        &notices.join("rust-dependencies.json"),
        &canonical(&Value::Array(packages)),
    )?;

    let find = |name: &str| packages_meta.iter().find(|package| package["name"] == name);
    let rustvani = find("rustvani").ok_or("rustvani is not a dependency")?;
    let rustvani_dir = Path::new(
        rustvani["manifest_path"]
            .as_str()
            .ok_or("rustvani manifest")?,
    )
    .parent()
    .ok_or("rustvani directory")?;
    write(
        &notices.join("rustvani-license.txt"),
        &read(&rustvani_dir.join("LICENSE"))?,
    )?;
    write(
        &notices.join("rustvani-third-party.md"),
        &read(&rustvani_dir.join("THIRD_PARTY_NOTICES.md"))?,
    )?;
    let mut license_sources = serde_json::Map::new();
    for (name, url, expected) in LICENSES {
        let bytes = download(url)?;
        if sha256(&bytes) != expected {
            return Err(format!("pinned notice digest changed: {name}"));
        }
        write(&notices.join(name), &bytes)?;
        license_sources.insert(name.into(), json!({"url": url, "sha256": expected}));
    }

    let ort_sys = find("ort-sys").ok_or("ort-sys is not a dependency")?;
    if ort_sys["version"] != ORT_SYS_VERSION {
        return Err("unexpected ONNX Runtime binding version".into());
    }
    let ort_target = match target {
        "linux-aarch64" => "aarch64-unknown-linux-gnu",
        "linux-x86_64" => "x86_64-unknown-linux-gnu",
        _ => "aarch64-apple-darwin",
    };
    let ort_dir = Path::new(
        ort_sys["manifest_path"]
            .as_str()
            .ok_or("ort-sys manifest")?,
    )
    .parent()
    .ok_or("ort-sys directory")?;
    let dist = String::from_utf8_lossy(&read(&ort_dir.join("dist.txt"))?).into_owned();
    let ort_archive = dist
        .lines()
        .map(|row| row.split('\t').collect::<Vec<_>>())
        .find(|fields| fields.len() == 4 && fields[0] == "none" && fields[1] == ort_target)
        .map(|fields| json!({"url": fields[2], "sha256": fields[3].to_lowercase()}))
        .ok_or("pinned ONNX Runtime archive is missing")?;
    write(
        &notices.join("sources.json"),
        &canonical(&json!({
            "ort": format!("onnxruntime 1.22.0 selected by ort-sys {ORT_SYS_VERSION}"),
            "ort_archive": ort_archive,
            "rustvani": RUSTVANI_REVISION,
            "models": pins,
            "license_sources": license_sources,
        })),
    )?;

    let linux = target.starts_with("linux-");
    let opus = if linux {
        write(
            &notices.join("libopus-distro-copyright.txt"),
            &read(Path::new("/usr/share/doc/libopus0/copyright"))?,
        )?;
        output("dpkg-query", &["-W", "-f=${Version}", "libopus0"], None)?
    } else {
        output("brew", &["list", "--versions", "opus"], None)?
    };
    write(
        &notices.join("native-dependencies.json"),
        &canonical(&json!({
            "onnxruntime": {"version": "1.22.0", "link": "static"},
            "opus": {"package": opus.trim(), "link": if linux { "dynamic" } else { "static" }},
            "system_trust_roots": if linux { "ca-certificates" } else { "macOS system trust store" },
        })),
    )
}

/// Copies the shared libraries the binary needs besides the system's into `lib/` and points the binary there.
fn linux_libraries(binary: &Path, lib_dir: &Path) -> Result<String> {
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
fn mac_libraries(binary: &Path, lib_dir: &Path) -> Result<String> {
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

// ---------------------------------------------------------------------------------------------------------------
// verify

/// Unpacks an archive into `destination` with every bound and identity check, and returns its root and inventory.
fn unpack_checked(archive: &Path, destination: &Path) -> Result<(PathBuf, Value)> {
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
    if fields
        != BTreeSet::from([
            "schema",
            "kind",
            "target",
            "source_sha",
            "entrypoint",
            "files",
        ])
    {
        return Err("wrong inventory fields".into());
    }
    if inventory["schema"] != 1
        || inventory["kind"] != KIND
        || inventory["entrypoint"] != ENTRYPOINT
    {
        return Err("wrong native kind or entrypoint".into());
    }
    let target = inventory["target"].as_str().unwrap_or("");
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

fn validate_name(name: &str) -> Result<()> {
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

/// Unpacks the archive somewhere else (a path with a space), runs the detector self-test from there, starts the
/// core, checks its ready file and health over the local socket, and checks it leaves nothing behind on SIGTERM.
fn verify(archive: &Path) -> Result<()> {
    let work = TempDir::new("sidevoice relocated tree")?;
    let (root, inventory) = unpack_checked(archive, &work.0)?;
    let binary = root.join(ENTRYPOINT);
    let models = root.join("models");
    let command = |args: &[&str]| {
        let mut command = Command::new(&binary);
        command
            .args(args)
            .env("RUSTVANI_CACHE_DIR", &models)
            .env("SIDEVOICE_STUN_URLS", "")
            .env_remove("ORT_DYLIB_PATH");
        command
    };
    if inventory["target"]
        .as_str()
        .unwrap_or("")
        .starts_with("linux-")
        && fs::metadata("/etc/ssl/certs/ca-certificates.crt")
            .map(|meta| meta.len())
            .unwrap_or(0)
            == 0
    {
        return Err("this Linux host has no ca-certificates trust bundle".into());
    }

    let wav = root.join("checks/detector-16k.wav");
    let result = command(&[
        "--self-test",
        wav.to_str().ok_or("path")?,
        models.to_str().ok_or("path")?,
    ])
    .output()
    .map_err(|error| format!("self-test: {error}"))?;
    if !result.status.success() {
        return Err(format!(
            "self-test failed: {}",
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    let report = parse_json(&result.stdout, "self-test")?;
    let detectors = &report["detectors"];
    if detectors["sample_rate"] != 16000
        || detectors["max_voice_confidence"].as_f64().unwrap_or(0.0) <= 0.5
        || detectors["smart_turn_complete"] != true
    {
        return Err(format!("detector self-test failed: {detectors}"));
    }
    if report["opus_decoded_samples"] != 320 {
        return Err("Opus decode self-test failed".into());
    }

    let data = work.0.join("private state");
    mkdir(&data)?;
    chmod(&data, 0o700)?;
    let launch_id = "relocated-native";
    let log = work.0.join("core.stderr");
    let mut core = command(&[
        "--data-dir",
        data.to_str().ok_or("path")?,
        "--port",
        "0",
        "--launch-id",
        launch_id,
        "--idle-exit",
        "0",
    ])
    .stdout(Stdio::null())
    .stderr(fs::File::create(&log).map_err(|error| error.to_string())?)
    .spawn()
    .map_err(|error| format!("start the core: {error}"))?;
    let tail = || {
        let text = fs::read_to_string(&log).unwrap_or_default();
        text[text.len().saturating_sub(3000)..].to_string()
    };
    let checked = (|| -> Result<Value> {
        let deadline = Instant::now() + Duration::from_secs(40);
        while !data.join("core.json").exists() {
            if Instant::now() > deadline {
                return Err(format!("the core wrote no core.json: {}", tail()));
            }
            if core
                .try_wait()
                .map_err(|error| error.to_string())?
                .is_some()
            {
                return Err(format!("the core exited: {}", tail()));
            }
            sleep(Duration::from_millis(100));
        }
        let ready = parse_json(&read(&data.join("core.json"))?, "core.json")?;
        let mut socket = UnixStream::connect(data.join("local.sock"))
            .map_err(|error| format!("local.sock: {error}"))?;
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|error| error.to_string())?;
        socket
            .write_all(b"GET /api/local/health HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .map_err(|error| error.to_string())?;
        let mut response = Vec::new();
        socket
            .read_to_end(&mut response)
            .map_err(|error| error.to_string())?;
        let response = String::from_utf8_lossy(&response).into_owned();
        let (head, body) = response
            .split_once("\r\n\r\n")
            .ok_or("malformed health response")?;
        if !head.starts_with("HTTP/1.0 200") && !head.starts_with("HTTP/1.1 200") {
            return Err(format!("health: {head}"));
        }
        let health = parse_json(body.as_bytes(), "health")?;
        let pid = core.id();
        if ready["launch_id"] != launch_id || health["launch_id"] != launch_id {
            return Err("ready/health launch identity mismatch".into());
        }
        if ready["pid"] != pid
            || health["pid"] != pid
            || health["fingerprint"].as_str().unwrap_or("").is_empty()
        {
            return Err("ready/health process or node identity mismatch".into());
        }
        Ok(health)
    })();
    let _ = Command::new("kill")
        .args(["-TERM", &core.id().to_string()])
        .status();
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = core.try_wait().map_err(|error| error.to_string())? {
            break Some(status);
        }
        if Instant::now() > deadline {
            let _ = core.kill();
            break None;
        }
        sleep(Duration::from_millis(100));
    };
    let health = checked?;
    if !status.is_some_and(|status| status.success()) {
        return Err(format!("core shutdown failed: {}", tail()));
    }
    if data.join("core.json").exists() || data.join("local.sock").exists() {
        return Err("the core left its ready file or socket after shutdown".into());
    }
    println!(
        "{}",
        json!({"target": inventory["target"], "source_sha": inventory["source_sha"], "detectors": detectors,
               "opus_decoded_samples": report["opus_decoded_samples"], "fingerprint": health["fingerprint"],
               "files": inventory["files"].as_array().map(Vec::len), "relocation": true})
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// manifest

fn manifest(dir: &Path, tag: Option<&str>) -> Result<()> {
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

// ---------------------------------------------------------------------------------------------------------------
// publish

/// The signer every asset must carry: the reusable build workflow on main, for nightlies and releases alike.
fn signer() -> Result<String> {
    let repository = env::var("GH_REPO").map_err(|_| "GH_REPO is not set")?;
    Ok(format!(
        "https://github.com/{repository}/.github/workflows/build.yml@refs/heads/main"
    ))
}

fn gh(args: &[&str]) -> Result<String> {
    output("gh", args, None)
}

fn publish(dir: &Path, tag: &str) -> Result<()> {
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
