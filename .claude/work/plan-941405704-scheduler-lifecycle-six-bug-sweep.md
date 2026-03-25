# Plan 941405704: scheduler derivation-lifecycle — 6-bug sweep

Six correctness bugs in the scheduler's derivation lifecycle, all touching
[`rio-scheduler/src/actor/completion.rs`](../../rio-scheduler/src/actor/completion.rs).
Grouped into one plan because file-level serialization forces sequential
implementation anyway — splitting into six plans would just add merge
overhead. Sequence: T1 → T2 → T3 → T4 → T5 → T6 (file-collision order).

Origin: bughunter sweep, reports `bug_043`, `merged_bug_024`,
`merged_bug_052`, `merged_bug_033`, `merged_bug_039`, `bug_022` at
`/tmp/bughunter/prompt-6701aef0_2/reports/`.

**Bug summaries:**

- **bug_043** — `handle_completion` at
  [`completion.rs:364-375`](../../rio-scheduler/src/actor/completion.rs)
  has no `worker_id == assigned_worker` guard. A stale completion report
  from a previously-assigned worker (arrived after reassignment) corrupts
  state for the now-running instance.
- **m024** — `reassign_derivations` at
  [`worker.rs:324`](../../rio-scheduler/src/actor/worker.rs) increments
  `retry_count` for `Assigned`-only derivations (never started running);
  starvation guard at
  [`completion.rs:1224`](../../rio-scheduler/src/actor/completion.rs)
  compares `failed_workers.len()` instead of intersection with live
  workers.
- **m052** — `cascade_dependency_failure` at
  [`completion.rs:1510`](../../rio-scheduler/src/actor/completion.rs) and
  CA-cutoff at [`:703`](../../rio-scheduler/src/actor/completion.rs) only
  notify the trigger's `interested_builds` — merged builds sharing the
  cascaded/skipped derivations hang `Active` forever.
- **m033** — `cancel_build_derivations` at
  [`build.rs:38`](../../rio-scheduler/src/actor/build.rs) skips
  `Queued`/`Ready`/`Created`; `handle_derivation_failure` at
  [`completion.rs:1567`](../../rio-scheduler/src/actor/completion.rs)
  with `keep_going=false` doesn't cancel remaining derivations.
- **m039** — `upsert_path_tenants` is called only at
  [`completion.rs:1046`](../../rio-scheduler/src/actor/completion.rs);
  missing at CA-cutoff (`:718`), merge.rs (`:149`, `:285`), recovery.rs
  (`:621`). Paths completed via these routes get no tenant attribution →
  GC under-retention.
- **bug_022** — `find_roots` at
  [`dag/mod.rs:813`](../../rio-scheduler/src/dag/mod.rs) uses global
  `parents` map, not build-scoped — a derivation that's a root for build
  X but a non-root for build Y is treated as non-root for both.

## Tasks

### T1 — `fix(scheduler):` handle_completion guards worker_id == assigned_worker (bug_043)

After the existing state-load at
[`completion.rs:375`](../../rio-scheduler/src/actor/completion.rs), add:

```rust
// r[impl sched.completion.idempotent]
// Stale-report guard: if this completion is from a worker that no
// longer owns the derivation (reassigned after disconnect/timeout),
// drop it. The current assigned_worker's report is authoritative.
if let Some(assigned) = deriv.assigned_worker
    && assigned != worker_id
{
    tracing::debug!(
        drv_hash = %drv_hash,
        stale_worker = %worker_id,
        current_worker = %assigned,
        "dropping stale completion report"
    );
    return Ok(());
}
```

This extends the existing idempotency (`completed → completed` no-op)
to cover stale-worker reports.

### T2 — `fix(scheduler):` retry_count only for was-running; starvation = all-live-failed (m024)

At [`worker.rs:315-331`](../../rio-scheduler/src/actor/worker.rs),
capture `was_running` before `reset_to_ready`:

```rust
let was_running = matches!(deriv.status, DerivationStatus::Running);
// ... reset_to_ready ...
if was_running {
    deriv.retry_count += 1;
}
```

An `Assigned`-but-never-`Running` derivation didn't consume a retry
budget — the worker disconnected before starting it.

At [`completion.rs:1224-1230`](../../rio-scheduler/src/actor/completion.rs),
replace `failed_workers.len() >= worker_count` with:

```rust
// r[impl sched.retry.per-worker-budget]
// Starvation = every LIVE worker has failed this derivation.
// failed_workers may contain dead workers; intersect with live set.
let all_live_failed = self.workers.keys()
    .all(|w| deriv.failed_workers.contains(w));
if all_live_failed && !self.workers.is_empty() {
```

### T3 — `fix(scheduler):` cascade + CA-cutoff notify union of interested_builds (m052)

`cascade_dependency_failure` at
[`completion.rs:1510`](../../rio-scheduler/src/actor/completion.rs)
currently returns `()`. Change to return `HashSet<DrvHash>` — the set of
derivations it transitioned. Callers at
[`:1154`](../../rio-scheduler/src/actor/completion.rs),
[`:1414`](../../rio-scheduler/src/actor/completion.rs),
[`:1476`](../../rio-scheduler/src/actor/completion.rs) collect the union
of `interested_builds` over (cascaded ∪ {trigger}) and notify that union,
not just the trigger's set.

CA-cutoff at
[`completion.rs:703`](../../rio-scheduler/src/actor/completion.rs):
collect `interested_builds` from each skipped derivation into a
`HashSet<BuildId>`, union into the `interested_builds` used at
[`:1097`](../../rio-scheduler/src/actor/completion.rs) for the
completion notification. Annotate with
`// r[impl sched.ca.cutoff-propagate]`.

### T4 — `fix(scheduler):` cancel Queued/Ready/Created on build cancel + keep_going=false (m033)

At [`build.rs:38`](../../rio-scheduler/src/actor/build.rs)
(`cancel_build_derivations`): before `remove_build_interest`, transition
sole-interest `Queued`/`Ready`/`Created` derivations to
`DependencyFailed`. "Sole-interest" = `interested_builds == {this_build}`.

At [`completion.rs:1567`](../../rio-scheduler/src/actor/completion.rs)
(`handle_derivation_failure`, `keep_going=false` branch): call
`cancel_build_derivations(build_id)` before
`transition_build_to_failed(build_id)`. Annotate with
`// r[impl sched.build.keep-going]`.

### T5 — `fix(scheduler):` upsert_path_tenants at all completion paths (m039)

The call at
[`completion.rs:1046`](../../rio-scheduler/src/actor/completion.rs) is
the only one. Add calls at:

- [`completion.rs:718-760`](../../rio-scheduler/src/actor/completion.rs)
  (CA-cutoff skipped nodes — they have output paths from the cached
  realization)
- [`merge.rs:149-189`](../../rio-scheduler/src/actor/merge.rs)
  (cache hits during merge — path already in store, new tenant needs
  attribution)
- [`merge.rs:285-311`](../../rio-scheduler/src/actor/merge.rs)
  (pre-existing `Completed` derivation merged from another build)
- [`recovery.rs:621-687`](../../rio-scheduler/src/actor/recovery.rs)
  (orphan-completion during recovery — derivation was `Running` at
  crash, completed during downtime)

Each call follows the pattern at `:1046`: collect `output_paths` +
`tenant_ids` from `interested_builds`, call
`.upsert_path_tenants(&output_paths, &tenant_ids)`. Annotate each with
`// r[impl sched.gc.path-tenants-upsert]`.

### T6 — `fix(scheduler):` find_roots scopes to build-interested parents (bug_022)

At [`dag/mod.rs:813`](../../rio-scheduler/src/dag/mod.rs), `find_roots`
filters parents by build interest:

```rust
// r[impl sched.dag.build-scoped-roots]
// A derivation is a root FOR THIS BUILD if no parent interested in
// this build depends on it. Global parents may include derivations
// from other merged builds — those don't make it a non-root here.
let has_interested_parent = self.parents(drv_hash)
    .iter()
    .any(|p| self.nodes[p].interested_builds.contains(&build_id));
if !has_interested_parent {
    roots.push(*drv_hash);
}
```

### T7 — `test(scheduler):` regression test per bug

One regression test per bug in the scheduler test module
(`rio-scheduler/src/actor/` test submodules or `tests/`):

- `test_stale_completion_dropped` — bug_043, `// r[verify sched.completion.idempotent]`
- `test_assigned_only_no_retry_bump` + `test_starvation_intersects_live` — m024, `// r[verify sched.retry.per-worker-budget]`
- `test_cascade_notifies_merged_builds` + `test_ca_cutoff_notifies_merged` — m052, `// r[verify sched.ca.cutoff-propagate]`
- `test_cancel_transitions_queued` + `test_keep_going_false_cancels` — m033, `// r[verify sched.build.keep-going]`
- `test_upsert_at_ca_cutoff` + `test_upsert_at_merge_cache_hit` + `test_upsert_at_recovery_orphan` — m039, `// r[verify sched.gc.path-tenants-upsert]`
- `test_find_roots_build_scoped` — bug_022, `// r[verify sched.dag.build-scoped-roots]`

## Exit criteria

- `/nixbuild .#ci` green
- `grep -c 'upsert_path_tenants' rio-scheduler/src/actor/` ≥ 5
- VM test `vm-scheduling-disrupt-standalone` green (covered by `.#ci`; exercises reassignment/disconnect paths relevant to T1/T2)
- `cargo nextest run -p rio-scheduler` shows ≥ 11 new tests passing
- `tracey query rule sched.dag.build-scoped-roots` shows impl + verify
- Each of the 6 bug-fix sites carries the listed `r[impl ...]` annotation

## Tracey

References existing markers:
- `r[sched.completion.idempotent]` — T1 extends (stale-worker guard), T7 verifies
- `r[sched.retry.per-worker-budget]` — T2 implements (was_running gate + live intersection), T7 verifies
- `r[sched.ca.cutoff-propagate]` — T3 implements (union notification), T7 verifies
- `r[sched.build.keep-going]` — T4 implements (cancel on keep_going=false), T7 verifies
- `r[sched.gc.path-tenants-upsert]` — T5 implements (4 new call sites), T7 verifies

Adds new marker to component specs:
- `r[sched.dag.build-scoped-roots]` → `docs/src/components/scheduler.md` (see ## Spec additions) — T6 implements, T7 verifies

## Spec additions

Add to `docs/src/components/scheduler.md` after the `r[sched.merge.dedup]`
block (around line 166):

```markdown
r[sched.dag.build-scoped-roots]
`find_roots(build_id)` MUST treat a derivation as a root for a given
build if no parent *interested in that build* depends on it. The global
`parents` map includes parents from all merged builds; a derivation
that is a root for build X may have a parent from build Y. Using the
unscoped parent set incorrectly marks X's root as a non-root, stalling
X's dispatch. The filter is
`parents(d).any(|p| p.interested_builds.contains(build_id))`.
```

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "T1: worker_id guard; T2: starvation intersection; T3: cascade returns HashSet, CA-cutoff union; T4: keep_going=false cancel; T5: upsert at :718; T7: tests"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "T2: was_running gate on retry_count"},
  {"path": "rio-scheduler/src/actor/build.rs", "action": "MODIFY", "note": "T4: transition Queued/Ready/Created to DependencyFailed"},
  {"path": "rio-scheduler/src/actor/merge.rs", "action": "MODIFY", "note": "T5: upsert_path_tenants at :149 cache-hit + :285 pre-existing Completed"},
  {"path": "rio-scheduler/src/actor/recovery.rs", "action": "MODIFY", "note": "T5: upsert_path_tenants at :621 orphan-completion"},
  {"path": "rio-scheduler/src/dag/mod.rs", "action": "MODIFY", "note": "T6: find_roots build-scoped parent filter"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "r[sched.dag.build-scoped-roots] marker"}
]
```

```
rio-scheduler/src/
├── actor/
│   ├── completion.rs                 # T1,T2,T3,T4,T5,T7
│   ├── worker.rs                     # T2
│   ├── build.rs                      # T4
│   ├── merge.rs                      # T5
│   └── recovery.rs                   # T5
└── dag/
    └── mod.rs                        # T6
docs/src/components/
└── scheduler.md                      # new marker
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [], "note": "standalone 6-bug sweep — serialized internally by completion.rs touch"}
```

**Depends on:** none. All six bugs are in existing scheduler code with no
pending upstream changes.

**Conflicts with:** `rio-scheduler/src/actor/completion.rs` is the hot
file — all 6 tasks touch it. Internal serialization (T1→T2→T3→T4→T5→T6)
is the reason this is one plan, not six. Independent of
[P941405701](plan-941405701-nar-entry-name-validation.md),
[P941405702](plan-941405702-framed-total-nar-size-align.md),
[P941405703](plan-941405703-store-gc-drain-toctou-sweep-gaps.md)
(different crates).
