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

    // chunks table refcounts all == 1 (first upload).
    let refcounts: Vec<(i32,)> = sqlx::query_as("SELECT refcount FROM chunks")
        .fetch_all(&s.db.pool)
        .await?;
    assert_eq!(refcounts.len(), chunk_count);
    for (rc,) in &refcounts {
        assert_eq!(*rc, 1, "first upload: all refcounts should be 1");
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
    let payload: Vec<u8> = (0u64..1024 * 1024)
        .map(|i| (i.wrapping_mul(7919).wrapping_add(5) % 251) as u8)
        .collect();
    let (nar_b, _) = make_nar(&payload);
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

    // Refcounts should all be 2 (both manifests reference every chunk).
    let refcounts: Vec<(i32,)> = sqlx::query_as("SELECT refcount FROM chunks")
        .fetch_all(&s.db.pool)
        .await?;
    for (rc,) in &refcounts {
        assert_eq!(*rc, 2, "two uploads of same content: refcount should be 2");
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
/// Validation fails → abort_upload. Verify: no manifest_data, no chunks,
/// refcounts untouched.
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

/// GT13 verify — multi-output atomicity gap. SCOPE-GATING for P0267.
///
/// Simulates a 2-output derivation where output-1's PutPath succeeds
/// and output-2's fails. Proves the architectural gap:
///
///   worker upload_all_outputs() → buffer_unordered(4) independent RPCs
///   → each PutPath is per-path transactional (put_chunked correct)
///   → NO cross-output transaction exists
///   → output-1 row stays 'complete' after output-2 fails
///
/// This test PASSES by asserting the gap (one row survives). P0267
/// adds cross-output atomicity and inverts this assertion.
///
/// No proto-level batch-put RPC exists (`rg PutPathBatch rio-proto/` = 0).
///
/// TODO(P0267): invert assertion once atomic multi-output lands.
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
        "GT13-OUTCOME: real — output-1 survives output-2 failure. \
         P0267 scope: full sqlx::Transaction wrap, NOT tracey-annotate-only."
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
