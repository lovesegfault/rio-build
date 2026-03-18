# Plan 0212: GC automation — run_gc extraction + sweep metrics + controller cron

Wave 2. Three pieces: (1) extract `run_gc` from the gRPC handler so it's callable outside the admin stream context; (2) emit sweep metrics; (3) add a controller cron reconciler that calls `TriggerGC` on a schedule. Today GC is manual-only (`rio-cli gc` or direct gRPC).

**R-CONN constraint:** per [`lang-gotchas.md`](../../.claude/lang-gotchas.md), "eager `.connect().await?` in main = process-exit" and tonic has no default connect timeout — a stale IP hangs on SYN forever. The cron loop **connects fresh per iteration INSIDE the loop with `tokio::time::timeout(30s, ...)`**; on Err → `warn!` + increment failure counter + `continue`. **NEVER `?`-propagate connect error out of the loop.**

No dependency on P0206/P0207 — GC automation works with or without tenant retention. The controller just calls `TriggerGC`; whatever mark-phase seeds exist at the time are what get used.

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync-markers.md) merged (marker `r[ctrl.gc.cron-schedule]` exists in `controller.md`)

## Tasks

### T1 — `refactor(store):` extract `run_gc` from `grpc/admin.rs::trigger_gc`

At [`rio-store/src/gc/mod.rs`](../../rio-store/src/gc/mod.rs), add:

```rust
/// Mark→sweep→spawn-drain with advisory locks. Extracted from
/// grpc/admin.rs::trigger_gc so it's callable outside the stream context.
///
/// Audit C #27: GcParams struct (was: positional with grace_hours +
/// extra_roots missing — would break gRPC API that accepts both).
pub struct GcParams {
    pub dry_run: bool,
    pub grace_hours: u32,
    pub extra_roots: Vec<String>,
}
pub async fn run_gc(
    pool: &PgPool,
    backend: Arc<dyn ChunkBackend>,
    params: &GcParams,
    progress_tx: mpsc::Sender<GcProgress>,
) -> Result<GcStats> {
    // Advisory locks GC_LOCK_ID + GC_MARK_LOCK_ID (currently at admin.rs:24).
    // MOVE the r[impl store.gc.empty-refs-gate] annotation here from admin.rs.
    // compute_unreachable(pool, params.grace_hours, &params.extra_roots)
    // check_empty_refs_gate(pool, params.grace_hours, ...)
    // ... mark → sweep → spawn-drain ...
}
// Cron caller: run_gc(pool, backend, &GcParams{dry_run:false, grace_hours:2, extra_roots:vec![]}, tx)
```

Shrink [`rio-store/src/grpc/admin.rs:344-539+`](../../rio-store/src/grpc/admin.rs) `trigger_gc` to a thin wrapper: create channel, spawn `run_gc`, stream-forward `GcProgress` to the gRPC response stream.

**IMPORTANT:** `r[impl store.gc.empty-refs-gate]` currently lives in admin.rs — it moves WITH the code to `gc/mod.rs::run_gc`. This is a relocation, not a new annotation.

### T2 — `feat(store):` sweep metrics

At [`rio-store/src/gc/sweep.rs`](../../rio-store/src/gc/sweep.rs), after computing `GcStats` (fields already exist at [`gc/mod.rs:79-94`](../../rio-store/src/gc/mod.rs)):

```rust
// Audit B1 #13: singular naming (matches rio_store_gc_path_resurrected_total)
// + correct field (GcStats has s3_keys_enqueued NOT chunks_enqueued, mod.rs:85)
metrics::counter!("rio_store_gc_path_swept_total").increment(stats.paths_deleted as u64);
metrics::counter!("rio_store_gc_s3_key_enqueued_total").increment(stats.s3_keys_enqueued as u64);
```

Register both in [`rio-store/src/lib.rs`](../../rio-store/src/lib.rs) describe block AND add to [`docs/src/observability.md`](../../docs/src/observability.md) (CLAUDE.md: metric names must match observability.md).

### T3 — `feat(controller):` GC cron reconciler

NEW file `rio-controller/src/reconcilers/gc_schedule.rs`:

```rust
//! GC cron reconciler. Calls store-admin TriggerGC on an interval.
//!
//! r[impl ctrl.gc.cron-schedule]
//!
//! CRITICAL: tonic has no default connect timeout — a stale IP hangs
//! on SYN forever. Connect fresh per-tick with timeout; on failure,
//! warn + counter + continue. NEVER ?-propagate out of the loop.

use std::time::Duration;
use tokio::time::{interval, timeout};
use tokio_util::sync::CancellationToken;

pub async fn run(store_admin_addr: String, tick: Duration, shutdown: CancellationToken) {
    let mut ticker = interval(tick);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => {
                match timeout(Duration::from_secs(30), connect_store_admin(&store_admin_addr)).await {
                    Ok(Ok(mut client)) => {
                        match client.trigger_gc(/* dry_run: false */).await {
                            Ok(mut stream) => {
                                while let Some(progress) = stream.message().await.transpose() {
                                    match progress {
                                        Ok(p) => info!(?p, "gc progress"),
                                        Err(e) => { warn!(?e, "gc stream error"); break; }
                                    }
                                }
                                metrics::counter!("rio_controller_gc_runs_total", "result" => "success").increment(1);
                            }
                            Err(e) => {
                                warn!(?e, "TriggerGC rpc failed");
                                metrics::counter!("rio_controller_gc_runs_total", "result" => "rpc_failure").increment(1);
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        warn!(?e, addr = %store_admin_addr, "store-admin connect failed");
                        metrics::counter!("rio_controller_gc_runs_total", "result" => "connect_failure").increment(1);
                    }
                    Err(_elapsed) => {
                        warn!(addr = %store_admin_addr, "store-admin connect timed out (30s)");
                        metrics::counter!("rio_controller_gc_runs_total", "result" => "connect_failure").increment(1);
                    }
                }
            }
        }
    }
}
```

Default interval 24h, configurable via `controller.toml gc_interval_hours` (0 = disabled — reconciler not spawned).

Wire in [`rio-controller/src/main.rs`](../../rio-controller/src/main.rs) via `spawn_monitored("gc-cron", ...)` near `:346`. Add `pub mod gc_schedule;` to [`rio-controller/src/reconcilers/mod.rs`](../../rio-controller/src/reconcilers/mod.rs).

### T4 — `docs(observability):` flip metric row to "emitted"

At [`docs/src/observability.md:185`](../../docs/src/observability.md): flip `rio_controller_gc_runs_total` from "(Phase 4+) not yet emitted" to the actual row with labels `result={success,connect_failure,rpc_failure}`.

### T5 — `test(controller):` paused-time cron unit test

With `tokio::time::pause` + mock StoreAdminClient: advance 24h → assert 1 `TriggerGC` call → assert `rio_controller_gc_runs_total{result="success"}` = 1.

Connect-fail path: mock client returns `Err` → assert counter incremented `result="connect_failure"` AND loop continues (advance another 24h → another attempt).

Marker: `// r[verify ctrl.gc.cron-schedule]`

## Exit criteria

- `/nbr .#ci` green
- Paused-time unit test: exactly 1 `TriggerGC` call per 24h tick
- Connect-fail path: `result="connect_failure"` counter increments, loop continues (next tick tries again)
- `rio_store_gc_paths_swept_total` and `rio_store_gc_chunks_enqueued_total` registered in `rio-store/src/lib.rs`
- `observability.md:185` row updated to "Implemented"

## Tracey

References existing markers:
- `r[ctrl.gc.cron-schedule]` — T3 implements (gc_schedule.rs); T5 verifies (paused-time test)
- `r[store.gc.empty-refs-gate]` — **MOVES** from `admin.rs` to `gc/mod.rs::run_gc` (relocation of existing annotation, not new)

## Files

```json files
[
  {"path": "rio-store/src/gc/mod.rs", "action": "MODIFY", "note": "T1: add run_gc fn; r[impl store.gc.empty-refs-gate] moves here"},
  {"path": "rio-store/src/gc/sweep.rs", "action": "MODIFY", "note": "T2: emit paths_swept_total + chunks_enqueued_total counters"},
  {"path": "rio-store/src/grpc/admin.rs", "action": "MODIFY", "note": "T1: shrink trigger_gc to thin wrapper (create channel, spawn run_gc, forward stream)"},
  {"path": "rio-store/src/lib.rs", "action": "MODIFY", "note": "T2: describe_counter for 2 new sweep metrics"},
  {"path": "rio-controller/src/reconcilers/gc_schedule.rs", "action": "NEW", "note": "T3: cron loop with timeout+connect-per-tick; r[impl ctrl.gc.cron-schedule]"},
  {"path": "rio-controller/src/reconcilers/mod.rs", "action": "MODIFY", "note": "T3: pub mod gc_schedule;"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "T3: spawn_monitored gc-cron near :346 (gated on gc_interval_hours > 0)"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T4: flip rio_controller_gc_runs_total row to implemented"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "T3: note the cron reconciler under the r[ctrl.gc.cron-schedule] marker"}
]
```

```
rio-store/src/
├── gc/
│   ├── mod.rs                     # T1: +run_gc (marker moves here)
│   └── sweep.rs                   # T2: metrics
├── grpc/admin.rs                  # T1: shrink to wrapper
└── lib.rs                         # T2: describe
rio-controller/src/
├── reconcilers/
│   ├── gc_schedule.rs             # T3 (NEW): cron loop
│   └── mod.rs                     # T3: pub mod
└── main.rs                        # T3: spawn_monitored
docs/src/
├── observability.md               # T4: metric row
└── components/controller.md       # T3: cron note
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [], "note": "No dep on P0206/P0207 — GC automation is orthogonal to tenant retention. gc/mod.rs trivially mergeable w/ P0207."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync-markers.md) — marker `r[ctrl.gc.cron-schedule]` must exist.

**Conflicts with:**
- [`gc/mod.rs`](../../rio-store/src/gc/mod.rs) — P0207 adds `pub mod tenant;` at `:44` (1 line); T1 adds `run_gc` fn elsewhere in the 465-line file. Trivially mergeable.
- `grpc/admin.rs` — single-writer in 4b.
- `controller/main.rs` — single-writer in 4b.
