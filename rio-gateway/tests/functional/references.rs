//! Reference chain through REAL store.
//!
//! Port of Lix `store/test_add.py::TestNix3AddPath::test_nix3_rec_some_references`
//! semantics: add paths with non-empty `references` field.
//!
//! Every `wire_opcodes` test sends `references: NO_STRINGS`. Real nix
//! workflows are reference chains: `hello.drv` → `glibc`, `bash`. This
//! is the first test to send a real chain through the gateway.
//!
//! rio-store stores references in `narinfo."references"` (PG `TEXT[]`).
//! No separate edges table — `wopQueryPathInfo` reads them back from
//! that column. The white-box assertions here prove the array actually
//! lands in PG, not just echoes through the wire.

use super::*;

// r[verify gw.opcode.query-missing]
/// Chain: C → B → A. Add in order (leaf first), query each back,
/// verify references round-trip. Then `wopQueryMissing` against the
/// present set — everything is already there, so all three response
/// sets are empty.
#[tokio::test(flavor = "multi_thread")]
async fn references_chain_query_missing() -> TestResult {
    let mut stack = RioStack::new_ready().await?;

    let path_a = test_store_path("chain-a");
    let path_b = test_store_path("chain-b");
    let path_c = test_store_path("chain-c");

    // A: leaf (no refs).
    let (nar_a, hash_a) = make_nar(b"leaf node content");
    add_to_store_nar(&mut stack.stream, &path_a, &nar_a, hash_a, &[]).await?;

    // B: references A. Content mentions path_a (as real NARs do — store
    // path strings embedded in binaries/scripts are how refscan finds them).
    let b_content = format!("depends on {path_a}");
    let (nar_b, hash_b) = make_nar(b_content.as_bytes());
    add_to_store_nar(&mut stack.stream, &path_b, &nar_b, hash_b, &[&path_a]).await?;

    // C: references B (transitively A via B's references).
    let c_content = format!("depends on {path_b}");
    let (nar_c, hash_c) = make_nar(c_content.as_bytes());
    add_to_store_nar(&mut stack.stream, &path_c, &nar_c, hash_c, &[&path_b]).await?;

    // --- Query B back, verify references field ---
    wire_send!(&mut stack.stream;
        u64: 26,                           // wopQueryPathInfo
        string: &path_b,
    );
    drain_stderr_until_last(&mut stack.stream).await?;
    let valid = wire::read_bool(&mut stack.stream).await?;
    assert!(valid, "B should be valid");
    let _deriver = wire::read_string(&mut stack.stream).await?;
    let _nar_hash = wire::read_string(&mut stack.stream).await?;
    let refs_b = wire::read_strings(&mut stack.stream).await?;
    let _regtime = wire::read_u64(&mut stack.stream).await?;
    let _nar_size = wire::read_u64(&mut stack.stream).await?;
    let _ultimate = wire::read_bool(&mut stack.stream).await?;
    let _sigs = wire::read_strings(&mut stack.stream).await?;
    let _ca = wire::read_string(&mut stack.stream).await?;

    assert_eq!(
        refs_b,
        vec![path_a.clone()],
        "B's references should be [A] — round-tripped through PG narinfo.\"references\" TEXT[]"
    );

    // --- wopQueryMissing for all three — everything present ---
    // Send as opaque paths (not drv!output). With everything present,
    // the store's FindMissingPaths returns an empty missing set, so
    // all three response sets should be empty.
    wire_send!(&mut stack.stream;
        u64: 40,                           // wopQueryMissing
        strings: &[path_a.as_str(), path_b.as_str(), path_c.as_str()],
    );
    drain_stderr_until_last(&mut stack.stream).await?;

    let will_build = wire::read_strings(&mut stack.stream).await?;
    let will_substitute = wire::read_strings(&mut stack.stream).await?;
    let unknown = wire::read_strings(&mut stack.stream).await?;
    let _download_size = wire::read_u64(&mut stack.stream).await?;
    let _nar_size = wire::read_u64(&mut stack.stream).await?;

    // wire_opcodes/opcodes_read.rs tests this against MockStore (always
    // empty missing set). Here the empty sets are REAL: FindMissingPaths
    // hit PG, found all three in narinfo, returned no missing paths.
    assert!(will_build.is_empty(), "all present → nothing to build");
    assert!(will_substitute.is_empty(), "no substituter support");
    assert!(unknown.is_empty(), "all present → nothing unknown");

    // --- White-box: references column in PG has the edges ---
    // This is the exit criterion proof. narinfo."references" is a TEXT[]
    // (not a separate table — see migrations/002_store.sql:27).
    let refs_b_db: Vec<String> =
        sqlx::query_scalar(r#"SELECT "references" FROM narinfo WHERE store_path = $1"#)
            .bind(&path_b)
            .fetch_one(&stack.db.pool)
            .await?;
    assert_eq!(refs_b_db, vec![path_a.clone()], "B→A edge in PG");

    let refs_c_db: Vec<String> =
        sqlx::query_scalar(r#"SELECT "references" FROM narinfo WHERE store_path = $1"#)
            .bind(&path_c)
            .fetch_one(&stack.db.pool)
            .await?;
    assert_eq!(refs_c_db, vec![path_b.clone()], "C→B edge in PG");

    // Count total edges across all paths. A has 0, B has 1, C has 1.
    let total_edges: i64 = sqlx::query_scalar(
        r#"SELECT coalesce(sum(array_length("references", 1)), 0) FROM narinfo"#,
    )
    .fetch_one(&stack.db.pool)
    .await?;
    assert_eq!(total_edges, 2, "2 edges total: B→A, C→B");

    stack.finish().await;
    Ok(())
}

/// Add B with references=[A] when A does NOT exist. rio-store currently
/// accepts this (no dangling-reference check on put — references are
/// declarative metadata, not FK-enforced). Documents the current behavior;
/// if a future `r[store.integrity.*]` spec requires rejection, this test
/// inverts.
///
/// Lix's behavior (observed): also accepts. The reference check happens
/// at GC time (can't delete A if B references it), not at add time.
#[tokio::test(flavor = "multi_thread")]
async fn add_with_dangling_reference_accepted() -> TestResult {
    let mut stack = RioStack::new_ready().await?;

    let path_a = test_store_path("dangling-target"); // NOT added
    let path_b = test_store_path("dangling-source");

    let b_content = format!("references nonexistent {path_a}");
    let (nar_b, hash_b) = make_nar(b_content.as_bytes());
    // references=[A] but A is not in the store.
    add_to_store_nar(&mut stack.stream, &path_b, &nar_b, hash_b, &[&path_a]).await?;

    // B is valid even though A doesn't exist. The reference is stored
    // verbatim — it's metadata, not a foreign key.
    wire_send!(&mut stack.stream; u64: 26, string: &path_b);
    drain_stderr_until_last(&mut stack.stream).await?;
    let valid = wire::read_bool(&mut stack.stream).await?;
    assert!(
        valid,
        "B should be valid despite dangling ref (current behavior)"
    );
    let _deriver = wire::read_string(&mut stack.stream).await?;
    let _nar_hash = wire::read_string(&mut stack.stream).await?;
    let refs = wire::read_strings(&mut stack.stream).await?;
    let _regtime = wire::read_u64(&mut stack.stream).await?;
    let _nar_size = wire::read_u64(&mut stack.stream).await?;
    let _ultimate = wire::read_bool(&mut stack.stream).await?;
    let _sigs = wire::read_strings(&mut stack.stream).await?;
    let _ca = wire::read_string(&mut stack.stream).await?;

    assert_eq!(
        refs,
        vec![path_a.clone()],
        "dangling reference still stored verbatim"
    );

    stack.finish().await;
    Ok(())
}
