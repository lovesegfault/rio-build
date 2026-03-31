# Plan 522: warn+continue needs an escalation threshold — silent-degradation trade-off

[P0516](plan-0516-manifest-quota-deadlock.md) T2 at [`manifest.rs:357-371`](../../rio-controller/src/reconcilers/builderpool/manifest.rs) changed spawn-error handling from `Err(e)?` to `warn!(...); continue`. The rationale (`:358-365`) is sound for transient errors: a quota blip, an apiserver flap, a race on a name — subsequent spawns in the batch may succeed, and the idle-reapable pass below is independent. The loop `continue` matches delete-error handling in the sweep loop.

But there's NO escalation threshold. A **persistent** spawn error (admission webhook permanently blocking, malformed spec, RBAC gap) logs `warn` every 10s tick forever.

**Pre-T2:** `Err(e)?` → requeue backoff → visible in `rio_controller_reconcile_errors_total` ([`observability.md:224`](../../docs/src/observability.md)) → alertable.

**Post-T2:** silent beyond log-grep. No metric increment, no k8s Event, no requeue backoff signal. An operator watching `reconcile_errors_total` sees zero while the pool spawns nothing for hours.

The P0516 validator scrutiny-2 FAIL verdict was a clippy false-positive, but this concern (raised in the same pass) is real: **T1 (sweep-before-spawn reorder) ALREADY fixes the deadlock**. T2 is defense-in-depth — but it trades one failure mode (deadlock on quota exhaustion) for another (silent degradation on persistent spawn error). discovered_from=516. origin=reviewer.

**Also informs [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T503 (warn-THEN-bail blind spot):** if this plan builds `jobs_api` mock infra for the threshold test, that mock covers the P0311 runtime-test gap too. The structural test `spawn_loop_no_early_return_on_error` can't distinguish `warn; continue` from `warn; Err(e)?` — both have a warn followed by something.

## Entry criteria

- [P0516](plan-0516-manifest-quota-deadlock.md) merged (introduced the warn+continue arm at `:357`)

## Tasks

### T1 — `fix(controller):` N-consecutive-fail threshold → bail + increment metric

Track consecutive spawn failures per-tick. After `N` consecutive (suggest `N = 5`: covers a full headroom batch without bailing on the first quota blip, but doesn't spin forever), bail with the last error AND increment a metric.

```rust
// Before the spawn loop (:~330):
let mut consecutive_fails = 0_u32;
const SPAWN_FAIL_THRESHOLD: u32 = 5;

// At :357, replace the warn+continue arm:
Err(e) => {
    consecutive_fails += 1;
    metrics::MANIFEST_SPAWN_FAILURES.with_label_values(&[name]).inc();
    if consecutive_fails >= SPAWN_FAIL_THRESHOLD {
        warn!(
            pool = %name, consecutive = consecutive_fails,
            error = %e,
            "manifest Job spawn: {SPAWN_FAIL_THRESHOLD} consecutive \
             failures this tick; bailing. Check admission webhooks, \
             RBAC, and pod spec validity."
        );
        return Err(Error::Kube(e));
    }
    // warn+continue (P0516 T2): <N-1 fails in a row, loop continues
    // — subsequent spawns may succeed (different bucket → different
    // resource limits), idle-reapable pass below is independent.
    warn!(
        pool = %name, job = %job_name, bucket = ?directive.bucket,
        error = %e, consecutive = consecutive_fails,
        "manifest Job spawn failed; continuing tick \
         ({consecutive_fails}/{SPAWN_FAIL_THRESHOLD})"
    );
}
// Reset on success (at the Ok arm :347):
Ok(_) => {
    consecutive_fails = 0;
    info!(...);
}
```

The `Ok → reset` means intermittent failures (fail, succeed, fail) don't accumulate — only a run of N straight fails bails. A webhook that rejects SOME buckets (big-memory tier denied, small-memory tier allowed) doesn't trip.

**Metric:** new `rio_controller_manifest_spawn_failures_total{pool}` counter. Add to [`observability.md`](../../docs/src/observability.md) table near `:226` (alongside `rio_controller_scaling_decisions_total`).

**Alternative: k8s Event instead of metric.** `SpawnFailed` Warning event, dedup'd like `CrashLoopDetected` at `:256-274`. The event message interpolates a coarse tier (`"≥5"`, `"≥20"`) so apiserver dedup collapses per-tick emits. Prefer the metric — events are ephemeral (default 1h retention), metrics are scrapeable + alertable.

### T2 — `test(controller):` mock `jobs_api.create` persistent-fail → bail at threshold

This IS the mock infra that [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T503 needs. A `MockJobsApi` (or `kube-mock` if already a dep — check) that returns `Err` for N calls:

```rust
// r[verify ctrl.pool.manifest-reconcile]
#[tokio::test]
async fn spawn_bails_after_consecutive_threshold_not_before() {
    // Mock jobs_api.create: fails first 5, succeeds 6th.
    // Plan has 6 directives (headroom=6).
    //
    // Expected: 5th fail → bail with Err. 6th never attempted.
    // Pre-fix (warn+continue, no threshold): all 6 attempted,
    //   1 success, 5 warns, returns Ok — silent.
    //
    // Also covers P0311 T503 warn-THEN-bail blind spot:
    // structural test can't distinguish `warn; continue` from
    // `warn; Err(e)?` — this runtime test can (attempt count).
}

#[tokio::test]
async fn spawn_intermittent_fail_does_not_bail() {
    // Mock: fail, succeed, fail, succeed, ... (alternating).
    // Plan has 10 directives.
    //
    // Expected: all 10 attempted, returns Ok. consecutive_fails
    // never exceeds 1 (reset on each Ok). Metric increments 5×.
    // Proves the threshold is CONSECUTIVE not CUMULATIVE.
}
```

### T3 — `docs(obs):` add `rio_controller_manifest_spawn_failures_total` to observability table

[`observability.md`](../../docs/src/observability.md) Controller Metrics table near `:226`:

```markdown
| `rio_controller_manifest_spawn_failures_total` | Counter | Manifest Job spawn failures (labeled by pool). Non-zero rate with zero `reconcile_errors_total` = warn+continue absorbing errors below threshold; sustained high rate = threshold bailing every tick (check admission webhooks/RBAC). |
```

The "non-zero rate with zero errors" interpretation note is the operator's decoder ring — that gap is exactly the silent-degradation window this plan narrows.

## Exit criteria

- `/nbr .#ci` green
- `grep 'SPAWN_FAIL_THRESHOLD\|consecutive_fails' rio-controller/src/reconcilers/builderpool/manifest.rs` → ≥3 hits (threshold const + counter + check)
- `grep 'consecutive_fails = 0' rio-controller/src/reconcilers/builderpool/manifest.rs` → ≥1 hit (reset on Ok — consecutive not cumulative)
- `cargo nextest run -p rio-controller spawn_bails_after_consecutive_threshold` → passes; mutation: threshold `5 → 999` → test FAILS (never bails)
- `cargo nextest run -p rio-controller spawn_intermittent_fail_does_not_bail` → passes; proves reset-on-Ok
- `grep 'manifest_spawn_failures_total' rio-controller/src/metrics.rs docs/src/observability.md` → ≥2 hits (registered + documented)
- Post-plan, [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T503's mock-infra need is covered — note in T503's T-body that this plan's MockJobsApi (or equivalent) is the dependency

## Tracey

References existing markers:
- `r[ctrl.pool.manifest-reconcile]` — T2's test verifies the reconcile loop's error-handling behavior under this marker at [`controller.md:137`](../../docs/src/components/controller.md). The warn+continue → threshold-bail change is a behavior refinement under the existing spec (spec says "MUST reconcile", doesn't specify error-handling granularity).
- `r[obs.metric.controller]` — T3 extends the controller metrics table (doc-side, no `r[impl]` annotation on metric registration per project convention).

## Files

```json files
[
  {"path": "rio-controller/src/reconcilers/builderpool/manifest.rs", "action": "MODIFY", "note": "T1: consecutive-fail threshold at :357, reset at :347. HOT — P520 touches :131+:228 (diff section); serialize if both in-flight"},
  {"path": "rio-controller/src/metrics.rs", "action": "MODIFY", "note": "T1: register MANIFEST_SPAWN_FAILURES counter"},
  {"path": "rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs", "action": "MODIFY", "note": "T2: MockJobsApi + 2 tests. P520-T2 also adds a test here (diff section: sweep_cap table)"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T3: manifest_spawn_failures_total row near :226"}
]
```

```
rio-controller/src/
├── reconcilers/builderpool/
│   ├── manifest.rs          # T1: threshold :357, reset :347
│   └── tests/manifest_tests.rs  # T2: MockJobsApi + 2 tests
└── metrics.rs               # T1: register counter
docs/src/observability.md    # T3: table row :226
```

## Dependencies

```json deps
{"deps": [516], "soft_deps": [520], "note": "P0516 introduced warn+continue. P520 (soft) touches manifest.rs :131+:228 — diff section but same file; serialize. This plan's MockJobsApi covers P0311-T503's mock-infra need."}
```

**Depends on:** [P0516](plan-0516-manifest-quota-deadlock.md) — introduced the warn+continue arm at `:357` that this plan thresholds.

**Conflicts with:** `manifest.rs` is HOT. [P520](plan-520-sweep-cap-extract-clamp.md) touches `:131` + `:228` — diff sections, but serialize if both in-flight. `manifest_tests.rs` also shared with P520-T2 — additive (both add tests), low conflict. [P0295](plan-0295-doc-rot-batch-sweep.md)-T494 touches `manifest.rs:724-726` — diff section.
