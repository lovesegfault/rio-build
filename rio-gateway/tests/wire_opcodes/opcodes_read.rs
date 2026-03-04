use super::*;

// ===========================================================================
// Opcode tests: IsValidPath (1)
// ===========================================================================

#[tokio::test]
async fn test_is_valid_path_exists() -> anyhow::Result<()> {
    let mut h = TestHarness::setup().await?;
    let (nar, hash) = make_nar(b"hello");
    h.store.seed(make_path_info(TEST_PATH_A, &nar, hash), nar);

    wire_send!(&mut h.stream;
        u64: 1,                             // wopIsValidPath
        string: TEST_PATH_A,
    );

    drain_stderr_until_last(&mut h.stream).await?;
    let valid = wire::read_bool(&mut h.stream).await?;
    assert!(valid, "seeded path should be valid");

    h.finish().await;
    Ok(())
}

#[tokio::test]
async fn test_is_valid_path_missing() -> anyhow::Result<()> {
    let mut h = TestHarness::setup().await?;

    wire_send!(&mut h.stream;
        u64: 1,                             // wopIsValidPath
        string: TEST_PATH_MISSING,
    );

    drain_stderr_until_last(&mut h.stream).await?;
    let valid = wire::read_bool(&mut h.stream).await?;
    assert!(!valid, "missing path should be invalid");

    h.finish().await;
    Ok(())
}

// ===========================================================================
// Opcode tests: EnsurePath (10)
// ===========================================================================

#[tokio::test]
async fn test_ensure_path_exists() -> anyhow::Result<()> {
    let mut h = TestHarness::setup().await?;
    let (nar, hash) = make_nar(b"ensure");
    h.store.seed(make_path_info(TEST_PATH_A, &nar, hash), nar);

    wire_send!(&mut h.stream;
        u64: 10,                            // wopEnsurePath
        string: TEST_PATH_A,
    );

    drain_stderr_until_last(&mut h.stream).await?;
    let result = wire::read_u64(&mut h.stream).await?;
    assert_eq!(result, 1, "EnsurePath should return 1 (success)");

    h.finish().await;
    Ok(())
}

/// Phase 2a: EnsurePath is a stub that always returns success regardless of
/// whether the path exists. It reads the path argument and returns 1.
#[tokio::test]
async fn test_ensure_path_stub_always_succeeds() -> anyhow::Result<()> {
    let mut h = TestHarness::setup().await?;

    wire_send!(&mut h.stream;
        u64: 10,                            // wopEnsurePath
        string: TEST_PATH_MISSING,
    );

    // Phase 2a stub: always STDERR_LAST + 1, even for missing paths.
    drain_stderr_until_last(&mut h.stream).await?;
    let result = wire::read_u64(&mut h.stream).await?;
    assert_eq!(result, 1, "EnsurePath stub returns 1 unconditionally");

    h.finish().await;
    Ok(())
}

// ===========================================================================
// Opcode tests: QueryPathInfo (26)
// ===========================================================================

#[tokio::test]
async fn test_query_path_info_exists() -> anyhow::Result<()> {
    let mut h = TestHarness::setup().await?;
    let (nar, hash) = make_nar(b"pathinfo");
    let info = make_path_info(TEST_PATH_A, &nar, hash);
    h.store.seed(info, nar.clone());

    wire_send!(&mut h.stream;
        u64: 26,                            // wopQueryPathInfo
        string: TEST_PATH_A,
    );

    drain_stderr_until_last(&mut h.stream).await?;

    // Response: bool(valid) + if valid: deriver, hex_nar_hash, refs, regtime, nar_size, ultimate, sigs, ca
    let valid = wire::read_bool(&mut h.stream).await?;
    assert!(valid, "path should be valid");
    let deriver = wire::read_string(&mut h.stream).await?;
    assert_eq!(deriver, "");
    let nar_hash_hex = wire::read_string(&mut h.stream).await?;
    assert_eq!(nar_hash_hex, hex::encode(hash), "nar hash should match");
    let refs = wire::read_strings(&mut h.stream).await?;
    assert!(refs.is_empty());
    let _regtime = wire::read_u64(&mut h.stream).await?;
    let nar_size = wire::read_u64(&mut h.stream).await?;
    assert_eq!(nar_size, nar.len() as u64);
    let _ultimate = wire::read_bool(&mut h.stream).await?;
    let _sigs = wire::read_strings(&mut h.stream).await?;
    let _ca = wire::read_string(&mut h.stream).await?;

    h.finish().await;
    Ok(())
}

#[tokio::test]
async fn test_query_path_info_missing_returns_invalid() -> anyhow::Result<()> {
    let mut h = TestHarness::setup().await?;

    wire_send!(&mut h.stream;
        u64: 26,                            // wopQueryPathInfo
        string: TEST_PATH_MISSING,
    );

    // QueryPathInfo for missing path returns STDERR_LAST + valid=false
    // (not STDERR_ERROR — the Nix protocol uses valid=false here).
    drain_stderr_until_last(&mut h.stream).await?;
    let valid = wire::read_bool(&mut h.stream).await?;
    assert!(!valid, "missing path should return valid=false");

    h.finish().await;
    Ok(())
}

// ===========================================================================
// Opcode tests: QueryPathFromHashPart (29)
// ===========================================================================

#[tokio::test]
async fn test_query_path_from_hash_part_found() -> anyhow::Result<()> {
    let mut h = TestHarness::setup().await?;
    let (nar, hash) = make_nar(b"hashpart");
    h.store.seed(make_path_info(TEST_PATH_A, &nar, hash), nar);

    // Hash part is the 32-char basename prefix
    let hash_part = "00000000000000000000000000000000";

    wire_send!(&mut h.stream; u64: 29, string: hash_part); // wopQueryPathFromHashPart

    drain_stderr_until_last(&mut h.stream).await?;
    let result = wire::read_string(&mut h.stream).await?;
    assert_eq!(result, TEST_PATH_A, "should return the full store path");

    h.finish().await;
    Ok(())
}

#[tokio::test]
async fn test_query_path_from_hash_part_not_found() -> anyhow::Result<()> {
    let mut h = TestHarness::setup().await?;

    wire_send!(&mut h.stream;
        u64: 29,                            // wopQueryPathFromHashPart
        string: "11111111111111111111111111111111",
    );

    drain_stderr_until_last(&mut h.stream).await?;
    let result = wire::read_string(&mut h.stream).await?;
    assert_eq!(result, "", "not found should return empty string");

    h.finish().await;
    Ok(())
}

// ===========================================================================
// Opcode tests: AddTempRoot (11)
// ===========================================================================

#[tokio::test]
async fn test_add_temp_root() -> anyhow::Result<()> {
    let mut h = TestHarness::setup().await?;

    wire_send!(&mut h.stream; u64: 11, string: TEST_PATH_A); // wopAddTempRoot

    drain_stderr_until_last(&mut h.stream).await?;
    let result = wire::read_u64(&mut h.stream).await?;
    assert_eq!(result, 1, "AddTempRoot always returns 1 (success)");

    h.finish().await;
    Ok(())
}

// ===========================================================================
// Opcode tests: NarFromPath (38)
// ===========================================================================

#[tokio::test]
async fn test_nar_from_path_streams_chunks() -> anyhow::Result<()> {
    let mut h = TestHarness::setup().await?;
    let (nar, hash) = make_nar(b"nar-from-path content");
    h.store
        .seed(make_path_info(TEST_PATH_A, &nar, hash), nar.clone());

    wire_send!(&mut h.stream;
        u64: 38,                            // wopNarFromPath
        string: TEST_PATH_A,
    );

    // NarFromPath: STDERR_LAST first, then RAW NAR bytes (NOT STDERR_WRITE).
    // Nix client: processStderr(ex) with no sink → copyNAR(from, sink).
    let msgs = drain_stderr_until_last(&mut h.stream).await?;
    assert!(
        msgs.is_empty(),
        "no STDERR messages expected before STDERR_LAST; got: {msgs:?}"
    );
    // Read the raw NAR bytes that follow STDERR_LAST.
    let mut received = vec![0u8; nar.len()];
    tokio::io::AsyncReadExt::read_exact(&mut h.stream, &mut received).await?;
    assert_eq!(received, nar, "received NAR bytes should match seeded NAR");

    h.finish().await;
    Ok(())
}

#[tokio::test]
async fn test_nar_from_path_missing_returns_error() -> anyhow::Result<()> {
    let mut h = TestHarness::setup().await?;

    wire_send!(&mut h.stream;
        u64: 38,                            // wopNarFromPath
        string: TEST_PATH_MISSING,
    );

    let err = drain_stderr_expecting_error(&mut h.stream).await?;
    assert!(!err.message.is_empty(), "error message should be non-empty");

    h.finish().await;
    Ok(())
}

#[tokio::test]
async fn test_nar_from_path_invalid_path_returns_error() -> anyhow::Result<()> {
    let mut h = TestHarness::setup().await?;

    // Send a string that fails StorePath::parse (no /nix/store/ prefix).
    wire_send!(&mut h.stream;
        u64: 38,                            // wopNarFromPath
        string: "not-a-valid-store-path",
    );

    let err = drain_stderr_expecting_error(&mut h.stream).await?;
    assert!(
        err.message.contains("invalid store path"),
        "error should mention invalid store path, got: {}",
        err.message
    );

    // Session must terminate after STDERR_ERROR (handler returns Err now).
    h.finish().await;
    Ok(())
}

/// QueryRealisation: malformed id → empty set (soft-fail, same as the old
/// stub). "abc" is 3 hex chars, not 64.
#[tokio::test]
async fn test_query_realisation_malformed_id_returns_empty() -> anyhow::Result<()> {
    let mut h = TestHarness::setup().await?;

    wire_send!(&mut h.stream;
        u64: 43,                            // wopQueryRealisation
        string: "sha256:abc!out",
    );

    drain_stderr_until_last(&mut h.stream).await?;
    let count = wire::read_u64(&mut h.stream).await?;
    assert_eq!(count, 0, "malformed id should return empty set");

    h.finish().await;
    Ok(())
}

/// QueryRealisation: valid id but not in MockStore → empty set (cache miss).
#[tokio::test]
async fn test_query_realisation_miss_returns_empty() -> anyhow::Result<()> {
    let mut h = TestHarness::setup().await?;

    let drv_hash_hex = "bb".repeat(32);
    wire_send!(&mut h.stream;
        u64: 43,
        string: &format!("sha256:{drv_hash_hex}!out"),
    );

    drain_stderr_until_last(&mut h.stream).await?;
    let count = wire::read_u64(&mut h.stream).await?;
    assert_eq!(count, 0, "cache miss should return empty set");

    h.finish().await;
    Ok(())
}

/// QueryRealisation: hit. Returns count=1 + Realisation JSON with the
/// outPath from MockStore. This is the actual cache-hit path that makes
/// CA derivations skip rebuilds.
#[tokio::test]
async fn test_query_realisation_hit_returns_json() -> anyhow::Result<()> {
    let mut h = TestHarness::setup().await?;

    let drv_hash_hex = "cc".repeat(32);
    let drv_hash = hex::decode(&drv_hash_hex)?;
    let out_path = "/nix/store/11111111111111111111111111111111-ca-hit";

    // Seed MockStore's realisations map directly.
    h.store.realisations.write().unwrap().insert(
        (drv_hash.clone(), "out".into()),
        rio_proto::types::Realisation {
            drv_hash,
            output_name: "out".into(),
            output_path: out_path.into(),
            output_hash: vec![0xDDu8; 32],
            signatures: vec!["sig:seeded".into()],
        },
    );

    let id = format!("sha256:{drv_hash_hex}!out");
    wire_send!(&mut h.stream;
        u64: 43,
        string: &id,
    );

    drain_stderr_until_last(&mut h.stream).await?;
    let count = wire::read_u64(&mut h.stream).await?;
    assert_eq!(count, 1, "cache hit should return count=1");

    let json_str = wire::read_string(&mut h.stream).await?;
    let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
    // The id echoes what we sent (gateway reconstructs it from the same
    // string, not from the store response).
    assert_eq!(parsed["id"], id);
    // outPath comes from MockStore — this is the payload that matters for
    // the cache hit.
    assert_eq!(parsed["outPath"], out_path);
    // Sigs roundtrip.
    assert_eq!(parsed["signatures"][0], "sig:seeded");
    // dependentRealisations always empty (phase5 fills this).
    assert!(
        parsed["dependentRealisations"]
            .as_object()
            .is_some_and(|o| o.is_empty()),
        "dependentRealisations should be empty object"
    );

    h.finish().await;
    Ok(())
}

/// wopQueryMissing (40): reads strings(paths), writes willBuild + willSubstitute
/// + unknown + downloadSize + narSize.
#[tokio::test]
async fn test_query_missing_reports_will_build() -> anyhow::Result<()> {
    let mut h = TestHarness::setup().await?;

    // Don't seed the .drv: handler filters paths whose store_path is NOT in
    // the missing set. A Built path's store_path() is the .drv; if the .drv
    // is missing from store, the handler reports it in willBuild.
    let drv_path = "/nix/store/00000000000000000000000000000000-test.drv";

    wire_send!(&mut h.stream;
        u64: 40,                            // wopQueryMissing
        strings: &[format!("{drv_path}!out")],
    );

    drain_stderr_until_last(&mut h.stream).await?;

    let will_build = wire::read_strings(&mut h.stream).await?;
    let will_substitute = wire::read_strings(&mut h.stream).await?;
    let unknown = wire::read_strings(&mut h.stream).await?;
    let download_size = wire::read_u64(&mut h.stream).await?;
    let nar_size = wire::read_u64(&mut h.stream).await?;

    assert_eq!(
        will_build.len(),
        1,
        "missing Built .drv should be in willBuild"
    );
    assert!(will_build[0].contains(drv_path));
    assert!(will_substitute.is_empty(), "Phase 2a: no substitutes");
    assert!(unknown.is_empty());
    assert_eq!(download_size, 0);
    assert_eq!(nar_size, 0);

    h.finish().await;
    Ok(())
}

/// wopQueryDerivationOutputMap (41): reads drv path, writes count +
/// (name, path) pairs. Error path: missing .drv in store.
#[tokio::test]
async fn test_query_derivation_output_map_missing_drv() -> anyhow::Result<()> {
    let mut h = TestHarness::setup().await?;

    wire_send!(&mut h.stream;
        u64: 41,
        string: "/nix/store/11111111111111111111111111111111-missing.drv",
    );

    // Missing .drv: STDERR_ERROR
    let err = drain_stderr_expecting_error(&mut h.stream).await?;
    assert!(!err.message.is_empty());

    h.finish().await;
    Ok(())
}

/// wopQueryDerivationOutputMap (41) happy path: .drv is in store, returns
/// output name -> path map.
#[tokio::test]
async fn test_query_derivation_output_map_found() -> anyhow::Result<()> {
    let mut h = TestHarness::setup().await?;

    let drv_text = r#"Derive([("out","/nix/store/zzz-output","",""),("dev","/nix/store/yyy-dev","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("out","/nix/store/zzz-output")])"#;
    let (drv_nar, drv_hash) = make_nar(drv_text.as_bytes());
    let drv_path = "/nix/store/00000000000000000000000000000000-test.drv";
    h.store
        .seed(make_path_info(drv_path, &drv_nar, drv_hash), drv_nar);

    wire_send!(&mut h.stream; u64: 41, string: drv_path);

    drain_stderr_until_last(&mut h.stream).await?;

    let count = wire::read_u64(&mut h.stream).await?;
    assert_eq!(count, 2, "two outputs (out, dev)");
    let mut outputs = std::collections::HashMap::new();
    for _ in 0..count {
        let name = wire::read_string(&mut h.stream).await?;
        let path = wire::read_string(&mut h.stream).await?;
        outputs.insert(name, path);
    }
    assert_eq!(
        outputs.get("out").expect("output present"),
        "/nix/store/zzz-output"
    );
    assert_eq!(
        outputs.get("dev").expect("output present"),
        "/nix/store/yyy-dev"
    );

    h.finish().await;
    Ok(())
}

// ===========================================================================
// Additional coverage: K2/K3 from phase 2a review
// ===========================================================================
//
// Note on error-path behavior discovered during test development:
// Many opcodes GRACEFULLY handle invalid store paths instead of sending
// STDERR_ERROR. This is intentional Nix-compatible behavior:
//   - IsValidPath (1):   invalid path -> returns false (not error)
//   - EnsurePath (10):   invalid path -> ignored, returns success
//   - AddTempRoot (11):  invalid path -> ignored, returns success
//   - QueryPathInfo (26): invalid path -> returns valid=false (not error)
//   - SetOptions (19):   no error path (fixed-width fields only)
// These opcodes DO have STDERR_ERROR paths for gRPC failures (store unreachable),
// but those are harder to trigger in the mock harness and are covered by the
// timeout integration path rather than byte-level tests.
//
// Opcodes with true STDERR_ERROR paths for client-side invalid input:
//   - NarFromPath (38):  invalid path -> error (tested above)
//   - AddToStoreNar (39): invalid path, oversized nar_size -> error (tested below)
//   - BuildDerivation (36): parse failure -> connection drop (wire read error)

/// QueryValidPaths (31) happy path: returns paths present in the mock store.
#[tokio::test]
async fn test_query_valid_paths_filters_missing() -> anyhow::Result<()> {
    let mut h = TestHarness::setup().await?;
    let (nar, hash) = make_nar(b"qvp");
    h.store.seed(make_path_info(TEST_PATH_A, &nar, hash), nar);
    // TEST_PATH_MISSING is not seeded.

    wire_send!(&mut h.stream;
        u64: 31,                            // wopQueryValidPaths
        strings: &[TEST_PATH_A, TEST_PATH_MISSING],
        bool: false,                        // substitute
    );

    drain_stderr_until_last(&mut h.stream).await?;
    let valid = wire::read_strings(&mut h.stream).await?;
    assert_eq!(
        valid,
        vec![TEST_PATH_A.to_string()],
        "only seeded path should be valid"
    );

    h.finish().await;
    Ok(())
}

/// QueryValidPaths with empty input returns empty output.
#[tokio::test]
async fn test_query_valid_paths_empty() -> anyhow::Result<()> {
    let mut h = TestHarness::setup().await?;

    wire_send!(&mut h.stream;
        u64: 31,
        strings: wire::NO_STRINGS,
        bool: false,
    );

    drain_stderr_until_last(&mut h.stream).await?;
    let valid = wire::read_strings(&mut h.stream).await?;
    assert!(valid.is_empty());

    h.finish().await;
    Ok(())
}

/// IsValidPath with unparseable path returns false (not STDERR_ERROR).
/// This documents the graceful-degradation behavior for Nix compatibility.
#[tokio::test]
async fn test_is_valid_path_garbage_returns_false() -> anyhow::Result<()> {
    let mut h = TestHarness::setup().await?;

    wire_send!(&mut h.stream;
        u64: 1,
        string: "garbage-not-a-store-path",
    );

    // Should NOT receive STDERR_ERROR — just STDERR_LAST + false.
    drain_stderr_until_last(&mut h.stream).await?;
    let valid = wire::read_bool(&mut h.stream).await?;
    assert!(!valid, "garbage path should return false, not error");

    h.finish().await;
    Ok(())
}
