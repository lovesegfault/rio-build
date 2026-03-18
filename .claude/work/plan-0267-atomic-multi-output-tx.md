# Plan 0267: Atomic multi-output tx (scope gated on P0245 GT13)

**Scope depends on [P0245](plan-0245-prologue-phase5-markers-gt-verify.md)'s GT13 verify outcome.** Check `.claude/notes/phase5-partition.md` for `<!-- GT13-OUTCOME: {real|false-alarm} -->` before starting.

Per GT13/A11: `phase5.md:28` claims "partial registration is possible on upload failure." Unverified — `rg 'atomic|transaction|BEGIN' rio-store/src/cas.rs` shows zero tx wrapper, but absence of wrapper ≠ bug (single-statement INSERT is already atomic).

**Per R10:** tx covers DB rows only. Blob-store writes are NOT rolled back — orphaned blobs are refcount-zero and GC-eligible next sweep. Document bound: ≤1 NAR-size per failure.

## Entry criteria

- [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) merged (GT13-OUTCOME recorded)
- [P0208](plan-0208-xmax-inserted-check-refactor.md) merged (4b `cas.rs` xmax refactor — file serial)

## Tasks

### T1a — `fix(store):` IF GT13-OUTCOME=real: wrap in sqlx::Transaction

MODIFY [`rio-store/src/cas.rs`](../../rio-store/src/cas.rs) or [`rio-store/src/grpc/put_path.rs`](../../rio-store/src/grpc/put_path.rs):

```rust
// r[impl store.atomic.multi-output]
// GT13-OUTCOME=real. Tx covers DB rows ONLY — blob-store writes are
// NOT rolled back (orphaned blobs are refcount-zero, GC-eligible).
// Bound: ≤1 NAR-size per failure.
let mut tx = pool.begin().await?;
for output in &drv.outputs {
    sqlx::query!("INSERT INTO paths (...) VALUES (...)", ...)
        .execute(&mut *tx).await?;
}
tx.commit().await?;
```

Fault-inject test: mid-loop failure → zero rows committed.

### T1b — `docs(store):` IF GT13-OUTCOME=false-alarm: annotate-only

Add `// r[impl store.atomic.multi-output]` comment above the existing atomic INSERT (4c P0226 GT2 pattern). No code change. Close the marker, delete the phase5.md:28 claim.

### T2 — `test(store):` fault-inject (IF real) OR annotation verify (IF false-alarm)

```rust
// r[verify store.atomic.multi-output]
#[tokio::test]
async fn multi_output_rolls_back_on_mid_loop_failure() {
    // 2-output drv. Mock blob-store put to fail on output-2.
    // Assert ZERO rows in paths table (atomic rollback).
}
```

## Exit criteria

- `/nbr .#ci` green
- `nix develop -c tracey query rule store.atomic.multi-output` shows impl + verify

## Tracey

References existing markers:
- `r[store.atomic.multi-output]` — T1 implements, T2 verifies (seeded by P0245)

## Files

```json files
[
  {"path": "rio-store/src/cas.rs", "action": "MODIFY", "note": "T1: tx wrap OR annotate-only (scope per GT13-OUTCOME)"}
]
```

```
rio-store/src/
└── cas.rs                        # T1 (scope per GT13-OUTCOME)
```

## Dependencies

```json deps
{"deps": [245, 208], "soft_deps": [], "note": "SCOPE GATED on P0245 GT13-OUTCOME. cas.rs count=10 — serial after 4b P0208 (xmax refactor). R10: tx = DB atomicity only, blob orphans GC-eligible."}
```

**Depends on:** [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) — GT13 outcome. [P0208](plan-0208-xmax-inserted-check-refactor.md) — 4b `cas.rs` refactor (file serial).
**Conflicts with:** `cas.rs` count=10 — serial after P0208.
