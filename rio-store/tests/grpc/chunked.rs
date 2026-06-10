//! Chunked CAS PutPath (FastCDC write-ahead flow).
// r[verify store.put.wal-manifest]

use super::*;

// r[verify store.inline.threshold]
/// Small NAR + chunked backend: should STILL go inline (under threshold).
#[tokio::test]
async fn test_chunked_small_nar_stays_inline() -> TestResult {
    let (mut s, backend) = StoreSession::new_chunked().await?;

    let store_path = test_store_path("chunked-small");
    let nar = make_nar(b"tiny").0;
    let info = make_path_info_for_nar(&store_path, &nar);

    let created = put_path(&mut s.client, info, nar).await?;
    assert!(created);

    // Chunk backend should be empty — went inline.
    assert!(
        backend.is_empty(),
        "small NAR should not reach chunk backend"
    );

    // Manifest should be Inline (has inline_blob).
    let inline_blob: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT m.inline_blob FROM manifests m JOIN narinfo n \
         ON m.store_path_hash = n.store_path_hash WHERE n.store_path = $1",
    )
    .bind(&store_path)
    .fetch_one(&s.db.pool)
    .await?;
    assert!(inline_blob.is_some(), "small NAR should have inline_blob");

    Ok(())
}

/// Large NAR: chunked path activates. Backend gets chunks, inline_blob NULL,
/// manifest_data populated.
#[tokio::test]
async fn test_chunked_large_nar_chunks() -> TestResult {
    let (mut s, backend) = StoreSession::new_chunked().await?;

    // 1 MiB — well over INLINE_THRESHOLD (256 KiB).
    let (nar, info, store_path) = make_large_nar(1, 1024 * 1024);

    let created = put_path(&mut s.client, info, nar).await?;
    assert!(created);

    // Chunk backend should have chunks (1 MiB / 64 KiB avg ≈ 16).
    let chunk_count = backend.len();
    assert!(
        chunk_count > 0,
        "large NAR should reach chunk backend, got {chunk_count} chunks"
    );
    assert!(
        chunk_count > 4,
        "1 MiB at 64 KiB avg should be >4 chunks, got {chunk_count}"
    );

    // inline_blob should be NULL (chunked marker).
    let inline_blob: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT m.inline_blob FROM manifests m JOIN narinfo n \
         ON m.store_path_hash = n.store_path_hash WHERE n.store_path = $1",
    )
    .bind(&store_path)
    .fetch_one(&s.db.pool)
    .await?;
    assert!(
        inline_blob.is_none(),
        "chunked NAR should have NULL inline_blob"
    );

    // manifest_data should exist.
    let md_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM manifest_data md JOIN narinfo n \
         ON md.store_path_hash = n.store_path_hash WHERE n.store_path = $1",
    )
    .bind(&store_path)
    .fetch_one(&s.db.pool)
    .await?;
    assert_eq!(md_count, 1, "manifest_data row should exist");

    // chunks table: one row per backend chunk, every one confirmed
    // uploaded, none touched by a second reference yet.
    let rows: Vec<(bool, bool)> = sqlx::query_as(
        "SELECT (uploaded_at IS NOT NULL), (last_referenced_at IS NOT NULL) FROM chunks",
    )
    .fetch_all(&s.db.pool)
    .await?;
    assert_eq!(rows.len(), chunk_count);
    for (uploaded, touched) in &rows {
        assert!(
            *uploaded,
            "first upload: every chunk row confirmed uploaded"
        );
        assert!(!*touched, "first upload: no chunk re-referenced yet");
    }

    Ok(())
}

/// The dedup test: upload two large NARs that share most of their content.
/// The second upload should skip most chunks (backend chunk count should
/// NOT double).
#[tokio::test]
async fn test_chunked_dedup_across_uploads() -> TestResult {
    let (mut s, backend) = StoreSession::new_chunked().await?;

    // Two NARs with IDENTICAL payloads (seed=5 both times). Different
    // store paths (so they're different PutPath calls) but same content
    // → same chunks → 100% dedup on the second upload.
    //
    // In practice two store paths with the same NAR content would be a
    // weird nixpkgs thing (two fetchurl of the same file), but it DOES
    // happen, and it's the clearest dedup test.
    let (nar_a, info_a, _) = make_large_nar(5, 1024 * 1024);
    put_path(&mut s.client, info_a, nar_a).await?;
    let chunks_after_a = backend.len();
    assert!(chunks_after_a > 4);

    // Second NAR: same seed = same payload = same chunks.
    // Different store_path (different name arg) so it's a fresh PutPath.
    let (nar_b, _) = make_nar(&pseudo_random_bytes(5, 1024 * 1024));
    let path_b = test_store_path("large-nar-5-dup");
    let info_b = make_path_info_for_nar(&path_b, &nar_b);

    put_path(&mut s.client, info_b, nar_b).await?;
    let chunks_after_b = backend.len();

    // THE dedup assertion: chunk count should NOT have doubled.
    // Identical payloads → identical chunks → zero new uploads.
    assert_eq!(
        chunks_after_b, chunks_after_a,
        "identical content should dedup 100%: {chunks_after_a} chunks after A, \
         {chunks_after_b} after B (should be equal)"
    );

    // Every chunk row was re-referenced by the second manifest (the
    // upsert's conflict arm recorded the touch) and stays confirmed.
    let rows: Vec<(bool, bool)> = sqlx::query_as(
        "SELECT (uploaded_at IS NOT NULL), (last_referenced_at IS NOT NULL) FROM chunks",
    )
    .fetch_all(&s.db.pool)
    .await?;
    for (uploaded, touched) in &rows {
        assert!(*uploaded, "dedup never clears confirmed presence");
        assert!(
            *touched,
            "two uploads of same content: every chunk re-referenced"
        );
    }

    Ok(())
}

/// Idempotent PutPath for chunked: second upload of same store path returns
/// created=false, doesn't touch chunks.
#[tokio::test]
async fn test_chunked_idempotent() -> TestResult {
    let (mut s, backend) = StoreSession::new_chunked().await?;

    let (nar, info, _) = make_large_nar(7, 512 * 1024);

    let first = put_path(&mut s.client, info.clone(), nar.clone()).await?;
    assert!(first);
    let chunks_first = backend.len();

    // Same path again: idempotency short-circuits at check_manifest_complete,
    // before any chunking happens.
    let second = put_path(&mut s.client, info, nar).await?;
    assert!(!second, "second PutPath should return created=false");
    assert_eq!(
        backend.len(),
        chunks_first,
        "idempotent PutPath should not touch chunks"
    );

    Ok(())
}

/// Hash mismatch rollback: send a large NAR declaring the WRONG hash.
/// Validation fails → abort_upload. Verify: no manifest_data, no chunk
/// rows leaked.
///
/// This exercises the OLD abort path (pre-chunking) — the validation
/// failure happens at step 5, BEFORE put_chunked is called. So this is
/// really testing that the inline abort path still works for large NARs.
#[tokio::test]
async fn test_chunked_hash_mismatch_no_leaked_state() -> TestResult {
    let (mut s, backend) = StoreSession::new_chunked().await?;

    let (_good_nar, good_info, _) = make_large_nar(9, 512 * 1024);
    let (bad_nar, _, _) = make_large_nar(10, 512 * 1024);

    // Declare good_nar's hash, send bad_nar → validation fails.
    let result = put_path(&mut s.client, good_info, bad_nar).await;
    assert!(result.is_err(), "hash mismatch should be rejected");

    // No leaked state: chunks empty, no manifest rows.
    assert!(backend.is_empty());
    let mf_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM manifests")
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(mf_count, 0);
    let chunk_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chunks")
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(chunk_count, 0);

    // Retry with correct NAR succeeds.
    let (good_nar, info, _) = make_large_nar(9, 512 * 1024);
    let retry = put_path(&mut s.client, info, good_nar).await?;
    assert!(retry);

    Ok(())
}

/// GT13 verify — the gap independent PutPath calls have (by design).
///
/// Two independent `put_path()` calls → output-1 commits, output-2 fails,
/// output-1 stays 'complete'. This is ARCHITECTURAL: separate RPCs pull
/// separate pool connections; there's no shared tx. This test documents
/// the gap; `gt13_batch_rpc_atomic` below proves `PutPathBatch` closes it.
///
/// Kept (not inverted) because independent PutPath is still the fallback
/// when an output is too large for the v1 batch handler's inline-only
/// limit — the gap persists in that case and this test documents it.
#[tokio::test]
async fn gt13_multi_output_not_atomic() -> TestResult {
    let mut s = StoreSession::new().await?;

    // Output 1: valid. Small NAR → inline (no chunking needed for this
    // demonstration — the gap is at the per-RPC level, not per-chunk).
    let out1_path = test_store_path("gt13-out1");
    let (out1_nar, _) = make_nar(b"output one content");
    let out1_info = make_path_info_for_nar(&out1_path, &out1_nar);
    let r1 = put_path(&mut s.client, out1_info, out1_nar).await?;
    assert!(r1, "output-1 PutPath succeeds");

    // Output 2: hash mismatch (declare out1's hash, send garbage).
    // Models: network corruption, worker crash mid-stream, S3 fault.
    let out2_path = test_store_path("gt13-out2");
    let (out2_good_nar, _) = make_nar(b"output two content");
    let out2_info = make_path_info_for_nar(&out2_path, &out2_good_nar);
    let (out2_bad_nar, _) = make_nar(b"CORRUPTED");
    let r2 = put_path(&mut s.client, out2_info, out2_bad_nar).await;
    assert!(r2.is_err(), "output-2 PutPath fails (hash mismatch)");

    // THE GAP: output-1 is already 'complete' in PG. Nothing rolled it
    // back when output-2 failed — there's no mechanism that COULD roll
    // it back (separate RPC, separate transaction, already committed).
    //
    // A consumer querying output-1 right now gets a valid response. For
    // a multi-output derivation this breaks the "all outputs or none"
    // contract — partial registration is visible.
    let complete: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM manifests m JOIN narinfo n \
         ON m.store_path_hash = n.store_path_hash \
         WHERE n.store_path = $1 AND m.status = 'complete'",
    )
    .bind(&out1_path)
    .fetch_one(&s.db.pool)
    .await?;
    assert_eq!(
        complete, 1,
        "independent PutPath: output-1 survives output-2 failure \
         (architectural — separate RPCs, separate transactions). \
         Use PutPathBatch for cross-output atomicity."
    );

    // Output-2 correctly rolled back (per-path rollback DOES work).
    let out2_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM manifests m JOIN narinfo n \
         ON m.store_path_hash = n.store_path_hash \
         WHERE n.store_path = $1",
    )
    .bind(&out2_path)
    .fetch_one(&s.db.pool)
    .await?;
    assert_eq!(out2_rows, 0, "output-2 per-path rollback works correctly");

    Ok(())
}

// r[verify store.atomic.multi-output]
/// `PutPathBatch`: all outputs commit in one transaction. Mid-batch
/// failure → ZERO rows committed (not even the outputs that validated
/// cleanly before the bad one).
///
/// This is the inverse of `gt13_multi_output_not_atomic`: same 2-output
/// shape (output-1 valid, output-2 corrupt), but sent via the batch RPC.
/// Assert zero `'complete'` rows — the WHOLE batch rolled back.
///
/// Then: retry with BOTH valid → both commit. Proves the rollback was
/// clean (no stale placeholders blocking the retry).
#[tokio::test]
async fn gt13_batch_rpc_atomic() -> TestResult {
    let s = StoreSession::new().await?;

    // Output 0: valid content + matching trailer.
    let out0_path = test_store_path("batch-out0");
    let (out0_nar, _) = make_nar(b"output zero content");
    let out0_info = make_path_info_for_nar(&out0_path, &out0_nar);

    // Output 1: trailer declares out1_good's hash but we send out1_bad's bytes.
    // Same fault as gt13_multi_output_not_atomic (hash mismatch mid-batch).
    let out1_path = test_store_path("batch-out1");
    let (out1_good_nar, _) = make_nar(b"output one content");
    let out1_info = make_path_info_for_nar(&out1_path, &out1_good_nar);
    let (out1_bad_nar, _) = make_nar(b"CORRUPTED BYTES");

    // --- Attempt 1: mid-batch failure → zero rows ---
    let (tx, rx) = mpsc::channel(16);
    // Send output-0 FULLY first (metadata → chunk → trailer), then
    // output-1. Serial — matches the worker's batch streaming shape.
    send_batch_output(&tx, 0, out0_info.clone().into(), out0_nar.clone()).await;
    send_batch_output(&tx, 1, out1_info.clone().into(), out1_bad_nar).await;
    drop(tx);

    let mut client = s.client.clone();
    let r = client.put_path_batch(ReceiverStream::new(rx)).await;
    assert!(r.is_err(), "batch with corrupt output-1 must fail: {r:?}");
    let status = r.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("output 1"),
        "error should name the failing output: {}",
        status.message()
    );

    // THE ATOMICITY ASSERTION: zero 'complete' rows. Output-0 validated
    // fine, but was NEVER committed (phase-3 tx rolled back when
    // output-1 failed validation in phase-2 — actually phase-2 bails
    // BEFORE phase-3's tx even opens).
    let complete: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM manifests WHERE status = 'complete'")
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(
        complete, 0,
        "store.atomic.multi-output: mid-batch failure → ZERO commits \
         (contrast gt13_multi_output_not_atomic where output-0 survives)"
    );

    // Placeholders cleaned up too. PlaceholderGuard's drop-spawn is
    // async — poll until the reap lands so the retry below doesn't
    // hit Concurrent.
    let total = poll_scalar_until(&s.db.pool, "SELECT COUNT(*) FROM manifests", 0i64).await;
    assert_eq!(total, 0, "placeholders must be cleaned up (clean retry)");

    // --- Attempt 2: both valid → both commit (clean retry) ---
    let (tx, rx) = mpsc::channel(16);
    send_batch_output(&tx, 0, out0_info.into(), out0_nar).await;
    send_batch_output(&tx, 1, out1_info.into(), out1_good_nar).await;
    drop(tx);

    let resp = client
        .put_path_batch(ReceiverStream::new(rx))
        .await
        .context("retry with valid inputs should succeed")?
        .into_inner();
    assert_eq!(resp.created, vec![true, true], "both outputs newly created");

    // Both complete. 2 rows — that's the precondition-shaped assert from
    // plan-review-preferences ("proves nothing" guard): we checked the
    // test STARTED from zero rows above, so 2 here proves BOTH this batch
    // committed AND the first batch truly rolled back.
    let complete: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM manifests WHERE status = 'complete'")
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(complete, 2, "both outputs committed atomically");

    // QueryPathInfo works for both (full visibility).
    for p in [&out0_path, &out1_path] {
        let info = client
            .query_path_info(QueryPathInfoRequest {
                store_path: p.clone(),
            })
            .await?
            .into_inner();
        assert_eq!(&info.store_path, p);
    }

    Ok(())
}

// r[verify store.put.stale-reclaim]
/// I-207 batch path: PutPathBatch reclaims a stale `'uploading'`
/// placeholder same as PutPath. Mirrors `test_putpath_reclaims_stale_
/// uploading` (core.rs) — kept separate so the batch handler's reclaim
/// has its own coverage (the verifier flagged it).
#[tokio::test]
async fn gt13_batch_reclaims_stale_uploading() -> TestResult {
    let s = StoreSession::new().await?;
    let store_path = test_store_path("i207-batch-stale");
    let (nar, _) = make_nar(b"i207 batch stale");
    let info = make_path_info_for_nar(&store_path, &nar);

    // Stale placeholder past SUBSTITUTE_STALE_THRESHOLD (5min).
    let sph = rio_nix::store_path::StorePath::parse(&store_path)
        .unwrap()
        .sha256_digest();
    sqlx::query(
        r#"INSERT INTO narinfo (store_path_hash, store_path, nar_hash,
               nar_size, "references") VALUES ($1, $2, $3, 0, '{}')"#,
    )
    .bind(sph.as_slice())
    .bind(&store_path)
    .bind(&[0u8; 32] as &[u8])
    .execute(&s.db.pool)
    .await?;
    // claim_id minted like every real uploader's row (M_052) — a
    // claim-less 'uploading' row means released-in-place and is
    // immediately claimable, which would bypass the stale-reclaim arm
    // this test exercises.
    sqlx::query(
        "INSERT INTO manifests (store_path_hash, status, claim_id, updated_at) \
         VALUES ($1, 'uploading', gen_random_uuid(), now() - make_interval(secs => 600))",
    )
    .bind(sph.as_slice())
    .execute(&s.db.pool)
    .await?;

    let (tx, rx) = mpsc::channel(4);
    send_batch_output(&tx, 0, info.into(), nar).await;
    drop(tx);

    let resp = s
        .client
        .clone()
        .put_path_batch(ReceiverStream::new(rx))
        .await
        .context("I-207: stale placeholder must be reclaimed by PutPathBatch")?
        .into_inner();
    assert_eq!(
        resp.created,
        vec![true],
        "I-207: batch path reclaims stale placeholder → created=true"
    );

    Ok(())
}

/// The `bail!` macro in `put_path_batch_impl` is load-bearing: it
/// emits the per-output `result="error"` metric before returning.
/// Placeholder cleanup is now structural (PlaceholderGuard's Drop
/// reaps owned placeholders on ANY exit including a bare `?`), but a
/// `?` still bypasses the metric increment — the SLI at
/// observability.typ would over-report availability. Every error
/// return in phase-2/phase-3 MUST go through `bail!`.
///
/// Brittle-by-design: a false-positive on a `?` inside a closure or a
/// pre-placeholder helper is preferable to silent metric drift. If a
/// legitimate `?` is added, slice the body more tightly or convert the
/// `?` to `match + bail!`.
#[test]
fn put_path_batch_impl_no_question_mark_bypass() {
    let src = include_str!("../../src/grpc/put_path_batch.rs");

    // Slice the impl body: between `fn put_path_batch_impl(` and
    // `async fn drain_batch_stream` (the next sibling fn). Every `?`
    // in the phase-2/3 part of that slice is a suspect.
    let start = src
        .find("fn put_path_batch_impl(")
        .expect("put_path_batch_impl present");
    let end = src[start..]
        .find("async fn drain_batch_stream")
        .expect("drain_batch_stream sibling present")
        + start;
    let body = &src[start..end];

    // Phase-1 (stream drain) never arms guards or counts outputs, so a
    // `?` there is harmless. Slice from the phase-2 marker.
    let phase2_start = body.find("--- Phase 2:").expect("phase-2 marker present");
    let tail = &body[phase2_start..];

    // Count `?` tokens used as try-propagation. Match `?;` (expression
    // terminator) and `?\n` (`?` at end of line without explicit `;`,
    // e.g. inside a chain). Both are bypasses if they reach the outer
    // `Result<_, Status>` return.
    let q_count = tail.matches("?;").count() + tail.matches("?\n").count();
    assert_eq!(
        q_count, 0,
        "found `?` inside put_path_batch_impl phase-2/3 body — \
         every error return after a placeholder may be pushed MUST \
         use bail! (which calls abort_batch). A bare `?` leaks the \
         placeholder until the 2h orphan sweep. See P0342."
    );
}

/// Phase-2 loop iteration: output-0's placeholder is pushed via
/// `owned_placeholders.push`, then output-1's
/// `insert_manifest_uploading` returns `Ok(false)` (pre-seeded
/// conflict — concurrent uploader owns it). The `!inserted` branch's
/// `bail!` must call `abort_batch()` → delete output-0's placeholder
/// too. Without cleanup, output-0 is wedged: next batch hits
/// `!inserted` on output-0's stale placeholder → aborts forever
/// (until the 2h orphan sweep).
///
/// NOTE: this tests the `!inserted` branch's `bail!`, which shares
/// `abort_batch` with the `Err(e) => bail!(metadata_status(...))` path
/// (`insert_manifest_uploading` returning `Err`, not `Ok(false)`).
/// That Err path requires PG fault injection mid-handler — covered
/// statically by [`put_path_batch_impl_no_question_mark_bypass`]
/// instead. Both paths reach the same `abort_batch` cleanup.
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

    // Pre-seed an 'uploading' placeholder for output-1 via raw SQL
    // (simulates a concurrent uploader owning the slot).
    // `insert_manifest_uploading` for output-1 will return `Ok(false)`
    // → `!inserted` branch → bail!.
    //
    // Can't call `rio_store::metadata::insert_manifest_uploading`
    // directly — the `metadata` module is `pub(crate)`. Inline the two
    // INSERTs (see metadata/inline.rs:69-95). Skip the GC-lock
    // preamble (no GC running in a test).
    let out1_hash: Vec<u8> = out1_info.store_path.sha256_digest().to_vec();
    sqlx::query(
        r#"INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size, "references")
           VALUES ($1, $2, $3, 0, $4)"#,
    )
    .bind(&out1_hash)
    .bind(&out1_path)
    .bind(&[0u8; 32] as &[u8])
    .bind(Vec::<String>::new())
    .execute(&s.db.pool)
    .await?;
    // claim_id minted like every real uploader's row (M_052: ownership
    // unrepresentable-as-absent). A claim-less 'uploading' row means
    // RELEASED-in-place since the stall machinery landed — immediately
    // claimable by any caller — which would defeat this fixture's
    // purpose of modeling a LIVE concurrent owner.
    sqlx::query(
        r#"INSERT INTO manifests (store_path_hash, status, claim_id)
           VALUES ($1, 'uploading', gen_random_uuid())"#,
    )
    .bind(&out1_hash)
    .execute(&s.db.pool)
    .await?;

    // Send the batch. Output-0 metadata+chunk+trailer, then output-1.
    // Handler drains (phase-1) → validates + inserts placeholders
    // (phase-2) → commits (phase-3). BTreeMap iteration: idx 0 first,
    // then idx 1 — so output-0's placeholder IS inserted before
    // output-1's insert hits the conflict.
    let (tx, rx) = mpsc::channel(16);
    send_batch_output(&tx, 0, out0_info.clone().into(), out0_nar.clone()).await;
    send_batch_output(&tx, 1, out1_info.clone().into(), out1_nar.clone()).await;
    drop(tx);

    let mut client = s.client.clone();
    let r = client.put_path_batch(ReceiverStream::new(rx)).await;
    let status = r.expect_err("batch must fail — output-1 slot owned by concurrent uploader");
    assert_eq!(status.code(), tonic::Code::Aborted);
    assert!(
        status.message().contains(rio_proto::CONCURRENT_PUTPATH_MSG),
        "expected CONCURRENT_PUTPATH_MSG, got: {}",
        status.message()
    );

    // THE CLEANUP ASSERTION: output-0's placeholder was deleted by the
    // PlaceholderGuard's drop-spawn. Only output-1's (pre-seeded, not
    // owned by this handler — never armed a guard) remains. Total
    // 'uploading' rows = 1, and it's output-1's hash. The guard's reap
    // is spawned async on Drop — poll until it lands.
    let uploading: Vec<(Vec<u8>,)> = {
        let mut tries = 0;
        loop {
            let rows: Vec<(Vec<u8>,)> =
                sqlx::query_as("SELECT store_path_hash FROM manifests WHERE status = 'uploading'")
                    .fetch_all(&s.db.pool)
                    .await?;
            if rows.len() <= 1 || tries >= 50 {
                break rows;
            }
            tries += 1;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };
    assert_eq!(
        uploading.len(),
        1,
        "only the pre-seeded output-1 placeholder survives; \
         output-0's placeholder was cleaned up by PlaceholderGuard drop"
    );
    assert_eq!(
        uploading[0].0, out1_hash,
        "survivor is the pre-seeded output-1, not output-0 \
         (pre-seed on the HIGHER index so a reversed iteration \
         order would leave the wrong row and fail this assert)"
    );

    // Secondary: retry after external cleanup succeeds for BOTH —
    // proves abort_batch left no junk blocking output-0, and
    // output-1's slot is freed by deleting our pre-seed. Inline
    // `delete_manifest_uploading` (manifests first, FK ordering).
    sqlx::query("DELETE FROM manifests WHERE store_path_hash = $1 AND status = 'uploading'")
        .bind(&out1_hash)
        .execute(&s.db.pool)
        .await?;
    sqlx::query("DELETE FROM narinfo WHERE store_path_hash = $1 AND nar_size = 0")
        .bind(&out1_hash)
        .execute(&s.db.pool)
        .await?;

    let (tx, rx) = mpsc::channel(16);
    send_batch_output(&tx, 0, out0_info.into(), out0_nar).await;
    send_batch_output(&tx, 1, out1_info.into(), out1_nar).await;
    drop(tx);
    let resp = client
        .put_path_batch(ReceiverStream::new(rx))
        .await?
        .into_inner();
    assert_eq!(
        resp.created,
        vec![true, true],
        "clean retry succeeds for both outputs"
    );

    Ok(())
}

/// `PutPathBatch` with a chunk backend, both outputs over
/// `INLINE_THRESHOLD`: phase-2 stages each via `cas::stage_chunked`
/// (chunks uploaded + chunk rows written, manifest still `'uploading'`),
/// phase-3's atomic tx flips both to `'complete'` via
/// `complete_manifest_in_conn`. Asserts both are queryable and
/// both landed as chunked (`manifest_data.chunk_list IS NOT NULL`,
/// `manifests.inline_blob IS NULL`).
#[tokio::test]
async fn gt13_batch_chunked_happy_path() -> TestResult {
    let (s, backend) = StoreSession::new_chunked().await?;
    let mut client = s.client.clone();

    // 512 KiB each — well over INLINE_THRESHOLD (256 KiB). Distinct
    // seeds → distinct store_paths AND distinct chunk content.
    let (nar0, info0, path0) = make_large_nar(20, 512 * 1024);
    let (nar1, info1, path1) = make_large_nar(21, 512 * 1024);

    let (tx, rx) = mpsc::channel(16);
    send_batch_output(&tx, 0, info0.into(), nar0).await;
    send_batch_output(&tx, 1, info1.into(), nar1).await;
    drop(tx);

    let resp = client
        .put_path_batch(ReceiverStream::new(rx))
        .await
        .context("chunked batch happy path should succeed")?
        .into_inner();
    assert_eq!(resp.created, vec![true, true], "both outputs newly created");

    // Both queryable end-to-end.
    for p in [&path0, &path1] {
        let info = client
            .query_path_info(QueryPathInfoRequest {
                store_path: p.clone(),
            })
            .await?
            .into_inner();
        assert_eq!(&info.store_path, p);
    }

    // Both persisted as CHUNKED: manifest_data.chunk_list populated,
    // manifests.inline_blob NULL, status='complete'.
    let chunked_complete: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM manifests m \
         JOIN manifest_data md ON m.store_path_hash = md.store_path_hash \
         WHERE m.status = 'complete' AND m.inline_blob IS NULL \
         AND md.chunk_list IS NOT NULL",
    )
    .fetch_one(&s.db.pool)
    .await?;
    assert_eq!(
        chunked_complete, 2,
        "both outputs flipped to status='complete' via complete_manifest_in_conn"
    );

    // Backend received chunks (2× 512 KiB at ~64 KiB avg ≈ 16 chunks).
    assert!(
        backend.len() > 4,
        "chunk backend should hold >4 chunks, got {}",
        backend.len()
    );

    Ok(())
}

/// `PutPathBatch` chunked abort: output-0 large+valid (gets fully
/// staged in phase-2: placeholder owned, chunks uploaded, chunk rows
/// written), output-1 hash-mismatches → phase-2 `bail!` → `abort_batch`
/// → `reap_one(output-0)` MUST delete output-0's placeholder rows so
/// its staged chunks are left unreferenced (the "GC-eligible orphan"
/// guarantee from the `put_path_batch.rs` doc-comment — the next
/// collect cycle owns them). DB ends with no committed manifests and
/// no manifest references to the staged chunks; only the S3-side blobs
/// orphan (spec: "blob-store writes are NOT rolled back").
#[tokio::test]
async fn gt13_batch_chunked_abort_leaves_chunks_unreferenced() -> TestResult {
    let (s, backend) = StoreSession::new_chunked().await?;
    let mut client = s.client.clone();

    // Output-0: valid large NAR.
    let (nar0, info0, _) = make_large_nar(22, 512 * 1024);
    // Output-1: declare info for seed=23's content but SEND seed=24's
    // bytes → trailer hash mismatch → validate_nar_digest fails.
    let (_, info1, _) = make_large_nar(23, 512 * 1024);
    let (bad_nar1, _, _) = make_large_nar(24, 512 * 1024);

    let (tx, rx) = mpsc::channel(16);
    send_batch_output(&tx, 0, info0.into(), nar0).await;
    send_batch_output(&tx, 1, info1.into(), bad_nar1).await;
    drop(tx);

    let r = client.put_path_batch(ReceiverStream::new(rx)).await;
    let status = r.expect_err("batch with corrupt output-1 must fail");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("output 1"),
        "error should name the failing output: {}",
        status.message()
    );

    // No 'complete' rows (atomic tx never opened — phase-2 bailed).
    let complete: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM manifests WHERE status = 'complete'")
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(complete, 0, "no output committed");

    // No 'uploading' rows (PlaceholderGuard's drop-spawn reaped
    // output-0's placeholder; output-1 never claimed one — hash check
    // is BEFORE claim_placeholder). The guard's reap is spawned async
    // on Drop — poll until it lands.
    let uploading = poll_scalar_until(
        &s.db.pool,
        "SELECT COUNT(*) FROM manifests WHERE status = 'uploading'",
        0i64,
    )
    .await;
    assert_eq!(
        uploading, 0,
        "PlaceholderGuard drop reaped output-0's staged placeholder"
    );

    // THE UNREFERENCED ASSERTION: output-0's staged chunk rows are no
    // longer referenced by any manifest (the reap CASCADE-deleted its
    // manifest_data), so the next collect cycle can reclaim them. The
    // reap itself never touches the chunk rows. The same `reap_one` tx
    // commits the placeholder DELETE, so once `uploading == 0` above
    // this is settled — but poll for symmetry.
    let referencing_manifests =
        poll_scalar_until(&s.db.pool, "SELECT COUNT(*) FROM manifest_data", 0i64).await;
    assert_eq!(
        referencing_manifests, 0,
        "PlaceholderGuard drop's reap_one must delete the staged manifest_data"
    );
    let (chunk_rows, soft_deleted): (i64, i64) =
        sqlx::query_as("SELECT COUNT(*), COUNT(*) FILTER (WHERE deleted) FROM chunks")
            .fetch_one(&s.db.pool)
            .await?;
    assert!(chunk_rows > 0, "the staged chunk rows are left in place");
    assert_eq!(soft_deleted, 0, "the reap never soft-deletes chunk rows");

    // Blob-store writes are NOT rolled back — output-0's chunks orphan
    // in the backend until S3 GC sweeps them. This is the documented
    // bound (≤1 NAR-size of orphaned blob per failed output).
    assert!(
        !backend.is_empty(),
        "staged chunks remain in blob backend (spec: not rolled back)"
    );

    Ok(())
}

/// `PutPathBatch` mixed inline+chunked: output-0 small (< threshold,
/// → `NarPersist::Inline`), output-1 large (≥ threshold, →
/// `NarPersist::ChunkedStaged`). Phase-3's atomic tx must flip BOTH
/// to `'complete'` together — proves the two `complete_manifest_*_in_tx`
/// variants compose inside one transaction.
#[tokio::test]
async fn gt13_batch_mixed_inline_chunked() -> TestResult {
    let (s, _backend) = StoreSession::new_chunked().await?;
    let mut client = s.client.clone();

    // Output-0: tiny → inline.
    let path0 = test_store_path("batch-mixed-inline");
    let (nar0, _) = make_nar(b"tiny mixed-batch output");
    let info0 = make_path_info_for_nar(&path0, &nar0);
    // Output-1: 512 KiB → chunked.
    let (nar1, info1, path1) = make_large_nar(25, 512 * 1024);

    let (tx, rx) = mpsc::channel(16);
    send_batch_output(&tx, 0, info0.into(), nar0).await;
    send_batch_output(&tx, 1, info1.into(), nar1).await;
    drop(tx);

    let resp = client
        .put_path_batch(ReceiverStream::new(rx))
        .await
        .context("mixed inline+chunked batch should succeed")?
        .into_inner();
    assert_eq!(resp.created, vec![true, true]);

    // Both 'complete'.
    let complete: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM manifests WHERE status = 'complete'")
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(complete, 2, "both outputs committed in one tx");

    // Output-0 is inline: inline_blob NOT NULL, no manifest_data row.
    let inline0: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT m.inline_blob FROM manifests m JOIN narinfo n \
         ON m.store_path_hash = n.store_path_hash WHERE n.store_path = $1",
    )
    .bind(&path0)
    .fetch_one(&s.db.pool)
    .await?;
    assert!(inline0.is_some(), "small output stored inline");

    // Output-1 is chunked: inline_blob NULL, manifest_data.chunk_list NOT NULL.
    let inline1: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT m.inline_blob FROM manifests m JOIN narinfo n \
         ON m.store_path_hash = n.store_path_hash WHERE n.store_path = $1",
    )
    .bind(&path1)
    .fetch_one(&s.db.pool)
    .await?;
    assert!(inline1.is_none(), "large output NOT stored inline");

    let chunk_list1: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT md.chunk_list FROM manifest_data md JOIN narinfo n \
         ON md.store_path_hash = n.store_path_hash WHERE n.store_path = $1",
    )
    .bind(&path1)
    .fetch_one(&s.db.pool)
    .await?;
    assert!(chunk_list1.is_some(), "large output has chunk_list");

    Ok(())
}

/// `PutPathBatch` chunked idempotency: resend the same large-output
/// batch → `created=[false,false]`. Phase-2's `claim_placeholder` hits
/// `AlreadyComplete` for both, sets `accum.already_complete=true`,
/// SKIPS `stage_nar_for_batch` — backend chunk count must not grow.
#[tokio::test]
async fn gt13_batch_chunked_idempotent() -> TestResult {
    let (s, backend) = StoreSession::new_chunked().await?;
    let mut client = s.client.clone();

    let (nar0, info0, _) = make_large_nar(26, 512 * 1024);
    let (nar1, info1, _) = make_large_nar(27, 512 * 1024);

    // First send: both created.
    let (tx, rx) = mpsc::channel(16);
    send_batch_output(&tx, 0, info0.clone().into(), nar0.clone()).await;
    send_batch_output(&tx, 1, info1.clone().into(), nar1.clone()).await;
    drop(tx);
    let first = client
        .put_path_batch(ReceiverStream::new(rx))
        .await?
        .into_inner();
    assert_eq!(first.created, vec![true, true]);
    let chunks_after_first = backend.len();
    assert!(chunks_after_first > 0);

    // Resend: idempotency short-circuits at check_manifest_complete.
    let (tx, rx) = mpsc::channel(16);
    send_batch_output(&tx, 0, info0.into(), nar0).await;
    send_batch_output(&tx, 1, info1.into(), nar1).await;
    drop(tx);
    let second = client
        .put_path_batch(ReceiverStream::new(rx))
        .await?
        .into_inner();
    assert_eq!(
        second.created,
        vec![false, false],
        "resend returns created=[false,false]"
    );
    assert_eq!(
        backend.len(),
        chunks_after_first,
        "idempotent resend must not stage new chunks"
    );

    Ok(())
}

/// Send one output's full message sequence (metadata → chunk → trailer)
/// tagged with `output_index`. Mirrors `put_path_raw` but wraps each
/// inner message in `PutPathBatchRequest`.
async fn send_batch_output(
    tx: &mpsc::Sender<rio_proto::types::PutPathBatchRequest>,
    output_index: u32,
    mut info: PathInfo,
    nar: Vec<u8>,
) {
    use rio_proto::types::{PutPathBatchRequest, PutPathRequest, put_path_request};

    // Extract hash/size for trailer, zero them in metadata (trailer-only mode).
    let trailer = PutPathTrailer {
        nar_hash: std::mem::take(&mut info.nar_hash),
        nar_size: std::mem::take(&mut info.nar_size),
    };

    tx.send(PutPathBatchRequest {
        output_index,
        inner: Some(PutPathRequest {
            msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
                info: Some(info),
            })),
        }),
    })
    .await
    .expect("fresh channel");

    tx.send(PutPathBatchRequest {
        output_index,
        inner: Some(PutPathRequest {
            msg: Some(put_path_request::Msg::NarChunk(nar)),
        }),
    })
    .await
    .expect("fresh channel");

    tx.send(PutPathBatchRequest {
        output_index,
        inner: Some(PutPathRequest {
            msg: Some(put_path_request::Msg::Trailer(trailer)),
        }),
    })
    .await
    .expect("fresh channel");
}

/// bug_142: a single batch's `held_permits` accumulates ALL outputs'
/// permits until handler return. Per-output cap × MAX_BATCH_OUTPUTS
/// can exceed the global budget → `acquire_many` blocks on permits the
/// SAME task holds (self-deadlock). Fixed by the cumulative MAX_NAR_SIZE
/// cap in `drain_batch_stream`; a batch under that cap (≤ 1/8 of the
/// default budget) can never self-deadlock. Verified here with a 2 MiB
/// budget and 3 × 600 KiB outputs (1.8 MiB cumulative — under both
/// MAX_NAR_SIZE and the budget, so no rejection AND no deadlock).
///
/// bug_128: the cap previously counted RAW bytes while the semaphore
/// charged `max(len, MIN_NAR_CHUNK_CHARGE)`; a 1-byte stream undercounted
/// 256× and could exhaust the budget before the cap fired. The cap now
/// tracks `nar_chunk_charge(len)` — same unit. End-to-end deadlock
/// reproduction needs ~16M tiny chunks (MAX_NAR_SIZE / 256), so the unit
/// alignment is verified at `rio_common::limits::nar_chunk_charge` and by
/// the `total_charged` formula in `drain_batch_stream`; this test
/// continues to cover the >256-byte chunk case.
// r[verify store.put.nar-bytes-budget+4]
#[tokio::test]
async fn batch_no_self_deadlock_under_budget() -> TestResult {
    let s =
        StoreSession::build(|pool| StoreServiceImpl::new(pool).with_nar_budget(2 * 1024 * 1024))
            .await?;
    let mut client = s.client.clone();

    let (nar0, info0, _) = make_large_nar(50, 600 * 1024);
    let (nar1, info1, _) = make_large_nar(51, 600 * 1024);
    let (nar2, info2, _) = make_large_nar(52, 600 * 1024);

    let (tx, rx) = mpsc::channel(32);
    send_batch_output(&tx, 0, info0.into(), nar0).await;
    send_batch_output(&tx, 1, info1.into(), nar1).await;
    send_batch_output(&tx, 2, info2.into(), nar2).await;
    drop(tx);

    // 30s is enormous headroom for a sub-2-MiB inline batch on
    // ephemeral PG; pre-fix this would hang forever once cumulative
    // bytes > 2 MiB budget.
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        client.put_path_batch(ReceiverStream::new(rx)),
    )
    .await
    .map_err(|_| anyhow::anyhow!("PutPathBatch self-deadlocked on its own permits"))??
    .into_inner();
    assert_eq!(resp.created, vec![true, true, true]);

    Ok(())
}

/// bug_063: batch armed NO drop-guard for owned placeholders. If the
/// handler future was DROPPED (tonic RST_STREAM, server shutdown), N
/// placeholders leaked at `'uploading'` for 5 min. With
/// `PlaceholderGuard`, dropping the future spawns `reap_one` for each
/// owned placeholder.
///
/// We can't easily drop the in-process tonic handler future from the
/// client side, so this test reuses the bail!-path machinery: phase-2
/// inserts output-0's placeholder, output-1 fails verify_nar, the
/// `return Err` drops `placeholder_guards`. Unlike the
/// `gt13_batch_placeholder_cleanup_on_midloop_abort` test (which also
/// asserted cleanup but via the explicit `abort_batch` loop), this
/// asserts the GUARD's drop-spawn does the work — `abort_batch` no
/// longer exists.
// r[verify store.put.drop-cleanup+2]
#[tokio::test]
async fn batch_guard_drop_reaps_placeholders() -> TestResult {
    let s = StoreSession::new().await?;

    let out0_path = test_store_path("guarddrop-out0");
    let (out0_nar, _) = make_nar(b"guard zero");
    let out0_info = make_path_info_for_nar(&out0_path, &out0_nar);

    let out1_path = test_store_path("guarddrop-out1");
    let (out1_nar, _) = make_nar(b"guard one");
    // Metadata declares hash-of-"guard one" but we'll send corrupted
    // bytes — verify_nar fails AFTER output-0's placeholder is owned.
    // Actually: send order is metadata-0, chunk-0, trailer-0,
    // metadata-1, BAD-chunk-1, trailer-1; phase-2 iterates 0 then 1.
    let out1_info = make_path_info_for_nar(&out1_path, &out1_nar);
    let out1_bad_nar = vec![0xFFu8; out1_nar.len()];

    let (tx, rx) = mpsc::channel(16);
    send_batch_output(&tx, 0, out0_info.clone().into(), out0_nar).await;
    send_batch_output(&tx, 1, out1_info.into(), out1_bad_nar).await;
    drop(tx);

    let mut client = s.client.clone();
    let r = client.put_path_batch(ReceiverStream::new(rx)).await;
    let status = r.expect_err("output-1 hash mismatch → batch fails");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("hash mismatch"));

    // Guard's drop-spawn is async — poll until it lands.
    let uploading = poll_scalar_until(
        &s.db.pool,
        "SELECT COUNT(*) FROM manifests WHERE status = 'uploading'",
        0i64,
    )
    .await;
    assert_eq!(
        uploading, 0,
        "PlaceholderGuard drop must reap output-0's placeholder \
         (no abort_batch loop anymore)"
    );

    // And output-0 is uploadable again (claim returns Owned, not
    // Concurrent) — proven by a successful single PutPath.
    let recreated = put_path(&mut client, out0_info, make_nar(b"guard zero").0).await?;
    assert!(recreated, "output-0 slot freed by guard drop");

    Ok(())
}

// =========================================================================
// BW8-S1 NAR-budget mixed-population battery (merged_bug_001 +
// merged_bug_021's ingest edges). The mini upstream is a narinfo+NAR
// axum pair (the in-file `spawn_flex_upstream`'s subset, ported here
// because the substitute leg of the mixed population must be drivable
// from the gRPC integration plane). Shrunk envelopes ride the
// sanctioned builder overrides (`with_nar_ingest_envelope`,
// `with_stall_window`) — the R17 violability lane; assertions are
// structural with ≥10× slack on shrunk bounds.
// =========================================================================

mod bw8s1_budget {
    use super::*;
    use base64::Engine;
    use rio_store::grpc::{NarIngestEnvelopeCfg, StoreServiceImpl as Svc};
    use rio_store::substitute::Substituter;
    use sha2::Digest as _;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;

    pub struct MiniUpstream {
        pub url: String,
        pub trusted_key: String,
        _task: tokio::task::JoinHandle<()>,
    }

    /// Serve ONE (path, NAR) pair with a valid signature. When
    /// `frames` is set, the NAR body streams those frames — every
    /// frame after the first awaits the gate (one `notify_one` per
    /// frame; Notify stores the permit).
    pub async fn spawn_mini_upstream(
        store_path: &str,
        nar_bytes: Vec<u8>,
        key_name: &str,
        frames: Option<(Vec<Vec<u8>>, Arc<tokio::sync::Notify>)>,
    ) -> MiniUpstream {
        use axum::routing::get;

        let seed = [0x42u8; 32];
        let signer = rio_store::signing::Signer::from_seed(key_name, &seed);
        let pubkey = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key();
        let trusted_key = format!(
            "{key_name}:{}",
            base64::engine::general_purpose::STANDARD.encode(pubkey.as_bytes())
        );
        let nar_hash: [u8; 32] = sha2::Sha256::digest(&nar_bytes).into();
        let nar_hash_str = format!(
            "sha256:{}",
            rio_nix::store_path::nixbase32::encode(&nar_hash)
        );
        let fp = rio_nix::narinfo::fingerprint(store_path, &nar_hash, nar_bytes.len() as u64, &[]);
        let sig = signer.sign(&fp);
        let sp = rio_nix::store_path::StorePath::parse(store_path).unwrap();
        let hash_part = sp.hash_part();
        let narinfo_body = format!(
            "StorePath: {store_path}\nURL: nar/{hash_part}.nar\nCompression: none\n\
             NarHash: {nar_hash_str}\nNarSize: {}\nReferences: \nSig: {sig}\n",
            nar_bytes.len()
        );
        let narinfo_path = format!("/{hash_part}.narinfo");
        let nar_path = format!("/nar/{hash_part}.nar");
        let app = axum::Router::new()
            .route(
                "/nix-cache-info",
                get(|| async { "StoreDir: /nix/store\nWantMassQuery: 1\n" }),
            )
            .route(&narinfo_path, get(move || async move { narinfo_body }))
            .route(
                &nar_path,
                get(move || {
                    let nar = nar_bytes.clone();
                    let frames = frames.clone();
                    async move {
                        use axum::response::IntoResponse;
                        if let Some((frames, gate)) = frames {
                            let stream = futures_util::stream::unfold(
                                (frames.into_iter(), gate, true),
                                |(mut it, gate, first)| async move {
                                    let f = it.next()?;
                                    if !first {
                                        gate.notified().await;
                                    }
                                    Some((
                                        Ok::<_, std::io::Error>(axum::body::Bytes::from(f)),
                                        (it, gate, false),
                                    ))
                                },
                            );
                            return axum::body::Body::from_stream(stream).into_response();
                        }
                        nar.into_response()
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        MiniUpstream {
            url: format!("http://{addr}"),
            trusted_key,
            _task: task,
        }
    }

    pub async fn seed_upstream(pool: &sqlx::PgPool, tid: uuid::Uuid, up: &MiniUpstream) {
        sqlx::query(
            "INSERT INTO tenant_upstreams (tenant_id, url, priority, trusted_keys, sig_mode) \
             VALUES ($1, $2, 50, $3, 'keep')",
        )
        .bind(tid)
        .bind(&up.url)
        .bind(vec![up.trusted_key.clone()])
        .execute(pool)
        .await
        .unwrap();
    }

    /// PutPath holder driver: send metadata + chunk1, return the open
    /// tx so the test controls the rest of the stream.
    pub fn holder_stream(
        info: rio_proto::types::PathInfo,
        chunk1: Vec<u8>,
    ) -> (
        mpsc::Sender<PutPathRequest>,
        ReceiverStream<PutPathRequest>,
        PutPathTrailer,
    ) {
        let mut info = info;
        let (tx, rx) = mpsc::channel(8);
        let trailer = PutPathTrailer {
            nar_hash: std::mem::take(&mut info.nar_hash),
            nar_size: std::mem::take(&mut info.nar_size),
        };
        tx.try_send(PutPathRequest {
            msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
                info: Some(info),
            })),
        })
        .expect("fresh channel");
        tx.try_send(PutPathRequest {
            msg: Some(put_path_request::Msg::NarChunk(chunk1)),
        })
        .expect("fresh channel");
        (tx, ReceiverStream::new(rx), trailer)
    }

    /// Shrunk service-side envelope for this battery: wait grace
    /// 500ms; ingest hold envelope = 5×500ms + cap/2⁶⁴ ≈ 2.5s.
    fn shrunk_cfg() -> NarIngestEnvelopeCfg {
        NarIngestEnvelopeCfg {
            budget_wait_grace: Duration::from_millis(500),
            hold_stall_window: Duration::from_millis(500),
            hold_floor_rate: u64::MAX,
        }
    }

    // W8-A (R16 statement): the JOINT mixed population the R7 census
    // named and W-3 withheld — a real PutPath holder and a real parked
    // whole-NAR substitution head on ONE pool reach completion /
    // typed-shed within the typed bounds — end-to-end through
    // production gRPC and the production Substituter, asserted
    // structurally (permit counts, completion, typed status codes).
    // Batch-as-holder is W8-F (WO-S1-2a), same slot, same gate.
    /// RED pre-fix (verbatim, run at 83e596f0c): `left: neither leg
    /// completed within the observation bound; available_permits == 0
    /// pinned below both demands (head demands 2016 > B − H = 1096;
    /// the holder's next chunk acquire queues behind the parked head;
    /// no release can occur in this two-party instance) / right:
    /// substitution complete; upload completed or shed
    /// resource_exhausted; permits restored`.
    #[tokio::test]
    async fn mixed_population_wedge_dissolves_within_typed_bounds() -> TestResult {
        let db = TestDb::new(&MIGRATOR).await;
        let budget_bytes = 4096usize;
        let service = Svc::new(db.pool.clone())
            .with_nar_budget(budget_bytes)
            .with_nar_ingest_envelope(shrunk_cfg());
        let budget = service.nar_bytes_budget().clone();
        let (client, _server) = spawn_store_server(service).await?;

        // Substituter on the SAME pool (the production main.rs wiring
        // shape: one semaphore, two disciplines).
        let tid = rio_store::test_helpers::seed_tenant(&db.pool, "bw8s1-r1").await;
        let sub_content = rio_test_support::fixtures::pseudo_random_bytes(11, 1900);
        let (sub_nar, _h) = make_nar(&sub_content);
        let sub_path = test_store_path("bw8s1-r1-sub");
        let up = spawn_mini_upstream(&sub_path, sub_nar.clone(), "cache.bw8r1", None).await;
        seed_upstream(&db.pool, tid, &up).await;
        let sub_stall = Duration::from_millis(300);
        let sub = Arc::new(
            Substituter::new(db.pool.clone(), None)
                .with_http_client(rio_store::test_helpers::sandbox_http())
                .with_nar_bytes_budget(budget.clone())
                .with_stall_window(sub_stall),
        );

        // Fixture sizing pins: D > B − H (the head demands more than
        // the holder leaves) and chunk2 ≤ B − H (the holder's next
        // chunk is grantable in the head's absence) and D < B.
        let h1 = 3000usize;
        let chunk2_len = 800usize;
        let d = sub_nar.len();
        assert!(d > budget_bytes - h1, "sizing pin: D > B − H ({d})");
        assert!(d < budget_bytes, "sizing pin: D < B ({d})");
        assert!(
            chunk2_len <= budget_bytes - h1,
            "sizing pin: chunk2 fits sans head"
        );

        // Holder: real PutPath, chunk 1, pause (stream open).
        let nar_a: Vec<u8> = rio_test_support::fixtures::pseudo_random_bytes(12, 3800);
        let info_a = make_path_info_for_nar(&test_store_path("bw8s1-r1-hold"), &nar_a);
        let (tx, rx, _trailer) = holder_stream(info_a.into(), nar_a[..h1].to_vec());
        let mut put_client = client.clone();
        let put = tokio::spawn(async move { put_client.put_path(rx).await });
        let wait = tokio::time::Instant::now() + Duration::from_secs(5);
        while budget.available_permits() != budget_bytes - h1 {
            assert!(
                tokio::time::Instant::now() < wait,
                "holder never acquired chunk-1 permits; available={}",
                budget.available_permits()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Head: real substitution parks (declared D > available B−H).
        let leg = {
            let s = Arc::clone(&sub);
            let p = sub_path.clone();
            tokio::spawn(async move { s.try_substitute(tid, &p).await })
        };
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!leg.is_finished(), "head must park on its reservation");

        // Holder resumes: chunk 2 queues BEHIND the parked head — and
        // now sheds typed within the wait grace, releasing the pool.
        tx.send(PutPathRequest {
            msg: Some(put_path_request::Msg::NarChunk(
                nar_a[h1..h1 + chunk2_len].to_vec(),
            )),
        })
        .await
        .expect("stream open");

        // The POST property, within 4× the summed typed bounds
        // (wait grace + ingest envelope + the head's hold deadline).
        let summed = Duration::from_millis(500)
            + Duration::from_millis(2500)
            + sub_stall * 5
            + Duration::from_secs(1);
        let put_res = tokio::time::timeout(summed * 4, put)
            .await
            .expect("upload must complete or shed typed within 4× summed bounds")
            .unwrap();
        let status = put_res.expect_err("the parked-head freeze must shed the holder typed");
        assert_eq!(
            status.code(),
            tonic::Code::ResourceExhausted,
            "typed shed expected, got {status:?}"
        );
        let leg_res = tokio::time::timeout(summed * 4, leg)
            .await
            .expect("substitution must complete within 4× summed bounds")
            .unwrap()
            .expect("substitute leg ok");
        let info = leg_res.expect("upstream has the path");
        assert_eq!(info.nar_size, sub_nar.len() as u64);
        // Pool restored.
        let restore = tokio::time::Instant::now() + Duration::from_secs(5);
        while budget.available_permits() != budget_bytes {
            assert!(
                tokio::time::Instant::now() < restore,
                "permits not restored: {} of {budget_bytes}",
                budget.available_permits()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(tx);
        Ok(())
    }

    // W8-C (R16 statement): ingest-holder residency through the
    // production stream path including cleanup — a stopped-but-
    // connected client cannot hold permits past the ingest envelope.
    /// RED pre-fix (verbatim, run at 83e596f0c): `left: held_permits
    /// retained at 3× the would-be ingest envelope (stopped-but-
    /// connected client; available_permits == 57344 of 65536) /
    /// right: typed resource_exhausted abort; permits restored ≤
    /// bound`.
    #[tokio::test]
    async fn stopped_client_putpath_releases_by_the_ingest_deadline() -> TestResult {
        let db = TestDb::new(&MIGRATOR).await;
        let budget_bytes = 64 * 1024usize;
        let service = Svc::new(db.pool.clone())
            .with_nar_budget(budget_bytes)
            .with_nar_ingest_envelope(shrunk_cfg());
        let budget = service.nar_bytes_budget().clone();
        let (client, _server) = spawn_store_server(service).await?;

        let nar: Vec<u8> = rio_test_support::fixtures::pseudo_random_bytes(13, 8192);
        let info = make_path_info_for_nar(&test_store_path("bw8s1-r3"), &nar);
        let (tx, rx, _trailer) = holder_stream(info.into(), nar.clone());
        let mut put_client = client.clone();
        let put = tokio::spawn(async move { put_client.put_path(rx).await });
        let wait = tokio::time::Instant::now() + Duration::from_secs(5);
        while budget.available_permits() != budget_bytes - nar.len() {
            assert!(
                tokio::time::Instant::now() < wait,
                "holder never acquired; available={}",
                budget.available_permits()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Silence (stream open, tx alive). The ingest envelope
        // (≈2.5s shrunk) must abort the holder typed within 10×.
        let put_res = tokio::time::timeout(Duration::from_secs(25), put)
            .await
            .expect("ingest envelope must abort the stopped client")
            .unwrap();
        let status = put_res.expect_err("stopped client must be aborted typed");
        assert_eq!(
            status.code(),
            tonic::Code::ResourceExhausted,
            "typed ingest-envelope abort expected, got {status:?}"
        );
        // Permits restored ≤ bound.
        let restore = tokio::time::Instant::now() + Duration::from_secs(5);
        while budget.available_permits() != budget_bytes {
            assert!(
                tokio::time::Instant::now() < restore,
                "permits not restored: {} of {budget_bytes}",
                budget.available_permits()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(tx);
        Ok(())
    }

    // W8-D (R16 statement): the wait axis at the sole non-reservation
    // acquire chokepoint — grant-or-typed-shed within
    // BUDGET_WAIT_GRACE (batch included by the census-pinned shared
    // body), with the pool packed by a production reservation gated
    // mid-read.
    /// RED pre-fix (verbatim, run at 83e596f0c): `left: chunk acquire
    /// still pending at 3× the would-be wait grace (parked behind a
    /// production reservation; the chokepoint has no wait bound) /
    /// right: resource_exhausted ≤ grace+slack; a post-drain retry
    /// succeeds`.
    #[tokio::test]
    async fn chunk_acquire_sheds_typed_after_wait_grace() -> TestResult {
        let db = TestDb::new(&MIGRATOR).await;
        let budget_bytes = 4096usize;
        let service = Svc::new(db.pool.clone())
            .with_nar_budget(budget_bytes)
            .with_nar_ingest_envelope(shrunk_cfg());
        let budget = service.nar_bytes_budget().clone();
        let (client, _server) = spawn_store_server(service).await?;

        // Production reservation: substitute leg holds D ≈ 3500,
        // gated mid-read (default stall window — no clock interferes
        // with the gated read inside this test's horizon).
        let tid = rio_store::test_helpers::seed_tenant(&db.pool, "bw8s1-r4").await;
        let sub_content = rio_test_support::fixtures::pseudo_random_bytes(14, 3400);
        let (sub_nar, _h) = make_nar(&sub_content);
        let d = sub_nar.len();
        assert!(d < budget_bytes, "sizing pin: D < B ({d})");
        let sub_path = test_store_path("bw8s1-r4-sub");
        let gate = Arc::new(tokio::sync::Notify::new());
        let frames = vec![sub_nar[..1000].to_vec(), sub_nar[1000..].to_vec()];
        let up = spawn_mini_upstream(
            &sub_path,
            sub_nar.clone(),
            "cache.bw8r4",
            Some((frames, gate.clone())),
        )
        .await;
        seed_upstream(&db.pool, tid, &up).await;
        let sub = Arc::new(
            Substituter::new(db.pool.clone(), None)
                .with_http_client(rio_store::test_helpers::sandbox_http())
                .with_nar_bytes_budget(budget.clone()),
        );
        let leg = {
            let s = Arc::clone(&sub);
            let p = sub_path.clone();
            tokio::spawn(async move { s.try_substitute(tid, &p).await })
        };
        let wait = tokio::time::Instant::now() + Duration::from_secs(5);
        while budget.available_permits() != budget_bytes - d {
            assert!(
                tokio::time::Instant::now() < wait,
                "reservation never taken; available={}",
                budget.available_permits()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // PutPath chunk whose charge exceeds the remainder: parks,
        // then sheds typed within 10× the wait grace.
        let chunk_len = 1000usize;
        assert!(chunk_len > budget_bytes - d, "sizing pin: chunk > B − D");
        let nar_b: Vec<u8> = rio_test_support::fixtures::pseudo_random_bytes(15, chunk_len);
        let path_b = test_store_path("bw8s1-r4-up");
        let info_b = make_path_info_for_nar(&path_b, &nar_b);
        let (tx_b, rx_b, _trailer) = holder_stream(info_b.clone().into(), nar_b.clone());
        let mut put_client = client.clone();
        let put = tokio::spawn(async move { put_client.put_path(rx_b).await });
        let put_res = tokio::time::timeout(Duration::from_secs(5), put)
            .await
            .expect("chunk acquire must shed within 10× the wait grace")
            .unwrap();
        let status = put_res.expect_err("packed pool must shed the chunk acquire typed");
        assert_eq!(
            status.code(),
            tonic::Code::ResourceExhausted,
            "typed shed expected, got {status:?}"
        );
        drop(tx_b);

        // Drain: release the gated leg; it completes and frees the pool.
        gate.notify_one();
        let leg_res = tokio::time::timeout(Duration::from_secs(15), leg)
            .await
            .expect("gated leg completes after release")
            .unwrap()
            .expect("substitute leg ok");
        assert!(leg_res.is_some(), "upstream has the path");

        // A post-drain retry succeeds.
        let mut retry_client = client.clone();
        let created = put_path(&mut retry_client, info_b, nar_b).await?;
        assert!(created, "post-drain retry must succeed");
        Ok(())
    }

    // Envelope-violation red (R17, wait-grace axis): zero grace ⇒ a
    // contended chunk acquire sheds immediately, while an uncontended
    // acquire still succeeds — the knob binds the WAIT, not the grant.
    #[tokio::test]
    async fn wait_grace_binds() -> TestResult {
        let db = TestDb::new(&MIGRATOR).await;
        let budget_bytes = 4096usize;
        let service = Svc::new(db.pool.clone())
            .with_nar_budget(budget_bytes)
            .with_nar_ingest_envelope(NarIngestEnvelopeCfg {
                budget_wait_grace: Duration::ZERO,
                ..shrunk_cfg()
            });
        let budget = service.nar_bytes_budget().clone();
        let (client, _server) = spawn_store_server(service).await?;

        // Uncontended: a full upload succeeds under zero grace (the
        // acquire is granted on first poll).
        let nar_ok: Vec<u8> = rio_test_support::fixtures::pseudo_random_bytes(16, 1024);
        let info_ok = make_path_info_for_nar(&test_store_path("bw8s1-wg-ok"), &nar_ok);
        let mut c1 = client.clone();
        assert!(put_path(&mut c1, info_ok, nar_ok).await?);

        // Contended (holder pins most of the pool): immediate shed.
        let nar_hold: Vec<u8> = rio_test_support::fixtures::pseudo_random_bytes(17, 3500);
        let info_hold = make_path_info_for_nar(&test_store_path("bw8s1-wg-hold"), &nar_hold);
        let (tx, rx, _t) = holder_stream(info_hold.into(), nar_hold.clone());
        let mut c2 = client.clone();
        let put_hold = tokio::spawn(async move { c2.put_path(rx).await });
        let wait = tokio::time::Instant::now() + Duration::from_secs(5);
        while budget.available_permits() != budget_bytes - nar_hold.len() {
            assert!(
                tokio::time::Instant::now() < wait,
                "holder never acquired; available={}",
                budget.available_permits()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let nar_b: Vec<u8> = rio_test_support::fixtures::pseudo_random_bytes(18, 1000);
        let info_b = make_path_info_for_nar(&test_store_path("bw8s1-wg-b"), &nar_b);
        let (_tx_b, rx_b, _t2) = holder_stream(info_b.into(), nar_b);
        let mut c3 = client.clone();
        let put_b = tokio::spawn(async move { c3.put_path(rx_b).await });
        let res = tokio::time::timeout(Duration::from_secs(2), put_b)
            .await
            .expect("zero grace must shed a contended acquire immediately")
            .unwrap();
        let status = res.expect_err("contended acquire under zero grace sheds");
        assert_eq!(status.code(), tonic::Code::ResourceExhausted);
        drop(tx);
        let _ = put_hold.await;
        Ok(())
    }

    /// Batch holder driver: send output-0 metadata + chunk, return the
    /// open tx so the test controls the rest of the stream.
    pub fn batch_holder_stream(
        info: rio_proto::types::PathInfo,
        chunk1: Vec<u8>,
    ) -> (
        mpsc::Sender<rio_proto::types::PutPathBatchRequest>,
        ReceiverStream<rio_proto::types::PutPathBatchRequest>,
    ) {
        use rio_proto::types::PutPathBatchRequest;
        let mut info = info;
        let (tx, rx) = mpsc::channel(8);
        // Trailer-only mode: zero the metadata hash/size (the trailer
        // itself is never sent -- the client stops).
        info.nar_hash = Vec::new();
        info.nar_size = 0;
        tx.try_send(PutPathBatchRequest {
            output_index: 0,
            inner: Some(PutPathRequest {
                msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
                    info: Some(info),
                })),
            }),
        })
        .expect("fresh channel");
        tx.try_send(PutPathBatchRequest {
            output_index: 0,
            inner: Some(PutPathRequest {
                msg: Some(put_path_request::Msg::NarChunk(chunk1)),
            }),
        })
        .expect("fresh channel");
        (tx, ReceiverStream::new(rx))
    }

    // W8-F (R16 statement): batch-holder residency through the
    // production batch stream path -- with it, the holder census has
    // zero unenveloped rows and the slot theorem's quantifier domain
    // is the complete census.
    /// RED pre-fix (run at the WO-S1-1 tip a721c259d -- batch was the
    /// one remaining unbounded holder): see the commit body for the
    /// verbatim left/right transcript.
    #[tokio::test]
    async fn stopped_client_batch_releases_by_the_ingest_deadline() -> TestResult {
        let db = TestDb::new(&MIGRATOR).await;
        let budget_bytes = 64 * 1024usize;
        let service = Svc::new(db.pool.clone())
            .with_nar_budget(budget_bytes)
            .with_nar_ingest_envelope(shrunk_cfg());
        let budget = service.nar_bytes_budget().clone();
        let (client, _server) = spawn_store_server(service).await?;

        let nar: Vec<u8> = rio_test_support::fixtures::pseudo_random_bytes(19, 8192);
        let info = make_path_info_for_nar(&test_store_path("bw8s1-r5"), &nar);
        let (tx, rx) = batch_holder_stream(info.into(), nar.clone());
        let mut put_client = client.clone();
        let put = tokio::spawn(async move { put_client.put_path_batch(rx).await });
        let wait = tokio::time::Instant::now() + Duration::from_secs(5);
        while budget.available_permits() != budget_bytes - nar.len() {
            assert!(
                tokio::time::Instant::now() < wait,
                "batch holder never acquired; available={}",
                budget.available_permits()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Silence (stream open, tx alive). The ingest envelope
        // (~2.5s shrunk) must abort the holder typed within 10x.
        let res = tokio::time::timeout(Duration::from_secs(25), put).await;
        let Ok(joined) = res else {
            panic!(
                "left: batch permits retained at 10x the ingest envelope \
                 (stopped-but-connected batch client; available_permits == \
                 {} of {budget_bytes}; cross-output held_permits has no \
                 clock) / right: typed resource_exhausted abort; permits \
                 restored <= bound",
                budget.available_permits()
            );
        };
        let status = joined
            .unwrap()
            .expect_err("stopped batch client must be aborted typed");
        assert_eq!(
            status.code(),
            tonic::Code::ResourceExhausted,
            "typed ingest-envelope abort expected, got {status:?}"
        );
        // Permits restored <= bound.
        let restore = tokio::time::Instant::now() + Duration::from_secs(5);
        while budget.available_permits() != budget_bytes {
            assert!(
                tokio::time::Instant::now() < restore,
                "permits not restored: {} of {budget_bytes}",
                budget.available_permits()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(tx);
        Ok(())
    }
}
