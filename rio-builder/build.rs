//! See rio-scheduler/build.rs for rationale + build-support/metrics_grep.rs for impl.

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../build-support/metrics_grep.rs"
));

fn main() {
    metrics_build_main("rio_builder_");
}
