//! Add → query roundtrip with REAL hash verification.
//!
//! Port of Lix `store/test_add.py::test_hash` semantics: add a path,
//! query it back, assert the nar\_hash matches what we sent — but
//! through PG + `StoreServiceImpl::put_path`'s `validate_nar_digest`
//! call, not `MockStore`'s `HashMap::insert`.
//!
//! The `wire_opcodes/opcodes_write.rs::test_add_to_store_nar_accepts_valid`
//! version sends the right hash and asserts `put_calls[0].nar_hash` — but
//! `MockStore` accepts ANY hash. `add_with_wrong_hash_rejected` below is
//! the difference made concrete.

use super::*;

// r[verify store.integrity.verify-on-put]
/// Add with CORRECT hash → query returns it. Real store computes sha256
/// of received NAR bytes and compares against the declared hash.
#[tokio::test(flavor = "multi_thread")]
async fn add_then_query_path_info_real_hash() -> TestResult {
    let mut stack = RioStack::new_ready().await?;
    let path = test_store_path("func-roundtrip");
    let (nar, nar_hash) = make_nar(b"hello functional tier");

    // wopAddToStoreNar with the CORRECT hash. Real store verifies it
    // (validate_nar_digest at rio-store/src/validate.rs:155).
    add_to_store_nar(&mut stack.stream, &path, &nar, nar_hash, &[]).await?;

    // wopQueryPathInfo — round-trip through real PG, real narinfo table.
    wire_send!(&mut stack.stream;
        u64: 26,                           // wopQueryPathInfo
        string: &path,
    );
    drain_stderr_until_last(&mut stack.stream).await?;

    let valid = wire::read_bool(&mut stack.stream).await?;
    assert!(valid, "path should be valid after add");
    let _deriver = wire::read_string(&mut stack.stream).await?;
    let nar_hash_hex = wire::read_string(&mut stack.stream).await?;
    assert_eq!(
        nar_hash_hex,
        hex::encode(nar_hash),
        "queried nar_hash should match what we sent — and it's been through \
         PG narinfo.nar_hash, not a HashMap echo"
    );
    let _refs = wire::read_strings(&mut stack.stream).await?;
    let _regtime = wire::read_u64(&mut stack.stream).await?;
    let nar_size = wire::read_u64(&mut stack.stream).await?;
    assert_eq!(nar_size, nar.len() as u64);
    let _ultimate = wire::read_bool(&mut stack.stream).await?;
    let _sigs = wire::read_strings(&mut stack.stream).await?;
    let _ca = wire::read_string(&mut stack.stream).await?;

    // White-box: narinfo row actually exists in PG with the right hash.
    // This is the proof the store isn't short-circuiting — the wire
    // response above COULD theoretically come from an in-memory cache
    // if someone added one to the gateway. The PG row can't lie.
    let (db_path, db_hash): (String, Vec<u8>) =
        sqlx::query_as("SELECT store_path, nar_hash FROM narinfo WHERE store_path = $1")
            .bind(&path)
            .fetch_one(&stack.db.pool)
            .await?;
    assert_eq!(db_path, path);
    assert_eq!(
        db_hash,
        nar_hash.to_vec(),
        "PG narinfo.nar_hash should match"
    );

    let manifest_count: i64 = sqlx::query_scalar("SELECT count(*) FROM manifests")
        .fetch_one(&stack.db.pool)
        .await?;
    assert_eq!(manifest_count, 1, "exactly one manifest row");

    stack.finish().await;
    Ok(())
}

// r[verify store.integrity.verify-on-put]
/// Add with WRONG hash → `STDERR_ERROR`. This is the MockStore gap made
/// concrete: `MockStore` has no hash verification; `StoreServiceImpl`'s
/// `validate_nar_digest` rejects the mismatch.
///
/// Exit criterion: this test FAILS if `StoreServiceImpl::put_path` is
/// patched to skip verification — the mutation-test proof that this tier
/// sees what `MockStore` can't.
#[tokio::test(flavor = "multi_thread")]
async fn add_with_wrong_hash_rejected() -> TestResult {
    let mut stack = RioStack::new_ready().await?;
    let path = test_store_path("func-wronghash");
    let (nar, _actual_hash) = make_nar(b"integrity test content");

    // LIE about the hash. MockStore accepts this (no verification).
    // Real store: sha256(nar) ≠ [0;32] → validate_nar_digest fails →
    // PutPath returns InvalidArgument → gateway sends STDERR_ERROR.
    let wrong_hash = [0u8; 32];

    wire_send!(&mut stack.stream;
        u64: 39,                           // wopAddToStoreNar
        string: &path,
        string: "",                        // deriver
        string: &hex::encode(wrong_hash),  // narHash — WRONG
        strings: wire::NO_STRINGS,
        u64: 0,
        u64: nar.len() as u64,
        bool: false,
        strings: wire::NO_STRINGS,
        string: "",
        bool: false, bool: true,
        framed: &nar,
    );

    let err = drain_stderr_expecting_error(&mut stack.stream).await?;
    // rio-store/src/validate.rs:170 — "NAR hash mismatch: declared {}, computed {}"
    // Gateway wraps store's gRPC error; the substring survives.
    assert!(
        err.message.contains("hash mismatch") || err.message.contains("mismatch"),
        "expected hash-mismatch error from real store, got: {}",
        err.message
    );

    // White-box: NO narinfo row (rejected before commit).
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM narinfo WHERE store_path = $1")
        .bind(&path)
        .fetch_one(&stack.db.pool)
        .await?;
    assert_eq!(count, 0, "rejected add should leave no narinfo row");

    Ok(())
}

// NOTE: a wrong-nar_size companion test isn't meaningfully possible at
// this tier. The gateway's framed-stream read enforces `nar_size` bytes
// (EOF if short) BEFORE the store's `validate_nar_digest` runs. Size
// lies are caught by the gateway; only hash lies reach the store.
// `wire_opcodes/opcodes_write.rs::test_add_multiple_to_store_truncated_nar`
// already covers the gateway-level check.
