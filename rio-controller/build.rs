//! See rio-scheduler/build.rs for rationale + rio-test-support/src/metrics_grep.rs for impl.

include!("../rio-test-support/src/metrics_grep.rs");

fn main() {
    emit_metrics_grep(
        env!("CARGO_MANIFEST_DIR"),
        &std::env::var("OUT_DIR").unwrap(),
    );
}
