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
    visit_rio_crates(&mut |_crate, body| {
        names.extend(re.captures_iter(body).map(|c| c[1].to_string()));
    })?;
    Ok(json!({"names": names.into_iter().collect::<Vec<_>>()}))
}

/// Walk every `rio-*/src/**/*.rs` file under the repo root, calling `f`
/// with `(crate_name, file_body)`. The `rio-*` prefix scan is the same
/// ground-truth filter the workspace `members` glob uses.
fn visit_rio_crates(f: &mut impl FnMut(&str, &str)) -> Result<()> {
    for entry in fs::read_dir(repo_root())? {
        let crate_dir = entry?.path();
        let Some(crate_name) = crate_dir
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| n.starts_with("rio-"))
            .map(str::to_owned)
        else {
            continue;
        };
        let src = crate_dir.join("src");
        if src.is_dir() {
            visit_rs(&src, &mut |body| f(&crate_name, body))?;
        }
    }
    Ok(())
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
    let body =
        fs::read_to_string(repo_root().join("infra/helm/rio-build/templates/prometheusrule.yaml"))?;
    let re = Regex::new(r"(?m)^\s*-\s*alert:\s*(\w+)")?;
    let names: BTreeSet<_> = re.captures_iter(&body).map(|c| c[1].to_string()).collect();
    Ok(json!({"names": names.into_iter().collect::<Vec<_>>()}))
}

fn errors() -> Result<serde_json::Value> {
    // Enum-block: `pub enum <Name>Error {` to the next `}` at column 0.
    // Error enums are always top-level items so the col-0 close brace is
    // unambiguous (struct-variant `}` are indented).
    let enum_re = Regex::new(r"(?ms)^pub enum (\w*Error)\s*\{(.*?)^\}")?;
    // Variant: `#[error("msg" ...)] Ident`. `(?s)` so the gap between the
    // attr's `)]` and the variant ident may span newlines (rustfmt wraps
    // long #[error(...)] bodies). One `[^"]*` for the message is fine —
    // thiserror format strings don't contain raw `"` (escapes go through
    // `{...:?}`). Intervening attrs between `#[error]` and the ident
    // aren't seen in this codebase (`#[cfg]` always precedes `#[error]`);
    // if one shows up the variant is silently skipped, which is acceptable
    // for a doc cross-ref table.
    let variant_re = Regex::new(r#"(?s)#\[error\(\s*r?"([^"]*)"[^\]]*\]\s*([A-Z]\w*)"#)?;
    let mut variants = Vec::new();
    visit_rio_crates(&mut |crate_name, body| {
        for em in enum_re.captures_iter(body) {
            let enum_name = em[1].to_string();
            for vm in variant_re.captures_iter(&em[2]) {
                // Collapse rustfmt line-continuation whitespace inside
                // multi-line #[error(...)] strings (e.g. `"foo \\\n  bar"`).
                let msg = vm[1]
                    .replace("\\\n", "")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                variants.push(json!({
                    "enum": enum_name,
                    "name": vm[2].to_string(),
                    "crate": crate_name,
                    "msg": msg,
                }));
            }
        }
    })?;
    variants.sort_by(|a, b| {
        let k = |v: &serde_json::Value| {
            (
                v["crate"].as_str().unwrap().to_owned(),
                v["enum"].as_str().unwrap().to_owned(),
                v["name"].as_str().unwrap().to_owned(),
            )
        };
        k(a).cmp(&k(b))
    });
    Ok(json!({"variants": variants}))
}

fn config() -> Result<serde_json::Value> {
    Ok(json!({"components": {}})) // C5 (next batch)
}
