//! `cargo xtask verify ARCHIVE`: unpack it elsewhere and start the core from there.
//! `cargo xtask verify-floor ARCHIVE`: the same, starting the core on the oldest Linux it supports.

use std::env;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::thread::sleep;
use std::time::{Duration, Instant};

use interprocess::local_socket::{prelude::*, GenericFilePath, Stream};
use serde_json::{json, Value};
use sidevoice_platform::stop::{StopRequest, StopRequests};

use crate::archive::unpack_checked;
use crate::glibc;
use crate::libraries::{windows_libraries, LINUX_SYSTEM};
use crate::util::*;
use crate::{entrypoint, Result, ENTRYPOINT_STEM};

/// Where `verify-floor` mounts the unpacked archive, and the xtask that starts it, in the container.
const CONTAINER_ROOT: &str = "/opt/sidevoice-core-rust";
const CONTAINER_XTASK: &str = "/opt/sidevoice-xtask";

/// Where a Linux system keeps its CA bundle: Debian and Ubuntu, then RHEL, AlmaLinux and Fedora.
const CA_BUNDLES: [&str; 2] = [
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
];

/// Unpacks the archive somewhere else (a path with a space), checks on Linux that the binary needs nothing but the
/// system's libraries and no glibc newer than the floor the inventory records, then starts the core from there,
/// checks its ready file and health over the local socket, and checks it leaves nothing behind on SIGTERM.
///
/// With `image` (a container image of this machine's architecture, whose glibc must be the inventory's floor), the
/// start runs in that container instead: the unpacked tree mounted read-only, no network, driven
/// by this xtask built against the floor.
pub(crate) fn verify(archive: &Path, image: Option<&str>) -> Result<()> {
    let work = TempDir::new_in(&scratch_base(), "sidevoice relocated tree")?;
    let (root, inventory) = unpack_checked(archive, &work.0)?;
    let target = inventory["target"].as_str().unwrap_or("");
    let entrypoint = entrypoint(target);
    if target != host_target()? {
        return Err(format!(
            "the archive is for {target}, this machine is {}",
            host_target()?
        ));
    }
    // A Windows binary may import only the system's DLLs, and not the Visual C++ runtime (it links it statically).
    let imports = if target.starts_with("windows-") {
        Some(windows_libraries(&root.join(&entrypoint))?)
    } else {
        None
    };
    let floor = inventory["glibc"].as_str();
    let needed = match floor {
        Some(floor) => Some(check_floor(&root, floor)?),
        None => None,
    };
    let started = match image {
        Some(image) => {
            let floor = floor.ok_or("the archive records no glibc floor to run it on")?;
            in_container(image, &root, floor)?
        }
        None => start(&root, &entrypoint)?,
    };
    println!(
        "{}",
        json!({"target": inventory["target"], "source_sha": inventory["source_sha"], "started": started,
               "glibc": {"floor": floor, "needed": needed}, "windows_imports": imports, "ran_in": image,
               "files": inventory["files"].as_array().map(Vec::len), "relocation": true})
    );
    Ok(())
}

/// The binary may name only the system's libraries, and may not need a glibc newer than `floor`. Returns the newest
/// glibc it needs.
fn check_floor(root: &Path, floor: &str) -> Result<String> {
    let path = root.join(ENTRYPOINT_STEM);
    let path = path.to_str().ok_or("path")?;
    for name in glibc::needed_libraries(path)? {
        if !LINUX_SYSTEM.contains(&name.as_str()) {
            return Err(format!(
                "{ENTRYPOINT_STEM} needs {name}, which is not on every system"
            ));
        }
    }
    let needed = glibc::needed(path)?;
    glibc::check(ENTRYPOINT_STEM, &needed, floor)?;
    Ok(glibc::newest(String::from("2.0"), needed))
}

/// `verify-tree ROOT`: the start of an already unpacked and checked tree, as `verify-floor` runs
/// them in its container.
pub(crate) fn verify_tree(root: &Path) -> Result<()> {
    println!("{}", start(root, &entrypoint(host_target()?))?);
    Ok(())
}

/// Checks the container's glibc is `floor`, builds this xtask against the floor, and runs `verify-tree` in a
/// throwaway container of `image` with the unpacked `root` mounted read-only and no network.
fn in_container(image: &str, root: &Path, floor: &str) -> Result<Value> {
    let docker = |args: &[&str]| -> Result<String> {
        let mut command = Command::new("docker");
        command
            .args(["run", "--rm", "--network", "none", "--mount"])
            .arg(format!(
                "type=bind,source={},target={CONTAINER_ROOT},readonly",
                root.to_str().ok_or("path")?
            ));
        command.args(args);
        let result = command
            .output()
            .map_err(|error| format!("docker: {error}"))?;
        if !result.status.success() {
            return Err(format!(
                "docker run {image}: {}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            ));
        }
        Ok(String::from_utf8_lossy(&result.stdout).into_owned())
    };
    let listing = docker(&[image, "ldd", "--version"])?;
    let present = listing
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().last())
        .unwrap_or("");
    if present != floor {
        return Err(format!(
            "{image} has glibc {present:?}, not the floor {floor}"
        ));
    }
    let xtask = floor_xtask()?;
    let mount = format!(
        "type=bind,source={},target={CONTAINER_XTASK},readonly",
        xtask.to_str().ok_or("path")?
    );
    let report = docker(&[
        "--mount",
        &mount,
        image,
        CONTAINER_XTASK,
        "verify-tree",
        CONTAINER_ROOT,
    ])?;
    parse_json(report.trim().as_bytes(), "verify-tree")
}

/// This xtask, built with `cargo zigbuild` against the glibc floor so it runs in the floor's container.
fn floor_xtask() -> Result<PathBuf> {
    let triple = glibc::host_triple()?;
    let manifest = repo().join("xtask/Cargo.toml");
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let target = format!("{triple}.{}", glibc::FLOOR);
    let status = Command::new(&cargo)
        .args(["zigbuild", "--locked", "--quiet", "--manifest-path"])
        .arg(&manifest)
        .args(["--target", &target])
        .status()
        .map_err(|error| format!("cargo zigbuild: {error}"))?;
    if !status.success() {
        return Err("cargo zigbuild of the xtask failed".into());
    }
    Ok(repo().join("xtask/target").join(triple).join("debug/xtask"))
}

/// `GET /api/local/health` over the core's local channel at `endpoint` (a Unix socket path or a named pipe's name, as
/// its ready file records it): the raw response, read to the end, within 10 seconds.
fn get_health(endpoint: &Path) -> Result<Vec<u8>> {
    let endpoint = endpoint.to_str().ok_or("path")?.to_string();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let fetched = (|| -> Result<Vec<u8>> {
            let name = endpoint
                .as_str()
                .to_fs_name::<GenericFilePath>()
                .map_err(|error| format!("{endpoint}: {error}"))?;
            let mut socket =
                Stream::connect(name).map_err(|error| format!("{endpoint}: {error}"))?;
            socket
                .write_all(b"GET /api/local/health HTTP/1.0\r\nHost: localhost\r\n\r\n")
                .map_err(|error| error.to_string())?;
            let mut response = Vec::new();
            // The server closes after answering. A pipe can report that as an error after the whole answer.
            match socket.read_to_end(&mut response) {
                Err(error) if response.is_empty() => Err(error.to_string()),
                _ => Ok(response),
            }
        })();
        let _ = sender.send(fetched);
    });
    receiver
        .recv_timeout(Duration::from_secs(10))
        .map_err(|_| "no health response within 10 seconds".to_string())?
}

/// Starts the core from the unpacked tree, checks its ready file and health over the local socket, and checks it
/// leaves nothing behind on SIGTERM.
fn start(root: &Path, entrypoint: &str) -> Result<Value> {
    // Private, as the connector's state root is: the core's log goes beside its data directory, and the core refuses a
    // log whose directory others can enter.
    let work = TempDir::new_in(&scratch_base(), "sidevoice relocated state")?;
    chmod(&work.0, 0o700)?;
    let binary = root.join(entrypoint);
    let command = |args: &[&str]| {
        let mut command = Command::new(&binary);
        command.args(args);
        command
    };
    if cfg!(target_os = "linux")
        && !CA_BUNDLES.iter().any(|path| {
            fs::metadata(path)
                .map(|meta| meta.len() > 0)
                .unwrap_or(false)
        })
    {
        return Err("this Linux host has no CA certificate bundle".into());
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
    let checked = (|| -> Result<(Value, PathBuf)> {
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
        // The socket is where the ready file says: in this user's runtime directory when there is one.
        let path = PathBuf::from(
            ready["endpoint"]
                .as_str()
                .ok_or("core.json names no endpoint")?,
        );
        let response = String::from_utf8_lossy(&get_health(&path)?).into_owned();
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
        Ok((health, path))
    })();
    // Asked to leave the way the OS does (Unix SIGTERM, Windows its stop event); a core that does not is killed below.
    let _ = StopRequests::ask(core.id());
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
    let (health, socket) = checked?;
    if !status.is_some_and(|status| status.success()) {
        return Err(format!("core shutdown failed: {}", tail()));
    }
    if data.join("core.json").exists() || socket.exists() {
        return Err("the core left its ready file or socket after shutdown".into());
    }
    Ok(json!({"fingerprint": health["fingerprint"]}))
}
