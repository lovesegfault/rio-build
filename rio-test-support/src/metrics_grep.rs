// This file is `include!()`-ed by crate-level build.rs files, NOT
// compiled as a rio-test-support module. See rio-scheduler/build.rs
// for the full rationale on why this is a build-time source grep
// rather than a runtime recorder hook.
//
// The `#[allow(dead_code)]` below is defensive — consumers call
// `emit_metrics_grep` from build.rs `main()`, so it is not dead at
// the `include!` expansion site. The attribute guards against a
// future accidental `mod metrics_grep;` in rio-test-support's lib.rs
// (where it WOULD be dead, and clippy --deny warnings would break
// the build).

#[allow(dead_code)]
fn emit_metrics_grep(manifest_dir: &str, out_dir: &str) {
    use std::{collections::BTreeSet, fs, path::Path};

    let src = Path::new(manifest_dir).join("src");
    let mut names = BTreeSet::new();

    // Matches `metrics::counter!("lit")`, `metrics::gauge!("lit")`,
    // `metrics::histogram!("lit")`. The `\bmetrics::` prefix is
    // REQUIRED (matches this codebase's convention — no one imports
    // the macros unqualified) and avoids false-matching
    // `describe_counter!("lit")` (which has no `metrics::` prefix).
    //
    // `\s*` handles multi-line calls (rustfmt breaks long lines after
    // the paren; see rio-scheduler dispatch.rs, completion.rs). `\s`
    // in Rust's regex crate matches `\n`.
    //
    // `[a-z0-9_]+` — NOT `[a-z_]+` — because `rio_store_s3_*`
    // metric names contain digits.
    let re = regex::Regex::new(r#"\bmetrics::(?:counter|gauge|histogram)!\s*\(\s*"([a-z0-9_]+)""#)
        .unwrap();

    fn walk(dir: &std::path::Path, re: &regex::Regex, out: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, re, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                for cap in re.captures_iter(&text) {
                    out.insert(cap[1].to_string());
                }
            }
        }
    }
    walk(&src, &re, &mut names);

    fs::write(
        Path::new(out_dir).join("emitted_metrics.txt"),
        names.into_iter().collect::<Vec<_>>().join("\n"),
    )
    .unwrap();

    // Re-run if ANY src file changes. Coarse but correct — a new
    // metrics:: call anywhere in src/ invalidates the grep output.
    println!("cargo:rerun-if-changed=src");
}

/// Extract metric names from markdown table rows whose first column
/// starts with `prefix`.
///
/// Table rows look like `` | `rio_component_metric_name` | Type | Desc | ``.
/// The first `|` is stripped, the cell is trimmed of backticks, and
/// the result must be purely `[a-z0-9_]+` (rejects prose mentions,
/// comma-separated cells like the Histogram Buckets table, `{label}`
/// examples, and the `|---|---|---|` separator row).
///
/// Not a general markdown table parser — relies on the
/// observability.md convention that each metric table has exactly
/// three `|`-separated columns with the name in column one.
#[allow(dead_code)]
fn grep_spec_names(obs_md_src: &str, prefix: &str) -> Vec<String> {
    let mut names: Vec<String> = obs_md_src
        .lines()
        .filter_map(|l| {
            // Must look like a table row (leading `|`).
            let l = l.strip_prefix('|')?;
            let first = l.split('|').next()?.trim().trim_matches('`');
            (first.starts_with(prefix)
                && !first.is_empty()
                && first.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
            .then(|| first.to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Grep the observability.md table for metric names with a given
/// prefix. Writes `spec_metrics.txt` (newline-separated, sorted,
/// deduplicated) to OUT_DIR.
///
/// Emits `cargo:rerun-if-changed` for `obs_md_path` — CRITICAL for
/// drift detection. Adding a row to observability.md must invalidate
/// the test binary so the spec→describe check re-runs against the
/// new list.
#[allow(dead_code)]
fn emit_spec_metrics_grep(obs_md_path: &str, out_dir: &str, prefix: &str) {
    let obs_md =
        std::fs::read_to_string(obs_md_path).unwrap_or_else(|e| panic!("read {obs_md_path}: {e}"));
    let names = grep_spec_names(&obs_md, prefix);
    let out = format!("{out_dir}/spec_metrics.txt");
    std::fs::write(&out, names.join("\n")).unwrap_or_else(|e| panic!("write {out}: {e}"));
    println!("cargo:rerun-if-changed={obs_md_path}");
}
