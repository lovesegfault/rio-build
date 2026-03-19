# Plan 0267: Atomic multi-output tx (scope gated on P0245 GT13)

> **PRE-IMPL CORRECTION (P0245 validation, 2026-03):** GT13-OUTCOME = **real** (verified @ [`9a659a10`](https://github.com/search?q=9a659a10&type=commits) via [`rio-store/tests/grpc/chunked.rs:225`](../../rio-store/tests/grpc/chunked.rs) `gt13_multi_output_not_atomic`). **BUT the T1a approach below is architecturally impossible.** The GT13-OUTCOME comment block at [`phase5-partition.md:119-134`](../notes/phase5-partition.md) concludes "P0267 scope: full sqlx::Transaction wrap" — that conclusion is **wrong**.
>
> **The architectural reality:** [`upload.rs:485-533`](../../rio-worker/src/upload.rs) `upload_all_outputs()` uses `buffer_unordered(MAX_PARALLEL_UPLOADS=4)`. Each output is an **independent `PutPath` RPC** — separate HTTP/2 stream, separate server-side handler invocation, separate DB connection from the pool. You **cannot** wrap independent gRPC handler invocations in one `sqlx::Transaction` — the transaction is connection-bound and each handler pulls its own connection. The test at [`chunked.rs:247`](../../rio-store/tests/grpc/chunked.rs) confirms: "separate RPC, separate transaction, already committed."
>
> **Actual scope (SUPERSEDES T1a below):**
> 1. **NEW proto RPC** in [`store.proto`](../../rio-proto/proto/store.proto): `rpc PutPathBatch(stream PutPathBatchRequest) returns (PutPathBatchResponse)` — single stream carries ALL outputs of one derivation, server-side handler opens ONE `sqlx::Transaction`, processes all outputs, commits once. `PutPathBatchRequest` is a tagged union: `{output_idx, PutPathRequest-body}` so the server knows which output each chunk belongs to.
> 2. **MODIFY** [`rio-worker/src/upload.rs`](../../rio-worker/src/upload.rs) — `upload_all_outputs()` branches: single-output → existing `PutPath` (unchanged); multi-output → new `PutPathBatch`. Parallelism moves server-side (the batch handler can still write blobs concurrently; the tx just wraps the final DB commit loop).
> 3. **NEW** `rio-store/src/grpc/put_path_batch.rs` — batch handler. Reuses `put_path.rs` chunk-accumulation logic per-output; wraps the `N × complete_manifest_chunked` calls in one tx.
> 4. **MODIFY** [`chunked.rs:223`](../../rio-store/tests/grpc/chunked.rs) — `TODO(P0267): invert assertion` is **insufficient**. The test as-written proves the gap using two independent `put_path()` calls — it will CONTINUE to pass (the gap persists for independent RPCs by design). New test: `gt13_batch_rpc_atomic` using the new batch RPC, asserting zero rows committed on mid-batch failure.
>
> **File set grows from 1 to 5.** Complexity LOW → MEDIUM. [`types.proto`](../../rio-proto/proto/types.proto) is count=33 — coordinate dispatch carefully. [`upload.rs`](../../rio-worker/src/upload.rs) has `// r[impl worker.upload.multi-output]` at line 6; this change implements it more correctly.

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
