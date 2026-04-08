//! See rio-scheduler/build.rs for rationale + rio-test-support/src/metrics_grep.rs for impl.

include!("../rio-test-support/src/metrics_grep.rs");

fn main() {
    metrics_build_main("rio_controller_");
}
