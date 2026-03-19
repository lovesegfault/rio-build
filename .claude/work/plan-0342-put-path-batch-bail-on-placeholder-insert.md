# Plan 342: PutPathBatch — `?` → `bail!` on placeholder insert (phase-2 leak)

Correctness finding from [P0267](plan-0267-atomic-multi-output-tx.md) post-merge review. [`put_path_batch.rs:275`](../../rio-store/src/grpc/put_path_batch.rs) uses `.map_err(...)?` instead of the handler-local `bail!` macro. The `bail!` macro at [`:82-87`](../../rio-store/src/grpc/put_path_batch.rs) calls `abort_batch(&owned_placeholders)` before returning; `?` does not. Result: when `insert_manifest_uploading` errors on output N (PG transient — connection drop, pool exhaustion, lock timeout), every placeholder inserted for outputs 0..N-1 stays in the `manifests` table with `status='uploading'`.

The next `PutPathBatch` for the same derivation hits [`:277`](../../rio-store/src/grpc/put_path_batch.rs) `!inserted` → `Status::aborted("concurrent upload in progress; retry")` — but the "concurrent uploader" is a ghost. The retry sees the same leaked placeholder, aborts again. Outputs 0..N-1 are wedged until the orphan scanner sweeps stale `'uploading'` rows (default 2h, per [`store.md:120`](../../docs/src/components/store.md)).

Every other error path in the handler uses `bail!` — [`:93`](../../rio-store/src/grpc/put_path_batch.rs) (stream error), [`:209`/`:214`/`:221`/`:227`/`:239`/`:246`](../../rio-store/src/grpc/put_path_batch.rs) (phase-2 validation), [`:262`](../../rio-store/src/grpc/put_path_batch.rs) (`check_manifest_complete` error — same PG-transient class). The `?` at `:275` is the lone exception. The sibling at [`:262`](../../rio-store/src/grpc/put_path_batch.rs) handles the same error class correctly via `Err(e) => bail!(...)`.

**Why `gt13_batch_rpc_atomic` doesn't catch this:** the existing test at [`chunked.rs:287`](../../rio-store/tests/grpc/chunked.rs) fails output-1 on **hash mismatch** (validation at [`:237-242`](../../rio-store/src/grpc/put_path_batch.rs), which correctly `bail!`s). The `:275` path requires `insert_manifest_uploading` itself to fail — which needs PG fault injection, not payload corruption. The test's "placeholders cleaned up" assertion at [`:340`](../../rio-store/tests/grpc/chunked.rs) passes because the hash-mismatch path is clean; the PG-transient path isn't exercised.

## Entry criteria

- [P0267](plan-0267-atomic-multi-output-tx.md) merged — **DONE** (`put_path_batch.rs` exists, `bail!` macro at `:82`, the bug at `:275`)

## Tasks

### T1 — `fix(store):` replace `.map_err(...)?` with `match` + `bail!` at :275

MODIFY [`rio-store/src/grpc/put_path_batch.rs`](../../rio-store/src/grpc/put_path_batch.rs) at the phase-2 `insert_manifest_uploading` call:

```rust
// Before:
let inserted = metadata::insert_manifest_uploading(
    &self.pool,
    &accum.store_path_hash,
    info.store_path.as_str(),
    &refs_str,
)
.await
.map_err(|e| metadata_status("PutPathBatch: insert_manifest_uploading", e))?;

// After:
let inserted = match metadata::insert_manifest_uploading(
    &self.pool,
    &accum.store_path_hash,
    info.store_path.as_str(),
    &refs_str,
)
.await
{
    Ok(i) => i,
    Err(e) => bail!(metadata_status(
        "PutPathBatch: insert_manifest_uploading",
        e
    )),
};
```

Mirrors the shape of [`:256-263`](../../rio-store/src/grpc/put_path_batch.rs) (`check_manifest_complete`) and [`:297-300`](../../rio-store/src/grpc/put_path_batch.rs) (`pool.begin()`) — both use `match` + `bail!` for the same PG-error class. `abort_batch` at [`:343`](../../rio-store/src/grpc/put_path_batch.rs) iterates `owned_placeholders` and calls `delete_manifest_uploading` for each — outputs 0..idx-1 get cleaned up; the failing output idx never made it to [`:284`](../../rio-store/src/grpc/put_path_batch.rs) so its hash isn't in the list (correct — no placeholder was inserted for it).

### T2 — `test(store):` placeholder cleanup on mid-loop PG error

NEW test in [`rio-store/tests/grpc/chunked.rs`](../../rio-store/tests/grpc/chunked.rs) after `gt13_batch_rpc_atomic` at [`:377`](../../rio-store/tests/grpc/chunked.rs). Reproduces the bug: pre-seed a **non-complete** placeholder for output-1's path **before** the batch runs, so `insert_manifest_uploading` returns `Ok(false)` for output-1 — the `!inserted` branch at [`:277`](../../rio-store/src/grpc/put_path_batch.rs) `bail!`s, which **does** clean up. That's not the `:275` bug.

The `:275` bug needs `insert_manifest_uploading` to **return `Err`**. That's a PG `execute()` failure. Two options:

**Option A — PG fault injection via pool teardown.** Start the batch stream (metadata+chunk+trailer for output-0 sent, server has accumulated it), then force a pool error before output-1's insert. Hard to time — the server processes the stream after `drop(tx)` closes it, not incrementally.

**Option B — schema-level poison for output-1.** Insert a row into `manifests` for output-1's `store_path_hash` with a value that makes the INSERT's `ON CONFLICT` handler error. `insert_manifest_uploading` at [`metadata/inline.rs:35`](../../rio-store/src/metadata/inline.rs) uses `INSERT ... ON CONFLICT DO NOTHING` — the `Ok(false)` return on conflict is the normal path, not an error. A PG-level error from INSERT requires something like a CHECK constraint violation, or a FOREIGN KEY violation on `references` (the refs column references `manifests.store_path` — but it's not FK'd). Fragile.

**Option C — unit-test `abort_batch` coverage + `grep '?'` assertion.** Two parts:

1. **Static assertion — every error return in `put_path_batch_impl` goes through `bail!`.** Add a test that asserts the source file contains no `?` on a line that's inside `put_path_batch_impl`'s body. Regex-fragile but catches regression of exactly this class:
   ```rust
   /// The bail! macro at :82-87 is load-bearing: it calls abort_batch()
   /// before returning. A `?` bypasses it — owned_placeholders leak,
   /// next PutPathBatch for those paths gets Status::aborted until the
   /// 2h orphan sweep. P0342 fixed the lone `?` at :275; this test
   /// catches any future reintroduction.
   ///
   /// Brittle-by-design: false-positive on a `?` inside a closure or
   /// helper is preferable to the silent 2h wedge.
   #[test]
   fn put_path_batch_impl_no_question_mark_bypass() {
       let src = include_str!("../../src/grpc/put_path_batch.rs");
       // Slice the impl body: between `fn put_path_batch_impl(` and
       // `async fn abort_batch(`. Every `?` in that slice is a suspect.
       let start = src.find("fn put_path_batch_impl(").expect("impl fn present");
       let end = src[start..].find("async fn abort_batch(").expect("sibling fn present") + start;
       let body = &src[start..end];
       // `?` on a tonic Status result = bypass. Allow `?` on Result<T, Status>
       // only inside the pre-placeholder prelude (before line 70ish).
       // Simpler guard: count `?` tokens after the first `owned_placeholders.push`
       // — by then at least one placeholder exists and bail! is mandatory.
       let push_pos = body.find("owned_placeholders.push")
           .expect("placeholder push present");
       // Actually: the bug was at :275 which is BEFORE the push at :284
       // in loop-iteration order but AFTER a prior iteration's push.
       // Simpler still: NO ? anywhere in the phase-2/phase-3 loops.
       let phase2_start = body.find("--- Phase 2:").expect("phase-2 marker");
       let tail = &body[phase2_start..];
       let q_count = tail.matches("?;").count() + tail.matches("?\n").count();
       assert_eq!(
           q_count, 0,
           "found `?` inside put_path_batch_impl phase-2/3 body — \
            every error return after placeholders may be inserted MUST use \
            bail! (which calls abort_batch). A bare ? leaks placeholders \
            until the 2h orphan sweep. See P0342."
       );
   }
   ```
   **Fragile but cheap.** If a legitimate `?` is added inside a closure or pre-placeholder helper, the test will false-positive and needs a `#[allow]` or a refined slice.

2. **Behavioral test via the `!inserted` path (proves abort_batch runs).** Not the `:275` PG-error path, but proves `bail!` → `abort_batch` cleanup works for the SHAPE of error — pre-seed a conflicting `'uploading'` placeholder for output-1, send a 2-output batch, assert `Status::aborted` AND zero rows after (output-0's placeholder was cleaned up). This covers the `:280` `bail!` which shares `abort_batch` with the fixed `:275`:

   ```rust
   /// Phase-2 loop iteration: output-0's placeholder is inserted, then
   /// output-1's insert hits a conflict (concurrent uploader owns it).
   /// The bail! at :280 must abort_batch() → delete output-0's placeholder
   /// too. Without cleanup, output-0 is wedged: next batch hits !inserted
   /// on output-0's stale placeholder → aborts forever (until 2h sweep).
   ///
   /// NOTE: this tests the :280 bail! path, which shares abort_batch with
   /// the :275 fix. The :275 path (insert_manifest_uploading returning Err,
   /// not Ok(false)) requires PG fault injection — covered statically by
   /// put_path_batch_impl_no_question_mark_bypass instead.
   // r[verify store.atomic.multi-output]
   #[tokio::test]
   async fn gt13_batch_placeholder_cleanup_on_midloop_abort() -> TestResult {
       let s = StoreSession::new().await?;

       let out0_path = test_store_path("cleanup-out0");
       let (out0_nar, _) = make_nar(b"cleanup zero");
       let out0_info = make_path_info_for_nar(&out0_path, &out0_nar);

       let out1_path = test_store_path("cleanup-out1");
       let (out1_nar, _) = make_nar(b"cleanup one");
       let out1_info = make_path_info_for_nar(&out1_path, &out1_nar);

       // Pre-seed: an 'uploading' placeholder for output-1 (simulates
       // concurrent uploader owning the slot). insert_manifest_uploading
       // for out1 will return Ok(false) → :277 !inserted → :280 bail!.
       let out1_hash: Vec<u8> = sha2::Sha256::digest(out1_path.as_bytes()).to_vec();
       rio_store::metadata::insert_manifest_uploading(
           &s.db.pool, &out1_hash, &out1_path, &[],
       ).await?;

       // Send the batch. Output-0 metadata+chunk+trailer, then output-1.
       // Serial per handler's drain (phase-1) → validate (phase-2) → commit
       // (phase-3) — so output-0's placeholder IS inserted before output-1's
       // insert fails. (BTreeMap iteration: idx 0 first, then idx 1.)
       let (tx, rx) = mpsc::channel(16);
       send_batch_output(&tx, 0, out0_info.clone().into(), out0_nar.clone()).await;
       send_batch_output(&tx, 1, out1_info.clone().into(), out1_nar.clone()).await;
       drop(tx);

       let mut client = s.client.clone();
       let r = client.put_path_batch(ReceiverStream::new(rx)).await;
       let status = r.expect_err("batch must fail — output-1 slot owned by concurrent uploader");
       assert_eq!(status.code(), tonic::Code::Aborted);
       assert!(status.message().contains("concurrent upload in progress"));

       // THE CLEANUP ASSERTION: output-0's placeholder was deleted by
       // abort_batch. Only output-1's (pre-seeded, not owned by this handler)
       // remains. Total 'uploading' rows = 1, and it's output-1's hash.
       let uploading: Vec<(Vec<u8>,)> = sqlx::query_as(
           "SELECT store_path_hash FROM manifests WHERE status = 'uploading'"
       ).fetch_all(&s.db.pool).await?;
       assert_eq!(uploading.len(), 1, "only the pre-seeded placeholder survives; \
            output-0's placeholder cleaned up by abort_batch");
       assert_eq!(uploading[0].0, out1_hash, "survivor is the pre-seeded output-1, not output-0");

       // Secondary: retry after external cleanup succeeds for BOTH.
       rio_store::metadata::delete_manifest_uploading(&s.db.pool, &out1_hash).await?;
       let (tx, rx) = mpsc::channel(16);
       send_batch_output(&tx, 0, out0_info.into(), out0_nar).await;
       send_batch_output(&tx, 1, out1_info.into(), out1_nar).await;
       drop(tx);
       let resp = client.put_path_batch(ReceiverStream::new(rx)).await?.into_inner();
       assert_eq!(resp.created, vec![true, true], "clean retry succeeds for both");

       Ok(())
   }
   ```

**Recommend Option C.** Option A requires infra for PG fault injection mid-handler (doesn't exist); Option B is schema-coincidence (breaks on `ON CONFLICT` behavior change). C's static guard is cheap and catches the exact regression class; C's behavioral test proves `abort_batch` plumbing works for the sibling `bail!` at `:280` which shares the cleanup mechanism.

**Check at dispatch:** `rio_store::metadata::insert_manifest_uploading` / `delete_manifest_uploading` may not be `pub` at the module level — they're called from `grpc/put_path.rs` via `crate::metadata::...`. If they're `pub(crate)`, re-export or inline the SQL (the placeholder insert is one `INSERT ... ON CONFLICT DO NOTHING`).

## Exit criteria

- `/nbr .#ci` green
- T1: `grep -n '\.map_err.*?;' rio-store/src/grpc/put_path_batch.rs` → 0 hits in the `put_path_batch_impl` body (the `?` at `:275` is gone; `?` outside the handler, e.g. at the outer `fn put_path_batch` delegation, is fine)
- T1: every `return Err` / error-propagation site in phase-2/phase-3 reaches `abort_batch` — visual audit (grep `bail!` count vs total error paths; was 18 `bail!` + 1 `?` → now 19 `bail!`)
- T2 static: `cargo nextest run -p rio-store put_path_batch_impl_no_question_mark_bypass` → pass
- T2 static mutation check: temporarily reintroduce a `?;` in phase-2 → static test fails with "found `?` inside put_path_batch_impl phase-2/3 body"
- T2 behavioral: `cargo nextest run -p rio-store gt13_batch_placeholder_cleanup_on_midloop_abort` → pass; the `uploading.len() == 1` assert is load-bearing (proves output-0's placeholder was cleaned up, not leaked)
- T2 behavioral precondition: `send_batch_output` sends output-0 BEFORE output-1, and `BTreeMap` iterates 0 → 1 in phase-2 — if a future change breaks ordering, the test's comment at "output-0's placeholder IS inserted before output-1's insert fails" becomes false. The `SELECT store_path_hash` assertion (`== out1_hash`) catches this: a reversed order would leave output-1's placeholder (which the handler doesn't own and can't clean), but the test pre-seeded that one — the assertion still holds. What changes: if order reverses AND the pre-seeded placeholder is for output-0 instead, output-1's leak would be invisible. Keep the pre-seed on the HIGHER index.
- `nix develop -c tracey query rule store.atomic.multi-output` shows ≥2 `verify` sites (the original at [`:275`](../../rio-store/tests/grpc/chunked.rs) `gt13_batch_rpc_atomic` + T2's new test)

## Tracey

References existing markers:
- `r[store.atomic.multi-output]` — T1 hardens the implementation (placeholder cleanup on every error path, not just the ones that happened to use `bail!`); T2 verifies (cleanup-on-midloop-abort)
- `r[store.put.wal-manifest]` — T1 touches the write-ahead-placeholder flow (step 1 of the WAL pattern at [`store.md:111`](../../docs/src/components/store.md)); the `'uploading'` placeholder leak is a WAL-consistency bug, not just atomicity

No new markers. The behavior was already spec'd ("zero rows on failure" at [`store.md:127`](../../docs/src/components/store.md) + "orphan scanner reclaims stale `'uploading'`" at [`:120`](../../docs/src/components/store.md)); the fix makes the code match spec for the PG-transient case.

## Files

```json files
[
  {"path": "rio-store/src/grpc/put_path_batch.rs", "action": "MODIFY", "note": "T1: :268-275 .map_err(..)? -> match+bail! (mirrors :256-263 check_manifest_complete shape)"},
  {"path": "rio-store/tests/grpc/chunked.rs", "action": "MODIFY", "note": "T2: +put_path_batch_impl_no_question_mark_bypass (static grep) + gt13_batch_placeholder_cleanup_on_midloop_abort (behavioral), after :377; r[verify store.atomic.multi-output]"}
]
```

```
rio-store/src/grpc/
└── put_path_batch.rs         # T1: ? → bail! at :275
rio-store/tests/grpc/
└── chunked.rs                # T2: static guard + behavioral cleanup test
```

## Dependencies

```json deps
{"deps": [267], "soft_deps": [311], "note": "T1/T2 fix a defect in P0267's put_path_batch.rs (DONE). discovered_from=267 (P0267 reviewer). Soft-dep P0311: its T17-T19 (added same batch as this plan) also touch chunked.rs PutPathBatch tests — T2 here adds after gt13_batch_rpc_atomic at :377, P0311 T17-T19 add separate tests (oversize FailedPrecondition, already_complete idempotency). All additive, non-overlapping test-fn bodies in the same file — sequence-independent, but if one lands first the other's line refs drift. T2's behavioral test pre-seeds via metadata::insert_manifest_uploading which may be pub(crate) — inline the SQL if so (one INSERT ON CONFLICT DO NOTHING)."}
```

**Depends on:** [P0267](plan-0267-atomic-multi-output-tx.md) — merged ([`7eb0efe1`](https://github.com/search?q=7eb0efe1&type=commits)). The `bail!` macro, `abort_batch`, `owned_placeholders`, and the bug at `:275` all exist.

**Conflicts with:** [`put_path_batch.rs`](../../rio-store/src/grpc/put_path_batch.rs) — fresh file (P0267 only); no collision. [`chunked.rs`](../../rio-store/tests/grpc/chunked.rs) — [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T17-T19 also add PutPathBatch tests here (FailedPrecondition oversize, already_complete idempotency). Both plans add AFTER `gt13_batch_rpc_atomic` at `:377` — additive, different test-fn names, sequence-independent. If P0311 lands first, T2's "after `:377`" drifts by ~150 lines.

[P0304](plan-0304-trivial-batch-p0222-harness.md) T37-T38 (added same batch as this plan) touch `put_path_batch.rs` at `:302` (unwrap→expect) and `:258`/`:331` (metrics). T1 here touches `:268-275` — same file, non-overlapping sections, all additive/inline-replacement. If both dispatch concurrently, rebase resolves clean.
