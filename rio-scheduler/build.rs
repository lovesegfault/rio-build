//! Grep metric-emit macro literals from src/ for the emit→describe
//! check in `tests/metrics_registered.rs`.
//!
//! Runs at `cargo build` time. Output consumed via
//! `include_str!(concat!(env!("OUT_DIR"), "/emitted_metrics.txt"))`
//! in the integration test.
//!
//! # Why a build script and not a runtime recorder hook
//!
//! You can't trigger every emit code path from a unit test: most
//! `metrics::counter!()` calls are deep in handlers gated on actor
//! state (backstop-timeout needs a Running derivation past threshold;
//! per-build-timeout needs an Active build past `build_timeout`). The
//! failure mode is "developer wrote a literal string in a macro call"
//! — textual by nature, so a source grep is the right tool.

use std::{collections::BTreeSet, env, fs, path::Path};

fn main() {
    let out = env::var("OUT_DIR").unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");

    let mut names = BTreeSet::new();
    grep_metrics(&src, &mut names);

    fs::write(
        Path::new(&out).join("emitted_metrics.txt"),
        names.into_iter().collect::<Vec<_>>().join("\n"),
    )
    .unwrap();

    // Re-run if ANY src file changes. Coarse but correct — a new
    // metrics:: call anywhere in src/ invalidates the grep output.
    println!("cargo:rerun-if-changed=src");
}

fn grep_metrics(dir: &Path, out: &mut BTreeSet<String>) {
    // Matches `metrics::counter!("lit")`, `metrics::gauge!("lit")`,
    // `metrics::histogram!("lit")`. The `\bmetrics::` prefix is
    // REQUIRED (matches this codebase's convention — no one imports
    // the macros unqualified) and avoids false-matching
    // `describe_counter!("lit")` (which has no `metrics::` prefix).
    //
    // `\s*` handles multi-line calls (rustfmt breaks long lines after
    // the paren; see dispatch.rs:101, completion.rs:390). `\s` in
    // Rust's regex crate matches `\n`.
    //
    // `[a-z0-9_]+` — NOT `[a-z_]+` — because `rio_store_s3_*`
    // metric names contain digits.
    let re = regex::Regex::new(r#"\bmetrics::(?:counter|gauge|histogram)!\s*\(\s*"([a-z0-9_]+)""#)
        .unwrap();

    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            grep_metrics(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let text = fs::read_to_string(&path).unwrap();
            for cap in re.captures_iter(&text) {
                out.insert(cap[1].to_string());
            }
        }
    }
}
