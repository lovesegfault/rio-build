//! Offline timed dry-run over the committed v0 fixture archive: the
//! planning entry point must summarize the schedule, the workload split,
//! and the offline supply resolution from the archive alone — no network,
//! no cluster — and finish quickly enough for an edit-loop check.

use std::path::PathBuf;

use rio_replay::archive::reader::ReplayArchive;
use rio_replay::run::spec::Knobs;
use rio_replay::run::timeline::plan_timed_dry_run;

/// Crate directory at *runtime* (not `env!()`): under nextest
/// `--workspace-remap` the compile-time path is a per-crate build sandbox
/// that no longer exists when the test binary runs.
fn manifest_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo/nextest"),
    )
}

#[test]
fn timed_dry_run_on_fixture_completes_offline() {
    // The committed v0 directory-form fixture, opened through the v0 compat
    // path. The planner takes no substituter URLs and no transport, and is a
    // synchronous call, so the whole dry run stays inside the archive.
    let archive =
        ReplayArchive::open(&manifest_dir().join("tests/fixtures/archive/v0-basic")).unwrap();
    let plan = plan_timed_dry_run(&archive, &Knobs::default(), None).unwrap();

    // Schedule shape: every recorded request is scheduled (no limit) and the
    // recorded offsets span a non-empty due window.
    assert_eq!(plan.requests, 4);
    assert_eq!(plan.schedule_len, 4);
    assert!(plan.due_window_secs > 0.0);

    // The fixture's only interruption-recorded drv is its impure one, and
    // impure-demoted units are supplied rather than rebuilt — so a demoted
    // unit's recorded interruption is never an interruption candidate.
    assert_eq!(plan.interruption_candidates, 0);
    assert_eq!(plan.demoted_impure, 1);

    // Workload split and offline supply resolution: with no substituters to
    // probe, the ladder can only answer workload / embedded / unresolved.
    assert!(plan.workload_units >= 2);
    assert!(plan.workload_outputs_never_supplied >= 1);
    assert!(plan.embedded_uploadable >= 1);
    assert!(plan.unresolved_offline >= 1);

    // Exact shape of the committed fixture (pinned so a fixture or planner
    // change shows up as a deliberate diff here): four requests over four
    // derivations, a 9-second recorded window at the default 1.0 speedup,
    // two workload outputs withheld, one embedded source uploadable, and the
    // demoted/cache-hit outputs left for a live run's substituter probes.
    assert!((plan.due_window_secs - 9.0).abs() < 1e-9, "{plan:?}");
    assert_eq!(plan.workload_units, 2);
    assert_eq!(plan.union_drvs, 4);
    assert_eq!(plan.union_paths, 10);
    assert_eq!(plan.workload_outputs_never_supplied, 2);
    assert_eq!(plan.embedded_uploadable, 1);
    assert_eq!(plan.unresolved_offline, 3);

    // The plan block is operator-facing JSON: keys are camelCase.
    let json = serde_json::to_value(&plan).unwrap();
    assert!(json.get("dueWindowSecs").is_some(), "{json}");
    assert!(json.get("workloadOutputsNeverSupplied").is_some(), "{json}");
    assert!(json.get("interruptionCandidates").is_some(), "{json}");
}
