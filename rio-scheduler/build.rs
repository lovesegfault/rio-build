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
//!
//! The grep implementation is shared via `include!()` from
//! rio-test-support (not a build-dependency — include!() sidesteps
//! the workspace publish=false build-dep constraint).

include!("../rio-test-support/src/metrics_grep.rs");

fn main() {
    emit_metrics_grep(
        env!("CARGO_MANIFEST_DIR"),
        &std::env::var("OUT_DIR").unwrap(),
    );
}
