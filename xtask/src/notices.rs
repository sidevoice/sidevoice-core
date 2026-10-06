//! The licence notices an archive carries: every Rust dependency's licence texts and the pinned upstream ones.

use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use crate::util::*;
use crate::Result;

pub(crate) const LICENSES: [(&str, &str, &str); 4] = [
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
pub(crate) const ORT_SYS_VERSION: &str = "2.0.0-rc.10";
pub(crate) const RUSTVANI_REVISION: &str = "d01f33e671f7a4d8a128e7bfe55dbf0e8963cb21";

pub(crate) fn stage_notices(notices: &Path, target: &str, pins: &Value) -> Result<()> {
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
