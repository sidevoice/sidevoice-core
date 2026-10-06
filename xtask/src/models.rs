//! `cargo xtask models [DIR]`: stage the detector models pinned in `assets/rust-models.json`.

use std::fs;
use std::path::Path;

use crate::util::*;
use crate::Result;

pub(crate) fn models(dir: &Path) -> Result<()> {
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
