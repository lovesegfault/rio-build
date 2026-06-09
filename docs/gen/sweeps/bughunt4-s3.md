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

## merged_bug_179 — every last_store_rpc_failure stamp site

    $ grep -rn "note_issued_store_rpc_failure(" rio-scheduler/src/actor --include="*.rs" | grep -v "fn note_issued"

    dispatch.rs    ready-check fold        (policy-gated: is_store_health_evidence)
    completion.rs  ca-cutoff-verify x2     (issued Err + issued timeout)
    recovery.rs    recovery-reconcile x2
    merge.rs       merge-stale-completed-verify x2, merge-ca-realisation-verify x2,
                   merge-cache-check x2, merge-topdown x2
    materialize.rs settlement-reprobe x2
    debug.rs       debug-cmd (cfg(test) hook)

    The field has ONE writer fn; the raw `last_store_rpc_failure = Some(..)`
    assignment count outside it is ZERO (grep verified). BudgetExpired never
    reaches the writer: the policy match refuses it and the fan-out
    short-circuit never polls the RPC future.

## merged_bug_200 — the other disposition counters at the same site

    $ grep -n 'metrics::counter!' rio-scheduler/src/actor/completion.rs | head

    Verified: rio_scheduler_store_degraded_requeues_total now emits ONLY in the
    recorded_row post-commit block of handle_infrastructure_failure. The other
    counters in the infra path (cache_check_failures at probe sites,
    undeclared_built_output at the membership filter) count OBSERVATIONS by
    design, not settlements -- their HELP text says so; no other settled-class
    counter ticks pre-commit in this file.

## merged_bug_262 — every f64-seconds -> Duration construction (ban-generated)

    $ cargo clippy --workspace --all-targets   # with the clippy.toml disallowed-methods entry

    The ban makes the sweep set compiler-generated: post-fix the workspace has
    EXACTLY ONE #[allow(clippy::disallowed_methods)] for from_secs_f64 -- inside
    rio_common::clamped::ClampedSecs::from_f64 (the total constructor). Converted
    lanes: rio-scheduler actor/materialize.rs parked_until (the crash-loop lane),
    sla/cost.rs CostTable::load + IceBackoff::new (+ sla.max_lead_time ensure! at
    validate_shape), admin/executors.rs attempt_opened (vacuous checked_add),
    state/executor.rs base+cap, state/recovered_instant.rs (the precedent absorbed),
    rio-common backoff.rs jitter apply, rio-builder fixture.rs, rio-store
    metadata/inline.rs, rio-controller sketch.rs epoch (ABSOLUTE epoch: total
    try_from_secs_f64 + warn/reset, not the 1yr age clamp). Planted-red verified:
    a raw call anywhere fails clippy -D warnings with the merged_bug_262 reason.

## C9b — epoch-vs-age domain census (sibling sweep of the C9 constructor)

Generated: `grep -rn 'clamped_duration_secs\|ClampedSecs::from_f64\|clamped::epoch_secs\|try_from_secs_f64' --include='*.rs' rio-* | grep -v 'rio-common/src/clamped.rs'` — every f64-seconds time construction in the workspace, classified by domain. The machine witness is the clippy `disallowed-methods` pair (from_secs_f64 AND try_from_secs_f64): an unclassified site cannot compile.

| site | fed expression | domain | constructor |
|---|---|---|---|
| rio-scheduler/src/actor/materialize.rs:859 | `park_remaining_secs` (PG `EXTRACT(EPOCH FROM parked_until - now())`) | AGE (remaining interval) | `ClampedSecs::from_f64` |
| rio-scheduler/src/state/recovered_instant.rs:60 | `age_secs` (recovery age) | AGE | `clamped_duration_secs` |
| rio-scheduler/src/state/recovered_instant.rs:94 | `thirty_hours` (test interval) | AGE | `clamped_duration_secs` |
| rio-scheduler/src/state/executor.rs:153 | `backoff_base_secs` (config interval) | AGE | `clamped_duration_secs` |
| rio-scheduler/src/state/executor.rs:158 | `backoff_max_secs` (config interval) | AGE | `clamped_duration_secs` |
| rio-builder/src/fixture.rs:103 | `o.wall_secs` (wall-clock duration) | AGE | `clamped_duration_secs` |
| rio-scheduler/src/sla/cost.rs:948 | `max_lead_time_secs` (interval) | AGE | `clamped_duration_secs` |
| rio-store/src/metadata/inline.rs:326 | `EXTRACT(EPOCH FROM (now() - updated_at))` (age) | AGE | `clamped_duration_secs` |
| rio-common/src/backoff.rs:143 | `safe` (backoff interval) | AGE | `clamped_duration_secs` |
| rio-scheduler/src/sla/cost.rs:612 | `EXTRACT(EPOCH FROM last_observed)` (timestamptz) | **EPOCH** | `clamped::epoch_secs` (was age-clamped — **defect, fixed**) |
| rio-scheduler/src/admin/executors.rs:88 | `assigned_at_epoch_secs` (`EXTRACT(EPOCH FROM a.assigned_at)`) | **EPOCH** | `clamped::epoch_secs` (was age-clamped — **defect, fixed**) |
| rio-controller/src/reconcilers/nodeclaim_pool/sketch.rs:568 | `sketch_epoch_secs` (timestamptz) | **EPOCH** | `clamped::epoch_secs` (was inline `try_from_secs_f64` — migrated to the funnel) |

Zero epoch-domain feeds of the age clamp remain; zero inline `try_from_secs_f64` sites remain (the ban admits only `clamped::epoch_secs`).

## C10 — session-liveness consumer census (bug_234)

Generated: `grep -rn 'heartbeat_at\|SESSION_STALE_AFTER\|IngestSessionObservation\|live_ingest_session_sql' --include='*.rs' rio-* | grep -v test` — every consumer of ingest-session liveness, all deriving from the ONE rio-migrations definition.

| consumer | role | derivation |
|---|---|---|
| rio-migrations/src/sql.rs:64 | THE definition (`SESSION_STALE_AFTER_SECS` + `live_ingest_session_sql`) | source of truth |
| rio-store sessions.rs:140 (`acquire` steal arm) | staleness = `NOT (live)` | shared fragment, $4 bind |
| rio-store sessions.rs:221 (`lookup_live`) | routing read: live only | shared fragment, $2 bind |
| rio-store sessions.rs:58 (`SESSION_STALE_AFTER`) | crate-local const | derived from the shared const + compile-time heartbeat-ratio assert |
| rio-store sweep.rs:187 (`sweep_stale_sessions`) | dead-row reap at 10× grace | shared fragment, $1 bind |
| rio-scheduler db/attempts.rs:651 (`gc_exec_rows` conjunct 5) | liveness veto (was existence — the defect) | shared fragment + shared const |
| rio-retry-kernel `IngestSessionObservation` | the typed 3-state alphabet behind conjunct 5; only `Live` vetoes | full-alphabet kani sweep, 5/5 covers |

Every comparison against `heartbeat_at` in the workspace routes through `live_ingest_session_sql`; zero hand-written copies remain.
