# Plan 0228: completion.rs — build_samples write + class_drift_total sibling counter

phase4c.md:14,19 + **GT1 correction** — two additions to the completion success path. (a) `insert_build_sample` call next to the EMA update, feeding P0229's rebalancer. (b) **SIBLING counter `rio_scheduler_class_drift_total`** — NOT a rename of the existing `rio_scheduler_misclassifications_total`.

**GT1 is load-bearing here.** The existing counter at [`completion.rs:336`](../../rio-scheduler/src/actor/completion.rs) (registered at [`lib.rs:147`](../../rio-scheduler/src/lib.rs)) fires when `actual_duration > 2× cutoff` — a **penalty trigger** that also overwrites the EMA. The phase doc wants a **drift measurement**: `classify(actual) != assigned_class`. These are different signals:

| Counter | Fires when | Semantics |
|---|---|---|
| `misclassifications_total` (existing) | actual > 2× cutoff | Penalty trigger — "this was so wrong we overwrite the EMA" |
| `class_drift_total` (NEW) | classify(actual) ≠ assigned | Cutoff drift — "if we re-classified post-hoc, we'd pick a different class" |

A build can trigger drift without triggering penalty (e.g., assigned `small` cutoff=60s, actual=65s, next cutoff=120s → drift fires, penalty doesn't). They're orthogonal signals for different alerts.

**completion.rs anchor moved by 4b.** [P0206](plan-0206-path-tenants-migration-upsert.md) adds `upsert_path_tenants` call in the same handler; [P0219](plan-0219-per-worker-failure-budget.md) adds per-worker failure tracking. The `:298` EMA line number is STALE. At dispatch: `git log -p --since='1 month' -- rio-scheduler/src/actor/completion.rs` to find the current EMA call site.

## Entry criteria

- [P0227](plan-0227-migration-013-build-samples-retention.md) merged (`build_samples` table + `insert_build_sample` fn exist + `.sqlx/` regenerated)
- [P0206](plan-0206-path-tenants-migration-upsert.md) merged (completion.rs serialized — `upsert_path_tenants` call in place)
- [P0219](plan-0219-per-worker-failure-budget.md) merged (completion.rs serialized — per-worker failure tracking in place)

## Tasks

### T1 — `feat(scheduler):` insert_build_sample call in completion success path

MODIFY [`rio-scheduler/src/actor/completion.rs`](../../rio-scheduler/src/actor/completion.rs) — near the EMA update (grep `ema_duration` or `update_build_history`):

```rust
// Raw sample for the CutoffRebalancer. Best-effort: warn, never
// fail completion on sample-write error.
if let Err(e) = self.db.insert_build_sample(
    &pname,
    &system,
    duration_secs,
    peak_memory_bytes as i64,
).await {
    warn!(?e, %pname, %system, "insert_build_sample failed");
}
```

~15 lines including the warn. Goes in the success branch, adjacent to the EMA call.

### T2 — `feat(scheduler):` class_drift_total sibling counter

Register the new counter in [`rio-scheduler/src/lib.rs`](../../rio-scheduler/src/lib.rs) near `:147` (where `misclassifications_total` is registered):

```rust
pub static CLASS_DRIFT_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "rio_scheduler_class_drift_total",
        "Count of completed builds where classify(actual_duration, actual_mem) \
         differs from the assigned class. Cutoff-drift signal; distinct from \
         misclassifications_total (penalty trigger, actual > 2× cutoff).",
        &["assigned_class", "actual_class"]
    ).unwrap()
});
```

Increment in completion.rs success path, near the existing misclassification block (around `:336`):

```rust
// Cutoff-drift counter: would we classify differently post-hoc?
// Distinct from the misclassification/penalty check above.
let actual_class = classify(duration_secs, peak_memory_bytes, &self.size_classes);
if actual_class != assigned_class {
    CLASS_DRIFT_TOTAL
        .with_label_values(&[&assigned_class, &actual_class])
        .inc();
}
```

`classify()` takes `&[SizeClassConfig]` — at this point still the static `&self.size_classes` slice (RwLock wires in P0230). Drift is measured against config-at-dispatch, which is correct: "given the cutoffs we dispatched with, would we have picked a different class?"

### T3 — `docs(observability):` clarify both counters

MODIFY [`docs/src/observability.md`](../../docs/src/observability.md) — add/update the scheduler metrics section to document BOTH counters explicitly:

```markdown
| `rio_scheduler_misclassifications_total` | counter | Fires when `actual_duration > 2× assigned_cutoff`. Penalty trigger — also overwrites the EMA (`r[sched.classify.penalty-overwrite]`). |
| `rio_scheduler_class_drift_total{assigned_class,actual_class}` | counter | Fires when `classify(actual) ≠ assigned_class`. Cutoff-drift signal — decoupled from penalty logic. A build can trigger drift without penalty (actual barely over cutoff, under 2×). |
```

### T4 — `test(scheduler):` integration test for sample write

Extend an existing completion integration test (or add one) to verify: submit build → complete → `SELECT COUNT(*) FROM build_samples` = 1, and the row's `(pname, system, duration_secs)` matches.

## Exit criteria

- `/nbr .#ci` green
- Integration test: submit + complete one build → exactly 1 row in `build_samples` with correct `(pname, system)`
- `rio_scheduler_class_drift_total` registered (grep `lib.rs` for the name)
- `observability.md` documents BOTH counters with the semantic distinction

## Tracey

No markers — the sample write feeds P0229's `r[sched.rebalancer.sita-e]`; the drift counter is observability, not normative spec.

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "T1+T2: insert_build_sample call + class_drift_total increment near EMA update (line number STALE — git log -p at dispatch)"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "T1: if insert_build_sample needs a helper variant (likely no change — P0227 adds the fn)"},
  {"path": "rio-scheduler/src/lib.rs", "action": "MODIFY", "note": "T2: register CLASS_DRIFT_TOTAL near :147"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T3: document both counters + semantic distinction"}
]
```

```
rio-scheduler/src/
├── actor/completion.rs    # T1+T2: sample write + drift counter (anchor MOVED by 4b)
├── db.rs                  # T1: likely no change
└── lib.rs                 # T2: register counter near :147
docs/src/observability.md  # T3: both counters documented
```

## Dependencies

```json deps
{"deps": [227, 206, 219], "soft_deps": [], "note": "SITA-E spine hop 2. deps:[P0227(table), P0206+P0219(completion.rs serial)]. completion.rs count=20 — ONLY 4c touch; hard-serialized after both 4b touchers."}
```

**Depends on:** [P0227](plan-0227-migration-013-build-samples-retention.md) — `build_samples` table + `insert_build_sample` fn + `.sqlx/` regen. [P0206](plan-0206-path-tenants-migration-upsert.md) + [P0219](plan-0219-per-worker-failure-budget.md) — **completion.rs serialization** (both 4b plans modify the same handler).
**Conflicts with:** `completion.rs` count=20 — this is the ONLY 4c touch, hard-serialized after P0206+P0219 via deps. `lib.rs` is tail-append (register counter next to existing registrations).

**Hidden check at dispatch:** `git log -p --since='1 month' -- rio-scheduler/src/actor/completion.rs` — find where P0206/P0219 landed the EMA call site. The `:298` anchor from phase doc is STALE.
