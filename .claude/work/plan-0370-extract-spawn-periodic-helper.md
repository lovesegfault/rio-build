# Plan 0370: Extract spawn_periodic helper — 11 copies, 7 lack `biased;`

Consolidator-mc140 finding. The `spawn_monitored(name, async move { interval; loop { select! { cancelled => break, tick => {} } body } })` pattern has reached eleven instances. Seven lack the `biased;` token — the [P0335](plan-0335-channelsession-drop-abort-race.md) lesson ([`tokio::select!`](https://docs.rs/tokio/latest/tokio/macro.select.html) defaults to **random** branch choice, not first-ready; see `lang-gotchas.md`). For shutdown-vs-tick, random ordering is usually benign (worst case: one extra tick after `cancelled()` fires). But the inconsistency means new copies propagate whichever variant they pattern-match from. The four sites that DO have `biased;` were each added after P0335 landed; the seven that don't pre-date it.

**The eleven sites** (at `49e3d34b`):

| # | File:line | Task name | `biased;`? | Body shape |
|---|---|---|---|---|
| 1 | [`rio-scheduler/src/main.rs:489`](../../rio-scheduler/src/main.rs) | `health-toggle-loop` | no | load→compare→set_serving |
| 2 | [`rio-scheduler/src/main.rs:553`](../../rio-scheduler/src/main.rs) | `build-samples-retention` | no | DELETE older than 30d |
| 3 | [`rio-scheduler/src/main.rs:584`](../../rio-scheduler/src/main.rs) | `tick-loop` | no | `try_send(Tick)` |
| 4 | [`rio-scheduler/src/admin/mod.rs:109`](../../rio-scheduler/src/admin/mod.rs) | `store-size-refresh` | no | `query_scalar` → gauge |
| 5 | [`rio-scheduler/src/lease/mod.rs:281`](../../rio-scheduler/src/lease/mod.rs) | lease renew (bare loop) | no | `try_acquire_or_renew` |
| 6 | [`rio-store/src/gc/drain.rs:195`](../../rio-store/src/gc/drain.rs) | `gc-drain-task` | no | `drain_once` |
| 7 | [`rio-store/src/gc/orphan.rs:183`](../../rio-store/src/gc/orphan.rs) | `gc-orphan-scanner` | no | `scan_once` |
| 8 | [`rio-scheduler/src/rebalancer.rs:325`](../../rio-scheduler/src/rebalancer.rs) | `rebalancer` | **yes** | `apply_pass` → gauges |
| 9 | [`rio-scheduler/src/logs/flush.rs:118`](../../rio-scheduler/src/logs/flush.rs) | flush loop | **yes** | batch-flush |
| 10 | [`rio-controller/src/reconcilers/gc_schedule.rs:120`](../../rio-controller/src/reconcilers/gc_schedule.rs) | `gc-cron` | **yes** | `TriggerGC` |
| 11 | [`rio-controller/src/scaling.rs:172`](../../rio-controller/src/scaling.rs) | autoscaler | **yes** | `ClusterStatus` → patch |

**Not included** (different shape):
- [`rio-worker/src/main.rs:377`](../../rio-worker/src/main.rs) `heartbeat-loop` — no `select!`, bare `interval.tick().await` (intentional: worker process exits on SIGTERM via main's signal-await, heartbeat loop doesn't need a shutdown arm)
- [`rio-common/src/signal.rs:137`](../../rio-common/src/signal.rs) SIGHUP reload — selects on signal-recv not interval-tick (event-driven, not periodic)
- [`rio-controller/src/reconcilers/workerpool/disruption.rs:83`](../../rio-controller/src/reconcilers/workerpool/disruption.rs) DisruptionTarget watcher — selects on watch-stream not interval-tick (stream-driven)

**Extraction:** `rio_common::task::spawn_periodic(name, interval, shutdown, body_fn)` where `body_fn: FnMut() -> impl Future<Output = ()>`. The helper owns the `tokio::time::interval`, the `loop`, the `select! { biased; cancelled => break, tick => {} }`, and the `spawn_monitored` wrap. Callers pass a closure for the tick body. Net reduction ~200L (each site drops from ~15-25L to ~5-8L).

**Correctness-adjacent, not purely refactor:** adding `biased;` to the seven sites is a behavior change (deterministic shutdown-wins-over-tick). For a 15-min GC interval this is immaterial; for the 1s `health-toggle-loop` and 10s `tick-loop` it shaves up to one interval off graceful-shutdown latency. No existing test asserts the random-vs-biased behavior (P0311-T36 tests the rebalancer's existing `biased;`, not the seven absent ones).

## Entry criteria

- [P0230](plan-0230-rwlock-wire-cpu-bump-classify.md) merged (DONE) — `rebalancer::spawn_task` at [`rebalancer.rs:325`](../../rio-scheduler/src/rebalancer.rs) exists with its `biased;` arm (one of the four correct sites; T2's migration preserves it).

## Tasks

### T1 — `feat(common):` spawn_periodic helper in task.rs

MODIFY [`rio-common/src/task.rs`](../../rio-common/src/task.rs) — add after `spawn_monitored` (`~:48`):

```rust
use std::time::Duration;
use crate::signal::Token;

/// Spawn a periodic background task: run `body` every `interval` until
/// `shutdown` fires. The select! is `biased;` — shutdown wins over a
/// ready tick deterministically (see P0335: tokio::select! defaults to
/// RANDOM branch choice, which can delay shutdown by up to one interval).
///
/// `MissedTickBehavior::Skip` by default: if one `body` call overruns
/// its interval, the next tick fires immediately once, then the
/// interval resynchronizes (no catch-up burst). Override via
/// [`spawn_periodic_with`] if `Burst` or `Delay` is needed.
///
/// The first tick fires immediately (tokio's default). If the caller
/// needs to skip the startup tick (e.g., rebalancer waits one interval
/// before first pass), call `body` conditionally or use
/// [`spawn_periodic_with`] with a pre-consumed first tick.
///
/// Panic inside `body` is caught and logged by the `spawn_monitored`
/// wrapper (task name in the error). The periodic loop does NOT restart
/// — panic ends the task. If restart-on-panic is wanted, wrap the body
/// in `catch_unwind` at the call site.
// r[impl common.task.periodic-biased]
pub fn spawn_periodic<F, Fut>(
    name: &'static str,
    interval: Duration,
    shutdown: Token,
    mut body: F,
) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    spawn_monitored(name, async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    tracing::debug!(task = name, "periodic task shutting down");
                    break;
                }
                _ = ticker.tick() => {}
            }
            body().await;
        }
    })
}
```

**Variant** for the three sites that customize setup (rebalancer skips first tick; lease loop sets `MissedTickBehavior::Skip` explicitly — which is now the default; logs/flush may need a different behavior):

```rust
/// Like [`spawn_periodic`] but the caller constructs the `Interval`
/// (to set a non-default `MissedTickBehavior`, or pre-consume the
/// first tick before the loop starts).
pub fn spawn_periodic_with<F, Fut>(
    name: &'static str,
    mut ticker: tokio::time::Interval,
    shutdown: Token,
    mut body: F,
) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    spawn_monitored(name, async move {
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    tracing::debug!(task = name, "periodic task shutting down");
                    break;
                }
                _ = ticker.tick() => {}
            }
            body().await;
        }
    })
}
```

**Decide at dispatch:** `spawn_periodic` can delegate to `spawn_periodic_with` to keep a single loop body. Also decide whether the `shutdown` arm should be `break` or `return` — several current sites use `return` (lease, rebalancer); `break` is equivalent when the loop is the last expression but clearer for the "loop exits cleanly" intent.

### T2 — `refactor(scheduler):` migrate 5 scheduler sites to spawn_periodic

MODIFY [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs) — sites 1–3 (`health-toggle-loop` :489, `build-samples-retention` :553, `tick-loop` :584). Each collapses from ~18L to ~8L:

```rust
// Before (health-toggle-loop, :489-528, ~40L including the prev-state edge detect):
rio_common::task::spawn_monitored("health-toggle-loop", async move {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    let mut prev: Option<bool> = None;
    loop {
        tokio::select! {
            _ = health_shutdown.cancelled() => { ...; break; }
            _ = interval.tick() => {}
        }
        // ... body
    }
});

// After (closure captures `prev` as a moved-in mutable local):
let mut prev: Option<bool> = None;
rio_common::task::spawn_periodic(
    "health-toggle-loop",
    Duration::from_secs(1),
    health_shutdown,
    move || {
        let is_leader = is_leader.clone();
        let reporter = reporter.clone();
        async move {
            let now = is_leader.load(Ordering::Relaxed);
            if prev != Some(now) { /* ... */ prev = Some(now); }
        }
    },
);
```

**Closure gotcha:** `FnMut() -> impl Future` can't borrow from the `FnMut` across `.await` (the future must be `'static`). Options:
- (a) `Arc<Mutex<Option<bool>>>` for cross-tick state (ugly for a bool)
- (b) change helper sig to take `async fn body(state: &mut S)` with a caller-provided initial `S` — more ergonomic for stateful loops
- (c) keep stateful loops (health-toggle, lease) as inline `spawn_monitored` and only migrate the stateless seven

**Prefer (c) if (b) blows the scope.** The correctness win is the seven `biased;` additions; the LoC reduction is secondary. A half-migration that covers the stateless majority (tick-loop, build-samples-retention, store-size-refresh, gc-drain, gc-orphan, gc-cron, autoscaler — 7 sites) and leaves health-toggle + lease + rebalancer + logs/flush as inline `spawn_monitored` with a `// stateful: not spawn_periodic (needs cross-tick mut state)` comment is fine.

MODIFY [`rio-scheduler/src/admin/mod.rs:109`](../../rio-scheduler/src/admin/mod.rs) — `store-size-refresh` (stateless, clean migration).
MODIFY [`rio-scheduler/src/lease/mod.rs`](../../rio-scheduler/src/lease/mod.rs) — **either** migrate with option-(b) **or** just add `biased;` at `:281` and leave inline (the lease loop has substantial cross-tick state: `was_leading`, `last_successful_renew`).
MODIFY [`rio-scheduler/src/rebalancer.rs:325`](../../rio-scheduler/src/rebalancer.rs) — already has `biased;`; migrate to `spawn_periodic_with` (needs the skip-first-tick pre-consume at `:331`) **or** leave inline with a `// already biased; not migrating — skip-first + gauge-emit makes the body non-trivial` note. P0311-T36's test still covers it either way.
MODIFY [`rio-scheduler/src/logs/flush.rs:118`](../../rio-scheduler/src/logs/flush.rs) — already has `biased;`; migrate or annotate.

### T3 — `refactor(store):` migrate 2 store sites to spawn_periodic

MODIFY [`rio-store/src/gc/drain.rs:195`](../../rio-store/src/gc/drain.rs) — `gc-drain-task`. Stateless body (`drain_once(&pool, &backend).await`), clean migration.
MODIFY [`rio-store/src/gc/orphan.rs:183`](../../rio-store/src/gc/orphan.rs) — `gc-orphan-scanner`. Stateless body (`scan_once`), clean migration.

Both already set `MissedTickBehavior::Skip` explicitly — which matches `spawn_periodic`'s default. Drop the explicit set.

### T4 — `refactor(controller):` migrate 2 controller sites to spawn_periodic

MODIFY [`rio-controller/src/reconcilers/gc_schedule.rs:120`](../../rio-controller/src/reconcilers/gc_schedule.rs) — `gc-cron`. Already has `biased;`. Migrate to `spawn_periodic`.
MODIFY [`rio-controller/src/scaling.rs:172`](../../rio-controller/src/scaling.rs) — autoscaler. Already has `biased;`. Has cross-tick state (`last_scale_time` per-pool via `BTreeMap`) — same closure-state gotcha as T2's health-toggle. Apply option-(c): leave inline with annotation.

### T5 — `test(common):` spawn_periodic biased-shutdown + panic-propagation

NEW tests in [`rio-common/src/task.rs`](../../rio-common/src/task.rs) tests mod (`~:62`):

```rust
/// biased; means shutdown wins over a ready tick. Without it,
/// tokio::select! picks randomly — under load, shutdown could
/// lose to tick indefinitely (unlikely but possible).
// r[verify common.task.periodic-biased]
#[tokio::test(start_paused = true)]
async fn spawn_periodic_biased_shutdown_wins() {
    let shutdown = Token::new();
    let ticks = Arc::new(AtomicU32::new(0));
    let t = ticks.clone();

    let handle = spawn_periodic(
        "test-biased",
        Duration::from_millis(10),
        shutdown.clone(),
        move || {
            let t = t.clone();
            async move { t.fetch_add(1, Ordering::Relaxed); }
        },
    );

    // First tick fires immediately (tokio interval default).
    tokio::task::yield_now().await;
    assert_eq!(ticks.load(Ordering::Relaxed), 1);

    // Advance PAST the interval — tick arm is ready. Cancel
    // simultaneously. biased; → shutdown arm wins, loop breaks
    // BEFORE the body runs again.
    tokio::time::advance(Duration::from_millis(15)).await;
    shutdown.cancel();
    tokio::task::yield_now().await;

    // Still 1: tick was ready but biased; ordering picked shutdown.
    // Without biased;, this would be 1 or 2 nondeterministically.
    assert_eq!(ticks.load(Ordering::Relaxed), 1,
        "biased; should make shutdown win over ready tick");

    handle.await.expect("clean shutdown");
}

/// Panic inside body is caught by spawn_monitored — logged, task
/// ends. JoinHandle reports the panic. The periodic loop does NOT
/// restart.
#[tokio::test(start_paused = true)]
async fn spawn_periodic_panic_ends_task() {
    let shutdown = Token::new();
    let ticks = Arc::new(AtomicU32::new(0));
    let t = ticks.clone();

    let handle = spawn_periodic(
        "test-panic",
        Duration::from_millis(10),
        shutdown,
        move || {
            let t = t.clone();
            async move {
                if t.fetch_add(1, Ordering::Relaxed) == 2 {
                    panic!("third tick panics");
                }
            }
        },
    );

    for _ in 0..5 {
        tokio::time::advance(Duration::from_millis(11)).await;
        tokio::task::yield_now().await;
    }

    let err = handle.await.unwrap_err();
    assert!(err.is_panic());
    // Exactly 3 ticks ran (0, 1, 2 — panic on 2). Loop did not
    // restart after panic.
    assert_eq!(ticks.load(Ordering::Relaxed), 3);
}
```

**Mutation check:** temporarily drop `biased;` from `spawn_periodic` → `spawn_periodic_biased_shutdown_wins` should fail (or flake — run with `--test-threads=1 -- --test-threads=1` a few times and assert at least one failure). This proves the test actually catches the regression. Restore `biased;` → passes.

## Exit criteria

- `/nbr .#ci` green
- `grep -c 'spawn_periodic' rio-common/src/task.rs` → ≥4 (fn def + `_with` variant + 2 tests)
- `grep -c 'tokio::select!' rio-scheduler/src/main.rs rio-scheduler/src/admin/mod.rs rio-store/src/gc/drain.rs rio-store/src/gc/orphan.rs` — reduced from 6 pre-dispatch (exact target depends on option-(c) decisions for stateful loops)
- `grep 'spawn_periodic' rio-scheduler/src/main.rs rio-scheduler/src/admin/mod.rs rio-store/src/gc/drain.rs rio-store/src/gc/orphan.rs rio-controller/src/reconcilers/gc_schedule.rs` → ≥5 hits (stateless sites migrated)
- For each site NOT migrated (stateful: health-toggle, lease, possibly rebalancer/scaling): `grep 'biased;' <file>` → ≥1 hit at that site's `select!` (option-(c) adds `biased;` inline if not already present)
- `cargo nextest run -p rio-common spawn_periodic_biased_shutdown_wins spawn_periodic_panic_ends_task` → 2 passed
- **T5 mutation:** drop `biased;` from `spawn_periodic` → `spawn_periodic_biased_shutdown_wins` FAILS (or at minimum is unable to deterministically pass; run 10× with paused clock — paused clock makes both arms ready on the same yield, so random choice surfaces)
- `nix develop -c tracey query rule common.task.periodic-biased` → shows 1 impl + 1 verify site
- Net LoC: `git diff --shortstat` shows deletions > insertions in the six caller crates (rio-scheduler, rio-store, rio-controller combined). Helper itself is ~60L new.

## Tracey

References existing markers:
- `r[common.drain.not-serving-before-exit]` — tangentially: the `biased;` fix means the periodic loops shut down within one scheduler tick of `shutdown.cancel()`, tightening the graceful-drain window. No annotation change — the drain spec is about tonic-health state, not periodic task cleanup.

Adds new marker to component spec:
- `r[common.task.periodic-biased]` → [`docs/src/observability.md`](../../docs/src/observability.md) (see ## Spec additions)

## Spec additions

New `r[common.task.periodic-biased]` goes into [`docs/src/observability.md`](../../docs/src/observability.md) after the `r[common.drain.not-serving-before-exit]` block (`~:218`), standalone paragraph, blank line before, col 0:

```
r[common.task.periodic-biased]
Periodic background tasks (interval-driven loops with a shutdown arm) MUST use `biased;` ordering in their `tokio::select!` so shutdown cancellation wins deterministically over a ready interval tick. Without `biased;`, tokio randomizes branch selection for fairness; a task may execute one more tick-body after cancellation fires, which delays graceful shutdown by up to one interval (seconds to hours depending on the task). The `rio_common::task::spawn_periodic` helper encapsulates this pattern. Stateful loops that cannot use the helper MUST inline `biased;` at their `select!`.
```

Rationale for placing in `observability.md` (not a new `common.md` component spec): the existing `r[common.*]` family lives there (`r[common.drain.not-serving-before-exit]` at `:212`). Periodic-biased is the same "graceful shutdown correctness" concern.

## Files

```json files
[
  {"path": "rio-common/src/task.rs", "action": "MODIFY", "note": "T1: +spawn_periodic + spawn_periodic_with after :48; T5: +2 tests in tests mod after :62"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "T2: migrate/annotate 3 sites (:489 health-toggle, :553 samples-retention, :584 tick-loop)"},
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "MODIFY", "note": "T2: migrate store-size-refresh :109 (stateless)"},
  {"path": "rio-scheduler/src/lease/mod.rs", "action": "MODIFY", "note": "T2: +biased; at :281 (stateful — option-c inline)"},
  {"path": "rio-scheduler/src/rebalancer.rs", "action": "MODIFY", "note": "T2: migrate to spawn_periodic_with (skip-first-tick) OR annotate :325 (already biased)"},
  {"path": "rio-scheduler/src/logs/flush.rs", "action": "MODIFY", "note": "T2: migrate OR annotate :118 (already biased)"},
  {"path": "rio-store/src/gc/drain.rs", "action": "MODIFY", "note": "T3: migrate gc-drain-task :195 (stateless)"},
  {"path": "rio-store/src/gc/orphan.rs", "action": "MODIFY", "note": "T3: migrate gc-orphan-scanner :183 (stateless)"},
  {"path": "rio-controller/src/reconcilers/gc_schedule.rs", "action": "MODIFY", "note": "T4: migrate gc-cron :120 (already biased)"},
  {"path": "rio-controller/src/scaling.rs", "action": "MODIFY", "note": "T4: annotate :172 stateful (already biased; option-c)"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T1: +r[common.task.periodic-biased] marker after :218"}
]
```

```
rio-common/src/task.rs          # T1: spawn_periodic helper + tests
rio-scheduler/src/
├── main.rs                     # T2: 3 sites (HOT count=35)
├── admin/mod.rs                # T2: store-size-refresh
├── lease/mod.rs                # T2: +biased; (stateful)
├── rebalancer.rs               # T2: migrate or annotate
└── logs/flush.rs               # T2: migrate or annotate
rio-store/src/gc/
├── drain.rs                    # T3: gc-drain-task
└── orphan.rs                   # T3: gc-orphan-scanner
rio-controller/src/
├── reconcilers/gc_schedule.rs  # T4: gc-cron
└── scaling.rs                  # T4: annotate (stateful)
docs/src/observability.md       # T1: new marker
```

## Dependencies

```json deps
{"deps": [230], "soft_deps": [335, 343, 355, 366, 311], "note": "discovered_from=consolidator-mc140. Hard-dep P0230 (DONE): rebalancer::spawn_task at :325 with biased; arm exists — one of the four correct sites; T2 migration preserves it. Soft-dep P0335 (DONE): the biased; lesson that motivates this — tokio::select! RANDOM default. Soft-dep P0343+P0355 (DONE): spawn_health_plaintext + spawn_drain_task extractions — same extraction-pattern for rio-common. Soft-dep P0366 (UNIMPL): touches rebalancer.rs spawn_task :350+ for gauge re-emit — T2 touches :325-348 (the loop shell, not the body); if P0366 lands first and P0366 migrates to spawn_periodic itself, T2's rebalancer touch is OBE; if T2 lands first, P0366's gauge-emit goes in the spawn_periodic body closure. Soft-dep P0311-T36: tests spawn_task's biased; via shutdown — still valid post-migration (the test exercises the biased behavior regardless of helper vs inline). rio-scheduler/src/main.rs is HOT (count=35, 5th highest) — T2's 3 sites are each ~15L delete + ~8L add at distinct call-spawns, non-overlapping with P0304-T95 (rebalancer cfg thread :295), P0304-T77 (insert_build jti :382). rio-common/src/task.rs is low-traffic (single addition since phase4a). observability.md (count=27) — T1 marker-add is standalone paragraph after :218, non-overlapping with P0295-T52 (Histogram Buckets :204), P0295-T57 (Worker Metrics :156), P0311-T35 (:208 sentence). Expected net LoC ~-200 across 11 sites - +80 helper = -120 total."}
```

**Depends on:** [P0230](plan-0230-rwlock-wire-cpu-bump-classify.md) (DONE) — `rebalancer::spawn_task` exists.
**Soft-deps:** [P0335](plan-0335-channelsession-drop-abort-race.md) (DONE — source of the `biased;` lesson), [P0343](plan-0343-extract-spawn-health-plaintext-common.md)/[P0355](plan-0355-extract-drain-jwt-load-helpers.md) (DONE — sibling extractions to rio-common), [P0366](plan-0366-cutoff-seconds-gauge-re-emit.md) (UNIMPL — same rebalancer.rs file, non-overlapping hunks).
**Conflicts with:** [`main.rs`](../../rio-scheduler/src/main.rs) count=35 — T2 edits three distinct spawn-sites, additive-collapse, non-overlapping with [P0304-T95](plan-0304-trivial-batch-p0222-harness.md) (rebalancer cfg). [`observability.md`](../../docs/src/observability.md) count=27 — T1 marker-add at `:218+`, non-overlapping with P0295-T52/T57, P0311-T35. [`rebalancer.rs`](../../rio-scheduler/src/rebalancer.rs) — [P0366](plan-0366-cutoff-seconds-gauge-re-emit.md) edits `:350+` body; T2 edits `:325-348` shell; rebase-clean either order.
