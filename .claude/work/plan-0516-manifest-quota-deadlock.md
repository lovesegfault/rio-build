# Plan 516: manifest-mode quota-exhaustion deadlock — spawn-fail short-circuits before sweep

[P0511](plan-0511-manifest-failed-job-sweep.md) review surfaced a control-flow ordering problem: `reconcile_manifest` runs **spawn → sweep** (not sweep → spawn). The spawn loop at [`manifest.rs:298-310`](../../rio-controller/src/reconcilers/builderpool/manifest.rs) returns on any non-409 `jobs_api.create` error:

```rust
Err(kube::Error::Api(ae)) if ae.code == 409 => { debug!(... "collision; will retry"); }
Err(e) => return Err(e.into()),   // :309 — short-circuits BEFORE sweep at :393
```

The Failed-Job sweep at [`:393-449`](../../rio-controller/src/reconcilers/builderpool/manifest.rs) never runs if spawn bailed. Under a `ResourceQuota` on `count/jobs.batch` (not in rio's helm but common in hardened clusters and default on GKE Autopilot), this deadlocks:

1. Crash-loop fills the namespace's Job quota with Failed Jobs
2. Next tick: spawn fires → **403 Forbidden** (`exceeded quota: count/jobs.batch`)
3. `return Err` at `:309` → sweep at `:393` never runs → Failed Jobs never cleared
4. Next tick: quota still exhausted → 403 → no sweep → repeat forever

The reconciler cannot recover without operator `kubectl delete`. Pathological (rio's helm sets no `ResourceQuota`) but real — GKE Autopilot's default quota is 5000 Jobs/namespace, reachable in ~14 hours of crash-loop at `replicas.max=10`.

The delete-error handling at [`:442-447`](../../rio-controller/src/reconcilers/builderpool/manifest.rs) already does the right thing (`warn!` + continue, not `return`). Spawn should do the same — or better, sweep FIRST so dead weight clears before we try to spawn into headroom it's occupying.

## Entry criteria

- [P0511](plan-0511-manifest-failed-job-sweep.md) merged (DONE — sweep block at `:393` exists)
- [P0515](plan-0515-manifest-sweep-cap-divergence.md) merged (const→param change touches the same `select_failed_jobs` call site; smaller diff, serialize it first)

## Tasks

### T1 — `fix(controller):` reorder — sweep Failed Jobs BEFORE spawn

MODIFY [`rio-controller/src/reconcilers/builderpool/manifest.rs`](../../rio-controller/src/reconcilers/builderpool/manifest.rs). Move the Failed-Job sweep block (`:393-449`) to immediately after the `failed_total`/`failed_jobs` computation at `:234-239`, BEFORE the spawn block (`:261-319`).

Rationale in the moved comment:

```rust
// ---- Sweep Failed Jobs FIRST ----
// This MUST run before spawn: under a namespace ResourceQuota on
// count/jobs.batch (GKE Autopilot default, common in hardened
// clusters), a crash-loop fills the quota with Failed Jobs. If
// spawn-before-sweep, jobs_api.create 403s on quota exhaustion →
// return Err → sweep never runs → deadlock (can't clear quota to
// make room for the spawn that would succeed next). Sweep-first
// clears dead weight; spawn then has room.
//
// Separate from the idle-reapable pass below: Failed Jobs need no
// idle-check (no running pod) and no ListExecutors RPC. This block
// is self-contained — runs unconditionally, bounded-per-tick
// (select_failed_jobs caps internally).
```

The `CrashLoopDetected` event emit stays with the sweep (moves together — same block). The idle-reapable scale-down at `:325-391` stays where it is (AFTER spawn is fine — it's not quota-relevant, it deletes Jobs that were successfully created earlier).

**Ordering detail:** after the move, `failed_jobs` references must still be valid at the sweep site. They are — `failed_jobs: Vec<&Job>` borrows from `jobs.items` which lives to end-of-function.

### T2 — `fix(controller):` spawn error → warn+continue, not bail

Even with T1's reorder, a non-quota spawn failure (e.g., admission webhook rejection, malformed spec) shouldn't abort the WHOLE reconcile tick. The delete loop at `:442-447` already `warn!`+`continue`s on error — apply the same pattern to spawn at `:309`:

```rust
// :306-310 replacement
Err(kube::Error::Api(ae)) if ae.code == 409 => {
    debug!(pool = %name, job = %job_name, "Job name collision; will retry");
}
Err(e) => {
    // warn+continue, not bail. A spawn failure (quota, webhook
    // rejection, transient apiserver blip) shouldn't skip the
    // rest of this tick's work — subsequent spawns in the batch
    // may succeed (different bucket → different resource limits),
    // and the idle-reapable pass below is independent. Matches
    // delete-error handling in the sweep loop.
    warn!(
        pool = %name, job = %job_name, bucket = ?directive.bucket,
        error = %e, "manifest Job spawn failed; continuing tick"
    );
}
```

This is defense-in-depth: T1 handles the specific quota-deadlock, T2 handles the general "one spawn error shouldn't skip unrelated work" principle.

### T3 — `test(controller):` sweep runs even when spawn would fail

MODIFY [`rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs`](../../rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs). The reconcile loop is IO-heavy (k8s API calls), so a full deadlock repro needs [P0512](plan-0512-manifest-reconcile-vm-test.md)'s VM scaffolding. But the control-flow property is unit-testable by inspection:

```rust
// r[verify ctrl.pool.manifest-failed-sweep]
#[test]
fn sweep_ordered_before_spawn_in_source() {
    // STRUCTURAL GUARD: the Failed-Job sweep MUST appear before the
    // spawn loop in reconcile_manifest's body. A return-on-spawn-
    // error can't skip a sweep that already ran. This test greps
    // the source — brittle to refactoring, but that brittleness is
    // the point: anyone reordering these sections trips this test
    // and has to consciously decide the ordering is still safe.
    //
    // Quota-deadlock scenario this guards: ResourceQuota on
    // count/jobs.batch exhausted by Failed Jobs → spawn 403 →
    // pre-reorder: return Err skipped sweep → can't clear quota.
    let src = include_str!("../manifest.rs");
    let sweep_marker = "---- Sweep Failed Jobs FIRST ----";
    let spawn_marker = "---- Spawn ----";
    let sweep_pos = src.find(sweep_marker)
        .expect("sweep section comment present");
    let spawn_pos = src.find(spawn_marker)
        .expect("spawn section comment present");
    assert!(
        sweep_pos < spawn_pos,
        "Failed-Job sweep at byte {} MUST precede spawn at byte {}. \
         Reordering spawn-before-sweep reintroduces the quota-\
         exhaustion deadlock (spawn 403 → early return → sweep \
         skipped → quota never clears).",
        sweep_pos, spawn_pos,
    );
}
```

Also verify T2's error-handling change:

```rust
#[test]
fn spawn_loop_no_early_return_on_error() {
    // T2: the Err arm at the jobs_api.create match is warn+continue,
    // not return. grep for the specific `return Err(e.into())` that
    // was the deadlock line — it should be gone from the spawn loop.
    let src = include_str!("../manifest.rs");
    let spawn_start = src.find("---- Spawn ----").unwrap();
    let spawn_end = src[spawn_start..].find("---- Scale-down").unwrap() + spawn_start;
    let spawn_block = &src[spawn_start..spawn_end];
    assert!(
        !spawn_block.contains("return Err(e.into())"),
        "spawn loop must warn+continue on create error, not bail — \
         bailing skips the idle-reapable pass + status patch"
    );
}
```

## Exit criteria

- `grep -n 'Sweep Failed Jobs FIRST' rio-controller/src/reconcilers/builderpool/manifest.rs` → hit at a line number LOWER than `grep -n -- '---- Spawn ----'` hit (T1: ordering)
- `grep 'return Err(e.into())' rio-controller/src/reconcilers/builderpool/manifest.rs` → 0 hits inside the spawn `for directive` loop (T2: warn+continue)
- `grep 'manifest Job spawn failed' rio-controller/src/reconcilers/builderpool/manifest.rs` → ≥1 hit (T2: warn message present)
- `cargo nextest run -p rio-controller sweep_ordered_before_spawn_in_source spawn_loop_no_early_return_on_error` → 2 passed
- `nix develop -c cargo clippy --all-targets -- --deny warnings` green
- `/nixbuild .#ci` green (all existing `reconcile_manifest` VM tests still pass — reorder is behavior-preserving for the non-quota path)

## Tracey

References existing markers:
- `r[ctrl.pool.manifest-failed-sweep]` — T1 strengthens this marker's guarantee: the sweep runs unconditionally per tick, not "unless spawn bailed". T3's `sweep_ordered_before_spawn_in_source` is `r[verify]`. **No bump** — the spec text at [`controller.md:157`](../../docs/src/components/controller.md) says "MUST delete Failed Jobs alongside idle-surplus deletes" without specifying ordering; T1 makes the code match what the spec already implies.
- `r[ctrl.pool.manifest-reconcile]` — T2's warn+continue is a reconcile-loop-resilience change under this umbrella.

## Files

```json files
[
  {"path": "rio-controller/src/reconcilers/builderpool/manifest.rs", "action": "MODIFY", "note": "T1: move :393-449 sweep block to after :239 (before spawn :261); T2: :309 return Err → warn+continue. Net ~0L (pure reorder + 6L warn arm)"},
  {"path": "rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs", "action": "MODIFY", "note": "T3: sweep_ordered_before_spawn_in_source structural guard + spawn_loop_no_early_return_on_error"}
]
```

```
rio-controller/src/reconcilers/builderpool/
├── manifest.rs           # T1: sweep→spawn reorder; T2: warn+continue
└── tests/
    └── manifest_tests.rs # T3: structural ordering guard
```

## Dependencies

```json deps
{"deps": [511, 515], "soft_deps": [512, 513], "note": "P0511 DONE (sweep block exists). P0515 FIRST (const rename + param add are small, cleaner to reorder post-rename than merge-conflict on the const name). Soft-dep P0512: VM test for the full reconcile loop — T3 here is structural; P0512 could add an actual quota-injection subtest (kubectl create resourcequota → crash-loop → assert recovery). Soft-dep P0513: job_common extraction — T1's moved block calls select_failed_jobs which may live in job_common post-P0513; reorder still applies (call site moves, callee location is orthogonal). discovered_from=511."}
```

**Depends on:** [P0511](plan-0511-manifest-failed-job-sweep.md) — DONE; sweep block at `:393` is the subject of T1's move. [P0515](plan-0515-manifest-sweep-cap-divergence.md) — serialize FIRST: it renames `FAILED_SWEEP_PER_TICK`→`FAILED_SWEEP_MIN` at `:120` and changes the `:239` call site; T1 moves the block that READS `:239`'s output. Landing this after P0515 means T1 moves the already-fixed code; landing before means P0515 rebases across a ~60-line block move.

**Conflicts with:** `manifest.rs` not top-20. [P0513](plan-0513-job-common-extraction.md) extracts segments at `:138-142, :202-206, :243-250, :367-388` — T1's moved block is `:393-449` (no overlap with segments) but the reorder shifts line numbers for everything below `:239` by ~+60. Prefer P0515 → **this** → P0513 (P0513 recounts line refs anyway per its own Risks §).

## Risks

- **`include_str!` structural test is brittle by design** — T3 greps the source for section-comment markers. If someone renames the `---- Spawn ----` comment, the test panics on `.expect()`. That's the intended friction: the test forces a conscious decision about ordering on any refactor. Document this in the test's doc-comment.
- **Reorder changes observable event timing:** `CrashLoopDetected` now fires BEFORE spawn attempts. An operator tailing events sees the warning slightly earlier in the tick — benign, arguably better (warn before making the situation worse with another failed spawn).
- **`failed_total` computed before sweep, used after?** No — `failed_total` at `:234` is for the event gate at `:408`, both move together in T1. The post-move `jobs.items` still contains the Failed Jobs (deletes are `jobs_api.delete()` calls to the apiserver, not in-memory `Vec` mutation), so later `active_jobs.len()` etc. are unaffected — they were already filtered from `jobs.items` anyway.
