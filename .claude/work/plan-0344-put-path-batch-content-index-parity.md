# Plan 0344: PutPathBatch content-index + observability parity

Consolidator finding (mc=84 window). [`put_path_batch.rs`](../../rio-store/src/grpc/put_path_batch.rs) is missing three post-commit steps that [`put_path.rs`](../../rio-store/src/grpc/put_path.rs) has. The sharp one is `content_index::insert` — **batch-uploaded paths are invisible to the `ContentLookup` RPC**, which the scheduler uses for CA early-cutoff. A multi-output CA derivation uploaded via `PutPathBatch` will rebuild on every subsequent identical-content submission because the content-index never hears about it. The other two are observability only: `bytes_total` + `hmac_rejected{reason=path_not_in_claims}`.

This is a **sibling** to [P0342](plan-0342-put-path-batch-bail-on-placeholder-insert.md), not an append. P0342 fixes the `:275` `?`→`bail!` placeholder leak; this fixes the post-commit parity gaps. Both touch `put_path_batch.rs`; neither overlaps the other's hunks. [P0304](plan-0304-trivial-batch-p0222-harness.md) T38 covers `result=exists` + duration histogram (the two metrics the original P0267 reviewer caught); this plan covers the three the consolidator found deeper.

**Why this wasn't caught at P0267 review:** `put_path.rs`'s `content_index::insert` is at [`:629`](../../rio-store/src/grpc/put_path.rs), 290 lines after the part `put_path_batch.rs` mirrors (phase-2 validation at `:237-280`). The batch handler stops at "commit + increment `result=created`" ([`:335`](../../rio-store/src/grpc/put_path_batch.rs)); the content-index call lives past the commit in the single-path handler, and the consolidator's diff scan between the two was the first thing to pair up the tail-end blocks.

## Entry criteria

- [P0267](plan-0267-atomic-multi-output-tx.md) merged — **DONE** (`put_path_batch.rs` exists with the phase-3 commit loop at `:297-335`)

## Tasks

### T1 — `fix(store):` content_index::insert per-output after commit

MODIFY [`rio-store/src/grpc/put_path_batch.rs`](../../rio-store/src/grpc/put_path_batch.rs) after `tx.commit()` at [`:326`](../../rio-store/src/grpc/put_path_batch.rs), before the success-metrics loop at [`:329`](../../rio-store/src/grpc/put_path_batch.rs):

```rust
// After: tx.commit().await? / bail!
// Before: for c in &created { ... }

// Content-index each created output. Same best-effort semantics as
// PutPath (put_path.rs:629-641): failure doesn't fail the upload
// (paths are addressable by store_path); CA ContentLookup just won't
// find them until a future single-path re-upload indexes them. Done
// AFTER tx commits so ContentLookup's INNER JOIN on status='complete'
// always sees a complete row.
//
// Iterate outputs not `created` — we need (nar_hash, store_path_hash)
// pairs, and `created` is just a flag array. Skip already_complete
// (someone else indexed them) and non-created entries.
for (idx, accum) in &outputs {
    if accum.already_complete || !created[*idx as usize] {
        continue;
    }
    // Phase-3 did info.take() + std::mem::take(store_path_hash), so
    // both are GONE from accum. We need them here. Two options:
    //   (a) Pre-collect (nar_hash, store_path_hash) into a Vec before
    //       phase-3's take() consumes them — see IMPL NOTE below.
    //   (b) Clone before take() in phase-3.
    // Option (a) is cleaner — one extra Vec<(…)> allocation, no
    // per-iteration clones.
}
```

**IMPL NOTE — ownership gymnastics.** Phase-3 does `accum.info.take()` at [`:308`](../../rio-store/src/grpc/put_path_batch.rs) and `std::mem::take(&mut accum.store_path_hash)` at [`:309`](../../rio-store/src/grpc/put_path_batch.rs). By the time we reach the content-index loop, both are `None`/empty in `accum`. The cleanest fix is to **pre-collect** in phase-2: after `info.nar_hash = hash` at [`:232`](../../rio-store/src/grpc/put_path_batch.rs) and `accum.store_path_hash` is set (phase-1 at `:157`), push `(accum.store_path_hash.clone(), hash)` into a `Vec<([u8; 32], [u8; 32])>` indexed parallel to `owned_placeholders`. Then the post-commit loop iterates that vec.

Alternatively, **don't `take()`** — use `.as_ref().expect(...)` in phase-3 and let the data survive for reuse. The `take()` was a move-optimization; `nar_data` is the big one and that's correctly moved via `Bytes::from(std::mem::take(...))` at [`:312`](../../rio-store/src/grpc/put_path_batch.rs). `info.nar_hash` (32 bytes) and `store_path_hash` (32 bytes) are cheap to clone or leave in place. **Recommend don't-`take`** — simplest, no new collection.

Either way, the loop becomes:

```rust
for (idx, accum) in &outputs {
    if accum.already_complete {
        continue;  // indexed by a previous upload
    }
    let info = accum.info.as_ref().expect("validated in phase 2, not taken");
    if let Err(e) = crate::content_index::insert(
        &self.pool,
        &info.nar_hash,
        &accum.store_path_hash,
    ).await {
        warn!(
            output_index = %idx,
            store_path = %info.store_path.as_str(),
            error = %e,
            "PutPathBatch: content_index insert failed (path still addressable by store_path)"
        );
    }
}
```

### T2 — `fix(store):` bytes_total counter per created output

MODIFY [`rio-store/src/grpc/put_path_batch.rs`](../../rio-store/src/grpc/put_path_batch.rs) in the same post-commit loop. `r[obs.metric.transfer-volume]` at [`observability.md:174`](../../docs/src/observability.md) specs `rio_store_put_path_bytes_total` as "Bytes accepted via PutPath (nar_size on success)". Currently batch uploads contribute **zero** to this counter — the [`:329-333`](../../rio-store/src/grpc/put_path_batch.rs) loop only increments `_total{result=created}`.

Fold into the same loop as T1:

```rust
// Also: bytes_total per created output (put_path.rs:645 parity).
// r[impl obs.metric.transfer-volume]
metrics::counter!("rio_store_put_path_bytes_total").increment(info.nar_size);
```

### T3 — `fix(store):` hmac_rejected counter at :151 path-not-in-claims bail

MODIFY [`rio-store/src/grpc/put_path_batch.rs:148-155`](../../rio-store/src/grpc/put_path_batch.rs). The path-not-in-claims `bail!` has no counter; [`put_path.rs:325`](../../rio-store/src/grpc/put_path.rs) has `rio_store_hmac_rejected_total{reason=path_not_in_claims}`. Add before the `bail!`:

```rust
if !claims.expected_outputs.iter().any(|o| o == path_str) {
    warn!(
        output_index = %idx,
        store_path = %path_str,
        worker_id = %claims.worker_id,
        "PutPathBatch: path not in assignment's expected_outputs"
    );
    metrics::counter!(
        "rio_store_hmac_rejected_total",
        "reason" => "path_not_in_claims"
    ).increment(1);
    bail!(Status::permission_denied(format!(
        "output {idx}: path not authorized by assignment token"
    )));
}
```

Note the batch handler's `verify_assignment_token` at [`:60`](../../rio-store/src/grpc/put_path_batch.rs) uses a shared helper — check whether that already increments `reason=invalid_token`/`reason=expired` (it should; `put_path.rs` doesn't duplicate those either, so [`put_path.rs:169/198/209`](../../rio-store/src/grpc/put_path.rs) are likely inside `verify_assignment_token`). T3's scope is **only** the `path_not_in_claims` reason which is per-output and happens outside the helper.

### T4 — `test(store):` ContentLookup finds batch-uploaded paths

NEW test in [`rio-store/tests/grpc/chunked.rs`](../../rio-store/tests/grpc/chunked.rs) after the existing `gt13_batch_rpc_atomic` at [`:377`](../../rio-store/tests/grpc/chunked.rs). This is the FUNCTIONAL regression test — proves T1's fix makes batch-uploaded paths findable by CA content hash:

```rust
/// Content-index parity: batch-uploaded outputs are findable by
/// nar_hash via ContentLookup. Without the fix, ContentLookup returns
/// empty for a path that only ever went through PutPathBatch — the
/// scheduler's CA cutoff check at sched.ca.cutoff-compare hits no
/// match, and identical-content derivations rebuild.
// r[verify store.atomic.multi-output]
// r[verify sched.ca.cutoff-compare]
#[tokio::test]
async fn gt13_batch_content_lookup_finds_outputs() -> TestResult {
    let s = StoreSession::new().await?;

    let out0_path = test_store_path("content-idx-0");
    let (out0_nar, out0_hash) = make_nar(b"content lookup zero");
    let out0_info = make_path_info_for_nar(&out0_path, &out0_nar);

    let (tx, rx) = mpsc::channel(8);
    send_batch_output(&tx, 0, out0_info.into(), out0_nar).await;
    drop(tx);
    let resp = s.client.clone()
        .put_path_batch(ReceiverStream::new(rx))
        .await?
        .into_inner();
    assert_eq!(resp.created, vec![true]);

    // THE ASSERTION: ContentLookup by nar_hash finds the batch output.
    let lookup = s.client.clone()
        .content_lookup(ContentLookupRequest {
            content_hash: out0_hash.to_vec(),
        })
        .await?
        .into_inner();
    assert!(
        !lookup.store_paths.is_empty(),
        "ContentLookup must find batch-uploaded path by nar_hash — \
         without content_index::insert, CA cutoff is blind to batch uploads"
    );
    assert!(lookup.store_paths.iter().any(|p| p == &out0_path));

    Ok(())
}
```

**Mutation check at dispatch:** comment out T1's `content_index::insert` call → this test fails with `lookup.store_paths.is_empty()`. Proves the test catches the regression.

## Exit criteria

- `/nbr .#ci` green
- T1: `grep -c 'content_index::insert' rio-store/src/grpc/put_path_batch.rs` → ≥1 (the call exists in the post-commit region, between `:326` and `:335`)
- T1: `grep 'already_complete' rio-store/src/grpc/put_path_batch.rs | grep -c continue` → ≥2 (phase-3's existing skip + T1's new skip in the content-index loop)
- T2: `grep 'rio_store_put_path_bytes_total' rio-store/src/grpc/put_path_batch.rs` → ≥1 hit
- T2: `grep -B2 'rio_store_put_path_bytes_total' rio-store/src/grpc/put_path_batch.rs | grep 'r\[impl obs.metric.transfer-volume\]'` → match (annotated)
- T3: `grep -c 'hmac_rejected_total' rio-store/src/grpc/put_path_batch.rs` → ≥1 (the `path_not_in_claims` increment — `verify_assignment_token` handles the other reasons)
- T4: `cargo nextest run -p rio-store gt13_batch_content_lookup_finds_outputs` → pass
- T4 mutation: revert T1's `content_index::insert` loop → T4 fails on `!lookup.store_paths.is_empty()`
- `nix develop -c tracey query rule obs.metric.transfer-volume` shows ≥2 `impl` sites ([`put_path.rs:644`](../../rio-store/src/grpc/put_path.rs) + T2's new annotation)
- `nix develop -c tracey query rule sched.ca.cutoff-compare` shows ≥1 `verify` site (T4)

## Tracey

References existing markers:
- `r[store.atomic.multi-output]` — T1/T2 complete the post-commit side of the atomic batch contract (content-index + bytes counter both happen after the single-tx commit); T4 verifies
- `r[obs.metric.transfer-volume]` — T2 implements for batch path; currently only single-path has the `r[impl]` annotation at [`put_path.rs:644`](../../rio-store/src/grpc/put_path.rs)
- `r[obs.metric.store]` — T3 implements (`rio_store_hmac_rejected_total` is in the table at [`observability.md:137`](../../docs/src/observability.md))
- `r[sched.ca.cutoff-compare]` — T4 verifies (content-index populated → scheduler's ContentLookup-backed cutoff check finds batch outputs)
- `r[store.put.wal-manifest]` — T1 completes step-6 ("content index entries" at [`store.md:116`](../../docs/src/components/store.md)) for the batch path

No new markers. `r[store.put.wal-manifest]` step 6 already specs "content index entries" as part of the complete-manifest flow; `put_path_batch.rs` was just incomplete against spec.

## Files

```json files
[
  {"path": "rio-store/src/grpc/put_path_batch.rs", "action": "MODIFY", "note": "T1: content_index::insert loop after :326 tx.commit; T2: bytes_total counter in same loop; T3: hmac_rejected counter at :148-155; may refactor phase-3 :308-309 to as_ref+clone instead of take (see T1 IMPL NOTE)"},
  {"path": "rio-store/tests/grpc/chunked.rs", "action": "MODIFY", "note": "T4: gt13_batch_content_lookup_finds_outputs after :377; r[verify store.atomic.multi-output] + r[verify sched.ca.cutoff-compare]"}
]
```

```
rio-store/src/grpc/
└── put_path_batch.rs      # T1-T3: content_index + bytes_total + hmac_rejected
rio-store/tests/grpc/
└── chunked.rs             # T4: ContentLookup finds batch outputs
```

## Dependencies

```json deps
{"deps": [267], "soft_deps": [342, 304, 311, 0345], "note": "P0267 (DONE) — put_path_batch.rs + content_index module exist. discovered_from=consolidator(mc84). Soft-dep P0342: fixes :275 ?→bail! same file, non-overlapping (T1 here is post-:326, T2 adds to :329 loop, T3 adds at :148; P0342 T1 edits :268-275). Soft-dep P0304-T38: adds result=exists at :258 + duration histogram at handler entry — same file, non-overlapping. T4 here adds a test to chunked.rs after :377; P0342-T2 and P0311-T17/T18 ALSO add tests after :377 — all additive, distinct test-fn names. Line drift if any land first; re-grep at dispatch. Soft-dep P0345 (validate_put_metadata extraction): if it lands first, the phase-1 metadata-validation block that T3's hmac_rejected counter lives in MOVES to a shared helper — T3's edit would go into the helper instead of :148-155."}
```

**Depends on:** [P0267](plan-0267-atomic-multi-output-tx.md) — merged. `put_path_batch.rs` exists with the phase-3 commit loop; `content_index::insert` exists at [`content_index.rs:54`](../../rio-store/src/content_index.rs).

**Conflicts with:** [`put_path_batch.rs`](../../rio-store/src/grpc/put_path_batch.rs) — [P0342](plan-0342-put-path-batch-bail-on-placeholder-insert.md) T1 edits `:268-275`; [P0304](plan-0304-trivial-batch-p0222-harness.md) T37-T38 edit `:302/:258/handler-entry`; T1 here edits post-`:326`, T3 edits `:148-155`. All non-overlapping hunks. [`chunked.rs`](../../rio-store/tests/grpc/chunked.rs) — P0342-T2 + P0311-T17/T18 + T4 here all add after `:377`, additive. [P0345](plan-0345-put-path-validate-metadata-helper.md) if it lands first moves the `:125-155` validation block into a shared helper — T3's edit migrates there.
