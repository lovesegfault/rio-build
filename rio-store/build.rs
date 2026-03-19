//! Grep metric-emit macro literals from src/ for the emit→describe
//! check in `tests/metrics_registered.rs`. See rio-scheduler/build.rs
//! for full rationale — this is a verbatim copy.

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

    println!("cargo:rerun-if-changed=src");
}

fn grep_metrics(dir: &Path, out: &mut BTreeSet<String>) {
    // `\bmetrics::` required (excludes describe_counter!); `[a-z0-9_]+`
    // (digits for s3 names); `\s*` handles multi-line. See
    // rio-scheduler/build.rs for the full regex rationale.
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
