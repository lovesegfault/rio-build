# bughunt-4 S3 generated sweep sets (banner b)

Each section is the committed output of the named command at the
commit that closes the finding. Re-run the command to re-derive; a
drifted set is a failed sweep, not a stale doc.

## bug_220 — every `defer_until` writer

    $ grep -rn "defer_until\s*=" rio-scheduler/src --include="*.rs" | grep -v tests/ | grep -vE "defer_until: |//|defer_until = defer"

    rio-scheduler/src/actor/materialize.rs:231  release_claim_deferring (THE production chokepoint)
    rio-scheduler/src/actor/materialize.rs:263  test_set_defer_until (#[cfg(test)] seeding only)

## merged_bug_025 — both replay entry points + the close-only flush

    $ grep -rn "replay_status_batch_guarded(" rio-scheduler/src --include="*.rs" | grep -v "fn replay"

    rio-scheduler/src/actor/housekeeping.rs:789  tick_flush_status_outbox (the ONLY production caller; latched_at threaded)
    rio-scheduler/src/db/tests/derivations.rs    test callers (close-only + precedence cells)

    persist_status_batch (fresh-write) remains the only OTHER status batch writer;
    fence_coverage.rs pins the production caller census.

## merged_bug_285 — every skew-tripwire arm audited for the strike+repair shape

    $ grep -rn 'polarity" =>' rio-scheduler/src/actor --include="*.rs"

    rio-scheduler/src/actor/housekeeping.rs:1007  split_release       -> two-strike + uncharged requeue repair (THIS close)
    rio-scheduler/src/actor/housekeeping.rs:1030  claimed_no_attempt  -> two-strike + uncharged release repair (merged_bug_055 C, pre-existing)

    Both arms of rio_scheduler_materialization_view_node_skew_total now carry
    strike + repair; no counter-only or assert-only tripwire arm remains.

## merged_bug_294 — every terminal verdict arm's count disposition

    $ grep -rn "terminal_log_epilogue(" rio-scheduler/src/actor --include="*.rs" | grep -v "fn terminal_log_epilogue"

    build.rs:148        cancel transition   None (no report exists yet; the LATE-report arm below fills it)
    recovery.rs:2136    recovery succeed    None (reportless trigger -- conservative, stays incomplete)
    completion.rs:1208  cancelled LATE report  Some(count) when >0 (THIS close: COALESCE gap-fill)
    completion.rs:1604  success             Some(count) (the report's count, as before)
    completion.rs:2539  failure             Some(count) (report-bearing failures, the wave-1 close)

    Every arm that HAS a report consumes its count; the reportless arms stamp
    None by design (never falsely claim completeness).

## merged_bug_049 — all four credential_for consumers (compiler-generated)

    $ # deleting `impl From<CredentialRejection> for Status` made the set a compile error list:
    error[E0277] executor_service.rs:660   pull_assignment            (was counted; now chokepoint)
    error[E0277] executor_service.rs:855   report_outcome             (was counted; now chokepoint)
    error[E0277] executor_service.rs:1017  list_materialization_jobs  (was a bare `?` reason-drop)
    error[E0277] executor_service.rs:1089  report_materialization_progress (was a bare `?` reason-drop)

    The only consuming path is CredentialRejection::into_status_counted(rpc);
    a fifth consumer cannot compile without going through it.
