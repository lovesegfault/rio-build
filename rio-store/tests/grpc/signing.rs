//! ed25519 narinfo signing at PutPath time.

use super::*;

// ===========================================================================
// narinfo signing
// ===========================================================================

use rio_store::signing::Signer;

/// Known seed → known keypair. The test verifies against the derived pubkey.
const TEST_SIGNING_SEED: [u8; 32] = [0x77; 32];

fn make_test_signer() -> Signer {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(TEST_SIGNING_SEED);
    Signer::parse(&format!("test.cache.org-1:{b64}")).expect("valid key string")
}

/// PutPath with a signer: signature lands in narinfo.signatures and is
/// cryptographically valid. This is the end-to-end signing test — proves
/// maybe_sign() runs, the fingerprint is correct, and the sig verifies.
#[tokio::test]
async fn test_putpath_signs_narinfo() -> TestResult {
    let mut s = StoreSession::new_with_signer(make_test_signer()).await?;

    let store_path = test_store_path("signed-path");
    let nar = make_nar(b"content for signing").0;
    let info = make_path_info_for_nar(&store_path, &nar);
    // Keep nar_hash + nar_size for fingerprint reconstruction below.
    let expected_hash = info.nar_hash;
    let expected_size = info.nar_size;

    put_path(&mut s.client, info, nar).await?;

    // Fetch back via QueryPathInfo — the sig should be in signatures.
    let fetched = s
        .client
        .query_path_info(QueryPathInfoRequest {
            store_path: store_path.clone(),
        })
        .await?
        .into_inner();

    assert_eq!(fetched.signatures.len(), 1, "should have exactly one sig");
    let sig_str = &fetched.signatures[0];
    assert!(
        sig_str.starts_with("test.cache.org-1:"),
        "sig should be prefixed with key name: {sig_str}"
    );

    // Verify the signature cryptographically. This is what a Nix client
    // does: reconstruct the fingerprint from narinfo fields, verify
    // against the pubkey from trusted-public-keys.
    use base64::Engine;
    use ed25519_dalek::{Signature, SigningKey, Verifier};

    let pubkey = SigningKey::from_bytes(&TEST_SIGNING_SEED).verifying_key();

    // Reconstruct the fingerprint. No refs in this test (make_path_info
    // produces empty references).
    let fp = rio_nix::narinfo::fingerprint(&store_path, &expected_hash, expected_size, &[]);

    let (_, sig_b64) = sig_str.split_once(':').unwrap();
    let sig_bytes = base64::engine::general_purpose::STANDARD.decode(sig_b64)?;
    let sig_arr: [u8; 64] = sig_bytes.as_slice().try_into()?;

    pubkey
        .verify(fp.as_bytes(), &Signature::from_bytes(&sig_arr))
        .context("signature must verify against reconstructed fingerprint")?;

    Ok(())
}

/// Without a signer: PutPath still works, signatures just empty.
/// The baseline test — signing is optional.
#[tokio::test]
async fn test_putpath_no_signer_no_sig() -> TestResult {
    // Regular StoreSession (no signer).
    let mut s = StoreSession::new().await?;

    let store_path = test_store_path("unsigned-path");
    let nar = make_nar(b"unsigned content").0;
    let info = make_path_info_for_nar(&store_path, &nar);
    put_path(&mut s.client, info, nar).await?;

    let fetched = s
        .client
        .query_path_info(QueryPathInfoRequest { store_path })
        .await?
        .into_inner();
    assert!(
        fetched.signatures.is_empty(),
        "no signer → no signature added"
    );

    Ok(())
}

/// Signature includes references in the fingerprint. A path with refs
/// produces a DIFFERENT signature than the same path without refs —
/// proves the fingerprint actually covers references (not just path+hash).
#[tokio::test]
async fn test_putpath_sig_covers_references() -> TestResult {
    let mut s = StoreSession::new_with_signer(make_test_signer()).await?;

    // Upload a dependency first (so the ref is a valid path).
    let dep_path = test_store_path("sig-dep");
    let dep_nar = make_nar(b"dependency").0;
    let dep_info = make_path_info_for_nar(&dep_path, &dep_nar);
    put_path(&mut s.client, dep_info, dep_nar).await?;

    // Upload the main path WITH a reference.
    let main_path = test_store_path("sig-main");
    let main_nar = make_nar(b"main content").0;
    let mut main_info = make_path_info_for_nar(&main_path, &main_nar);
    main_info.references =
        vec![rio_nix::store_path::StorePath::parse(&dep_path).context("parse dep")?];
    let main_hash = main_info.nar_hash;
    let main_size = main_info.nar_size;
    put_path(&mut s.client, main_info, main_nar).await?;

    // Fetch and verify. The fingerprint MUST include dep_path in refs.
    let fetched = s
        .client
        .query_path_info(QueryPathInfoRequest {
            store_path: main_path.clone(),
        })
        .await?
        .into_inner();
    let sig_str = &fetched.signatures[0];

    use base64::Engine;
    use ed25519_dalek::{Signature, SigningKey, Verifier};
    let pubkey = SigningKey::from_bytes(&TEST_SIGNING_SEED).verifying_key();

    // Fingerprint WITH the ref. slice::from_ref avoids a clone (clippy
    // is right — fingerprint only borrows).
    let fp_with_ref = rio_nix::narinfo::fingerprint(
        &main_path,
        &main_hash,
        main_size,
        std::slice::from_ref(&dep_path),
    );
    // Fingerprint WITHOUT the ref (wrong).
    let fp_no_ref = rio_nix::narinfo::fingerprint(&main_path, &main_hash, main_size, &[]);

    let (_, sig_b64) = sig_str.split_once(':').unwrap();
    let sig_bytes = base64::engine::general_purpose::STANDARD.decode(sig_b64)?;
    let sig_arr: [u8; 64] = sig_bytes.as_slice().try_into()?;
    let sig = Signature::from_bytes(&sig_arr);

    // Must verify against fingerprint WITH ref.
    pubkey
        .verify(fp_with_ref.as_bytes(), &sig)
        .context("should verify against fingerprint WITH ref")?;
    // Must NOT verify against fingerprint WITHOUT ref. If this passed,
    // references wouldn't be covered by the signature — an attacker
    // could serve a path claiming different dependencies.
    assert!(
        pubkey.verify(fp_no_ref.as_bytes(), &sig).is_err(),
        "signature must NOT verify against fingerprint without ref — refs must be covered"
    );

    Ok(())
}
