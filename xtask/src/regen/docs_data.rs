//! Regenerate `docs/gen/*.json` — validated cross-reference data for the
//! typst spec build (`docs/lib/refs.typ` asserts membership against these).

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result};
use regex::Regex;
use serde_json::json;

use crate::sh::repo_root;

/// `#[error("msg" ...)] Ident`. `(?s)` so the gap between the attr's
/// `)]` and the variant ident may span newlines (rustfmt wraps long
/// `#[error(...)]` bodies). The message capture handles `\"` and `\\`
/// escapes (rio-nix/src/derivation/mod.rs has `expected '\"'`).
/// Intervening attrs between `#[error]` and the ident aren't seen in
/// this codebase (`#[cfg]` always precedes `#[error]`); if one shows
/// up the variant is silently skipped, which is acceptable for a doc
/// cross-ref table.
const VARIANT_RE: &str = r#"(?s)#\[error\(\s*r?"((?:[^"\\]|\\.)*)"[^\]]*\]\s*([A-Z]\w*)"#;

/// `describe_{counter,gauge,histogram}!("rio_X_...", [Unit::*, ]"help")`.
/// Kind from the macro name; the metrics-crate `Unit` arg is optional
/// (sla/metrics.rs uses the 3-arg form for `_prediction_ratio`); help
/// from the trailing string literal (same `\"`/`\\`-aware capture and
/// `unescape_rust_str` postprocess as `VARIANT_RE`). The `rio_` anchor
/// avoids the literal `"describe_counter!("` strings rio-scheduler's
/// metrics self-test contains.
const METRICS_RE: &str = r#"(?s)describe_(counter|gauge|histogram)!\s*\(\s*"(rio_[a-zA-Z0-9_]+)"\s*,\s*(?:Unit::\w+\s*,\s*)?"((?:[^"\\]|\\.)*)""#;

/// Unescape a rust-source string-literal capture (the bytes BETWEEN
/// the surrounding `"`s as the regex sees them). Handles `\"`, `\\`,
/// `\n`, `\t`, and the rustfmt line-continuation `\<newline>`. Unknown
/// escapes pass through verbatim.
fn unescape_rust_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\n') => {} // line-continuation: \<LF> → ∅
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

pub async fn run() -> Result<()> {
    let out = repo_root().join("docs/gen");
    fs::create_dir_all(&out)?;
    write(&out, "metrics.json", &metrics()?)?;
    write(&out, "alerts.json", &alerts()?)?;
    write(&out, "errors.json", &errors()?)?;
    write(&out, "config.json", &config()?)?;
    write(&out, "workspace.json", &workspace()?)?;
    write(&out, "consts.json", &consts()?)?;
    write(&out, "helm-ns.json", &helm_ns()?)?;
    println!("wrote docs/gen/{{metrics,alerts,errors,config,workspace,consts,helm-ns}}.json");
    Ok(())
}

fn write(dir: &Path, name: &str, v: &serde_json::Value) -> Result<()> {
    fs::write(dir.join(name), serde_json::to_string_pretty(v)? + "\n")?;
    Ok(())
}

fn metrics() -> Result<serde_json::Value> {
    let re = Regex::new(METRICS_RE)?;
    // First describe_*! wins per name (some metrics are described in
    // multiple crates' lib.rs for nextest's per-crate spec floor).
    let mut seen = BTreeMap::<String, serde_json::Value>::new();
    visit_rio_crates(&mut |_crate, body| {
        for c in re.captures_iter(body) {
            let name = c[2].to_string();
            seen.entry(name.clone()).or_insert_with(|| {
                let help = unescape_rust_str(&c[3])
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                json!({"name": name, "kind": &c[1], "help": help})
            });
        }
    })?;
    // Group by `rio_{component}_` prefix. by_component is what
    // ref/metrics.typ iterates; flat `names` kept for the existing
    // refs.metric membership assert + docs-lint prefix derivation.
    let mut by_comp = BTreeMap::<String, Vec<serde_json::Value>>::new();
    for (name, m) in &seen {
        let comp = name
            .strip_prefix("rio_")
            .and_then(|s| s.split_once('_'))
            .map(|(c, _)| c.to_string())
            .unwrap_or_else(|| "misc".into());
        by_comp.entry(comp).or_default().push(m.clone());
    }
    Ok(json!({
        "names": seen.keys().cloned().collect::<Vec<_>>(),
        "by_component": by_comp,
    }))
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
    let variant_re = Regex::new(VARIANT_RE)?;
    let mut variants = Vec::new();
    visit_rio_crates(&mut |crate_name, body| {
        for em in enum_re.captures_iter(body) {
            let enum_name = em[1].to_string();
            for vm in variant_re.captures_iter(&em[2]) {
                let msg = unescape_rust_str(&vm[1])
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

fn workspace() -> Result<serde_json::Value> {
    // Parse via the toml crate, not regex: regex section-scraping
    // consumes the next `[` and silently skips [dev-dependencies] for
    // 11/13 crates. Not `cargo metadata`: docsData runs the
    // crate2nix-built xtask in a sandbox without cargo. workspace-hack
    // is the hakari stub (zeroed in nix/crate2nix.nix); excluded so
    // refs.crate-list() doesn't render it as a real crate.
    let root: toml::Table = fs::read_to_string(repo_root().join("Cargo.toml"))?.parse()?;
    let member_names: Vec<String> = root["workspace"]["members"]
        .as_array()
        .context("no [workspace] members")?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .filter(|n| n != "workspace-hack")
        .collect();

    // rio-* internal deps from a deps table. Only workspace-internal
    // edges (rio-*, xtask) — third-party deps are noise for the graph.
    let internal = |t: &toml::Table| -> BTreeSet<String> {
        t.keys()
            .filter(|k| k.starts_with("rio-") || *k == "xtask")
            .cloned()
            .collect()
    };

    let mut members = Vec::new();
    let mut deps = BTreeMap::<String, serde_json::Value>::new();
    for name in &member_names {
        let t: toml::Table =
            fs::read_to_string(repo_root().join(name).join("Cargo.toml"))?.parse()?;
        let desc = t
            .get("package")
            .and_then(|p| p.get("description"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        members.push(json!({"name": name, "description": desc}));

        let mut prod = BTreeSet::<String>::new();
        let mut dev = BTreeSet::<String>::new();
        if let Some(d) = t.get("dependencies").and_then(|v| v.as_table()) {
            prod.extend(internal(d));
        }
        if let Some(d) = t.get("dev-dependencies").and_then(|v| v.as_table()) {
            dev.extend(internal(d));
        }
        // [target.<cfg>.dependencies] — rio-test-support has a
        // cfg(target_os="linux") block.
        if let Some(tg) = t.get("target").and_then(|v| v.as_table()) {
            for cfg in tg.values() {
                if let Some(d) = cfg.get("dependencies").and_then(|v| v.as_table()) {
                    prod.extend(internal(d));
                }
                if let Some(d) = cfg.get("dev-dependencies").and_then(|v| v.as_table()) {
                    dev.extend(internal(d));
                }
            }
        }
        // Dep in both renders as solid only (rio-scheduler has
        // rio-store in both: schema feature prod, test-utils dev).
        let dev: Vec<_> = dev.difference(&prod).cloned().collect();
        deps.insert(
            name.clone(),
            json!({"prod": prod.into_iter().collect::<Vec<_>>(), "dev": dev}),
        );
    }
    Ok(json!({"members": members, "deps": deps}))
}

/// Doc-referenced rust consts. Curated allowlist, NOT a full scrape:
/// each entry is a const the spec book cites by value at ≥2 prose
/// sites, so per §Nth-strike it must derive. xtask fails if the regex
/// finds no match at the named file (catches the const moving/rename).
fn consts() -> Result<serde_json::Value> {
    const TABLE: &[(&str, &str)] = &[
        ("MAX_RECONNECT", "rio-gateway/src/handler/build.rs"),
        // Add more as docs cite them. Threshold: cited at ≥2 prose
        // sites, value is a plain integer literal.
    ];
    let mut out = serde_json::Map::new();
    for (name, path) in TABLE {
        let body = fs::read_to_string(repo_root().join(path))?;
        let re = Regex::new(&format!(r"const\s+{name}\s*:\s*\w+\s*=\s*(\d+)"))?;
        let v: u64 = re
            .captures(&body)
            .with_context(|| format!("const {name} not found at {path}"))?[1]
            .parse()?;
        out.insert((*name).into(), json!(v));
    }
    Ok(json!(out))
}

fn helm_ns() -> Result<serde_json::Value> {
    // namespaces: block from helm values.yaml — keyed by role
    // (system/store/builders/fetchers); re-key by .name so typst
    // prose says #(refs.psa)("rio-system").
    #[derive(serde::Deserialize)]
    struct Ns {
        name: String,
        psa: String,
    }
    #[derive(serde::Deserialize)]
    struct Values {
        namespaces: BTreeMap<String, serde_yml::Value>,
    }
    let body = fs::read_to_string(repo_root().join("infra/helm/rio-build/values.yaml"))?;
    let v: Values = serde_yml::from_str(&body)?;
    let mut out = serde_json::Map::new();
    for (role, raw) in v.namespaces {
        if role == "create" {
            continue; // `create: true` flag, not a namespace entry
        }
        let ns: Ns = serde_yml::from_value(raw)?;
        out.insert(ns.name, json!({"psa": ns.psa, "role": role}));
    }
    Ok(json!(out))
}

fn config() -> Result<serde_json::Value> {
    let mut components = serde_json::Map::new();
    macro_rules! component {
        ($name:literal, $ty:ty) => {{
            let schema = schemars::schema_for!($ty);
            let defaults = serde_json::to_value(<$ty>::default())?;
            components.insert($name.into(), flatten_schema(schema, &defaults));
        }};
    }
    component!("gateway", rio_gateway::config::Config);
    component!("scheduler", rio_scheduler::config::Config);
    component!("store", rio_store::config::Config);
    component!("builder", rio_builder::config::Config);
    component!("controller", rio_controller::config::Config);
    Ok(json!({"components": components}))
}

/// Walk a JSON Schema's `properties` into a flat list of
/// `{key, type, default, description}` rows for the typst config
/// reference. Nested objects (`UpstreamAddrs`, `JwtConfig`, …) flatten
/// as `parent.child` keys; `#[serde(flatten)]` (CommonConfig) inlines
/// at the parent level (schemars already does that). `defaults` is
/// `serde_json::to_value(Config::default())` — schemars doesn't
/// capture `#[serde(default)]` values, so they're zipped in by key.
fn flatten_schema(schema: schemars::Schema, defaults: &serde_json::Value) -> serde_json::Value {
    let root = schema.as_value();
    // schemars 1.x puts referenced sub-schemas under `$defs` keyed by
    // the bare type name (e.g., `UpstreamAddrs`). `$ref` values are
    // `#/$defs/<name>`.
    let defs = root
        .pointer("/$defs")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let mut rows = Vec::new();
    walk_props(root, defaults, &defs, "", &mut rows);
    serde_json::Value::Array(rows)
}

fn walk_props(
    schema: &serde_json::Value,
    defaults: &serde_json::Value,
    defs: &serde_json::Map<String, serde_json::Value>,
    prefix: &str,
    rows: &mut Vec<serde_json::Value>,
) {
    let Some(props) = schema.pointer("/properties").and_then(|v| v.as_object()) else {
        return;
    };
    // Preserve declaration order: schemars 1.x walks struct fields in
    // source order and serde_json's Map is insertion-ordered (we don't
    // enable `preserve_order` but schemars does for its own output).
    for (key, prop) in props {
        let dotted = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        let default = defaults
            .get(key)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        // Resolve `$ref` to the underlying `$defs` entry. Chase one
        // level — schemars doesn't emit transitive refs for our types.
        let (resolved, ref_name) = match prop.get("$ref").and_then(|v| v.as_str()) {
            Some(r) => {
                let name = r.trim_start_matches("#/$defs/");
                (defs.get(name).unwrap_or(prop), Some(name.to_owned()))
            }
            None => (prop, None),
        };
        // Nested struct (has its own `properties`) → recurse with
        // dotted prefix. Exception: tagged enums (`oneOf`) and arrays
        // stay as a single row.
        if resolved.get("properties").is_some() && resolved.get("oneOf").is_none() {
            walk_props(resolved, &default, defs, &dotted, rows);
            continue;
        }
        rows.push(json!({
            "key": dotted,
            "type": describe_type(resolved, ref_name.as_deref()),
            "default": render_default(&default),
            "description": prop
                .get("description")
                .or_else(|| resolved.get("description"))
                .and_then(|v| v.as_str())
                .map(first_sentence)
                .unwrap_or_default(),
        }));
    }
}

/// Human-readable type name for the docs table. Prefers the `$ref`
/// target's struct name (e.g., `ChunkBackendKind`) over generic
/// `object`; renders `Option<T>` (schemars: `type: ["T","null"]` or
/// `anyOf: [T, null]`) as the inner T.
fn describe_type(schema: &serde_json::Value, ref_name: Option<&str>) -> String {
    // anyOf: [<ref-or-type>, {type:null}] — Option<NestedStruct>.
    if let Some(any) = schema.get("anyOf").and_then(|v| v.as_array()) {
        let non_null: Vec<_> = any
            .iter()
            .filter(|v| v.get("type").and_then(|t| t.as_str()) != Some("null"))
            .collect();
        if let [inner] = non_null[..] {
            let inner_ref = inner
                .get("$ref")
                .and_then(|v| v.as_str())
                .map(|r| r.trim_start_matches("#/$defs/"));
            return describe_type(inner, inner_ref);
        }
    }
    if let Some(name) = ref_name {
        return name.to_string();
    }
    if schema.get("oneOf").is_some() {
        return "tagged enum".into();
    }
    match schema.get("type") {
        Some(serde_json::Value::String(t)) => match t.as_str() {
            "array" => {
                let item = schema
                    .pointer("/items/type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("any");
                format!("list<{item}>")
            }
            "integer" => schema
                .get("format")
                .and_then(|v| v.as_str())
                .unwrap_or("integer")
                .to_string(),
            "number" => schema
                .get("format")
                .and_then(|v| v.as_str())
                .unwrap_or("number")
                .to_string(),
            other => other.to_string(),
        },
        // type: ["string","null"] — Option<String> etc.
        Some(serde_json::Value::Array(ts)) => ts
            .iter()
            .filter_map(|v| v.as_str())
            .find(|&t| t != "null")
            .unwrap_or("any")
            .to_string(),
        _ => "object".into(),
    }
}

/// Compact JSON for the Default column. Empty string / null → blank
/// (rendered as `(required)` or `(unset)` in typst per the prose).
/// Object defaults are key-sorted via BTreeMap so the rendered string
/// is deterministic regardless of serde_json's `preserve_order`
/// feature (workspace-unified differently between cargo and crate2nix
/// — the docs-data-fresh check would otherwise see drift on a
/// HashMap-backed default like `{"fetcher-*":600,"*":60}`).
fn render_default(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) if s.is_empty() => String::new(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(a) if a.is_empty() => "[]".into(),
        serde_json::Value::Object(o) if o.values().all(is_emptyish) => String::new(),
        serde_json::Value::Object(o) => {
            let sorted: BTreeMap<_, _> = o.iter().collect();
            serde_json::to_string(&sorted).unwrap()
        }
        other => other.to_string(),
    }
}

fn is_emptyish(v: &serde_json::Value) -> bool {
    matches!(v, serde_json::Value::Null)
        || matches!(v, serde_json::Value::String(s) if s.is_empty())
}

/// Doc comments are paragraphs of design rationale; the reference
/// table wants the one-line summary. Take the first sentence (up to
/// the first `. ` followed by an uppercase letter, or the whole
/// string if no sentence break). Intra-doc links `[foo]` are kept
/// as-is — typst renders square brackets literally.
fn first_sentence(desc: &str) -> String {
    let collapsed = desc.split_whitespace().collect::<Vec<_>>().join(" ");
    // Find ". " followed by uppercase (real sentence boundary, not
    // "e.g. foo" or "1.5").
    let bytes = collapsed.as_bytes();
    for i in 0..bytes.len().saturating_sub(2) {
        if bytes[i] == b'.' && bytes[i + 1] == b' ' && bytes[i + 2].is_ascii_uppercase() {
            return collapsed[..=i].to_string();
        }
    }
    collapsed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_rust_str_handles_all_escapes() {
        assert_eq!(unescape_rust_str(r#"a \"q\" b"#), r#"a "q" b"#);
        assert_eq!(unescape_rust_str(r"a\\b"), r"a\b");
        assert_eq!(unescape_rust_str("a \\\n  b"), "a   b"); // line-cont
        assert_eq!(unescape_rust_str(r"\{0}"), r"\{0}"); // unknown passes through
    }

    #[test]
    fn variant_re_captures_then_unescape() {
        let re = Regex::new(VARIANT_RE).unwrap();
        let src = r#"
            #[error("expected '\"' to start string")]
            ExpectedStringStart,
            #[error("plain")]
            Plain,
        "#;
        let caps: Vec<_> = re
            .captures_iter(src)
            .map(|c| (unescape_rust_str(&c[1]), c[2].to_string()))
            .collect();
        assert_eq!(
            caps,
            vec![
                (
                    r#"expected '"' to start string"#.into(),
                    "ExpectedStringStart".into()
                ),
                ("plain".into(), "Plain".into()),
            ]
        );
    }

    #[test]
    fn metrics_re_handles_both_arg_forms() {
        let re = Regex::new(METRICS_RE).unwrap();
        let src = r#"
            describe_histogram!(
                "rio_scheduler_sla_prediction_ratio",
                Unit::Count,
                "actual/predicted, by dim"
            );
            describe_gauge!("rio_gateway_connections_active", "currently active");
        "#;
        let caps: Vec<_> = re
            .captures_iter(src)
            .map(|c| (c[1].to_string(), c[2].to_string()))
            .collect();
        assert_eq!(
            caps,
            vec![
                (
                    "histogram".into(),
                    "rio_scheduler_sla_prediction_ratio".into()
                ),
                ("gauge".into(), "rio_gateway_connections_active".into()),
            ]
        );
    }
}
