//! See rio-scheduler/build.rs for rationale + rio-test-support/src/metrics_grep.rs for impl.

include!("../rio-test-support/src/metrics_grep.rs");

fn main() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let out = std::env::var("OUT_DIR").unwrap();
    emit_metrics_grep(manifest, &out);
    emit_spec_metrics_grep(
        &format!("{manifest}/../docs/src/observability.md"),
        &out,
        "rio_gateway_",
    );
}
