# Plan 993342101: handle_tick — extract 5 collect-then-process helpers

Consolidator finding. [`handle_tick` at worker.rs:440-698](../../rio-scheduler/src/actor/worker.rs) is 258 lines. [P0214](plan-0214-per-build-timeout.md) added a 4th collect-then-process block at [`:570-609`](../../rio-scheduler/src/actor/worker.rs) and wrote it as-if-already-extracted — it touches `self.*` exclusively, no locals from the surrounding 200 lines. That's the shape of a private method that was never pulled out. Four other blocks have the same shape.

**Low-churn window is NOW.** Per consolidator: [P0228](plan-0228-completion-samples-write-classdrift-counter.md) touches `completion.rs` not `worker.rs`; no UNIMPL plan adds a 6th tick-check; [P0230](plan-0230-rwlock-wire-cpu-bump-classify.md) is DISPATCH-SOLO on `actor/mod.rs` but doesn't touch `handle_tick` in `worker.rs`. The file is count=26 in collisions — hot, but this is a pure-rearrangement refactor with fault lines clean.

Precedent: [P0062](plan-0062-scheduler-actor-split.md) split 4760-line `actor.rs` into the current module set. Same crate, same discipline (mechanical extraction, behavior-preserving, test-green throughout).

**Rider:** [`mod.rs:690`](../../rio-scheduler/src/actor/mod.rs) vs [`:713`](../../rio-scheduler/src/actor/mod.rs) have drifted Instant-backdate styles — `.or(Some(Instant::now()))` vs `.unwrap_or_else(Instant::now)`. Same semantics, different spelling. A 6-line `backdate(secs_ago: u64) -> Instant` helper unifies them. Included here because both touch `actor/` and the rider is too small for its own plan.

## Tasks

### T1 — `refactor(scheduler):` extract tick_check_heartbeats

The heartbeat block at [`worker.rs:443-460`](../../rio-scheduler/src/actor/worker.rs): scan `self.workers`, collect timed-out IDs, call `handle_worker_disconnected` on each. 18 lines, zero locals shared with the rest of `handle_tick` except `now` (pass as param).

```rust
async fn tick_check_heartbeats(&mut self, now: Instant) {
    let timeout = std::time::Duration::from_secs(HEARTBEAT_TIMEOUT_SECS);
    let mut timed_out_workers = Vec::new();
    for (worker_id, worker) in &mut self.workers {
        if now.duration_since(worker.last_heartbeat) > timeout {
            worker.missed_heartbeats += 1;
            if worker.missed_heartbeats >= MAX_MISSED_HEARTBEATS {
                timed_out_workers.push(worker_id.clone());
            }
        }
    }
    for worker_id in timed_out_workers {
        warn!(worker_id = %worker_id, "worker timed out (missed heartbeats)");
        self.handle_worker_disconnected(&worker_id).await;
    }
}
```

### T2 — `refactor(scheduler):` extract tick_scan_dag (poison-TTL + backstop, shared iter)

[`worker.rs:462-568`](../../rio-scheduler/src/actor/worker.rs). **The one coupled block** — poison-TTL-expiry collection and backstop-timeout collection share a single `self.dag.iter_nodes()` pass at `:469`. The consolidator flagged this as the exception to clean fault lines. Two options:

**Option A (keep coupled, single helper):** extract `tick_scan_dag(&mut self, now: Instant) -> (Vec<DrvHash>, Vec<(DrvHash, String, WorkerId)>)` that returns both collect-vectors. Caller processes. Preserves the single-pass property.

**Option B (split, two passes):** `tick_collect_expired_poisons` + `tick_collect_backstop_timeouts`, each with its own `iter_nodes()`. Two passes over the DAG instead of one.

**Choose A.** The single pass matters: `iter_nodes()` on a 10k-node DAG is ~microseconds, but `handle_tick` runs every 10s — adding passes is the kind of thing that compounds across future tick-checks. The coupling is a documented trade (comment at `:462-464` already frames them together). Extract the scan as one helper; keep the two process-loops (`:525-568` backstop, `:611-624` poison) as separate helpers that consume the scan's output.

```rust
/// Single DAG pass collecting both poison-TTL expiries and backstop-
/// timeout candidates. Coupled because the two checks share the
/// per-node iteration; splitting would double iter_nodes() passes
/// for no behavioral gain.
fn tick_scan_dag(&self, now: Instant)
    -> (Vec<DrvHash>, Vec<(DrvHash, String, WorkerId)>)
{
    let mut expired_poisons = Vec::new();
    let mut backstop_timeouts = Vec::new();
    for (drv_hash, state) in self.dag.iter_nodes() {
        // ... :470-518 body verbatim
    }
    (expired_poisons, backstop_timeouts)
}

async fn tick_process_backstop_timeouts(&mut self,
    timeouts: &[(DrvHash, String, WorkerId)]) { /* :525-568 */ }

async fn tick_process_expired_poisons(&mut self,
    poisons: Vec<DrvHash>) { /* :611-624 */ }
```

Caller in `handle_tick` becomes three lines: scan, process-backstop, process-poisons (in that order — backstop first matches current code, `:525` before `:611`).

### T3 — `refactor(scheduler):` extract tick_check_build_timeouts

[`worker.rs:570-609`](../../rio-scheduler/src/actor/worker.rs) — the P0214 block. Already shaped as a method: reads `self.builds`, writes `self.builds`, calls `self.cancel_build_derivations` + `self.transition_build_to_failed`. Zero locals from surrounding code. Move verbatim into `async fn tick_check_build_timeouts(&mut self)`. The `r[impl sched.timeout.per-build]` annotation at `:570` moves with it.

### T4 — `refactor(scheduler):` extract tick_sweep_event_log + tick_publish_gauges

Two remaining blocks, both cleanly separable:

- [`worker.rs:626-661`](../../rio-scheduler/src/actor/worker.rs): event-log time-based sweep (spawn_monitored DELETE, 24h retention, every 360 ticks). → `fn tick_sweep_event_log(&self)`. Not `async` — it spawns and returns.
- [`worker.rs:663-697`](../../rio-scheduler/src/actor/worker.rs): leader-gated gauge publish. → `fn tick_publish_gauges(&self)`. The `r[impl obs.metric.scheduler-leader-gate]` annotation at `:669` moves with it.

After T1-T4, `handle_tick` becomes:

```rust
pub(super) async fn handle_tick(&mut self) {
    self.maybe_refresh_estimator().await;
    let now = Instant::now();

    self.tick_check_heartbeats(now).await;

    let (expired_poisons, backstop_timeouts) = self.tick_scan_dag(now);
    self.tick_process_backstop_timeouts(&backstop_timeouts).await;
    self.tick_check_build_timeouts().await;
    self.tick_process_expired_poisons(expired_poisons).await;

    self.tick_sweep_event_log();
    self.tick_publish_gauges();
}
```

~15 lines. The ordering is load-bearing (backstop-process before build-timeout, poison-expire last — matches current `:525` → `:570` → `:611`). Add a comment explaining why.

### T5 — `refactor(scheduler):` backdate(secs_ago) helper — mod.rs style drift

Rider. [`mod.rs:690-692`](../../rio-scheduler/src/actor/mod.rs) uses `.or(Some(Instant::now()))`; [`mod.rs:713-715`](../../rio-scheduler/src/actor/mod.rs) uses `.unwrap_or_else(Instant::now)`. Both in `#[cfg(test)]` `DebugBackdate*` handlers, both with the same defensive-clamp comment. Unify:

```rust
// In mod.rs, near the other #[cfg(test)] helpers (or wherever
// DebugBackdateRunning's impl block lives):
#[cfg(test)]
fn backdate(secs_ago: u64) -> Instant {
    // checked_sub defensive: if secs_ago is absurd (u64::MAX) and
    // Instant::now() can't represent that far back, clamp to "now"
    // (effectively 0 elapsed). Tokio paused time can't mock Instant.
    Instant::now()
        .checked_sub(std::time::Duration::from_secs(secs_ago))
        .unwrap_or_else(Instant::now)
}
```

Both call-sites become `state.running_since = Some(backdate(secs_ago))` / `build.submitted_at = backdate(secs_ago)`. The comment lives once on the helper instead of twice (currently `:686-689` and `:710-711`). Delete the duplicates.

## Exit criteria

- `/nbr .#ci` green — **this is the whole bar for a behavior-preserving refactor**
- `wc -l rio-scheduler/src/actor/worker.rs` — total grows slightly (helper fn headers + doc-comments) but `handle_tick` itself shrinks to <25 lines
- `grep -c 'r\[impl sched.timeout.per-build\]' rio-scheduler/src/actor/worker.rs` → 1 (annotation moved with T3, not dropped)
- `grep -c 'r\[impl obs.metric.scheduler-leader-gate\]' rio-scheduler/src/actor/worker.rs` → 1 (annotation moved with T4, not dropped)
- `grep -c 'r\[impl sched.backstop.timeout\]' rio-scheduler/src/actor/worker.rs` → 1 (annotation stays in/near `tick_scan_dag` — it's on the collect, not the process)
- `nix develop -c cargo nextest run -p rio-scheduler` — **byte-identical pass/fail set** to pre-refactor. If a test name changes or a test is added, that's scope creep.
- `grep 'fn backdate' rio-scheduler/src/actor/mod.rs` → 1 hit (T5)
- `grep -c 'or(Some(Instant::now()))\|checked_sub.*unwrap_or_else' rio-scheduler/src/actor/mod.rs` → `grep backdate(secs_ago)` count ≥ 2 (both call-sites migrated) AND raw checked_sub-chain count drops to 1 (inside `backdate` itself)
- `nix develop -c tracey query validate` → `0 total error(s)` (no annotations broken by the move)

## Tracey

**Pure refactor — moves existing annotations, adds none.**

References existing markers (T1-T4 MOVE these; the plan asserts they land in the extracted helpers, not that they're newly implemented):
- `r[sched.backstop.timeout]` — moves from `:464` into `tick_scan_dag` (or its doc-comment)
- `r[sched.timeout.per-build]` — moves from `:570` into `tick_check_build_timeouts`
- `r[obs.metric.scheduler-leader-gate]` — moves from `:669` into `tick_publish_gauges`

No new markers. No spec additions. A refactor that needs a new spec marker isn't a refactor.

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "T1-T4: extract 7 private tick_* helpers from handle_tick; handle_tick shrinks to ~15 lines calling them in preserved order"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "T5: #[cfg(test)] backdate(secs_ago) helper; DebugBackdateRunning :690 + DebugBackdateSubmitted :713 call it; delete duplicate defensive-clamp comments"}
]
```

```
rio-scheduler/src/actor/
├── worker.rs             # T1-T4: handle_tick → 7 tick_* helpers
└── mod.rs                # T5: backdate() helper, 2 call-sites
```

## Dependencies

```json deps
{"deps": [214], "soft_deps": [230, 228], "note": "Hard-dep P0214 (DONE): the :570-609 block being extracted IS P0214's code. Pre-P0214 the block doesn't exist. Soft-dep P0230: DISPATCH-SOLO on actor/mod.rs (count=35, #1 hottest). T5 here is a 6-line #[cfg(test)] helper in the same file, different section — P0230 touches the size_classes field + rebalancer spawn, T5 touches the DebugBackdate handlers. Serializable if coordinator dispatches both same-day; otherwise parallel-safe. Soft-dep P0228: touches completion.rs not worker.rs (consolidator confirmed), but completion.rs and worker.rs are sibling actor modules — if P0228 changes a signature that worker.rs calls, T2's tick_process_backstop_timeouts (which calls reassign_derivations at :566) could be affected. Low probability. discovered_from=consolidator (no single origin plan)."}
```

**Depends on:** [P0214](plan-0214-per-build-timeout.md) merged (DONE) — T3 extracts its code.

**Conflicts with:** [`worker.rs`](../../rio-scheduler/src/actor/worker.rs) count=26. No UNIMPL plan adds a new tick-check (consolidator verified). [`mod.rs`](../../rio-scheduler/src/actor/mod.rs) count=35 — [P0230](plan-0230-rwlock-wire-cpu-bump-classify.md) is DISPATCH-SOLO on it. T5 is 6 lines in `#[cfg(test)]` handlers at `:680-721`; P0230 touches `size_classes` field declaration + rebalancer spawn. Different sections. If coordinator sequences P0230 first, T5 rebases trivially. **This plan should dispatch BEFORE any plan that adds a 6th tick-check** — extraction cost grows with each added block.
