# Plan 507: manifest truncation starvation livelock — per-bucket floor

[P0503](plan-0503-manifest-diff-reconciler.md) shipped `reconcile_manifest`
with a ceiling-truncation loop at
[`manifest.rs:240-272`](../../rio-controller/src/reconcilers/builderpool/manifest.rs)
that iterates `compute_spawn_plan` output in `BTreeMap` key order (small
buckets first) and stops when `budget` hits zero. The comment at
[`:222-225`](../../rio-controller/src/reconcilers/builderpool/manifest.rs)
says "Next tick re-diffs with the updated supply" — but under sustained
small-bucket demand that refills faster than `headroom` per tick, the
truncation point never advances. Large buckets and cold-start (last in plan
order, `bucket: None` pushed after the `BTreeMap` loop at
[`:437`](../../rio-controller/src/reconcilers/builderpool/manifest.rs))
never spawn.

Cold-start is worse than large-bucket: derivations that never get a pod
never build, never get a `build_history` row
([`admin_types.proto:249`](../../rio-proto/proto/admin_types.proto)), so
they never graduate out of cold-start. No escape hatch. A workload mix of
100 tiny + 3 huge + 20 cold-start with `max=10` and tiny-demand refilling
≥10/tick starves the other 23 derivations forever.

The truncation loop is pure (budget counter + Vec iteration) but embedded
in IO-ful `reconcile_manifest`, so it has zero unit-test coverage. P0503's
T4 tested `compute_spawn_plan` (deficit arithmetic) but not the ceiling
truncation on top of it.

**Policy decision:** per-bucket-floor. Every bucket (+ cold-start) with
`deficit > 0` gets at least 1 spawn before any bucket gets 2. Proportional
beyond the floor. This is the simplest policy that guarantees progress:
after `N` ticks where `N = distinct_buckets + 1`, every class has at least
one pod. Demand-proportional without a floor regresses to the current bug
under the same workload (tiny-heavy demand still eats the budget); large-
first inverts the starvation; cold-start-reserve is a special case of the
per-bucket-floor.

## Entry criteria

- [P0503](plan-0503-manifest-diff-reconciler.md) merged (provides
  `compute_spawn_plan`, `SpawnDirective`, the truncation loop this plan
  replaces)

## Tasks

### T1 — `refactor(controller):` extract truncate_plan — pure fn, testable

Extract the truncation loop at
[`manifest.rs:240-272`](../../rio-controller/src/reconcilers/builderpool/manifest.rs)
into a pure helper alongside `compute_spawn_plan`:

```rust
/// Truncate a spawn plan at `budget`, guaranteeing per-bucket-floor:
/// every directive with `count > 0` gets at least 1 before any gets 2.
///
/// Two passes. Pass 1 (floor): iterate `plan`, allocate 1 to each
/// directive until budget exhausted. Pass 2 (proportional): distribute
/// remaining budget proportional to `directive.count` (largest-remainder).
///
/// `compute_spawn_plan` emits BTreeMap-ordered (small-first) +
/// cold-start-last. Pass 1 preserves that order for the floor slot, so
/// under extreme budget (budget < num_buckets) small buckets still win —
/// but every bucket that fits in the budget gets its one. Under sustained
/// load, N ticks where N = plan.len() guarantees full coverage.
///
/// Returns directives with `count > 0` only (same filter as
/// `compute_spawn_plan`).
pub(super) fn truncate_plan(plan: &[SpawnDirective], budget: usize) -> Vec<SpawnDirective> {
    if budget == 0 || plan.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<SpawnDirective> = plan.iter().map(|d| SpawnDirective {
        bucket: d.bucket,
        count: 0,
    }).collect();
    let mut remaining = budget;

    // Pass 1: floor — one each, in plan order.
    for (i, d) in plan.iter().enumerate() {
        if remaining == 0 { break; }
        if d.count > 0 {
            out[i].count = 1;
            remaining -= 1;
        }
    }

    // Pass 2: proportional on what's left. Largest-remainder: integer
    // proportion first, then hand out the residue one at a time to the
    // directives with the largest fractional part. Stable: ties break
    // by plan-order (small-first).
    if remaining > 0 {
        let total_want: usize = plan.iter().map(|d| d.count.saturating_sub(1)).sum();
        if total_want > 0 {
            let mut residues: Vec<(usize, u64)> = Vec::with_capacity(plan.len());
            let mut distributed = 0usize;
            for (i, d) in plan.iter().enumerate() {
                let want = d.count.saturating_sub(1);
                // Scaled by u64 to avoid f64 imprecision on the remainder.
                let share_num = (want as u64) * (remaining as u64);
                let share = (share_num / total_want as u64) as usize;
                let residue = share_num % total_want as u64;
                out[i].count += share;
                distributed += share;
                residues.push((i, residue));
            }
            // Hand out the residue (remaining - distributed), largest-
            // remainder first. Stable sort preserves plan-order on ties.
            residues.sort_by(|a, b| b.1.cmp(&a.1));
            for (i, _) in residues.into_iter().take(remaining - distributed) {
                out[i].count += 1;
            }
        }
        // total_want == 0 means every directive wanted exactly 1 and
        // pass 1 covered them all. Residue budget is unused (no more
        // demand). Correct — we don't over-spawn.
    }

    out.into_iter().filter(|d| d.count > 0).collect()
}
```

Call site at `:240` simplifies to:

```rust
let truncated = truncate_plan(&plan, budget);
for directive in &truncated {
    for _ in 0..directive.count {
        let job = build_manifest_job(/* ... */)?;
        // ... (same create/409/error handling)
    }
}
```

The inner `if budget == 0 { break; }` at `:242-244` and `budget -= 1` at
`:245` are gone — `truncate_plan` returns exact counts. The K8s `create`
loop is now purely mechanical (no policy).

### T2 — `fix(controller):` update spawn-order comment — small-first is floor-order

The comment at
[`manifest.rs:222-225`](../../rio-controller/src/reconcilers/builderpool/manifest.rs)
("Next tick re-diffs with the updated supply") and at
[`:422-424`](../../rio-controller/src/reconcilers/builderpool/manifest.rs)
("if the ceiling truncates the plan mid-spawn, we've spawned the cheap
pods. Next tick retries the expensive ones") describe the OLD truncation
policy. After T1:

```rust
// Ceiling: same `spec.replicas.max` cap as ephemeral. A manifest
// with 200 distinct derivations shouldn't spawn 200 pods if the
// operator said max=10. `truncate_plan` applies per-bucket-floor:
// every bucket with demand gets ≥1 before any gets 2 — prevents
// the small-first starvation livelock where large buckets and
// cold-start never spawn under sustained tiny-heavy load. See
// r[ctrl.pool.manifest-fairness].
```

And at `:422-424`:

```rust
// BTreeMap iteration: deterministic (by key). Smaller buckets
// first — under extreme budget pressure (budget < num_buckets),
// the floor pass covers small buckets first. But every bucket
// that fits gets its floor slot; no bucket starves. See
// `truncate_plan`.
```

### T3 — `test(controller):` truncate_plan — starvation regression + floor coverage

Add to
[`tests/manifest_tests.rs`](../../rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs)
after the `compute_spawn_plan` block:

```rust
// r[verify ctrl.pool.manifest-fairness]
#[test]
fn truncate_plan_per_bucket_floor_under_sustained_tiny_load() {
    // The P0503 regression: 100-tiny + 3-huge + 20-cold, budget=10.
    // OLD truncation: 10× tiny, 0× huge, 0× cold. Cold never escapes.
    // NEW: floor pass gives 1 each (3 directives → 3 budget), then
    // proportional on 7 remaining (tiny gets most; huge + cold get
    // their proportion of 2+19 / 121).
    let plan = vec![
        SpawnDirective { bucket: Some((4 * GI, 2000)), count: 100 },  // tiny
        SpawnDirective { bucket: Some((48 * GI, 16000)), count: 3 },  // huge
        SpawnDirective { bucket: None, count: 20 },                   // cold
    ];
    let out = truncate_plan(&plan, 10);

    assert_eq!(out.len(), 3, "all three classes spawn at least once");
    // Floor: every class got ≥1.
    for d in &out {
        assert!(d.count >= 1, "bucket {:?} starved (count={})", d.bucket, d.count);
    }
    // Budget conserved.
    assert_eq!(out.iter().map(|d| d.count).sum::<usize>(), 10);
    // Cold-start specifically (the escape-hatch bug): it's in the output.
    assert!(out.iter().any(|d| d.bucket.is_none() && d.count >= 1),
        "cold-start must spawn — otherwise derivations never graduate");
}

#[test]
fn truncate_plan_budget_exceeds_demand_no_overspawn() {
    // Total demand 5, budget 20 → spawn exactly 5 (don't over-allocate).
    let plan = vec![
        SpawnDirective { bucket: Some((8 * GI, 4000)), count: 3 },
        SpawnDirective { bucket: None, count: 2 },
    ];
    let out = truncate_plan(&plan, 20);
    assert_eq!(out.iter().map(|d| d.count).sum::<usize>(), 5);
    assert_eq!(out[0].count, 3);
    assert_eq!(out[1].count, 2);
}

#[test]
fn truncate_plan_budget_less_than_buckets_covers_prefix() {
    // 5 buckets, budget 3 → first 3 get their floor, last 2 starve
    // THIS tick. Next tick, supply has changed (those 3 are now
    // counted), so floor-pass covers the next slice. N ticks = coverage.
    let plan: Vec<_> = (1..=5).map(|i| SpawnDirective {
        bucket: Some((i as u64 * 4 * GI, 2000)), count: 10,
    }).collect();
    let out = truncate_plan(&plan, 3);
    assert_eq!(out.len(), 3);
    for d in &out { assert_eq!(d.count, 1); }
}

#[test]
fn truncate_plan_zero_budget_noop() {
    let plan = vec![SpawnDirective { bucket: None, count: 5 }];
    assert!(truncate_plan(&plan, 0).is_empty());
}
```

Export `GI` from `manifest.rs` as `pub(super)` for the test constants (or
inline `1024 * 1024 * 1024` — it's a test, either is fine).

### T4 — `docs(controller):` add r[ctrl.pool.manifest-fairness] marker

Add to
[`docs/src/components/controller.md`](../../docs/src/components/controller.md)
after `r[ctrl.pool.manifest-single-build]` at `:155`:

```markdown
r[ctrl.pool.manifest-fairness]
When the manifest ceiling (`spec.replicas.max - active`) is less than
total demand, the reconciler MUST apply per-bucket-floor truncation:
every bucket with nonzero deficit (including cold-start) gets at least
one spawn before any bucket gets a second. Prevents starvation under
sustained small-bucket-heavy load where BTreeMap small-first iteration
would never reach large buckets or cold-start. Cold-start starvation is
a livelock: derivations that never build never get a `build_history`
sample, so they never graduate out of cold-start.
```

The `r[impl]` goes on `truncate_plan` (T1). The `r[verify]` goes on
`truncate_plan_per_bucket_floor_under_sustained_tiny_load` (T3).

## Exit criteria

- `cargo nextest run -p rio-controller truncate_plan` → 4 tests pass
- `grep -c 'fn truncate_plan' rio-controller/src/reconcilers/builderpool/manifest.rs` ≥ 1
- `grep 'budget == 0.*break\|budget -= 1' rio-controller/src/reconcilers/builderpool/manifest.rs` inside `reconcile_manifest` → 0 hits (T1 removed inline truncation)
- Mutation check: change T1's pass-1 floor loop to `break` after first directive → `truncate_plan_per_bucket_floor_under_sustained_tiny_load` FAILS with "cold-start must spawn"
- `nix develop -c tracey query rule ctrl.pool.manifest-fairness` shows 1 impl + 1 verify site
- `grep 'Next tick retries the expensive ones' rio-controller/src/reconcilers/builderpool/manifest.rs` → 0 hits (T2 replaced the stale comment)
- `/nixbuild .#ci` green

## Tracey

Adds new marker to component spec:
- `r[ctrl.pool.manifest-fairness]` → [`docs/src/components/controller.md`](../../docs/src/components/controller.md) after `:155` (T4 adds it; T1 `r[impl]`; T3 `r[verify]`)

References existing markers:
- `r[ctrl.pool.manifest-reconcile]` — T1 touches the reconciler loop under this marker; no annotation change (the reconcile contract itself doesn't change, only the truncation policy inside it)

## Spec additions

```markdown
r[ctrl.pool.manifest-fairness]
When the manifest ceiling (`spec.replicas.max - active`) is less than
total demand, the reconciler MUST apply per-bucket-floor truncation:
every bucket with nonzero deficit (including cold-start) gets at least
one spawn before any bucket gets a second. Prevents starvation under
sustained small-bucket-heavy load where BTreeMap small-first iteration
would never reach large buckets or cold-start. Cold-start starvation is
a livelock: derivations that never build never get a `build_history`
sample, so they never graduate out of cold-start.
```

## Files

```json files
[
  {"path": "rio-controller/src/reconcilers/builderpool/manifest.rs", "action": "MODIFY", "note": "T1: extract truncate_plan pure fn at :240-272 + r[impl ctrl.pool.manifest-fairness]; T2: update comments at :222-225 + :422-424"},
  {"path": "rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs", "action": "MODIFY", "note": "T3: 4× truncate_plan tests + r[verify ctrl.pool.manifest-fairness]"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "T4: add r[ctrl.pool.manifest-fairness] marker after :155"}
]
```

```
rio-controller/src/reconcilers/builderpool/
├── manifest.rs              # T1+T2: truncate_plan + comments
└── tests/manifest_tests.rs  # T3: regression + floor tests
docs/src/components/
└── controller.md            # T4: new fairness marker
```

## Dependencies

```json deps
{"deps": [503], "soft_deps": [505], "note": "Serializes with P0505 (both touch manifest.rs). P0505 is scale-DOWN grace (BTreeMap<Bucket, Instant> surplus-timestamp); this is scale-UP fairness. Different code sections (:240-272 vs surplus-tracking), but same file. Ship this FIRST — starvation is a prod bug today; scale-down grace is a future refinement. P0505's rebase on top of this is trivial (adjacent additions, no overlap)."}
```

**Depends on:** [P0503](plan-0503-manifest-diff-reconciler.md) — provides
`compute_spawn_plan`, `SpawnDirective`, the reconcile loop this plan fixes.

**Conflicts with:** [P0505](plan-0505-manifest-scaledown-grace.md) touches
the same file (`manifest.rs`). P0505's surplus-timestamp map is a
different section (additive, after the spawn loop), but both modify
`reconcile_manifest`. Serialize: this plan first (higher priority — prod
bug), P0505 rebases. `onibus collisions top` doesn't show `manifest.rs`
in top-30 (new file, low collision count).
