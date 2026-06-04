//! Regenerate `docs/gen/*.json` — validated cross-reference data for the
//! typst spec build (`docs/lib/refs.typ` asserts membership against these).

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result};
use heck::ToLowerCamelCase;
use regex::Regex;
use serde_json::json;

use crate::sh::repo_root;

/// Optional `///` doc-block, optional intervening attrs (`#[cfg]`,
/// `#[allow]`), then `#[error("msg" ...)] Ident`. `(?ms)` so `^`
/// matches per-line for the doc/attr captures and `.` spans the
/// `)]`→ident newline (rustfmt wraps long `#[error(...)]` bodies). The
/// message capture handles `\"`/`\\` escapes (rio-nix/src/derivation/
/// mod.rs has `expected '\"'`). Captures: 1=doc-block, 2=msg, 3=ident.
/// bug_014: without the attr-skip group, a `#[cfg]` between `///` and
/// `#[error]` would orphan the doc capture (matched as zero-width).
/// bug_017: the `#[error(...)]` arg may be `transparent` instead of a
/// string literal — group 2 is then absent (`.get(2)`).
const VARIANT_RE: &str = r#"(?ms)((?:^\s*///[^\n]*\n)*)(?:^\s*#\[[^\]]+\]\s*\n)*\s*#\[error\((?:\s*r?"((?:[^"\\]|\\.)*)"[^\]]*|\s*transparent\s*)\)\]\s*([A-Z]\w*)"#;

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
    write(&out, "crds.json", &crds()?)?;
    write(&out, "modules.json", &modules()?)?;
    write(&out, "cli.json", &cli()?)?;
    write(&out, "protos.json", &protos()?)?;
    write(&out, "migrations.json", &migrations()?)?;
    println!(
        "wrote docs/gen/{{metrics,alerts,errors,config,workspace,consts,helm-ns,crds,modules,cli,protos,migrations}}.json"
    );
    Ok(())
}

fn write(dir: &Path, name: &str, v: &serde_json::Value) -> Result<()> {
    fs::write(dir.join(name), serde_json::to_string_pretty(v)? + "\n")?;
    Ok(())
}

/// The raw name → HELP map of every `describe_*!` callsite — shared
/// with `regen helm-obs`, which renders the SAME canonical strings
/// into the chart (bug_330: chart descriptions restated HELP by hand
/// and drifted; one scrape, two renderers).
pub(crate) fn metrics_help_map() -> Result<BTreeMap<String, String>> {
    let re = Regex::new(METRICS_RE)?;
    let mut seen = BTreeMap::<String, String>::new();
    visit_rio_crates(&mut |_crate, body| {
        for c in re.captures_iter(body) {
            seen.entry(c[2].to_string()).or_insert_with(|| {
                unescape_rust_str(&c[3])
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            });
        }
    })?;
    Ok(seen)
}

/// Scrape `describe_{counter,gauge,histogram}!` callsites into
/// `gen/metrics.json`. The `describe_*!()` help strings in each
/// component's `lib.rs::describe_metrics()` are the source of truth;
/// `docs/ref/metrics.typ` derives its tables from this output, NOT the
/// other way around (the typst migration inverted the pre-existing
/// "spec → describe" direction).
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

/// Alert inventory: names PLUS per-rule expr/for/severity/metrics
/// (merged_bug_001 class — runbooks restated alert exprs by hand and
/// drifted from the shipped PromQL; the runbook now RENDERS the expr
/// via `refs.alert-expr`, so a re-key propagates or `docs-data-fresh`
/// fails). Line-state machine over the template: helm `{{ }}` only
/// appears in annotations at tip (exprs are plain PromQL — verified),
/// and rule keys are physical lines, so no YAML parser is needed (the
/// file is a helm TEMPLATE and not parseable as YAML anyway).
fn alerts() -> Result<serde_json::Value> {
    let body =
        fs::read_to_string(repo_root().join("infra/helm/rio-build/templates/prometheusrule.yaml"))?;
    parse_alerts(&body)
}

/// Pure parser core of [`alerts`] — unit-tested directly (no repo
/// access; the fixture lives in the test).
fn parse_alerts(body: &str) -> Result<serde_json::Value> {
    let alert_re = Regex::new(r"^\s*-\s*alert:\s*(\w+)\s*$")?;
    let block_expr_re = Regex::new(r"^(\s*)expr:\s*[|>][+-]?\s*$")?;
    let inline_expr_re = Regex::new(r"^\s*expr:\s*(\S.*?)\s*$")?;
    let for_re = Regex::new(r"^\s*for:\s*(\S+)\s*$")?;
    let severity_re = Regex::new(r"^\s*severity:\s*(\S+)\s*$")?;
    let metric_re = Regex::new(r"\brio_[a-z0-9_]+")?;

    #[derive(Default)]
    struct Rule {
        name: String,
        expr: String,
        for_: String,
        severity: String,
    }
    let mut rules: Vec<Rule> = Vec::new();
    let mut cur: Option<Rule> = None;
    // Some(key-indent) while inside an `expr: |` block; lines indented
    // deeper belong to the expr, the first line at <= indent ends it.
    let mut block_indent: Option<usize> = None;
    for line in body.lines() {
        if let Some(indent) = block_indent {
            let trimmed = line.trim_start();
            if trimmed.is_empty() || line.len() - trimmed.len() > indent {
                if let (Some(r), false) = (cur.as_mut(), trimmed.is_empty()) {
                    if !r.expr.is_empty() {
                        r.expr.push(' ');
                    }
                    r.expr.push_str(trimmed);
                }
                continue;
            }
            block_indent = None; // falls through to keyed matches below
        }
        if let Some(c) = alert_re.captures(line) {
            if let Some(r) = cur.take() {
                rules.push(r);
            }
            cur = Some(Rule {
                name: c[1].to_string(),
                ..Default::default()
            });
        } else if let Some(c) = block_expr_re.captures(line) {
            block_indent = Some(c[1].len());
        } else if let Some(c) = inline_expr_re.captures(line) {
            if let Some(r) = cur.as_mut() {
                r.expr = c[1].to_string();
            }
        } else if let Some(c) = for_re.captures(line) {
            if let Some(r) = cur.as_mut() {
                r.for_ = c[1].to_string();
            }
        } else if let Some(c) = severity_re.captures(line) {
            if let Some(r) = cur.as_mut() {
                r.severity = c[1].to_string();
            }
        }
    }
    if let Some(r) = cur.take() {
        rules.push(r);
    }
    // Histogram series suffixes resolve to their base metric name (the
    // describe_histogram! name) — same normalization the obs-surface
    // lint applies.
    let strip = |m: &str| -> String {
        for suf in ["_bucket", "_sum", "_count"] {
            if let Some(base) = m.strip_suffix(suf) {
                return base.to_string();
            }
        }
        m.to_string()
    };
    let mut names: Vec<String> = rules.iter().map(|r| r.name.clone()).collect();
    names.sort();
    names.dedup();
    let mut rule_objs: Vec<serde_json::Value> = rules
        .iter()
        .map(|r| {
            let metrics: BTreeSet<String> = metric_re
                .find_iter(&r.expr)
                .map(|m| strip(m.as_str()))
                .collect();
            json!({
                "name": r.name,
                "expr": r.expr,
                "for": r.for_,
                "severity": r.severity,
                "metrics": metrics.into_iter().collect::<Vec<_>>(),
            })
        })
        .collect();
    rule_objs.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or_default()
            .cmp(b["name"].as_str().unwrap_or_default())
    });
    Ok(json!({"names": names, "rules": rule_objs}))
}

/// Migration-slug inventory (merged_bug_122 class — prose cited
/// migration NUMBERS, which the +2 renumber silently invalidated;
/// references carry the `NNN_slug` stem, validated against this
/// inventory by `refs.migration` and the docs-lint slug check).
fn migrations() -> Result<serde_json::Value> {
    let dir = repo_root().join("rio-migrations/migrations");
    let mut stems: Vec<String> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let p = entry?.path();
        if p.extension().is_some_and(|x| x == "sql") {
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                stems.push(stem.to_string());
            }
        }
    }
    stems.sort();
    Ok(json!({"stems": stems}))
}

/// Strip `^\s*///\s?` from each line of a captured doc-block, join,
/// whitespace-collapse. Shared by enum-level and variant-level `///`
/// captures.
fn strip_doc_prefix(s: &str) -> String {
    s.lines()
        .map(|l| l.trim_start().trim_start_matches("///").trim_start())
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn errors() -> Result<serde_json::Value> {
    // Enum-block: optional `///` doc, optional attrs (#[derive],
    // #[non_exhaustive]), `(?:pub\s+)?` so non-pub error enums are
    // scanned too (rio-gateway's StreamProcessError is private but
    // documented in ref/errors.typ). `}` at column 0 closes the enum —
    // error enums are always top-level items so this is unambiguous
    // (struct-variant `}` are indented). Captures: 1=enum-doc, 2=name,
    // 3=body.
    let enum_re = Regex::new(
        r"(?ms)((?:^\s*///[^\n]*\n)*)(?:^\s*#\[[^\]]+\]\s*\n)*^(?:pub\s+)?enum (\w*Error)\s*\{(.*?)^\}",
    )?;
    let variant_re = Regex::new(VARIANT_RE)?;
    let collapse = |s: String| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut variants = Vec::new();
    let mut enums = Vec::new();
    visit_rio_crates(&mut |crate_name, body| {
        for em in enum_re.captures_iter(body) {
            let enum_name = em[2].to_string();
            enums.push(json!({
                "name": enum_name,
                "crate": crate_name,
                "doc": strip_doc_prefix(&em[1]),
            }));
            for vm in variant_re.captures_iter(&em[3]) {
                let msg = vm
                    .get(2)
                    .map(|m| collapse(unescape_rust_str(m.as_str())))
                    .unwrap_or_else(|| "(transparent — delegates to inner error)".into());
                variants.push(json!({
                    "enum": enum_name,
                    "name": vm[3].to_string(),
                    "crate": crate_name,
                    "msg": msg,
                    "doc": strip_doc_prefix(&vm[1]),
                }));
            }
        }
    })?;
    let key = |ks: &'static [&str]| {
        move |v: &serde_json::Value| -> Vec<String> {
            ks.iter()
                .map(|k| v[k].as_str().unwrap().to_owned())
                .collect()
        }
    };
    variants.sort_by_key(key(&["crate", "enum", "name"]));
    enums.sort_by_key(key(&["crate", "name"]));
    Ok(json!({"variants": variants, "enums": enums}))
}

fn crds() -> Result<serde_json::Value> {
    // Scrape `#[kube(..., kind = "X", ...)]` and the following
    // `pub struct XSpec { pub field: ... }` from rio-crds. Same
    // regex-on-source approach as metrics()/errors() — docsData runs
    // without cargo so no kube-rs introspection.
    let kind_re = Regex::new(r#"kind\s*=\s*"(\w+)""#)?;
    let spec_re = Regex::new(r"(?ms)^pub struct (\w+)Spec\s*\{(.*?)^\}")?;
    let field_re = Regex::new(FIELD_RE)?;
    let mut kinds = BTreeSet::new();
    let mut fields = BTreeMap::<String, Vec<String>>::new();
    for entry in fs::read_dir(repo_root().join("rio-crds/src"))? {
        let p = entry?.path();
        if p.extension().is_none_or(|x| x != "rs") {
            continue;
        }
        let body = fs::read_to_string(&p)?;
        kinds.extend(kind_re.captures_iter(&body).map(|c| c[1].to_string()));
        for s in spec_re.captures_iter(&body) {
            let kind = s[1].to_string();
            // All *Spec structs use #[serde(rename_all = "camelCase")];
            // a per-field #[serde(rename = "...")] would diverge from
            // heck's output (e.g. providerID ≠ providerId). Fail loud
            // so future renames in a Spec body don't silently rot the
            // validator.
            anyhow::ensure!(
                !s[2].contains("#[serde(rename ="),
                "{kind}Spec has per-field #[serde(rename=...)]; crds() \
                 camelCase conversion would mis-render — handle explicitly"
            );
            let fs: Vec<_> = field_re
                .captures_iter(&s[2])
                .map(|c| c[1].trim_end_matches('_').to_lower_camel_case())
                .collect();
            fields.insert(kind, fs);
        }
    }
    Ok(json!({"kinds": kinds.into_iter().collect::<Vec<_>>(), "fields": fields}))
}

/// `pub <ident>:` field, raw-ident-aware. `(?:r#)?` is *inside* the
/// regex (`\w` can't span `#`), so the capture is the bare ident.
/// bug_004: the prior `(\w+)` regex couldn't reach `r#type`; the
/// `strip_prefix("r#")` postprocess was dead code and the test that
/// exercised it bypassed the regex.
const FIELD_RE: &str = r"(?m)^\s*pub\s+(?:r#)?(\w+)\s*:";

fn cli() -> Result<serde_json::Value> {
    // rio-cli's `#[derive(Subcommand)]` enum variants → kebab-case
    // subcommand names (clap's default rename). Runbooks cite these
    // ~55×; two were found stale (R4-024, R6-011). Nested subcommands
    // (pool/sla/upstream sub-enums) are NOT scraped this round —
    // `refs.cli-sub` validates top-level only.
    use heck::ToKebabCase;
    let body = fs::read_to_string(repo_root().join("rio-cli/src/main.rs"))?;
    let block = Regex::new(r"(?ms)^#\[derive\(Subcommand[^\)]*\)\]\s*.*?^enum\s+\w+\s*\{(.*?)^\}")?
        .captures(&body)
        .context("no Subcommand enum in rio-cli/src/main.rs")?[1]
        .to_string();
    let subs: Vec<String> = Regex::new(r"(?m)^\s{4}([A-Z]\w*)\b")?
        .captures_iter(&block)
        .map(|c| c[1].to_kebab_case())
        .collect();
    Ok(json!({"subcommands": subs}))
}

fn protos() -> Result<serde_json::Value> {
    // `service X` declarations + first `//` comment per .proto file.
    // crate-structure.typ's proto/ block derives from this — last
    // hand-tree (R7-030: it said BuilderService; file defines
    // ExecutorService). Service-less files keep their first-comment
    // summary so the row isn't blank.
    let svc_re = Regex::new(r"(?m)^service\s+(\w+)\b")?;
    let mut out = BTreeMap::<String, serde_json::Value>::new();
    for entry in fs::read_dir(repo_root().join("rio-proto/proto"))? {
        let p = entry?.path();
        if p.extension().is_some_and(|e| e == "proto") {
            let body = fs::read_to_string(&p)?;
            let svcs: Vec<_> = svc_re
                .captures_iter(&body)
                .map(|c| c[1].to_string())
                .collect();
            let doc = body
                .lines()
                .find(|l| l.trim_start().starts_with("//"))
                .map(|l| l.trim_start().trim_start_matches("//").trim().to_string())
                .unwrap_or_default();
            out.insert(
                p.file_name()
                    .and_then(|n| n.to_str())
                    .context("non-utf8 .proto filename")?
                    .to_owned(),
                json!({"services": svcs, "doc": doc}),
            );
        }
    }
    Ok(json!(out))
}

/// `[workspace] members` from root Cargo.toml, minus `workspace-hack`
/// (the hakari stub, zeroed in nix/crate2nix.nix). Shared by
/// `workspace()` and `modules()`.
fn workspace_members() -> Result<Vec<String>> {
    let root: toml::Table = fs::read_to_string(repo_root().join("Cargo.toml"))?.parse()?;
    Ok(root["workspace"]["members"]
        .as_array()
        .context("no [workspace] members")?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .filter(|n| n != "workspace-hack")
        .collect())
}

fn modules() -> Result<serde_json::Value> {
    // Recursive walk of each crate's src/ (depth ≤ 3, skip tests/),
    // first-line `//!` doc per file or per <dir>/mod.rs.
    // crate-structure.typ derives the per-crate module trees from this
    // (R4-m002 tls.rs + R5-m004 karpenter.rs + rio-common
    // k8s.rs/newtype.rs were hand-tree drift).
    fn first_doc_line(p: &Path) -> String {
        fs::read_to_string(p)
            .ok()
            .and_then(|b| {
                b.lines()
                    .next()
                    .filter(|l| l.starts_with("//!"))
                    .map(|l| l.trim_start_matches("//!").trim().to_string())
            })
            .unwrap_or_default()
    }
    fn walk(dir: &Path, prefix: &str, depth: u8, out: &mut Vec<serde_json::Value>) -> Result<()> {
        let mut entries: Vec<_> = fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e
                .file_name()
                .into_string()
                .map_err(|n| anyhow::anyhow!("non-utf8 src/ entry: {n:?}"))?;
            if name == "tests" || name == "tests.rs" {
                continue;
            }
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if e.path().is_dir() {
                let doc = first_doc_line(&e.path().join("mod.rs"));
                out.push(json!({"path": format!("{rel}/"), "depth": depth, "doc": doc}));
                if depth < 3 {
                    walk(&e.path(), &rel, depth + 1, out)?;
                }
            } else if name.ends_with(".rs") && name != "mod.rs" {
                out.push(json!({
                    "path": rel,
                    "depth": depth,
                    "doc": first_doc_line(&e.path()),
                }));
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::<String, Vec<serde_json::Value>>::new();
    for m in workspace_members()? {
        let src = repo_root().join(&m).join("src");
        if !src.is_dir() {
            continue;
        }
        let mut entries = Vec::new();
        walk(&src, "", 0, &mut entries)?;
        out.insert(m, entries);
    }
    Ok(json!(out))
}

fn workspace() -> Result<serde_json::Value> {
    // Parse via the toml crate, not regex: regex section-scraping
    // consumes the next `[` and silently skips [dev-dependencies] for
    // 11/13 crates. Not `cargo metadata`: docsData runs the
    // crate2nix-built xtask in a sandbox without cargo. workspace-hack
    // is the hakari stub (zeroed in nix/crate2nix.nix); excluded so
    // refs.crate-list() doesn't render it as a real crate.
    let member_names = workspace_members()?;

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

        // Default-feature dep: closure. BFS from `default` over feature→
        // feature refs, collecting `dep:X` tokens. Per-manifest only — no
        // cross-crate feature unification (autograph shows what `cargo
        // build -p <crate>` links, not what a downstream consumer might
        // enable).
        let default_deps = default_dep_closure(&t);

        // rio-* internal deps from a deps table, partitioned: required,
        // or `optional=true` AND not in the default-feature dep: closure.
        let internal = |t: &toml::Table| -> (BTreeSet<String>, BTreeSet<String>) {
            let mut req = BTreeSet::new();
            let mut opt = BTreeSet::new();
            for (k, v) in t {
                if !(k.starts_with("rio-") || k == "xtask") {
                    continue;
                }
                let is_opt = v
                    .as_table()
                    .and_then(|d| d.get("optional"))
                    .and_then(|o| o.as_bool())
                    .unwrap_or(false);
                if is_opt && !default_deps.contains(k) {
                    opt.insert(k.clone());
                } else {
                    req.insert(k.clone());
                }
            }
            (req, opt)
        };

        let mut prod = BTreeSet::<String>::new();
        let mut optional = BTreeSet::<String>::new();
        let mut dev = BTreeSet::<String>::new();
        if let Some(d) = t.get("dependencies").and_then(|v| v.as_table()) {
            let (r, o) = internal(d);
            prod.extend(r);
            optional.extend(o);
        }
        if let Some(d) = t.get("dev-dependencies").and_then(|v| v.as_table()) {
            dev.extend(internal(d).0); // dev-deps don't carry `optional`
        }
        // [target.<cfg>.dependencies] — same partition.
        if let Some(tg) = t.get("target").and_then(|v| v.as_table()) {
            for cfg in tg.values() {
                if let Some(d) = cfg.get("dependencies").and_then(|v| v.as_table()) {
                    let (r, o) = internal(d);
                    prod.extend(r);
                    optional.extend(o);
                }
                if let Some(d) = cfg.get("dev-dependencies").and_then(|v| v.as_table()) {
                    dev.extend(internal(d).0);
                }
            }
        }
        // Self-dep (rio-store has `path = "."` under dev-deps to enable
        // its own test-utils feature for tests/) — not a graph edge.
        prod.remove(name);
        optional.remove(name);
        dev.remove(name);
        // Dep in prod+dev → solid only; in optional+dev → dotted only
        // (rio-store has rio-test-support in both optional [deps] AND
        // [dev-deps]; without this filter the autograph would render a
        // dotted+dashed double-edge).
        let dev: Vec<_> = dev
            .difference(&prod)
            .filter(|d| !optional.contains(*d))
            .cloned()
            .collect();
        deps.insert(
            name.clone(),
            json!({
                "prod": prod.into_iter().collect::<Vec<_>>(),
                "optional": optional.into_iter().collect::<Vec<_>>(),
                "dev": dev,
            }),
        );
    }
    Ok(json!({"members": members, "deps": deps}))
}

/// BFS from `[features].default` over feature→feature refs, collecting
/// `dep:X` tokens. `crate/feat` and `crate?/feat` are cross-refs (they
/// enable a *dep's* feature), not sub-features of this manifest.
fn default_dep_closure(t: &toml::Table) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    let Some(features) = t.get("features").and_then(|v| v.as_table()) else {
        return deps;
    };
    let mut seen = BTreeSet::new();
    let mut queue = vec!["default".to_string()];
    while let Some(f) = queue.pop() {
        if !seen.insert(f.clone()) {
            continue;
        }
        for tok in features
            .get(&f)
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
        {
            if let Some(d) = tok.strip_prefix("dep:") {
                deps.insert(d.to_owned());
            } else if !tok.contains('/') {
                // Bare token = sub-feature ref. `crate/feat` and
                // `crate?/feat` enable a dep's feature, not ours.
                queue.push(tok.to_owned());
            }
        }
    }
    deps
}

/// Doc-referenced rust consts. Curated allowlist, NOT a full scrape:
/// each entry is a const the spec book cites by value at ≥2 prose
/// sites, so per §Nth-strike it must derive. xtask fails if the regex
/// finds no match at the named file (catches the const moving/rename).
fn consts() -> Result<serde_json::Value> {
    const TABLE: &[(&str, &str)] = &[
        ("MAX_RECONNECT", "rio-gateway/src/handler/build.rs"),
        ("DEFAULT_GC_GRACE_HOURS", "rio-store/src/grpc/admin.rs"),
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
        namespaces: BTreeMap<String, serde_json::Value>,
    }
    let body = fs::read_to_string(repo_root().join("infra/helm/rio-build/values.yaml"))?;
    let v: Values = serde_saphyr::from_str(&body)?;
    let mut out = serde_json::Map::new();
    for (role, raw) in v.namespaces {
        if role == "create" {
            continue; // `create: true` flag, not a namespace entry
        }
        let ns: Ns = serde_json::from_value(raw)?;
        out.insert(ns.name, json!({"psa": ns.psa, "role": role}));
    }
    Ok(json!(out))
}

/// Read each binary crate's committed `tests/fixtures/config-schema.json`
/// snapshot (`{"schema": <schema_for! output>, "defaults": <Config::default()>}`)
/// and flatten into the typst-facing rows. The fixtures are kept fresh by
/// the per-crate `config_schema_frozen` snapshot test
/// (`rio_test_support::config_schema_frozen!`) — xtask never compiles the
/// binary crates, so editing `rio-gateway/src/` doesn't rebuild xtask.
fn config() -> Result<serde_json::Value> {
    let root = repo_root();
    let mut components = serde_json::Map::new();
    for (name, crate_dir) in [
        ("gateway", "rio-gateway"),
        ("scheduler", "rio-scheduler"),
        ("store", "rio-store"),
        ("builder", "rio-builder"),
        ("controller", "rio-controller"),
    ] {
        let path = root
            .join(crate_dir)
            .join("tests/fixtures/config-schema.json");
        let raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).with_context(|| {
                format!(
                    "read {} (regenerate: BLESS=1 cargo nextest run -E 'test(config_schema_frozen)')",
                    path.display()
                )
            })?)?;
        let schema = raw
            .get("schema")
            .with_context(|| format!("{}: missing `schema` key", path.display()))?;
        let defaults = raw
            .get("defaults")
            .with_context(|| format!("{}: missing `defaults` key", path.display()))?;
        components.insert(name.into(), flatten_schema(schema, defaults));
    }
    Ok(json!({"components": components}))
}

/// Walk a JSON Schema's `properties` into a flat list of
/// `{key, type, default, description}` rows for the typst config
/// reference. Nested objects (`UpstreamAddrs`, `JwtConfig`, …) flatten
/// as `parent.child` keys; `#[serde(flatten)]` (CommonConfig) inlines
/// at the parent level (schemars already does that). `defaults` is the
/// fixture's `serde_json::to_value(Config::default())` — schemars doesn't
/// capture `#[serde(default)]` values, so they're zipped in by key.
fn flatten_schema(root: &serde_json::Value, defaults: &serde_json::Value) -> serde_json::Value {
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
    // Iteration order is declaration order: schemars walks fields in
    // source order and serde_json::Map preserves insertion order — the
    // workspace pins `preserve_order` deliberately (see the workspace
    // Cargo.toml). The committed config.json reflects declaration
    // order; nix/misc-checks.nix's docs-data-fresh jq-canonicalizes
    // both sides anyway, so ordering is not load-bearing — see
    // render_default() below.
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
/// feature or the source map's iteration order (the workspace now
/// pins `preserve_order` on both the cargo and crate2nix sides — see
/// walk_props — but a HashMap-backed default like
/// `{"fetcher-*":600,"*":60}` would still insert in per-process-random
/// order, and the docs-data-fresh check would see that as drift).
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
    // Alert-rule scraper (merged_bug_001 mechanism): inline and
    // `expr: |` block forms, for/severity capture, rio_* token
    // extraction with histogram-suffix normalization.
    #[test]
    fn alerts_scraper_parses_inline_and_block_exprs() {
        let yaml = r#"
spec:
  groups:
    - name: rio.rules
      rules:
        - alert: InlineOne
          expr: sum(rate(rio_scheduler_foo_total[5m])) > 0
          for: 2m
          labels:
            severity: warning
        - alert: BlockOne
          expr: |
            histogram_quantile(0.99,
              sum by (le) (rate(rio_store_bar_seconds_bucket[2m]))) > 1
          for: 0m
          labels:
            severity: critical
"#;
        let v = super::parse_alerts(yaml).unwrap();
        let rules = v["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 2);
        let inline = &rules[1];
        assert_eq!(inline["name"], "InlineOne");
        assert_eq!(inline["expr"], "sum(rate(rio_scheduler_foo_total[5m])) > 0");
        assert_eq!(inline["for"], "2m");
        assert_eq!(inline["severity"], "warning");
        assert_eq!(inline["metrics"][0], "rio_scheduler_foo_total");
        let block = &rules[0];
        assert_eq!(block["name"], "BlockOne");
        assert_eq!(
            block["expr"],
            "histogram_quantile(0.99, sum by (le) (rate(rio_store_bar_seconds_bucket[2m]))) > 1"
        );
        // _bucket suffix normalizes to the describe_histogram! name.
        assert_eq!(block["metrics"][0], "rio_store_bar_seconds");
    }

    use super::*;

    #[test]
    fn unescape_rust_str_handles_all_escapes() {
        assert_eq!(unescape_rust_str(r#"a \"q\" b"#), r#"a "q" b"#);
        assert_eq!(unescape_rust_str(r"a\\b"), r"a\b");
        assert_eq!(unescape_rust_str("a \\\n  b"), "a   b"); // line-cont
        assert_eq!(unescape_rust_str(r"\{0}"), r"\{0}"); // unknown passes through
    }

    #[test]
    fn variant_re_captures_doc_msg_ident() {
        let re = Regex::new(VARIANT_RE).unwrap();
        let src = r#"
            /// The Nix client disconnected.
            /// NOT reconnect-worthy.
            #[error("client disconnected: {0}")]
            Wire(anyhow::Error),
            #[error("expected '\"' to start string")]
            ExpectedStringStart,
        "#;
        let caps: Vec<_> = re
            .captures_iter(src)
            .map(|c| (c[1].to_string(), unescape_rust_str(&c[2]), c[3].to_string()))
            .collect();
        assert_eq!(caps.len(), 2);
        assert!(caps[0].0.contains("NOT reconnect-worthy"));
        assert_eq!(caps[0].1, "client disconnected: {0}");
        assert_eq!(caps[0].2, "Wire");
        assert_eq!(caps[1].0, ""); // no doc block
        assert_eq!(caps[1].1, r#"expected '"' to start string"#);
        assert_eq!(caps[1].2, "ExpectedStringStart");
    }

    #[test]
    fn variant_re_handles_attrs_between_doc_and_error() {
        let re = Regex::new(VARIANT_RE).unwrap();
        for src in [
            // single #[cfg]
            "    /// doc.\n    #[cfg(feature = \"server\")]\n    #[error(\"m\")]\n    Foo,",
            // stacked attrs
            "    /// doc.\n    #[cfg(test)]\n    #[allow(dead_code)]\n    #[error(\"m\")]\n    Foo,",
        ] {
            let c = re.captures(src).unwrap();
            assert!(c[1].contains("doc."), "doc not captured for: {src}");
            assert_eq!(&c[3], "Foo");
        }
    }

    #[test]
    fn variant_re_handles_transparent() {
        let re = Regex::new(VARIANT_RE).unwrap();
        let src = "    /// wraps inner.\n    #[error(transparent)]\n    Clock(#[from] ClockError),";
        let c = re.captures(src).unwrap();
        assert!(c[1].contains("wraps inner"));
        assert!(c.get(2).is_none()); // no msg literal
        assert_eq!(&c[3], "Clock");
    }

    #[test]
    fn crds_field_re_handles_raw_ident_and_camel() {
        let re = Regex::new(FIELD_RE).unwrap();
        let conv = |s: &str| {
            re.captures(s).unwrap()[1]
                .trim_end_matches('_')
                .to_lower_camel_case()
        };
        assert_eq!(conv("    pub host_network: bool,"), "hostNetwork");
        assert_eq!(conv("    pub systems: Vec<String>,"), "systems");
        assert_eq!(conv("    pub r#type: String,"), "type");
        assert_eq!(conv("    pub type_: String,"), "type");
    }

    #[test]
    fn default_dep_closure_bfs() {
        let t: toml::Table = r#"
            [features]
            default = ["server"]
            server = ["dep:rio-common", "dep:rio-auth", "schema", "tokio/rt"]
            schema = ["dep:rio-nix"]
            test-utils = ["server", "dep:rio-test-support"]
        "#
        .parse()
        .unwrap();
        let closure = default_dep_closure(&t);
        // default→server→{rio-common,rio-auth,schema}→{rio-nix};
        // tokio/rt is a cross-ref (skipped); test-utils not reachable.
        assert_eq!(
            closure,
            ["rio-auth", "rio-common", "rio-nix"]
                .into_iter()
                .map(String::from)
                .collect()
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
