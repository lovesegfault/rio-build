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
