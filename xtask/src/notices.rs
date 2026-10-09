//! The licence notices an archive carries: every Rust dependency's licence texts.

use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use crate::util::*;
use crate::Result;

pub(crate) fn stage_notices(notices: &Path, target: &str) -> Result<()> {
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

    let linux = target.starts_with("linux-");
    write(
        &notices.join("native-dependencies.json"),
        &canonical(&json!({
            "system_trust_roots": if linux { "ca-certificates" } else { "macOS system trust store" },
        })),
    )
}
