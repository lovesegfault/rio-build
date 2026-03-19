# Plan 990503601: Scheduler gauge leader-gate — standby publishes stale zeros

[P0222](plan-0222-grafana-dashboards.md) review surfaced a leader/standby observability bug that predates the dashboards but is now *visible* because the dashboards query the affected gauges. The gauge block at [`worker.rs:627-646`](../../rio-scheduler/src/actor/worker.rs) runs unconditionally inside `handle_tick` — no `is_leader` check. With `scheduler.replicas: 2` ([`values.yaml:166`](../../infra/helm/rio-build/values.yaml)), **both** pods export `rio_scheduler_derivations_queued`, `_workers_active`, `_builds_active`, `_derivations_running`. The standby's actor is warm (DAGs merge — `r[sched.lease.k8s-lease]` says "DAGs are still merged so state is warm for takeover") but workers don't connect to it (leader-guarded gRPC — `r[sched.grpc.leader-guard]`), so the standby's counts are stale-or-zero.

Prometheus scrapes both pods. A naked query like [`build-overview.json:31`](../../infra/helm/grafana/build-overview.json) `rio_scheduler_builds_active` returns **two series** (one per pod). The stat panel's `lastNotNull` reducer picks whichever series scraped last — nondeterministically the leader's real value or the standby's zero. Operator looks at "Active builds" mid-incident and sees `0` while 50 builds are actually running.

**Why `class_queue_depth` accidentally doesn't have this bug:** [`dispatch.rs:150,154`](../../rio-scheduler/src/actor/dispatch.rs) sets it *inside* `dispatch_ready`, which already gates on `is_leader` at [`dispatch.rs:18`](../../rio-scheduler/src/actor/dispatch.rs). The standby early-returns before ever touching the gauge. This is the correct behavior — but by accident of code location, not by design.

**Fix choice: gate the gauge block, not the dashboards.** The followup offered two fixes: gate the gauge block on `is_leader` (scheduler side) OR wrap dashboard queries in `max()` (dashboard side). The scheduler-side fix is cleaner:
- `max()` on every future gauge query is a footgun (easy to forget on the next dashboard panel)
- `max()` is semantically wrong during a split-brain window where both replicas briefly believe they're leader — you'd get the higher of two diverging truths
- The standby publishing stale data is *wrong* regardless of whether anyone queries it — alerting rules, recording rules, and ad-hoc `promql` queries all hit the same two-series problem
- Consistency with `class_queue_depth` (already gated by accident)

## Entry criteria

- [P0222](plan-0222-grafana-dashboards.md) merged (dashboards exist; this is where the bug surfaces to operators)

## Tasks

### T1 — `fix(scheduler):` gate handle_tick gauge block on is_leader

MODIFY [`rio-scheduler/src/actor/worker.rs`](../../rio-scheduler/src/actor/worker.rs) at `:622-646`. Wrap the entire gauge block:

```rust
// Update metrics. All gauges are set from ground-truth state on each
// Tick — this is self-healing against any counting bugs elsewhere.
// The inc/dec calls at connect/disconnect/heartbeat (worker.rs:52/
// :76/:384) stay — they give sub-tick responsiveness. This block
// corrects any drift every tick.
//
// r[impl obs.metric.scheduler-leader-gate]
// Leader-only: standby's actor is warm (DAGs merge for takeover) but
// workers don't connect to it (leader-guarded gRPC), so its counts are
// stale-or-zero. With replicas:2, Prometheus scrapes both; a naked
// gauge query returns two series. Stat-panel lastNotNull picks one
// nondeterministically. Gate here so the standby simply doesn't export
// the series — queries see one series, no max() wrapper needed.
if self.is_leader.load(std::sync::atomic::Ordering::Relaxed) {
    metrics::gauge!("rio_scheduler_derivations_queued").set(self.ready_queue.len() as f64);
    metrics::gauge!("rio_scheduler_workers_active")
        .set(self.workers.values().filter(|w| w.is_registered()).count() as f64);
    metrics::gauge!("rio_scheduler_builds_active").set(
        self.builds
            .values()
            .filter(|b| b.state() == BuildState::Active)
            .count() as f64,
    );
    metrics::gauge!("rio_scheduler_derivations_running").set(
        self.dag
            .iter_values()
            .filter(|s| {
                matches!(
                    s.status(),
                    DerivationStatus::Running | DerivationStatus::Assigned
                )
            })
            .count() as f64,
    );
}
```

`Ordering::Relaxed` matches the existing load at [`dispatch.rs:18`](../../rio-scheduler/src/actor/dispatch.rs) — per the field doc at [`mod.rs:152-156`](../../rio-scheduler/src/actor/mod.rs), it's a standalone flag with no other state to synchronize, and a one-tick lag on `false→true` is harmless (next tick publishes).

**Non-K8s mode unaffected:** [`mod.rs:249`](../../rio-scheduler/src/actor/mod.rs) initializes `is_leader` to `true` when no lease task runs. Single-replica deployments and VM tests see no change.

**Note on stale-gauge-after-step-down:** When a leader steps down (`true→false`), the last-set gauge values linger in that pod's Prometheus registry until the pod restarts — the `metrics` crate has no `unregister`. This is acceptable: after step-down the pod is a standby, `grpc.health.v1/Check` reports `NOT_SERVING`, and the typical deployment topology is that the stepped-down pod gets recycled shortly (RollingUpdate). If this becomes a real problem, a follow-up could zero the gauges on the `true→false` edge in `handle_leader_lost` — out of scope here.

### T2 — `test(scheduler):` verify standby tick does not set gauges

NEW test in [`rio-scheduler/src/actor/tests/misc.rs`](../../rio-scheduler/src/actor/tests/misc.rs) alongside the existing `test_not_leader_does_not_dispatch` (line 32). The `spawn_actor_with_flags` helper at line 15 already constructs an actor with `is_leader=false`.

```rust
// r[verify obs.metric.scheduler-leader-gate]
/// When is_leader=false, handle_tick must NOT set state gauges.
/// Standby actor is warm (DAGs merge) but workers don't connect to
/// it (leader-guarded gRPC) — its counts are stale/zero. Publishing
/// them creates a second Prometheus series that dashboards pick
/// nondeterministically.
///
/// Test mechanism: metrics crate has no global test recorder, so we
/// install a local DebuggingRecorder, fire a Tick through a standby
/// actor, and assert the gauge metric names are absent from captured
/// operations. Use with_local_recorder scope so this doesn't collide
/// with other tests in the same process.
#[tokio::test]
async fn test_not_leader_does_not_set_gauges() -> TestResult {
    use metrics_util::debugging::{DebuggingRecorder, Snapshotter};

    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();

    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = spawn_actor_with_flags(db.pool.clone(), false, true);

    // Merge a DAG so there's something to count (standby DOES merge
    // — r[sched.lease.k8s-lease]: "DAGs are still merged so state is
    // warm for takeover"). If the gate is broken, the gauge would be
    // set to a nonzero value.
    merge_single_node(&handle, Uuid::new_v4(), "sg-drv", PriorityClass::Scheduled).await?;

    // Fire a Tick inside the recorder scope.
    metrics::with_local_recorder(&recorder, || {
        // send Tick — implementation-dependent: either via
        // handle.send_unchecked(ActorCommand::Tick) or by triggering
        // the tick path directly. Check at impl what misc.rs tests do.
    });
    barrier(&handle).await;

    let snapshot = snapshotter.snapshot();
    let gauge_names: Vec<_> = snapshot
        .into_vec()
        .into_iter()
        .filter_map(|(key, _, _, _)| {
            (key.kind() == metrics::MetricKind::Gauge).then(|| key.key().name().to_string())
        })
        .collect();

    // The four handle_tick gauges must NOT appear.
    for name in [
        "rio_scheduler_derivations_queued",
        "rio_scheduler_workers_active",
        "rio_scheduler_builds_active",
        "rio_scheduler_derivations_running",
    ] {
        assert!(
            !gauge_names.contains(&name.to_string()),
            "standby set gauge {name} — leader-gate broken"
        );
    }
    Ok(())
}
```

**Implementer note:** `DebuggingRecorder` is in `metrics-util`; check `Cargo.toml` dev-deps. If it's not already there, add `metrics-util = { version = "*", features = ["debugging"] }` under `[dev-dependencies]`. If the actor's Tick can't be driven synchronously inside `with_local_recorder` (the actor runs on its own task), fall back to a unit test that calls `handle_tick` directly on a bare `DagActor` struct — the `tests/misc.rs` module is already inside `actor` so it has crate-private access.

### T3 — `docs:` add spec marker to observability.md

MODIFY [`docs/src/observability.md`](../../docs/src/observability.md) — add the marker immediately after the scheduler metric table (after line ~117, before `### Store Metrics`). See `## Spec additions` below for the marker text.

## Exit criteria

- `/nbr .#ci` green
- `grep -A1 'r\[impl obs.metric.scheduler-leader-gate\]' rio-scheduler/src/actor/worker.rs` → the `if self.is_leader.load(...)` line
- `test_not_leader_does_not_set_gauges` passes; deliberately removing the `if` gate makes it fail
- `tracey query rule obs.metric.scheduler-leader-gate` shows spec + impl + verify (3/3)

## Tracey

**References existing markers:**
- `r[sched.lease.k8s-lease]` — spec context: "DAGs are still merged so state is warm for takeover" (why standby has counts at all)
- `r[sched.grpc.leader-guard]` — spec context: why workers don't connect to standby (why standby counts are stale)
- `r[obs.metric.scheduler]` — the metric table that defines the four affected gauges

**Adds new marker to component specs:**
- `r[obs.metric.scheduler-leader-gate]` → [`docs/src/observability.md`](../../docs/src/observability.md) (see `## Spec additions`)

## Spec additions

New marker appended to `docs/src/observability.md` after the scheduler metric table (after the last `rio_scheduler_*` row, before `### Store Metrics`):

```markdown
r[obs.metric.scheduler-leader-gate]
Scheduler state gauges (`_builds_active`, `_derivations_queued`, `_derivations_running`, `_workers_active`, `_class_queue_depth`) are published **only by the leader**. The standby's actor is warm (DAGs merge for fast takeover per `r[sched.lease.k8s-lease]`) but workers do not connect to it (leader-guarded gRPC per `r[sched.grpc.leader-guard]`), so its counts are stale or zero. With `replicas>1`, publishing from both would create duplicate Prometheus series with identical labels; a naked gauge query returns both, and stat-panel reducers pick one nondeterministically. Counters and histograms are unaffected --- the standby's dispatch loop no-ops, so its counters stay at zero naturally, and `sum(rate(...))` is the idiomatic query form anyway.
```

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "T1: wrap gauge block :627-646 in is_leader gate"},
  {"path": "rio-scheduler/src/actor/tests/misc.rs", "action": "MODIFY", "note": "T2: test_not_leader_does_not_set_gauges alongside test_not_leader_does_not_dispatch"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T3: new r[obs.metric.scheduler-leader-gate] marker after scheduler metric table"}
]
```

```
rio-scheduler/src/actor/
├── worker.rs            # T1: is_leader gate around :627-646 gauge block
└── tests/misc.rs        # T2: new test next to test_not_leader_does_not_dispatch
docs/src/observability.md  # T3: new marker after scheduler metric table
```

## Dependencies

```json deps
{"deps": [222], "soft_deps": [], "note": "P0222 review surfaced this. P0222 merged (dashboards exist) — dep satisfied immediately. worker.rs is a 25-plan collision hot file: P0211/P0214/P0219/P0285 all UNIMPL and touch it. This plan's edit is tiny (one if-wrap at end of file) — low textual conflict risk, but coordinator should prefer dispatching this EARLY since it's a 4-line edit that unblocks correct dashboard readings."}
```

**Depends on:** [P0222](plan-0222-grafana-dashboards.md) — the dashboards that surface this bug. Merged onto sprint-1 at [`6b723def`](https://github.com/search?q=6b723def&type=commits); dep satisfied.

**Conflicts with:** `rio-scheduler/src/actor/worker.rs` is a **25-plan hot file**. UNIMPL plans touching it: [P0211](plan-0211-scheduler-consume-store-degraded.md) (has_capacity in WorkerState), [P0214](plan-0214-per-build-timeout.md) (timeout in handle_tick — **same function**), [P0219](plan-0219-per-worker-failure-budget.md) (failure tracking), [P0285](plan-0285-drainworker-disruptiontarget-watcher.md) (DisruptionTarget watcher). This plan's edit is a 4-line wrap at `:622-646` (end of `handle_tick`). P0214 also edits `handle_tick` — textual-merge-conflict likely. **Prefer dispatching this plan BEFORE P0214** (this one is trivial, P0214 adds substantial logic; easier for P0214 to rebase over an `if`-wrap than vice versa).
