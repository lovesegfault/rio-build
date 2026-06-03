//! PutPath HMAC assignment-token enforcement tests.
//!
//! Spec `sec.boundary.grpc-hmac`: workers must present a scheduler-
//! signed token proving the upload is for an assigned build. Without
//! this, a compromised worker could upload arbitrary paths.
//!
//! All tests use `StoreSession::new_with_hmac` so `hmac_verifier` is
//! Some — in dev mode (None) the check is bypassed entirely.

use super::*;
use rio_auth::hmac::{AssignmentClaims, HmacSigner, ServiceClaims};
use std::time::{SystemTime, UNIX_EPOCH};

const TEST_KEY: &[u8] = b"test-hmac-key-at-least-32-bytes!!!";
const SERVICE_KEY: &[u8] = b"test-service-hmac-key-32-bytes!!!!";

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Build a valid Claims with the given expected_outputs and sign it.
fn sign_claims(outputs: Vec<String>, expiry_offset_secs: i64) -> String {
    sign_claims_full("test-worker", outputs, false, expiry_offset_secs)
}

fn sign_claims_full(
    executor_id: &str,
    outputs: Vec<String>,
    is_ca: bool,
    expiry_offset_secs: i64,
) -> String {
    sign_claims_tenant(executor_id, outputs, is_ca, expiry_offset_secs, None)
}

fn sign_claims_tenant(
    executor_id: &str,
    outputs: Vec<String>,
    is_ca: bool,
    expiry_offset_secs: i64,
    tenant: Option<&str>,
) -> String {
    let claims = AssignmentClaims {
        executor_id: executor_id.into(),
        drv_hash: "0000000000000000000000000000000000000000000000000000000000000000".into(),
        expected_outputs: outputs,
        expiry_unix: (now_unix() as i64 + expiry_offset_secs) as u64,
        is_ca,
        is_fixed_output: false,
        tenant: tenant.map(String::from),
    };
    HmacSigner::from_key(TEST_KEY.to_vec()).sign(&claims)
}

/// Sign claims for a fixed-output assignment: the path IS known (so it
/// goes in `expected_outputs` and `is_ca = false`), and the scheduler
/// marks the assignment `is_fixed_output = true`, which obliges the
/// worker to present its `fixed:` descriptor on upload.
fn sign_claims_fod(executor_id: &str, outputs: Vec<String>, expiry_offset_secs: i64) -> String {
    let claims = AssignmentClaims {
        executor_id: executor_id.into(),
        drv_hash: "0000000000000000000000000000000000000000000000000000000000000000".into(),
        expected_outputs: outputs,
        expiry_unix: (now_unix() as i64 + expiry_offset_secs) as u64,
        is_ca: false,
        is_fixed_output: true,
        tenant: None,
    };
    HmacSigner::from_key(TEST_KEY.to_vec()).sign(&claims)
}

// ---------------------------------------------------------------------------
// Enforcement ON + no token → reject
// ---------------------------------------------------------------------------

/// Mint a leaf IA deriver (CppNix-shape: declared == derived) and
/// ingest it under an assignment token (a `.drv` upload is exempt from
/// the IA proof gate — the text-CA gate owns it — but must still pass
/// membership). Returns `(deriver_drv_path, derived_out_path)` so
/// callers can claim a path the STORE can prove belongs to the deriver
/// (`store.put.ia-deriver-proof+4`).
async fn stage_ia_deriver(
    client: &mut StoreServiceClient<Channel>,
    tag: &str,
) -> anyhow::Result<(String, String)> {
    use rio_nix::derivation::{Derivation, input_addressed_output_paths};
    use rio_nix::hash::{HashAlgo, NixHash};
    use rio_nix::store_path::StorePath;
    use sha2::{Digest, Sha256};

    let build = |out: &str| {
        format!(
            r#"Derive([("out","{out}","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("name","{tag}"),("out","{out}")])"#
        )
    };
    let masked = Derivation::parse(&build("")).unwrap();
    let name_only = format!("/nix/store/{}-{tag}.drv", "a".repeat(32));
    let none = |_: &str| -> Option<&Derivation> { None };
    let paths =
        input_addressed_output_paths(&masked, &name_only, &none, &mut Default::default()).unwrap();
    let out = paths["out"].as_str().to_owned();
    let aterm = build(&out);
    let h = NixHash::new(HashAlgo::SHA256, Sha256::digest(aterm.as_bytes()).to_vec()).unwrap();
    let drv_path = StorePath::make_text(&format!("{tag}.drv"), &h, &[])
        .unwrap()
        .as_str()
        .to_owned();

    let node = rio_nix::nar::NarNode::Regular {
        executable: false,
        contents: aterm.into_bytes(),
    };
    let mut nar = Vec::new();
    rio_nix::nar::serialize(&mut nar, &node)?;
    let info = make_path_info_for_nar(&drv_path, &nar);
    let token = sign_claims(vec![drv_path.clone()], 60);
    anyhow::ensure!(
        put_path_with_token(client, info, nar, &token).await?,
        "deriver ingested"
    );
    Ok((drv_path, out))
}

/// Sign claims naming a REAL deriver (post-proof-gate IA staging).
fn sign_claims_for_deriver(deriver: &str, outputs: Vec<String>, expiry_offset_secs: i64) -> String {
    let claims = AssignmentClaims {
        executor_id: "w-test".into(),
        drv_hash: deriver.into(),
        expected_outputs: outputs,
        expiry_unix: (now_unix() as i64 + expiry_offset_secs) as u64,
        is_ca: false,
        is_fixed_output: false,
        tenant: None,
    };
    HmacSigner::from_key(TEST_KEY.to_vec()).sign(&claims)
}

fn sign_service(caller: &str, expiry_offset_secs: i64) -> String {
    let claims = ServiceClaims {
        caller: caller.into(),
        expiry_unix: (now_unix() as i64 + expiry_offset_secs) as u64,
    };
    HmacSigner::from_key(SERVICE_KEY.to_vec()).sign(&claims)
}

// ---------------------------------------------------------------------------
// Service-token bypass (transport-agnostic CN-allowlist replacement)
// ---------------------------------------------------------------------------

// r[verify sec.authz.service-token]
#[tokio::test]
async fn service_token_bypasses_hmac() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    let path = test_store_path("svc-bypass");
    let (nar, _) = make_nar(b"content");
    let info = make_path_info_for_nar(&path, &nar);

    // Valid service token, NO assignment token → accepted (bypass).
    let created = put_path_with_header(
        &mut s.client,
        info,
        nar,
        rio_proto::SERVICE_TOKEN_HEADER,
        &sign_service("rio-gateway", 60),
    )
    .await?;
    assert!(created);
    Ok(())
}

#[tokio::test]
async fn expired_service_token_rejected() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    let path = test_store_path("svc-expired");
    let (nar, _) = make_nar(b"content");
    let info = make_path_info_for_nar(&path, &nar);

    let err = put_path_with_header(
        &mut s.client,
        info,
        nar,
        rio_proto::SERVICE_TOKEN_HEADER,
        &sign_service("rio-gateway", -60),
    )
    .await
    .expect_err("expired service token should be rejected");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    Ok(())
}

#[tokio::test]
async fn service_token_wrong_caller_rejected() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    let path = test_store_path("svc-wrong-caller");
    let (nar, _) = make_nar(b"content");
    let info = make_path_info_for_nar(&path, &nar);

    // Valid signature but caller not in allowlist → reject. Proves a
    // compromised builder that somehow obtained the service key still
    // cannot bypass without ALSO knowing an allowlisted caller name
    // (defense-in-depth; the primary defense is key isolation).
    let err = put_path_with_header(
        &mut s.client,
        info,
        nar,
        rio_proto::SERVICE_TOKEN_HEADER,
        &sign_service("rio-builder", 60),
    )
    .await
    .expect_err("non-allowlisted caller should be rejected");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(err.message().contains("no bypass for this method"));
    Ok(())
}

#[tokio::test]
async fn service_token_wrong_key_rejected() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    let path = test_store_path("svc-wrong-key");
    let (nar, _) = make_nar(b"content");
    let info = make_path_info_for_nar(&path, &nar);

    // Signed with the ASSIGNMENT key (which a builder might plausibly
    // obtain via a leaked WorkAssignment) → service_verifier rejects
    // (different key → InvalidSignature). This is the core threat-
    // model property: assignment-key compromise does NOT grant bypass.
    let forged = HmacSigner::from_key(TEST_KEY.to_vec()).sign(&ServiceClaims {
        caller: "rio-gateway".into(),
        expiry_unix: now_unix() + 60,
    });
    let err = put_path_with_header(
        &mut s.client,
        info,
        nar,
        rio_proto::SERVICE_TOKEN_HEADER,
        &forged,
    )
    .await
    .expect_err("wrong-key service token should be rejected");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    Ok(())
}

// r[verify sec.boundary.grpc-hmac]
#[tokio::test]
async fn hmac_no_token_rejected() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;

    let path = test_store_path("hmac-no-token");
    let (nar, _) = make_nar(b"content");
    let info = make_path_info_for_nar(&path, &nar);

    // No token header → permission_denied.
    let err = put_path(&mut s.client, info, nar)
        .await
        .expect_err("no token → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(
        err.message().contains("token required"),
        "msg: {}",
        err.message()
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Valid token with path in expected_outputs → accept
// ---------------------------------------------------------------------------

#[tokio::test]
// r[verify store.put.ia-deriver-proof+4]
async fn hmac_valid_token_accepted() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;

    // The legit IA flow: the deriver is store-resident and the claimed
    // path is one the STORE derives from it.
    let (deriver, path) = stage_ia_deriver(&mut s.client, "hmac-valid").await?;
    let (nar, _) = make_nar(b"authorized upload");
    let info = make_path_info_for_nar(&path, &nar);

    // Token lists the exact path we're uploading, signed for the
    // resident deriver.
    let token = sign_claims_for_deriver(&deriver, vec![path.clone()], 60);

    let created = put_path_with_token(&mut s.client, info, nar, &token)
        .await
        .context("put with valid token")?;
    assert!(created, "valid token → accepted + created");

    Ok(())
}

// ---------------------------------------------------------------------------
// Invalid token (wrong key / garbage) → reject
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hmac_invalid_token_rejected() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;

    let path = test_store_path("hmac-invalid");
    let (nar, _) = make_nar(b"content");
    let info = make_path_info_for_nar(&path, &nar);

    // Garbage token — HmacVerifier::verify will fail.
    let err = put_path_with_token(&mut s.client, info, nar, "garbage-token")
        .await
        .expect_err("invalid token → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(
        err.message().contains("assignment token:"),
        "msg: {}",
        err.message()
    );

    Ok(())
}

#[tokio::test]
async fn hmac_wrong_key_signed_rejected() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;

    let path = test_store_path("hmac-wrong-key");
    let (nar, _) = make_nar(b"content");
    let info = make_path_info_for_nar(&path, &nar);

    // Sign with a DIFFERENT key → MAC mismatch.
    let wrong_key = b"different-key-different-signature!!";
    let claims = AssignmentClaims {
        executor_id: "evil".into(),
        drv_hash: "00".repeat(32),
        expected_outputs: vec![path.clone()],
        expiry_unix: now_unix() + 60,
        is_ca: false,
        is_fixed_output: false,
        tenant: None,
    };
    let bad_token = HmacSigner::from_key(wrong_key.to_vec()).sign(&claims);

    let err = put_path_with_token(&mut s.client, info, nar, &bad_token)
        .await
        .expect_err("wrong-key token → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);

    Ok(())
}

// ---------------------------------------------------------------------------
// Expired token → reject (HmacVerifier checks expiry_unix)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hmac_expired_token_rejected() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;

    let path = test_store_path("hmac-expired");
    let (nar, _) = make_nar(b"content");
    let info = make_path_info_for_nar(&path, &nar);

    // Token expired 10 seconds ago.
    let token = sign_claims(vec![path.clone()], -10);

    let err = put_path_with_token(&mut s.client, info, nar, &token)
        .await
        .expect_err("expired → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);

    Ok(())
}

// ---------------------------------------------------------------------------
// Valid token but uploaded path NOT in expected_outputs → reject
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hmac_path_not_in_claims_rejected() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;

    let path = test_store_path("hmac-unauthorized-path");
    let (nar, _) = make_nar(b"content");
    let info = make_path_info_for_nar(&path, &nar);

    // Token authorizes a DIFFERENT path.
    let authorized = test_store_path("hmac-some-other-path");
    let token = sign_claims(vec![authorized], 60);

    let err = put_path_with_token(&mut s.client, info, nar, &token)
        .await
        .expect_err("path not in claims → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(
        err.message().contains("not authorized") || err.message().contains("not in"),
        "msg: {}",
        err.message()
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Sanity: verifier OFF (dev mode) → no enforcement
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hmac_disabled_no_token_accepted() -> TestResult {
    // Normal session — no verifier.
    let mut s = StoreSession::new().await?;

    let path = test_store_path("hmac-dev-mode");
    let (nar, _) = make_nar(b"content");
    let info = make_path_info_for_nar(&path, &nar);

    // No token, no verifier → accepted (dev bypass).
    let created = put_path(&mut s.client, info, nar)
        .await
        .context("dev-mode put")?;
    assert!(created);

    Ok(())
}

// ---------------------------------------------------------------------------
// Floating-CA: store_path derived server-side from verified nar_hash
// ---------------------------------------------------------------------------

/// Compute the floating-CA store path for `nar` the way the server
/// does (`make_fixed_output(name, sha256(nar), recursive, [])`).
fn ca_path_for(name: &str, nar: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let h = rio_nix::hash::NixHash::new(
        rio_nix::hash::HashAlgo::SHA256,
        Sha256::digest(nar).to_vec(),
    )
    .unwrap();
    rio_nix::store_path::StorePath::make_fixed_output(name, &h, true, &[])
        .unwrap()
        .to_string()
}

// r[verify sec.authz.ca-path-derived+9]
#[tokio::test]
async fn hmac_is_ca_correct_path_accepted() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let (nar, _) = make_nar(b"ca content");
    let path = ca_path_for("ca-ok", &nar);
    let info = make_path_info_for_nar(&path, &nar);

    // is_ca=true, expected_outputs empty (as at dispatch time). Upload
    // to the content-derived path → accepted.
    let token = sign_claims_full("test-worker", vec![String::new()], true, 60);
    let created = put_path_with_token(&mut s.client, info, nar, &token)
        .await
        .context("put to derived CA path")?;
    assert!(created);
    Ok(())
}

// r[verify sec.authz.ca-path-derived+9]
#[tokio::test]
async fn hmac_is_ca_wrong_path_rejected() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let (nar, _) = make_nar(b"evil content");
    // Upload to an ARBITRARY (IA-shaped) path that is NOT the
    // content-derived CA path. Pre-fix this was accepted — the
    // "backdoored libc" scenario.
    let path = test_store_path("evil-glibc-2.38");
    let info = make_path_info_for_nar(&path, &nar);

    let token = sign_claims_full("test-worker", vec![String::new()], true, 60);
    let err = put_path_with_token(&mut s.client, info, nar, &token)
        .await
        .expect_err("is_ca token to non-derived path → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(
        err.message().contains("content-derived CA path"),
        "msg: {}",
        err.message()
    );
    Ok(())
}

// r[verify sec.authz.ca-path-derived+9]
/// bug_094: pre-fix, `claim_placeholder` ran BEFORE `verify_ca_store_path`
/// for is_ca tokens, so a compromised worker could open a PutPath stream
/// to ANY path, send one chunk (no trailer), and hold the `'uploading'`
/// placeholder fresh — forcing legitimate uploaders into `Aborted` until
/// token expiry. Post-fix, the placeholder is not claimed until the
/// server-derived CA path is verified, so a held-open mismatched-path
/// stream never inserts a placeholder and a concurrent legitimate
/// uploader for that path is unaffected.
#[tokio::test]
async fn hmac_is_ca_wrong_path_leaves_no_placeholder() -> TestResult {
    let s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let mut attacker = s.client.clone();
    let mut victim = s.client.clone();

    // Victim's IA path the attacker wants to squat (deriver resident
    // so the victim's own upload passes the proof gate).
    let mut staging = s.client.clone();
    let (victim_deriver, victim_path) = stage_ia_deriver(&mut staging, "victim-glibc").await?;
    let (victim_nar, _) = make_nar(b"legitimate glibc");
    let victim_info = make_path_info_for_nar(&victim_path, &victim_nar);

    // Attacker: is_ca token, opens a PutPath stream targeting
    // victim_path with metadata + ONE chunk, NO trailer — held open.
    let (atx, arx) = mpsc::channel(8);
    let mut bogus_info: PathInfo =
        make_path_info_for_nar(&victim_path, &make_nar(b"evil").0).into();
    bogus_info.nar_hash = vec![];
    bogus_info.nar_size = 0;
    atx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(bogus_info),
        })),
    })
    .await
    .unwrap();
    atx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::NarChunk(vec![0u8])),
    })
    .await
    .unwrap();
    // atx kept alive — stream held open. Spawn the call so it parks
    // on `stream.message().await` server-side.
    let mut areq = tonic::Request::new(ReceiverStream::new(arx));
    let atoken = sign_claims_full("evil-worker", vec![String::new()], true, 60);
    areq.metadata_mut()
        .insert(rio_proto::ASSIGNMENT_TOKEN_HEADER, atoken.parse().unwrap());
    let attacker_call = tokio::spawn(async move { attacker.put_path(areq).await });

    // Give the server a beat to read metadata + chunk and (pre-fix)
    // insert the placeholder. Post-fix it's parked at
    // `stream.message().await` with NO PG row.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM manifests WHERE store_path_hash = sha256($1::text::bytea)",
    )
    .bind(&victim_path)
    .fetch_one(&s.db.pool)
    .await?;
    assert_eq!(
        n, 0,
        "is_ca held-open stream to non-derived path must NOT insert placeholder"
    );

    // Victim's legitimate IA upload (token lists victim_path) MUST
    // succeed — pre-fix it got `Aborted: concurrent PutPath`.
    let vtoken = sign_claims_for_deriver(&victim_deriver, vec![victim_path.clone()], 60);
    let created = put_path_with_token(&mut victim, victim_info, victim_nar, &vtoken)
        .await
        .context("victim upload while attacker stream held open")?;
    assert!(created, "victim's legitimate PutPath must not be blocked");

    // Close attacker stream → server reads EOF without trailer →
    // InvalidArgument (no trailer) — NOT Aborted/PermissionDenied
    // before that since no placeholder was ever claimed. The exact
    // status doesn't matter for the squat; just clean up.
    drop(atx);
    let _ = attacker_call.await;
    Ok(())
}

#[tokio::test]
async fn hmac_is_ca_wrong_hash_part_rejected() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let (nar, _) = make_nar(b"ca content 2");
    // Correct name, WRONG hash-part (test_store_path uses a fixed
    // TEST_HASH). Proves the check compares the full path, not just
    // the name.
    let derived = ca_path_for("ca-hashpart", &nar);
    let wrong = test_store_path("ca-hashpart");
    assert_ne!(derived, wrong, "test precondition: paths differ");
    let info = make_path_info_for_nar(&wrong, &nar);

    let token = sign_claims_full("test-worker", vec![String::new()], true, 60);
    let err = put_path_with_token(&mut s.client, info, nar, &token)
        .await
        .expect_err("wrong hash-part → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    Ok(())
}

// r[verify sec.authz.ca-path-derived+9]
/// `PutPathBatch` is the multi-output endpoint builders use; the CA
/// path-derivation gate must apply there too. Same attack as
/// [`hmac_is_ca_wrong_path_rejected`] but via the batch RPC.
#[tokio::test]
async fn hmac_is_ca_batch_wrong_path_rejected() -> TestResult {
    use rio_proto::types::{PutPathBatchRequest, PutPathRequest, put_path_request};

    let s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let mut client = s.client.clone();
    let (nar, _) = make_nar(b"evil batch content");
    let path = test_store_path("evil-batch-target");
    let mut info: PathInfo = make_path_info_for_nar(&path, &nar).into();
    let trailer = PutPathTrailer {
        nar_hash: std::mem::take(&mut info.nar_hash),
        nar_size: std::mem::take(&mut info.nar_size),
    };

    let (tx, rx) = mpsc::channel(8);
    let wrap = |m| PutPathBatchRequest {
        output_index: 0,
        inner: Some(PutPathRequest { msg: Some(m) }),
    };
    tx.send(wrap(put_path_request::Msg::Metadata(PutPathMetadata {
        info: Some(info),
    })))
    .await
    .unwrap();
    tx.send(wrap(put_path_request::Msg::NarChunk(nar)))
        .await
        .unwrap();
    tx.send(wrap(put_path_request::Msg::Trailer(trailer)))
        .await
        .unwrap();
    drop(tx);

    let mut req = tonic::Request::new(ReceiverStream::new(rx));
    let token = sign_claims_full("test-worker", vec![String::new()], true, 60);
    req.metadata_mut()
        .insert(rio_proto::ASSIGNMENT_TOKEN_HEADER, token.parse().unwrap());

    let err = client
        .put_path_batch(req)
        .await
        .expect_err("is_ca batch to non-derived path → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(
        err.message().contains("content-derived CA path"),
        "msg: {}",
        err.message()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Fixed-output assignments: descriptor is mandatory (signed claims bit)
// ---------------------------------------------------------------------------

/// A fixed-output upload shaped the way the builder produces it: the
/// path derived from the recursive SHA-256 of the NAR, plus the
/// `fixed:r:` descriptor recorded from the derivation's declared hash.
fn fod_upload_for(name: &str, nar: &[u8]) -> (String, String) {
    use sha2::{Digest, Sha256};
    let h = rio_nix::hash::NixHash::new(
        rio_nix::hash::HashAlgo::SHA256,
        Sha256::digest(nar).to_vec(),
    )
    .unwrap();
    let path = rio_nix::store_path::StorePath::make_fixed_output(name, &h, true, &[])
        .unwrap()
        .to_string();
    (path, format!("fixed:r:{}", h.to_colon()))
}

// r[verify sec.authz.ca-path-derived+9]
/// A worker holding a FOD-flagged token cannot skip content
/// verification by omitting its `fixed:` descriptor: the membership
/// check alone is not enough for a content-bound output.
#[tokio::test]
async fn hmac_fod_descriptorless_rejected() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let (nar, _) = make_nar(b"fod payload");
    let (path, _descriptor) = fod_upload_for("fod-out", &nar);
    let info = make_path_info_for_nar(&path, &nar); // content_address: None

    let token = sign_claims_fod("test-worker", vec![path.clone()], 60);
    let err = put_path_with_token(&mut s.client, info, nar, &token)
        .await
        .expect_err("descriptor-less upload under a FOD-flagged token → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(
        err.message().contains("fixed-output upload must carry"),
        "msg: {}",
        err.message()
    );
    Ok(())
}

// r[verify sec.authz.ca-path-derived+9]
/// End-to-end splice forgery (merged_bug_076): a FOD-flagged worker
/// uploads bytes embedding the claimed path's own hash part with a
/// descriptor carrying the hash MODULO those occurrences (plain hash ≠
/// descriptor). The gate must reject at the descriptor mismatch — the
/// discarded-self modulo retry is floating-CA-only — and the rejection
/// must leave NO manifest row behind (neither `'uploading'` placeholder
/// nor complete).
#[tokio::test]
async fn hmac_fod_spliced_modulo_rejected_and_no_row_persists() -> TestResult {
    use std::io::Write as _;
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;

    fn nar_of(contents: Vec<u8>) -> anyhow::Result<Vec<u8>> {
        let node = rio_nix::nar::NarNode::Regular {
            executable: false,
            contents,
        };
        let mut buf = Vec::new();
        rio_nix::nar::serialize(&mut buf, &node)?;
        Ok(buf)
    }

    // Splice construction (mirrors the unit fixture): content minted at
    // a scratch path, hashed modulo it, final path derived from the
    // modulo, scratch occurrences rewritten to the final hash part.
    let drv = rio_nix::store_path::StorePath::parse(&format!(
        "/nix/store/{}-fod-splice-e2e.drv",
        "b".repeat(32)
    ))?;
    let scratch = rio_nix::store_path::StorePath::make_scratch_output_path(&drv, "out")?;
    let content_at_scratch = format!("I live at {}\n", scratch.as_str()).into_bytes();
    let nar_at_scratch = nar_of(content_at_scratch.clone())?;
    let mut sink =
        rio_nix::ca::HashModuloSink::new(rio_nix::hash::HashAlgo::SHA256, &scratch.hash_part());
    sink.write_all(&nar_at_scratch)?;
    let (modulo, _) = sink.finish();
    let path = rio_nix::store_path::StorePath::make_fixed_output_with_self(
        "fod-splice-e2e",
        &modulo,
        true,
        &[],
        false,
    )?;
    let final_content = String::from_utf8(content_at_scratch)?
        .replace(&scratch.hash_part(), &path.hash_part())
        .into_bytes();
    let nar = nar_of(final_content)?;

    let mut info = make_path_info_for_nar(path.as_str(), &nar);
    info.content_address = Some(format!("fixed:r:{}", modulo.to_colon()));
    let token = sign_claims_fod("test-worker", vec![path.as_str().to_owned()], 60);
    let err = put_path_with_token(&mut s.client, info, nar, &token)
        .await
        .expect_err("spliced FOD upload must be rejected end-to-end");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);

    // No manifest row of ANY status persists for the forged path.
    let n = poll_scalar_until::<i64>(&s.db.pool, "SELECT count(*)::bigint FROM manifests", 0).await;
    assert_eq!(n, 0, "rejected splice upload must leave no manifest row");
    Ok(())
}

// r[verify sec.authz.ca-path-derived+9]
/// The honest FOD flow is unaffected: descriptor present, content
/// matches it, path re-derives from it → accepted.
#[tokio::test]
async fn hmac_fod_with_descriptor_accepted() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let (nar, _) = make_nar(b"honest fod payload");
    let (path, descriptor) = fod_upload_for("fod-honest", &nar);
    let mut info = make_path_info_for_nar(&path, &nar);
    info.content_address = Some(descriptor);

    let token = sign_claims_fod("test-worker", vec![path.clone()], 60);
    let created = put_path_with_token(&mut s.client, info, nar, &token)
        .await
        .context("honest FOD upload")?;
    assert!(created);
    Ok(())
}

// r[verify sec.authz.ca-path-derived+9]
/// Same enforcement on the batch ingestion path.
#[tokio::test]
async fn hmac_fod_batch_descriptorless_rejected() -> TestResult {
    use rio_proto::types::{PutPathBatchRequest, PutPathRequest, put_path_request};

    let s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let mut client = s.client.clone();
    let (nar, _) = make_nar(b"fod batch payload");
    let (path, _descriptor) = fod_upload_for("fod-batch", &nar);
    let mut info: PathInfo = make_path_info_for_nar(&path, &nar).into();
    let trailer = PutPathTrailer {
        nar_hash: std::mem::take(&mut info.nar_hash),
        nar_size: std::mem::take(&mut info.nar_size),
    };

    let (tx, rx) = mpsc::channel(8);
    let wrap = |m| PutPathBatchRequest {
        output_index: 0,
        inner: Some(PutPathRequest { msg: Some(m) }),
    };
    tx.send(wrap(put_path_request::Msg::Metadata(PutPathMetadata {
        info: Some(info),
    })))
    .await
    .unwrap();
    tx.send(wrap(put_path_request::Msg::NarChunk(nar)))
        .await
        .unwrap();
    tx.send(wrap(put_path_request::Msg::Trailer(trailer)))
        .await
        .unwrap();
    drop(tx);

    let mut req = tonic::Request::new(ReceiverStream::new(rx));
    let token = sign_claims_fod("test-worker", vec![path], 60);
    req.metadata_mut()
        .insert(rio_proto::ASSIGNMENT_TOKEN_HEADER, token.parse().unwrap());

    let err = client
        .put_path_batch(req)
        .await
        .expect_err("descriptor-less FOD batch entry → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(
        err.message().contains("fixed-output upload must carry"),
        "msg: {}",
        err.message()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// AppendHwPerfSample: pod_id derived from claims, not body
// ---------------------------------------------------------------------------

use rio_proto::types::AppendHwPerfSampleRequest;

async fn append_hw(
    client: &mut StoreServiceClient<Channel>,
    body_pod_id: &str,
    header: Option<(&'static str, &str)>,
) -> Result<(), tonic::Status> {
    let mut req = tonic::Request::new(AppendHwPerfSampleRequest {
        hw_class: "aws-8-ebs-hi".into(),
        pod_id: body_pod_id.into(),
        factor_json: r#"{"alu":1.5,"membw":1.0,"ioseq":1.0}"#.into(),
    });
    if let Some((h, v)) = header {
        req.metadata_mut().insert(h, v.parse().unwrap());
    }
    client.append_hw_perf_sample(req).await.map(|_| ())
}

// r[verify sec.boundary.grpc-hmac]
#[tokio::test]
async fn append_hw_perf_sample_without_token_rejected() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let err = append_hw(&mut s.client, "fake-pod", None)
        .await
        .expect_err("no token → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    Ok(())
}

// r[verify sec.boundary.grpc-hmac]
#[tokio::test]
async fn append_hw_perf_sample_forged_pod_id_ignored() -> TestResult {
    let s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let mut client = s.client.clone();
    // Token signed for executor_id="real-pod"; body says "fake-pod".
    // Server MUST write pod_id='real-pod'.
    let token = sign_claims_full("real-pod", vec![String::new()], false, 60);
    append_hw(
        &mut client,
        "fake-pod",
        Some((rio_proto::ASSIGNMENT_TOKEN_HEADER, &token)),
    )
    .await
    .context("append with valid token")?;

    let row: (String,) =
        sqlx::query_as("SELECT pod_id FROM hw_perf_samples WHERE hw_class = 'aws-8-ebs-hi'")
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(
        row.0, "real-pod",
        "pod_id from claims.executor_id, not body"
    );
    Ok(())
}

/// `submitting_tenant` is derived from `claims.tenant` (signed at
/// dispatch). The body field was REMOVED from the proto so it cannot be
/// supplied; this asserts the claims value reaches PG. A token without
/// a tenant claim → NULL.
// r[verify sched.sla.threat.hw-median-of-medians]
#[tokio::test]
async fn append_hw_perf_sample_tenant_from_claims() -> TestResult {
    let s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let mut client = s.client.clone();
    // Token with tenant claim → that value is written.
    let token = sign_claims_tenant(
        "p-with-tenant",
        vec![String::new()],
        false,
        60,
        Some("tenant-uuid-a"),
    );
    append_hw(
        &mut client,
        "ignored",
        Some((rio_proto::ASSIGNMENT_TOKEN_HEADER, &token)),
    )
    .await
    .context("append with tenant claim")?;
    // Token without tenant claim (pre-tenant scheduler / orphan) → NULL.
    let token = sign_claims_tenant("p-no-tenant", vec![String::new()], false, 60, None);
    append_hw(
        &mut client,
        "ignored",
        Some((rio_proto::ASSIGNMENT_TOKEN_HEADER, &token)),
    )
    .await
    .context("append without tenant claim")?;

    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT pod_id, submitting_tenant FROM hw_perf_samples \
         WHERE hw_class = 'aws-8-ebs-hi' ORDER BY pod_id",
    )
    .fetch_all(&s.db.pool)
    .await?;
    assert_eq!(
        rows,
        vec![
            ("p-no-tenant".into(), None),
            ("p-with-tenant".into(), Some("tenant-uuid-a".into())),
        ],
        "tenant from claims (signed), not body; absent claim → NULL"
    );
    Ok(())
}

/// `factor_json` is parsed, validated, then REBUILT from the present
/// scalars. Extra keys / padding in the body never reach PG — the
/// stored jsonb is exactly `{alu[, membw][, ioseq]}`. A 4MB body
/// padding would otherwise land verbatim in `hw_perf_samples.factor`.
// r[verify sched.sla.hw-bench-append-only]
#[tokio::test]
async fn append_hw_perf_sample_factor_json_extra_keys_stripped() -> TestResult {
    let s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let mut client = s.client.clone();
    let token = sign_claims_full("p0", vec![String::new()], false, 60);
    let padding = "x".repeat(64 * 1024);
    let mut req = tonic::Request::new(AppendHwPerfSampleRequest {
        hw_class: "aws-8-ebs-hi".into(),
        pod_id: "ignored".into(),
        factor_json: format!(
            r#"{{"alu":1.5,"membw":1.0,"ioseq":1.0,"evil":"{padding}","z":[1,2,3]}}"#
        ),
    });
    req.metadata_mut()
        .insert(rio_proto::ASSIGNMENT_TOKEN_HEADER, token.parse().unwrap());
    client
        .append_hw_perf_sample(req)
        .await
        .context("append with padded factor_json")?;

    let stored: serde_json::Value =
        sqlx::query_scalar("SELECT factor FROM hw_perf_samples WHERE pod_id = 'p0'")
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(
        stored,
        serde_json::json!({"alu": 1.5, "membw": 1.0, "ioseq": 1.0}),
        "stored jsonb rebuilt from validated scalars; extra keys dropped"
    );
    Ok(())
}

/// `hw_class` length-bounded at MAX_HW_CLASS_LEN. Unique key is
/// `(hw_class, pod_id)` — without a bound, one compromised builder with a
/// legitimate token could loop distinct multi-MB strings and fill
/// `hw_perf_samples` (M_041's "one row per pod start" assumed honest
/// callers). 65 chars → InvalidArgument; nothing inserted.
// r[verify sched.sla.hw-bench-append-only]
#[tokio::test]
async fn append_hw_perf_sample_oversized_hw_class_rejected() -> TestResult {
    let s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let mut client = s.client.clone();
    let token = sign_claims_full("real-pod", vec![String::new()], false, 60);
    let mut req = tonic::Request::new(AppendHwPerfSampleRequest {
        hw_class: "a".repeat(rio_common::limits::MAX_HW_CLASS_LEN + 1),
        pod_id: "ignored".into(),
        factor_json: r#"{"alu":1.5,"membw":1.0,"ioseq":1.0}"#.into(),
    });
    req.metadata_mut()
        .insert(rio_proto::ASSIGNMENT_TOKEN_HEADER, token.parse().unwrap());
    let err = client
        .append_hw_perf_sample(req)
        .await
        .expect_err("oversized hw_class → reject");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM hw_perf_samples")
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(n, 0, "rejected request must not INSERT");
    Ok(())
}

/// `hw_class` charset-bounded to `[a-z0-9-]` — the controller-stamped
/// 4-segment format. Slash, uppercase, unicode → InvalidArgument.
// r[verify sched.sla.hw-bench-append-only]
#[tokio::test]
async fn append_hw_perf_sample_bad_charset_rejected() -> TestResult {
    let s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let mut client = s.client.clone();
    let token = sign_claims_full("real-pod", vec![String::new()], false, 60);
    for bad in ["aws/7/ebs", "AWS-7-ebs-hi", "aws-7-ébs-hi"] {
        let mut req = tonic::Request::new(AppendHwPerfSampleRequest {
            hw_class: bad.into(),
            pod_id: "ignored".into(),
            factor_json: r#"{"alu":1.5,"membw":1.0,"ioseq":1.0}"#.into(),
        });
        req.metadata_mut()
            .insert(rio_proto::ASSIGNMENT_TOKEN_HEADER, token.parse().unwrap());
        let err = client
            .append_hw_perf_sample(req)
            .await
            .expect_err("bad charset → reject");
        assert_eq!(err.code(), tonic::Code::InvalidArgument, "hw_class={bad:?}");
    }
    Ok(())
}

#[tokio::test]
async fn append_hw_perf_sample_service_token_rejected() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    // Gateway has no business writing hw_perf_samples; service-token
    // bypass yields no executor_id → reject.
    let err = append_hw(
        &mut s.client,
        "fake-pod",
        Some((
            rio_proto::SERVICE_TOKEN_HEADER,
            &sign_service("rio-gateway", 60),
        )),
    )
    .await
    .expect_err("service token → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(
        err.message().contains("service-token"),
        "msg: {}",
        err.message()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// store_path_hash forgery — server MUST recompute (sec.boundary.grpc-hmac)
// ---------------------------------------------------------------------------

/// bug_068: HMAC binds `store_path` (string), not `store_path_hash`.
/// A worker holding a token for path A sends `{store_path: A,
/// store_path_hash: sha256(B)}`. The HMAC gate passes (store_path is
/// in claims); pre-fix, the server keyed A's narinfo under B's slot —
/// poisoning B's hash lookups. Post-fix, `validate_put_metadata`
/// recomputes `store_path_hash` from the gated `store_path`
/// unconditionally; the wire value is ignored.
// r[verify sec.boundary.grpc-hmac]
#[tokio::test]
async fn hmac_store_path_hash_mismatch_ignored() -> TestResult {
    use sha2::{Digest, Sha256};

    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;

    let (deriver_a, path_a) = stage_ia_deriver(&mut s.client, "hmac-hashforge-a").await?;
    let path_b = test_store_path("hmac-hashforge-b");
    let (nar, _) = make_nar(b"authorized payload for A");
    let info = make_path_info_for_nar(&path_a, &nar);

    // Token authorizes path A only.
    let token = sign_claims_for_deriver(&deriver_a, vec![path_a.clone()], 60);

    // Forge: store_path=A (passes HMAC) but store_path_hash=sha256(B).
    let mut raw: PathInfo = info.into();
    raw.store_path_hash = Sha256::digest(path_b.as_bytes()).to_vec();
    let trailer = PutPathTrailer {
        nar_hash: std::mem::take(&mut raw.nar_hash),
        nar_size: std::mem::take(&mut raw.nar_size),
    };

    let (tx, rx) = mpsc::channel(8);
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(raw),
        })),
    })
    .await
    .unwrap();
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::NarChunk(nar)),
    })
    .await
    .unwrap();
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Trailer(trailer)),
    })
    .await
    .unwrap();
    drop(tx);

    let mut req = tonic::Request::new(ReceiverStream::new(rx));
    req.metadata_mut().insert(
        rio_proto::ASSIGNMENT_TOKEN_HEADER,
        token.parse().expect("ascii token"),
    );
    let created = s.client.put_path(req).await?.into_inner().created;
    assert!(created, "upload of A with forged hash succeeds");

    // B's slot MUST be untouched.
    let b_result = s
        .client
        .query_path_info(QueryPathInfoRequest {
            store_path: path_b.clone(),
        })
        .await;
    assert_eq!(
        b_result.expect_err("B was never uploaded").code(),
        tonic::Code::NotFound,
        "forged store_path_hash must NOT key A's narinfo under B's slot"
    );

    // A's slot MUST hold the upload (server-derived hash).
    let a_info = s
        .client
        .query_path_info(QueryPathInfoRequest {
            store_path: path_a.clone(),
        })
        .await?
        .into_inner();
    assert_eq!(
        a_info.store_path, path_a,
        "A keyed under its own server-derived hash"
    );

    Ok(())
}

// r[verify store.put.ia-deriver-proof+4]
/// THE forged-claims kill test (compromised-scheduler simulation): a
/// VALIDLY SIGNED token whose expected_outputs (membership) include the
/// victim's path, naming a resident deriver that does NOT derive it —
/// the store's own bytes refuse what the signature alone would allow.
#[tokio::test]
async fn ia_proof_rejects_membership_passing_underivable_path() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let (attacker_deriver, _attacker_out) =
        stage_ia_deriver(&mut s.client, "proof-attacker").await?;
    let (_victim_deriver, victim_path) = stage_ia_deriver(&mut s.client, "proof-victim").await?;

    let (nar, _) = make_nar(b"squat content");
    let info = make_path_info_for_nar(&victim_path, &nar);
    // Signed, membership-passing, WRONG deriver.
    let token = sign_claims_for_deriver(&attacker_deriver, vec![victim_path.clone()], 60);
    let err = put_path_with_token(&mut s.client, info, nar, &token)
        .await
        .expect_err("underivable claimed path must be rejected");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(err.message().contains("not an output the store derives"));
    Ok(())
}

// r[verify store.put.ia-deriver-proof+4]
/// Deriver absent → unverifiable → fail closed; once the deriver is
/// ingested (read-through warms from resident bytes) the same upload
/// succeeds.
#[tokio::test]
async fn ia_proof_unverifiable_until_deriver_resident() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    // Mint but DO NOT upload yet: compute what the paths will be.
    let mut probe = s.client.clone();
    let (deriver, out) = stage_ia_deriver(&mut probe, "proof-late").await?;
    // Wipe the modulo row to simulate "resident but never populated"
    // — the read-through must recompute from the store's own bytes.
    sqlx::query("DELETE FROM drv_modulo_cache")
        .execute(&s.db.pool)
        .await?;

    let (nar, _) = make_nar(b"late content");
    let info = make_path_info_for_nar(&out, &nar);
    let token = sign_claims_for_deriver(&deriver, vec![out.clone()], 60);
    assert!(
        put_path_with_token(&mut s.client, info, nar, &token).await?,
        "read-through recomputes the proof from resident bytes"
    );
    // Cache warmed.
    let key: Vec<u8> = {
        use sha2::Digest as _;
        sha2::Sha256::digest(deriver.as_bytes()).to_vec()
    };
    let (n,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM drv_modulo_cache WHERE drv_path_hash = $1")
            .bind(key)
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(n, 1, "read-through warmed the cache");
    Ok(())
}

// r[verify store.put.ia-deriver-proof+4]
/// The PutPathBatch path enforces the same gate per output (the unary
/// fix alone would leave the batch door open).
#[tokio::test]
async fn ia_proof_batch_rejects_underivable_output() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let (attacker_deriver, _o) = stage_ia_deriver(&mut s.client, "proof-batch-a").await?;
    let (_vd, victim_path) = stage_ia_deriver(&mut s.client, "proof-batch-v").await?;

    let (nar, _) = make_nar(b"batch squat");
    let info = make_path_info_for_nar(&victim_path, &nar);
    let token = sign_claims_for_deriver(&attacker_deriver, vec![victim_path.clone()], 60);

    let (tx, rx) = mpsc::channel(16);
    {
        use rio_proto::types::{PutPathBatchRequest, PutPathRequest, put_path_request};
        let mut info: PathInfo = info.into();
        let trailer = PutPathTrailer {
            nar_hash: std::mem::take(&mut info.nar_hash),
            nar_size: std::mem::take(&mut info.nar_size),
        };
        for msg in [
            put_path_request::Msg::Metadata(PutPathMetadata { info: Some(info) }),
            put_path_request::Msg::NarChunk(nar),
            put_path_request::Msg::Trailer(trailer),
        ] {
            tx.send(PutPathBatchRequest {
                output_index: 0,
                inner: Some(PutPathRequest { msg: Some(msg) }),
            })
            .await
            .unwrap();
        }
    }
    drop(tx);
    let mut req = tonic::Request::new(tokio_stream::wrappers::ReceiverStream::new(rx));
    req.metadata_mut()
        .insert(rio_proto::ASSIGNMENT_TOKEN_HEADER, token.parse().unwrap());
    let err = s
        .client
        .put_path_batch(req)
        .await
        .expect_err("batch output must be proof-gated too");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    Ok(())
}

// r[verify store.put.ia-deriver-proof+4]
/// The capability split: a SCHEDULER service token is no PutPath bypass
/// (probe rights only) — even though "rio-scheduler" stays in the
/// general allowlist.
#[tokio::test]
async fn scheduler_service_token_has_no_putpath_bypass() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    let path = test_store_path("sched-no-put");
    let (nar, _) = make_nar(b"scheduler should not write");
    let info = make_path_info_for_nar(&path, &nar);
    let err = put_path_with_header(
        &mut s.client,
        info,
        nar,
        rio_proto::SERVICE_TOKEN_HEADER,
        &sign_service("rio-scheduler", 60),
    )
    .await
    .expect_err("scheduler service token must not bypass PutPath");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(err.message().contains("no bypass for this method"));
    Ok(())
}

// ---------------------------------------------------------------------------
// C1c2 (bug_092): idempotency precedence over the deriver proof
// ---------------------------------------------------------------------------

// r[verify store.put.ia-deriver-proof+4]
/// Re-upload of an already-complete path under IA claims naming a
/// NON-resident deriver must succeed with `created: false`: the path is
/// complete, so the proof is never consulted. Pre-fix, the proof ran
/// before the idempotency check and rejected the re-upload with
/// PERMISSION_DENIED — the exact shape a builder produces when
/// FindMissingPaths raced a concurrent registration (bug_092).
#[tokio::test]
async fn reupload_after_claimsless_completion_returns_created_false() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    let (nar, _) = make_nar(b"complete content");
    let path = test_store_path("idem-prec");

    // Claimsless completion (gateway nix-copy shape).
    let info = make_path_info_for_nar(&path, &nar);
    assert!(
        put_path_with_header(
            &mut s.client,
            info,
            nar.clone(),
            rio_proto::SERVICE_TOKEN_HEADER,
            &sign_service("rio-gateway", 60),
        )
        .await?
    );

    // Worker re-upload: IA claims naming a deriver that is NOT resident.
    let absent_deriver = format!("/nix/store/{}-absent.drv", "d".repeat(32));
    let info = make_path_info_for_nar(&path, &nar);
    let token = sign_claims_for_deriver(&absent_deriver, vec![path.clone()], 60);
    let created = put_path_with_token(&mut s.client, info, nar, &token)
        .await
        .context("already-complete re-upload must not consult the proof")?;
    assert!(
        !created,
        "re-upload of a complete path reports created=false"
    );
    Ok(())
}

// r[verify store.put.ia-deriver-proof+4]
/// Batch sibling: an already-complete output inside a PutPathBatch is
/// skipped (created=false) without consulting the proof, even when the
/// claims name a non-resident deriver.
#[tokio::test]
async fn batch_already_complete_skips_proof() -> TestResult {
    use rio_proto::types::{PutPathBatchRequest, PutPathRequest, put_path_request};
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    let (nar, _) = make_nar(b"batch complete content");
    let path = test_store_path("batch-idem-prec");

    let info = make_path_info_for_nar(&path, &nar);
    assert!(
        put_path_with_header(
            &mut s.client,
            info,
            nar.clone(),
            rio_proto::SERVICE_TOKEN_HEADER,
            &sign_service("rio-gateway", 60),
        )
        .await?
    );

    let absent_deriver = format!("/nix/store/{}-absent2.drv", "e".repeat(32));
    let token = sign_claims_for_deriver(&absent_deriver, vec![path.clone()], 60);

    let mut info: PathInfo = make_path_info_for_nar(&path, &nar).into();
    let trailer = rio_proto::types::PutPathTrailer {
        nar_hash: std::mem::take(&mut info.nar_hash),
        nar_size: std::mem::take(&mut info.nar_size),
    };
    let (tx, rx) = mpsc::channel(8);
    for msg in [
        put_path_request::Msg::Metadata(rio_proto::types::PutPathMetadata { info: Some(info) }),
        put_path_request::Msg::NarChunk(nar),
        put_path_request::Msg::Trailer(trailer),
    ] {
        tx.send(PutPathBatchRequest {
            output_index: 0,
            inner: Some(PutPathRequest { msg: Some(msg) }),
        })
        .await
        .unwrap();
    }
    drop(tx);
    let mut req = tonic::Request::new(ReceiverStream::new(rx));
    req.metadata_mut()
        .insert(rio_proto::ASSIGNMENT_TOKEN_HEADER, token.parse().unwrap());
    let resp = s
        .client
        .put_path_batch(req)
        .await
        .context("already-complete batch output must not consult the proof")?
        .into_inner();
    assert_eq!(resp.created, vec![false]);
    Ok(())
}

// r[verify store.put.ia-deriver-proof+4]
/// A FRESH (not-yet-registered) path under unprovable claims is still
/// denied — and the denial releases the placeholder it claimed, leaving
/// no `'uploading'` squat behind.
#[tokio::test]
async fn fresh_unprovable_denied_and_releases_placeholder() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let (nar, _) = make_nar(b"fresh unprovable");
    let path = test_store_path("fresh-unprovable");
    let absent_deriver = format!("/nix/store/{}-absent3.drv", "f".repeat(32));
    let info = make_path_info_for_nar(&path, &nar);
    let token = sign_claims_for_deriver(&absent_deriver, vec![path.clone()], 60);
    let err = put_path_with_token(&mut s.client, info, nar, &token)
        .await
        .expect_err("unprovable fresh registration must be denied");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(
        err.message().contains("unverifiable"),
        "denial names the unverifiable closure: {}",
        err.message()
    );
    let n = poll_scalar_until::<i64>(&s.db.pool, "SELECT count(*)::bigint FROM manifests", 0).await;
    assert_eq!(n, 0, "denied upload must release its placeholder");
    Ok(())
}
