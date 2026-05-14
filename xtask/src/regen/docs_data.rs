//! Regenerate `docs/gen/*.json` — validated cross-reference data for the
//! typst spec build (`docs/lib/refs.typ` asserts membership against these).

use std::{fs, path::Path};

use anyhow::Result;
use serde_json::json;

use crate::sh::repo_root;

pub async fn run() -> Result<()> {
    let out = repo_root().join("docs/gen");
    fs::create_dir_all(&out)?;
    write(&out, "metrics.json", &metrics()?)?;
    write(&out, "alerts.json", &alerts()?)?;
    write(&out, "errors.json", &errors()?)?;
    write(&out, "config.json", &config()?)?;
    println!("wrote docs/gen/{{metrics,alerts,errors,config}}.json");
    Ok(())
}

fn write(dir: &Path, name: &str, v: &serde_json::Value) -> Result<()> {
    fs::write(dir.join(name), serde_json::to_string_pretty(v)? + "\n")?;
    Ok(())
}

fn metrics() -> Result<serde_json::Value> {
    Ok(json!({"names": []})) // C2 fills
}

fn alerts() -> Result<serde_json::Value> {
    Ok(json!({"names": []})) // C3 fills
}

fn errors() -> Result<serde_json::Value> {
    Ok(json!({"variants": []})) // C4 (next batch)
}

fn config() -> Result<serde_json::Value> {
    Ok(json!({"components": {}})) // C5 (next batch)
}
