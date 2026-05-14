//! Regenerate `docs/gen/*.json` — validated cross-reference data for the
//! typst spec build (`docs/lib/refs.typ` asserts membership against these).

use std::{collections::BTreeSet, fs, path::Path};

use anyhow::Result;
use regex::Regex;
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
    // Capture is anchored to `rio_` (observability.md naming convention)
    // rather than `[^"]+`: rio-scheduler/src/sla/metrics.rs contains
    // literal `"describe_counter!("` strings in its own self-test that
    // would otherwise produce garbage captures.
    let re = Regex::new(r#"describe_(?:counter|gauge|histogram)!\s*\(\s*"(rio_[a-zA-Z0-9_]+)""#)?;
    let mut names = BTreeSet::new();
    // Scan rio-*/src/**/*.rs only — the workspace members list is the
    // ground truth for which crates ship metrics.
    for entry in fs::read_dir(repo_root())? {
        let crate_dir = entry?.path();
        if !crate_dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("rio-"))
        {
            continue;
        }
        let src = crate_dir.join("src");
        if src.is_dir() {
            visit_rs(&src, &mut |body| {
                names.extend(re.captures_iter(body).map(|c| c[1].to_string()));
            })?;
        }
    }
    Ok(json!({"names": names.into_iter().collect::<Vec<_>>()}))
}

/// Recurse `dir`, calling `f` with the body of every `.rs` file.
fn visit_rs(dir: &Path, f: &mut impl FnMut(&str)) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let p = entry?.path();
        if p.is_dir() {
            visit_rs(&p, f)?;
        } else if p.extension().is_some_and(|x| x == "rs") {
            f(&fs::read_to_string(&p)?);
        }
    }
    Ok(())
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
