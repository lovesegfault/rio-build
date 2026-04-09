//! See rio-scheduler/build.rs for rationale + build-support/metrics_grep.rs for impl.

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../build-support/metrics_grep.rs"
));

fn main() {
    // The metrics-grep output is consumed by tests/metrics_registered.rs,
    // which exercises `describe_metrics()` — a `server`-feature item.
    // Skip the grep for `schema`-only builds (rio-scheduler dep) so the
    // lean build doesn't read ../docs/ or walk server-gated source.
    if std::env::var_os("CARGO_FEATURE_SERVER").is_none() {
        println!("cargo:rerun-if-changed=build.rs");
        return;
    }
    metrics_build_main("rio_store_");
}
