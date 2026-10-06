//! `cargo xtask verify ARCHIVE`: unpack it elsewhere and start the core from there.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::archive::unpack_checked;
use crate::util::*;
use crate::{Result, ENTRYPOINT};

/// Unpacks the archive somewhere else (a path with a space), runs the detector self-test from there, starts the
/// core, checks its ready file and health over the local socket, and checks it leaves nothing behind on SIGTERM.
pub(crate) fn verify(archive: &Path) -> Result<()> {
    // Under /tmp, not $TMPDIR: on macOS that path is so long a Unix socket in it exceeds the 104-byte limit.
    let work = TempDir::new_in(Path::new("/tmp"), "sidevoice relocated tree")?;
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
