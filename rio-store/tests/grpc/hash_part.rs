//! QueryPathFromHashPart + AddSignatures.

use super::*;

// ===========================================================================
// QueryPathFromHashPart + AddSignatures
// ===========================================================================

use rio_proto::types::{AddSignaturesRequest, QueryPathFromHashPartRequest};

/// QueryPathFromHashPart: put a path, look it up by its 32-char hash part.
#[tokio::test]
async fn test_query_path_from_hash_part_found() -> TestResult {
    let mut s = StoreSession::new().await?;

    // Store path with a known nixbase32 hash-part. All-'a' is valid nixbase32.
    let hash_part = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let store_path = format!("/nix/store/{hash_part}-hashpart-test");
    let nar = make_nar(b"hash part test").0;
    let info = make_path_info_for_nar(&store_path, &nar);
    put_path(&mut s.client, info, nar).await?;

    let resp = s
        .client
        .query_path_from_hash_part(QueryPathFromHashPartRequest {
            hash_part: hash_part.into(),
        })
        .await
        .context("should find by hash part")?;
    assert_eq!(resp.into_inner().store_path, store_path);

    Ok(())
}

/// QueryPathFromHashPart: unknown hash → NOT_FOUND.
#[tokio::test]
async fn test_query_path_from_hash_part_not_found() -> TestResult {
    let mut s = StoreSession::new().await?;

    let result = s
        .client
        .query_path_from_hash_part(QueryPathFromHashPartRequest {
            hash_part: "00000000000000000000000000000000".into(),
        })
        .await;
    let status = result.expect_err("should be NOT_FOUND");
    assert_eq!(status.code(), tonic::Code::NotFound);

    Ok(())
}

/// QueryPathFromHashPart: validation rejects wrong length and non-nixbase32.
/// These flow into a LIKE pattern — without validation, `%` would be an
/// injection. The test proves the validator runs BEFORE the query.
#[tokio::test]
async fn test_query_path_from_hash_part_validation() -> TestResult {
    let mut s = StoreSession::new().await?;

    // Wrong length (INVALID_ARGUMENT, not NOT_FOUND).
    let short = s
        .client
        .query_path_from_hash_part(QueryPathFromHashPartRequest {
            hash_part: "short".into(),
        })
        .await
        .expect_err("short should be invalid");
    assert_eq!(short.code(), tonic::Code::InvalidArgument);
    assert!(
        short.message().contains("32 chars"),
        "got: {}",
        short.message()
    );

    // LIKE-injection attempt: `%` is not nixbase32. If validation didn't
    // catch this, the LIKE `/nix/store/%%%-%` would match EVERYTHING.
    // Getting INVALID_ARGUMENT here (not a spurious match or NOT_FOUND)
    // proves the charset check fires.
    let inject = s
        .client
        .query_path_from_hash_part(QueryPathFromHashPartRequest {
            hash_part: "%".repeat(32),
        })
        .await
        .expect_err("% should be invalid");
    assert_eq!(inject.code(), tonic::Code::InvalidArgument);
    assert!(
        inject.message().contains("nixbase32"),
        "got: {}",
        inject.message()
    );

    Ok(())
}

/// AddSignatures: append, re-query, verify sigs are persisted.
#[tokio::test]
async fn test_add_signatures_roundtrip() -> TestResult {
    let mut s = StoreSession::new().await?;

    let store_path = test_store_path("addsig-roundtrip");
    let nar = make_nar(b"addsig test").0;
    let info = make_path_info_for_nar(&store_path, &nar);
    put_path(&mut s.client, info, nar).await?;

    // Append two sigs.
    let sigs = vec![
        "cache.example.org-1:SIGNATURE_A".to_string(),
        "cache.example.org-1:SIGNATURE_B".to_string(),
    ];
    s.client
        .add_signatures(AddSignaturesRequest {
            store_path: store_path.clone(),
            signatures: sigs.clone(),
        })
        .await
        .context("add_signatures should succeed")?;

    // Re-query: sigs persisted. Compare as sets — the server dedups via
    // `array(SELECT DISTINCT unnest(...))`, which doesn't preserve insertion
    // order. Signature order is immaterial to Nix clients (each sig is
    // verified independently against trusted-public-keys).
    let info = s
        .client
        .query_path_info(QueryPathInfoRequest {
            store_path: store_path.clone(),
        })
        .await?
        .into_inner();
    let got: std::collections::BTreeSet<_> = info.signatures.iter().cloned().collect();
    let want: std::collections::BTreeSet<_> = sigs.iter().cloned().collect();
    assert_eq!(got, want, "signatures should be persisted (order-agnostic)");

    // Append a third — verifies dedup'd concat grows, not replaces.
    // Also re-append SIGNATURE_A: dedup should collapse it (len stays 3).
    s.client
        .add_signatures(AddSignaturesRequest {
            store_path: store_path.clone(),
            signatures: vec![
                "cache.example.org-1:SIGNATURE_C".to_string(),
                "cache.example.org-1:SIGNATURE_A".to_string(), // duplicate → deduped
            ],
        })
        .await?;
    let info = s
        .client
        .query_path_info(QueryPathInfoRequest { store_path })
        .await?
        .into_inner();
    assert_eq!(
        info.signatures.len(),
        3,
        "third sig appended, duplicate A deduped"
    );
    assert!(
        info.signatures
            .contains(&"cache.example.org-1:SIGNATURE_C".to_string()),
        "third sig present"
    );

    Ok(())
}

/// AddSignatures on unknown path → NOT_FOUND.
#[tokio::test]
async fn test_add_signatures_not_found() -> TestResult {
    let mut s = StoreSession::new().await?;

    let result = s
        .client
        .add_signatures(AddSignaturesRequest {
            store_path: test_store_path("addsig-missing"),
            signatures: vec!["cache.example.org-1:SIG".to_string()],
        })
        .await;
    let status = result.expect_err("should be NOT_FOUND");
    assert_eq!(status.code(), tonic::Code::NotFound);

    Ok(())
}

/// AddSignatures with empty list is a no-op (not an error). Happens when
/// `nix store sign` runs with no configured signing keys.
#[tokio::test]
async fn test_add_signatures_empty_is_noop() -> TestResult {
    let mut s = StoreSession::new().await?;

    // Path doesn't exist, but empty-sigs short-circuits before the DB
    // query — so NOT_FOUND is not returned. This is intentional: the
    // no-op should be cheap, not a PG roundtrip just to tell the client
    // "yes, you successfully did nothing to a path that doesn't exist".
    s.client
        .add_signatures(AddSignaturesRequest {
            store_path: test_store_path("addsig-empty"),
            signatures: vec![],
        })
        .await
        .context("empty sigs should be OK")?;

    Ok(())
}
