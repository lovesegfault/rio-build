//! Pull-unary identity posture (`pull_assignment` / `report_outcome`)
//! with the executor HMAC key configured: the metadata header is the
//! report's only carrier, the pull additionally accepts the body
//! token, and a missing/invalid credential is rejected closed.

use super::*;
use rio_proto::{ExecutorServiceClient, ExecutorServiceServer};

// r[verify sec.executor.identity-token+3]
/// With the executor HMAC key configured: `ReportOutcome` without the
/// `x-rio-executor-token` header is rejected `Unauthenticated`; with a
/// valid header it passes the identity gate and reaches the actor
/// (acked — unknown exec is the no-op arm); `PullAssignment` accepts
/// the body-only carrier (the self-contained fallback) and still
/// rejects a credential-less call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_unaries_enforce_executor_identity_when_hmac_configured() -> anyhow::Result<()> {
    use rio_auth::hmac::{ExecutorClaims, HmacKey};
    let (_db, mut grpc, _handle, _actor_task) = setup_grpc().await;
    let key = std::sync::Arc::new(HmacKey::from_key(
        b"test-key-32-bytes-long-here!!!!!".to_vec(),
    ));
    grpc.hmac_key = Some(std::sync::Arc::clone(&key));
    let router = tonic::transport::Server::builder().add_service(ExecutorServiceServer::new(grpc));
    let (addr, _server) = rio_test_support::grpc::spawn_grpc_server(router).await;
    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let mut client = ExecutorServiceClient::new(channel);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let token = key.sign(&ExecutorClaims {
        intent_id: "intent-pull".into(),
        kind: rio_proto::types::ExecutorKind::Builder as i32,
        expiry_unix: now + 600,
    });

    // 1. ReportOutcome without the header → Unauthenticated (the
    //    enforced posture has no body fallback for reports).
    let err = client
        .report_outcome(rio_proto::types::ReportOutcomeRequest {
            exec_id: uuid::Uuid::now_v7().to_string(),
            report: None,
            ..Default::default()
        })
        .await
        .expect_err("token-less ReportOutcome must be rejected when HMAC is configured");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);

    // 2. ReportOutcome with the metadata header → passes the identity
    //    gate and reaches the actor (unknown exec → acknowledged).
    let mut req = tonic::Request::new(rio_proto::types::ReportOutcomeRequest {
        exec_id: uuid::Uuid::now_v7().to_string(),
        report: None,
        ..Default::default()
    });
    req.metadata_mut()
        .insert(rio_proto::EXECUTOR_TOKEN_HEADER, token.parse()?);
    client
        .report_outcome(req)
        .await
        .expect("header-authenticated ReportOutcome reaches the actor");

    // 3. PullAssignment with the BODY-only token → accepted (the
    //    self-contained fallback carrier); the unknown intent answers
    //    Gone, which proves the call got past the identity gate.
    let resp = client
        .pull_assignment(rio_proto::types::PullAssignmentRequest {
            executor_token: token.clone(),
            intent_id: "intent-pull".into(),
            ..Default::default()
        })
        .await
        .expect("body-token PullAssignment is accepted")
        .into_inner();
    assert!(
        matches!(
            resp.outcome,
            Some(rio_proto::types::pull_assignment_response::Outcome::Gone(_))
        ),
        "the body-token pull reaches the admission kernel, got {resp:?}"
    );

    // 4. PullAssignment with neither carrier → Unauthenticated.
    let err = client
        .pull_assignment(rio_proto::types::PullAssignmentRequest {
            executor_token: String::new(),
            intent_id: "intent-pull".into(),
            ..Default::default()
        })
        .await
        .expect_err("credential-less PullAssignment must be rejected when HMAC is configured");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    Ok(())
}

// ── Materialization kind intake (the BC-1 identity rule) ──

// r[verify sched.materialize.job+2]
/// BC-1: a materialization pull MUST carry a per-replica executor
/// identity (`executor_instance` = the store pod name) — the identity
/// is what makes the kernel's one-winner arbitration per-replica. A
/// materialization-kind pull with an empty instance is rejected
/// `InvalidArgument` at the gRPC layer before any actor state is
/// touched. Build pulls are unaffected (the field stays ignored).
#[tokio::test]
async fn materialization_pull_with_empty_instance_rejected() -> anyhow::Result<()> {
    use rio_proto::ExecutorService;
    let (_db, grpc, _handle, _actor_task) = setup_grpc().await;
    let err = grpc
        .pull_assignment(Request::new(rio_proto::types::PullAssignmentRequest {
            executor_token: String::new(),
            intent_id: "drv-materialization-no-instance".into(),
            kind: rio_proto::types::AttemptKind::Materialization.into(),
            executor_instance: String::new(),
            resume_exec_id: String::new(),
            claim_nonce: String::new(),
            confirm_only: false,
        }))
        .await
        .expect_err("a materialization pull without executor_instance must be rejected");
    assert_eq!(
        err.code(),
        tonic::Code::InvalidArgument,
        "BC-1: the per-replica identity is mandatory for the materialization kind, got {err:?}"
    );

    // The same request as a BUILD pull (kind unset) is admitted and
    // answers Gone for the unknown intent — the materialization
    // fields are ignored for builds. The pull carries a token:
    // merged_bug_145's fail-closed hoist refuses ADMISSION to
    // token-less build pulls (no fence identity — the actor-level
    // polarity tests pin that refusal), so the as-built Gone answer
    // is asserted for a keyed pod, the production shape.
    let resp = grpc
        .pull_assignment(Request::new(rio_proto::types::PullAssignmentRequest {
            executor_token: "tok-grpc-build-pod".into(),
            intent_id: "drv-materialization-no-instance".into(),
            ..Default::default()
        }))
        .await
        .expect("build pulls are unaffected by the new fields")
        .into_inner();
    assert!(
        matches!(
            resp.outcome,
            Some(rio_proto::types::pull_assignment_response::Outcome::Gone(_))
        ),
        "build pull of an unknown intent answers Gone, got {resp:?}"
    );
    Ok(())
}

// ── Materialization authorization (security review: dormant ≠ unprotected) ──

// r[verify sched.materialize.job+2]
/// Phase A authorization: an executor token (the per-intent
/// builder/fetcher credential) never authorizes a materialization pull —
/// the kind-attested store credential is the Wave-4 store-executor
/// obligation, so until it exists every authenticated materialization
/// claim is rejected closed (PermissionDenied), on BOTH token carriers.
/// Build pulls with the same token are bit-identical as-built.
#[tokio::test]
async fn materialization_pull_with_executor_token_rejected() -> anyhow::Result<()> {
    use rio_auth::hmac::{ExecutorClaims, HmacKey};
    use rio_proto::ExecutorService;
    let (_db, mut grpc, _handle, _actor_task) = setup_grpc().await;
    let key = std::sync::Arc::new(HmacKey::from_key(
        b"test-key-32-bytes-long-here!!!!!".to_vec(),
    ));
    grpc.hmac_key = Some(std::sync::Arc::clone(&key));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let token = key.sign(&ExecutorClaims {
        intent_id: "drv-mat-authz".into(),
        kind: rio_proto::types::ExecutorKind::Builder as i32,
        expiry_unix: now + 600,
    });

    // Body-token carrier: a valid builder token requesting a
    // materialization pull → PermissionDenied (never admitted, never
    // parked — rejection is the closed posture).
    let err = grpc
        .pull_assignment(Request::new(rio_proto::types::PullAssignmentRequest {
            executor_token: token.clone(),
            intent_id: "drv-mat-authz".into(),
            kind: rio_proto::types::AttemptKind::Materialization.into(),
            executor_instance: "store-replica-0".into(),
            resume_exec_id: String::new(),
            claim_nonce: String::new(),
            confirm_only: false,
        }))
        .await
        .expect_err("a builder-kind executor token must not authorize a materialization pull");
    assert_eq!(
        err.code(),
        tonic::Code::PermissionDenied,
        "got {err:?} instead of PermissionDenied"
    );

    // Metadata carrier: same rejection.
    let mut req = Request::new(rio_proto::types::PullAssignmentRequest {
        executor_token: String::new(),
        intent_id: "drv-mat-authz".into(),
        kind: rio_proto::types::AttemptKind::Materialization.into(),
        executor_instance: "store-replica-0".into(),
        resume_exec_id: String::new(),
        claim_nonce: String::new(),
        confirm_only: false,
    });
    req.metadata_mut()
        .insert(rio_proto::EXECUTOR_TOKEN_HEADER, token.parse()?);
    let err = grpc
        .pull_assignment(req)
        .await
        .expect_err("metadata-carried builder token: same rejection");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);

    // The same token doing a BUILD pull is unaffected: it gets past the
    // identity gate and the unknown intent answers Gone (as-built).
    let resp = grpc
        .pull_assignment(Request::new(rio_proto::types::PullAssignmentRequest {
            executor_token: token.clone(),
            intent_id: "drv-mat-authz".into(),
            ..Default::default()
        }))
        .await
        .expect("build pulls with the same token are unaffected")
        .into_inner();
    assert!(
        matches!(
            resp.outcome,
            Some(rio_proto::types::pull_assignment_response::Outcome::Gone(_))
        ),
        "build pull reaches the admission kernel, got {resp:?}"
    );
    Ok(())
}

// r[verify sched.materialize.job+2]
/// Phase A authorization for the listing: executor tokens are per-intent
/// builder credentials — they do not authorize the fleet-wide
/// materialization-job listing (job descriptors carry cross-tenant drv
/// hashes and tenant ids). With HMAC configured, an executor token on
/// either carrier is rejected PermissionDenied; the store-service
/// credential that WILL authorize this is the Wave-4 obligation.
/// Dormant ≠ unprotected: the gate must be correct even while the list
/// is always empty.
#[tokio::test]
async fn list_materialization_jobs_with_executor_token_rejected() -> anyhow::Result<()> {
    use rio_auth::hmac::{ExecutorClaims, HmacKey};
    use rio_proto::ExecutorService;
    let (_db, mut grpc, _handle, _actor_task) = setup_grpc().await;
    let key = std::sync::Arc::new(HmacKey::from_key(
        b"test-key-32-bytes-long-here!!!!!".to_vec(),
    ));
    grpc.hmac_key = Some(std::sync::Arc::clone(&key));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let token = key.sign(&ExecutorClaims {
        intent_id: "drv-anything".into(),
        kind: rio_proto::types::ExecutorKind::Builder as i32,
        expiry_unix: now + 600,
    });

    // Metadata carrier.
    let mut req = Request::new(rio_proto::types::ListMaterializationJobsRequest {
        service_token: String::new(),
        limit: 16,
    });
    req.metadata_mut()
        .insert(rio_proto::EXECUTOR_TOKEN_HEADER, token.parse()?);
    let err = grpc
        .list_materialization_jobs(req)
        .await
        .expect_err("an executor token must not authorize the job listing");
    assert_eq!(
        err.code(),
        tonic::Code::PermissionDenied,
        "got {err:?} instead of PermissionDenied"
    );

    // Body carrier (service_token field carrying an executor token).
    let err = grpc
        .list_materialization_jobs(Request::new(
            rio_proto::types::ListMaterializationJobsRequest {
                service_token: token.clone(),
                limit: 16,
            },
        ))
        .await
        .expect_err("an executor token in the body carrier: same rejection");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);

    // Credential-less in HMAC mode → Unauthenticated (closed), same as
    // the other unaries.
    let err = grpc
        .list_materialization_jobs(Request::new(
            rio_proto::types::ListMaterializationJobsRequest {
                service_token: String::new(),
                limit: 16,
            },
        ))
        .await
        .expect_err("credential-less listing in HMAC mode is rejected");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    Ok(())
}

// r[verify sched.materialize.job+2]
/// BC-1 identity hygiene: `executor_instance` is interpolated into the
/// composite ExecutorId (`{intent}@{instance}`), so it must be a clean
/// DNS-1123 label — lowercase alphanumerics + interior hyphens, ≤63
/// chars. Anything else (uppercase, `@` injection, underscores, length)
/// is rejected InvalidArgument before any actor state is touched.
#[tokio::test]
async fn materialization_pull_instance_validated_as_dns_label() -> anyhow::Result<()> {
    use rio_proto::ExecutorService;
    let (_db, grpc, _handle, _actor_task) = setup_grpc().await;
    let too_long = "a".repeat(64);
    for bad in [
        "Store-Replica-0",
        "store@replica-0",
        "store_replica_0",
        "-leading-hyphen",
        "trailing-hyphen-",
        "dot.separated",
        too_long.as_str(),
    ] {
        let err = grpc
            .pull_assignment(Request::new(rio_proto::types::PullAssignmentRequest {
                executor_token: String::new(),
                intent_id: "drv-instance-validation".into(),
                kind: rio_proto::types::AttemptKind::Materialization.into(),
                executor_instance: bad.into(),
                resume_exec_id: String::new(),
                claim_nonce: String::new(),
                confirm_only: false,
            }))
            .await
            .expect_err("malformed executor_instance must be rejected");
        assert_eq!(
            err.code(),
            tonic::Code::InvalidArgument,
            "instance {bad:?} must be InvalidArgument, got {err:?}"
        );
    }
    // A valid DNS-1123 label is accepted (the unknown drv answers
    // Gone — the gate passed and the kernel answered).
    let resp = grpc
        .pull_assignment(Request::new(rio_proto::types::PullAssignmentRequest {
            executor_token: String::new(),
            intent_id: "drv-instance-validation".into(),
            kind: rio_proto::types::AttemptKind::Materialization.into(),
            executor_instance: "store-replica-0".into(),
            resume_exec_id: String::new(),
            claim_nonce: String::new(),
            confirm_only: false,
        }))
        .await
        .expect("a valid DNS-1123 instance is accepted")
        .into_inner();
    assert!(
        matches!(
            resp.outcome,
            Some(rio_proto::types::pull_assignment_response::Outcome::Gone(_))
        ),
        "valid instance: the gate passes and the kernel answers Gone for an unknown drv, got {resp:?}"
    );
    Ok(())
}

// r[verify sched.executor.input-bounds+2]
/// Identity hygiene (security review, separator confusion): `intent_id`
/// is the whole pulling identity for build pulls and the left half of
/// the composite materialization ExecutorId (`{intent}@{instance}`). A
/// literal `@` inside it lets two distinct work items collide on one
/// identity string — a build pull for intent `a@b` and a
/// materialization pull for intent `a` on replica `b` are
/// indistinguishable to the kernel's same-identity re-delivery arm.
/// Intent ids are scheduler-generated (drv hashes), so a `@` is never
/// legitimate; the gRPC layer rejects it closed on every carrier and
/// kind, before any actor state is touched.
#[tokio::test]
async fn pull_intent_id_with_separator_rejected() -> anyhow::Result<()> {
    use rio_auth::hmac::{ExecutorClaims, HmacKey};
    use rio_proto::ExecutorService;

    // 1. The finding's exact shape: a VERIFIED executor token whose
    //    claims carry an `@` intent. The attestation match forces the
    //    request to repeat the claims' intent, so the ambiguous string
    //    is exactly what would become the pulling identity.
    let (_db, mut grpc, _handle, _actor_task) = setup_grpc().await;
    let key = std::sync::Arc::new(HmacKey::from_key(
        b"test-key-32-bytes-long-here!!!!!".to_vec(),
    ));
    grpc.hmac_key = Some(std::sync::Arc::clone(&key));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let ambiguous = "drv-a@store-replica-0";
    let token = key.sign(&ExecutorClaims {
        intent_id: ambiguous.into(),
        kind: rio_proto::types::ExecutorKind::Builder as i32,
        expiry_unix: now + 600,
    });
    let err = grpc
        .pull_assignment(Request::new(rio_proto::types::PullAssignmentRequest {
            executor_token: token,
            intent_id: ambiguous.into(),
            kind: rio_proto::types::AttemptKind::Build.into(),
            executor_instance: String::new(),
            resume_exec_id: String::new(),
            claim_nonce: String::new(),
            confirm_only: false,
        }))
        .await
        .expect_err("a build pull whose attested intent contains '@' must be rejected");
    assert_eq!(
        err.code(),
        tonic::Code::InvalidArgument,
        "expected InvalidArgument for '@' in intent_id, got {err:?}"
    );

    // 2. Dev mode, materialization kind: the same string in the raw
    //    request field would be interpolated as the composite's left
    //    half — `a@b` + `c` collides with `a` + `b@c`-style splits.
    //    The hygiene does not depend on a token existing.
    let (_db2, grpc2, _handle2, _actor_task2) = setup_grpc().await;
    let err = grpc2
        .pull_assignment(Request::new(rio_proto::types::PullAssignmentRequest {
            executor_token: String::new(),
            intent_id: ambiguous.into(),
            kind: rio_proto::types::AttemptKind::Materialization.into(),
            executor_instance: "store-replica-0".into(),
            resume_exec_id: String::new(),
            claim_nonce: String::new(),
            confirm_only: false,
        }))
        .await
        .expect_err("a materialization pull whose intent contains '@' must be rejected");
    assert_eq!(
        err.code(),
        tonic::Code::InvalidArgument,
        "expected InvalidArgument for '@' in intent_id, got {err:?}"
    );
    Ok(())
}

// ── The store-service credential (Wave-4 security obligation:
//    the kind-attested materialization credential) ──

/// Sign an INSTANCE-LESS `ServiceClaims` credential with `key` — the
/// pre-T-5.1 wire shape (and the shape every non-store caller still
/// mints: gateway PutPath, controller, rio-cli).
fn store_service_token(key: &rio_auth::hmac::HmacKey, caller: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs();
    key.sign(&rio_auth::hmac::ServiceClaims {
        caller: caller.to_string(),
        expiry_unix: now + 60,
        instance: None,
    })
}

/// Sign an INSTANCE-BOUND `ServiceClaims` store credential — what the
/// store's materialization client mints since T-5.1 (the replica
/// identity is inside the signed body, so the scheduler VERIFIES the
/// `executor_instance` a claim asserts instead of trusting it).
fn store_service_token_bound(
    key: &rio_auth::hmac::HmacKey,
    caller: &str,
    instance: &str,
) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs();
    key.sign(&rio_auth::hmac::ServiceClaims {
        caller: caller.to_string(),
        expiry_unix: now + 60,
        instance: Some(instance.to_string()),
    })
}

/// An empty materialization Success outcome payload.
fn mat_success_outcome() -> rio_proto::types::MaterializationOutcome {
    rio_proto::types::MaterializationOutcome {
        outcome: Some(rio_proto::types::materialization_outcome::Outcome::Success(
            rio_proto::types::materialization_outcome::Success {
                ingested_paths: vec![],
                verified_paths: vec![],
                verified_tenants: vec![],
            },
        )),
    }
}

// r[verify sched.materialize.job+2]
/// Wave-4 kind-attested credential (T-5.1: now also INSTANCE-BOUND —
/// the enumerated Phase A assertion change): `ServiceClaims{caller=
/// "rio-store", instance: Some(<replica>)}` signed with the SERVICE
/// HMAC key (the separate x-rio-service-token family) authorizes
/// exactly the materialization operations — ListMaterializationJobs
/// (both carriers), kind=MATERIALIZATION PullAssignment, and
/// materialization ReportOutcome — while the executor HMAC posture
/// stays fully enforced. The empty-state answers: empty list, Gone,
/// acknowledged-and-ignored.
#[tokio::test]
async fn materialization_ops_accept_store_service_credential() -> anyhow::Result<()> {
    use rio_auth::hmac::HmacKey;
    use rio_proto::ExecutorService;
    let (_db, mut grpc, _handle, _actor_task) = setup_grpc().await;
    // The production posture: BOTH key families configured, distinct keys.
    let executor_key = std::sync::Arc::new(HmacKey::from_key(
        b"executor-key-32-bytes-long!!!!!!".to_vec(),
    ));
    let service_key = std::sync::Arc::new(HmacKey::from_key(
        b"service-key-32-bytes-long-here!!".to_vec(),
    ));
    grpc.hmac_key = Some(executor_key);
    grpc.service_verifier = Some(std::sync::Arc::clone(&service_key));
    let store_token = store_service_token_bound(&service_key, "rio-store", "store-replica-0");

    // 1. ListMaterializationJobs, metadata carrier → flag-off empty list.
    let mut req = Request::new(rio_proto::types::ListMaterializationJobsRequest {
        service_token: String::new(),
        limit: 16,
    });
    req.metadata_mut()
        .insert(rio_common::grpc::SERVICE_TOKEN_HEADER, store_token.parse()?);
    let resp = grpc
        .list_materialization_jobs(req)
        .await
        .expect("the store-service credential authorizes the listing (metadata carrier)")
        .into_inner();
    assert!(resp.jobs.is_empty(), "no jobs: the listing is empty");

    // 2. ListMaterializationJobs, body carrier → same.
    let resp = grpc
        .list_materialization_jobs(Request::new(
            rio_proto::types::ListMaterializationJobsRequest {
                service_token: store_token.clone(),
                limit: 16,
            },
        ))
        .await
        .expect("the store-service credential authorizes the listing (body carrier)")
        .into_inner();
    assert!(resp.jobs.is_empty());

    // 3. kind=MATERIALIZATION PullAssignment with the credential →
    //    Gone for the unknown drv, never a rejection.
    let mut req = Request::new(rio_proto::types::PullAssignmentRequest {
        executor_token: String::new(),
        intent_id: "drv-store-credential".into(),
        kind: rio_proto::types::AttemptKind::Materialization.into(),
        executor_instance: "store-replica-0".into(),
        resume_exec_id: String::new(),
        claim_nonce: String::new(),
        confirm_only: false,
    });
    req.metadata_mut()
        .insert(rio_common::grpc::SERVICE_TOKEN_HEADER, store_token.parse()?);
    let resp = grpc
        .pull_assignment(req)
        .await
        .expect("the store-service credential authorizes the materialization pull")
        .into_inner();
    assert!(
        matches!(
            resp.outcome,
            Some(rio_proto::types::pull_assignment_response::Outcome::Gone(_))
        ),
        "the credential authorizes; the unknown drv answers Gone, got {resp:?}"
    );

    // 4. Materialization ReportOutcome with the credential → reaches the
    //    actor (unknown exec → acknowledged-and-ignored), never rejected.
    let mut req = Request::new(rio_proto::types::ReportOutcomeRequest {
        exec_id: uuid::Uuid::now_v7().to_string(),
        report: None,
        materialization_outcome: Some(mat_success_outcome()),
    });
    req.metadata_mut()
        .insert(rio_common::grpc::SERVICE_TOKEN_HEADER, store_token.parse()?);
    grpc.report_outcome(req)
        .await
        .expect("the store-service credential authorizes the materialization report");
    Ok(())
}

// r[verify sched.materialize.job+2]
/// The credential is exactly `caller="rio-store"`: another control-plane
/// caller's service token (e.g. the gateway's) does NOT authorize
/// materialization operations; executor tokens stay rejected (the Wave-3
/// pins, unchanged); a build report presenting only the store credential
/// is still rejected (build reports require the per-intent executor
/// token).
#[tokio::test]
async fn store_service_credential_scoping_is_exact() -> anyhow::Result<()> {
    use rio_auth::hmac::HmacKey;
    use rio_proto::ExecutorService;
    let (_db, mut grpc, _handle, _actor_task) = setup_grpc().await;
    let executor_key = std::sync::Arc::new(HmacKey::from_key(
        b"executor-key-32-bytes-long!!!!!!".to_vec(),
    ));
    let service_key = std::sync::Arc::new(HmacKey::from_key(
        b"service-key-32-bytes-long-here!!".to_vec(),
    ));
    grpc.hmac_key = Some(executor_key);
    grpc.service_verifier = Some(std::sync::Arc::clone(&service_key));

    // (a) Wrong caller (a real, validly-signed gateway token) → rejected.
    let gateway_token = store_service_token(&service_key, "rio-gateway");
    let mut req = Request::new(rio_proto::types::ListMaterializationJobsRequest {
        service_token: String::new(),
        limit: 16,
    });
    req.metadata_mut().insert(
        rio_common::grpc::SERVICE_TOKEN_HEADER,
        gateway_token.parse()?,
    );
    let err = grpc
        .list_materialization_jobs(req)
        .await
        .expect_err("a non-store service caller must not authorize the listing");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);

    // (b) Wrong caller on the materialization pull → rejected.
    let mut req = Request::new(rio_proto::types::PullAssignmentRequest {
        executor_token: String::new(),
        intent_id: "drv-wrong-caller".into(),
        kind: rio_proto::types::AttemptKind::Materialization.into(),
        executor_instance: "store-replica-0".into(),
        resume_exec_id: String::new(),
        claim_nonce: String::new(),
        confirm_only: false,
    });
    req.metadata_mut().insert(
        rio_common::grpc::SERVICE_TOKEN_HEADER,
        gateway_token.parse()?,
    );
    let err = grpc
        .pull_assignment(req)
        .await
        .expect_err("a non-store service caller must not authorize a materialization pull");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);

    // (c) A token signed with the WRONG key (the executor key) in the
    //     service-token header → rejected (the key families are not
    //     interchangeable).
    let executor_signed =
        store_service_token(grpc.hmac_key.as_ref().expect("set above"), "rio-store");
    let mut req = Request::new(rio_proto::types::ListMaterializationJobsRequest {
        service_token: String::new(),
        limit: 16,
    });
    req.metadata_mut().insert(
        rio_common::grpc::SERVICE_TOKEN_HEADER,
        executor_signed.parse()?,
    );
    let err = grpc
        .list_materialization_jobs(req)
        .await
        .expect_err("a store token signed with the executor key must not verify");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);

    // (d) A BUILD report carrying only the store credential (no executor
    //     token) → still Unauthenticated: build reports require the
    //     per-intent executor identity, the store credential is not a
    //     substitute for it.
    let store_token = store_service_token(&service_key, "rio-store");
    let mut req = Request::new(rio_proto::types::ReportOutcomeRequest {
        exec_id: uuid::Uuid::now_v7().to_string(),
        report: Some(rio_proto::types::CompletionReport::default()),
        materialization_outcome: None,
    });
    req.metadata_mut()
        .insert(rio_common::grpc::SERVICE_TOKEN_HEADER, store_token.parse()?);
    let err = grpc
        .report_outcome(req)
        .await
        .expect_err("a build report must still require the executor token");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);

    // (e) Half-configured deployment (executor HMAC, no service
    //     verifier): the store credential cannot be verified → closed,
    //     and credential-less stays Unauthenticated (the Wave-3 pin).
    grpc.service_verifier = None;
    let mut req = Request::new(rio_proto::types::ListMaterializationJobsRequest {
        service_token: String::new(),
        limit: 16,
    });
    req.metadata_mut()
        .insert(rio_common::grpc::SERVICE_TOKEN_HEADER, store_token.parse()?);
    let err = grpc
        .list_materialization_jobs(req)
        .await
        .expect_err("a store token without a configured service verifier must not authorize");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    Ok(())
}

// ── T-5.1 (Phase B): the instance token-claim binding ──────────────────────

// r[verify sched.materialize.job+2]
/// T-5.1 (security obligation 1, red-first): the scheduler VERIFIES the
/// `executor_instance` a materialization claim asserts against the
/// instance bound INSIDE the signed store-service credential — a
/// compromised or misconfigured replica cannot claim under another
/// replica's identity by lying in the request field.
///
///   token instance = "store-a", request executor_instance = "store-b"
///   → PermissionDenied("instance claim mismatch")
///
/// The Phase A DNS-1123 validation of the request field stays (defense
/// in depth); the CLAIM is now the authority.
#[tokio::test]
async fn materialization_claim_with_mismatched_instance_rejected() -> anyhow::Result<()> {
    use rio_auth::hmac::HmacKey;
    use rio_proto::ExecutorService;
    let (_db, mut grpc, _handle, _actor_task) = setup_grpc().await;
    let executor_key = std::sync::Arc::new(HmacKey::from_key(
        b"executor-key-32-bytes-long!!!!!!".to_vec(),
    ));
    let service_key = std::sync::Arc::new(HmacKey::from_key(
        b"service-key-32-bytes-long-here!!".to_vec(),
    ));
    grpc.hmac_key = Some(executor_key);
    grpc.service_verifier = Some(std::sync::Arc::clone(&service_key));

    // The token attests replica "store-a"; the request asserts "store-b".
    let token_a = store_service_token_bound(&service_key, "rio-store", "store-a");
    let mut req = Request::new(rio_proto::types::PullAssignmentRequest {
        executor_token: String::new(),
        intent_id: "drv-instance-mismatch".into(),
        kind: rio_proto::types::AttemptKind::Materialization.into(),
        executor_instance: "store-b".into(),
        resume_exec_id: String::new(),
        claim_nonce: String::new(),
        confirm_only: false,
    });
    req.metadata_mut()
        .insert(rio_common::grpc::SERVICE_TOKEN_HEADER, token_a.parse()?);
    let err = grpc
        .pull_assignment(req)
        .await
        .expect_err("a claim asserting a different instance than its token must be rejected");
    assert_eq!(
        err.code(),
        tonic::Code::PermissionDenied,
        "instance mismatch → PermissionDenied, got {err:?}"
    );
    assert!(
        err.message().contains("instance claim mismatch"),
        "the rejection names the mismatch, got: {}",
        err.message()
    );

    // The same token claiming AS "store-a" (claim == request) is
    // admitted: the unknown drv answers Gone, which proves the gate
    // passed and the kernel answered.
    let mut req = Request::new(rio_proto::types::PullAssignmentRequest {
        executor_token: String::new(),
        intent_id: "drv-instance-mismatch".into(),
        kind: rio_proto::types::AttemptKind::Materialization.into(),
        executor_instance: "store-a".into(),
        resume_exec_id: String::new(),
        claim_nonce: String::new(),
        confirm_only: false,
    });
    req.metadata_mut()
        .insert(rio_common::grpc::SERVICE_TOKEN_HEADER, token_a.parse()?);
    let resp = grpc
        .pull_assignment(req)
        .await
        .expect("a claim whose instance matches its token's binding is admitted")
        .into_inner();
    assert!(
        matches!(
            resp.outcome,
            Some(rio_proto::types::pull_assignment_response::Outcome::Gone(_))
        ),
        "matching instance: the kernel answers Gone for an unknown drv, got {resp:?}"
    );
    Ok(())
}

// r[verify sched.materialize.job+2]
/// T-5.1 privilege narrowing (red-first): an INSTANCE-LESS ServiceClaims
/// token — the gateway-PutPath-style credential every non-store caller
/// mints — no longer authorizes materialization operations. The work
/// surfaces (claim, listing, outcome report) require the instance-bound
/// form; only the display-only progress relay keeps accepting the
/// fleet-level credential.
#[tokio::test]
async fn materialization_claim_without_instance_claim_rejected() -> anyhow::Result<()> {
    use rio_auth::hmac::HmacKey;
    use rio_proto::ExecutorService;
    let (_db, mut grpc, _handle, _actor_task) = setup_grpc().await;
    let executor_key = std::sync::Arc::new(HmacKey::from_key(
        b"executor-key-32-bytes-long!!!!!!".to_vec(),
    ));
    let service_key = std::sync::Arc::new(HmacKey::from_key(
        b"service-key-32-bytes-long-here!!".to_vec(),
    ));
    grpc.hmac_key = Some(executor_key);
    grpc.service_verifier = Some(std::sync::Arc::clone(&service_key));
    // A validly-signed, correctly-callered, but INSTANCE-LESS token.
    let unbound = store_service_token(&service_key, "rio-store");

    // (a) The materialization claim → PermissionDenied.
    let mut req = Request::new(rio_proto::types::PullAssignmentRequest {
        executor_token: String::new(),
        intent_id: "drv-unbound-token".into(),
        kind: rio_proto::types::AttemptKind::Materialization.into(),
        executor_instance: "store-replica-0".into(),
        resume_exec_id: String::new(),
        claim_nonce: String::new(),
        confirm_only: false,
    });
    req.metadata_mut()
        .insert(rio_common::grpc::SERVICE_TOKEN_HEADER, unbound.parse()?);
    let err = grpc
        .pull_assignment(req)
        .await
        .expect_err("an instance-less service token must not authorize a materialization claim");
    assert_eq!(
        err.code(),
        tonic::Code::PermissionDenied,
        "instance-less claim → PermissionDenied, got {err:?}"
    );

    // (b) The job listing → PermissionDenied (both carriers).
    let mut req = Request::new(rio_proto::types::ListMaterializationJobsRequest {
        service_token: String::new(),
        limit: 16,
    });
    req.metadata_mut()
        .insert(rio_common::grpc::SERVICE_TOKEN_HEADER, unbound.parse()?);
    let err = grpc
        .list_materialization_jobs(req)
        .await
        .expect_err("an instance-less service token must not authorize the job listing");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    let err = grpc
        .list_materialization_jobs(Request::new(
            rio_proto::types::ListMaterializationJobsRequest {
                service_token: unbound.clone(),
                limit: 16,
            },
        ))
        .await
        .expect_err("body-carried instance-less token: same rejection");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);

    // (c) The materialization outcome report → PermissionDenied.
    let mut req = Request::new(rio_proto::types::ReportOutcomeRequest {
        exec_id: uuid::Uuid::now_v7().to_string(),
        report: None,
        materialization_outcome: Some(mat_success_outcome()),
    });
    req.metadata_mut()
        .insert(rio_common::grpc::SERVICE_TOKEN_HEADER, unbound.parse()?);
    let err = grpc
        .report_outcome(req)
        .await
        .expect_err("an instance-less service token must not authorize an outcome report");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);

    // (d) The display-only progress relay keeps accepting the
    //     fleet-level credential (no state effect to bind — and the
    //     Phase A progress battery stays byte-identical).
    let mut req = Request::new(rio_proto::types::ReportMaterializationProgressRequest {
        exec_id: uuid::Uuid::now_v7().to_string(),
        bytes_done: 1,
        bytes_expected: 2,
        upstream_uri: String::new(),
    });
    req.metadata_mut()
        .insert(rio_common::grpc::SERVICE_TOKEN_HEADER, unbound.parse()?);
    grpc.report_materialization_progress(req)
        .await
        .expect("the display-only progress relay accepts the fleet-level credential");

    // (e) An instance-bound token presented to the NON-materialization
    //     surfaces is unaffected: a build report still requires the
    //     executor token (Unauthenticated, not a new acceptance) — the
    //     binding narrows materialization privileges, it grants nothing.
    let bound = store_service_token_bound(&service_key, "rio-store", "store-replica-0");
    let mut req = Request::new(rio_proto::types::ReportOutcomeRequest {
        exec_id: uuid::Uuid::now_v7().to_string(),
        report: Some(rio_proto::types::CompletionReport::default()),
        materialization_outcome: None,
    });
    req.metadata_mut()
        .insert(rio_common::grpc::SERVICE_TOKEN_HEADER, bound.parse()?);
    let err = grpc
        .report_outcome(req)
        .await
        .expect_err("build reports still require the per-intent executor token");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    Ok(())
}

// r[verify sched.materialize.job+2]
/// Defense in depth, symmetric with the pull-side kind rejection: an
/// EXECUTOR-authenticated ReportOutcome carrying a materialization
/// outcome is rejected PermissionDenied — a builder pod that somehow
/// learned a materialization attempt's exec_id cannot consume it. The
/// same payload under the store-service credential is accepted.
#[tokio::test]
async fn executor_token_report_with_materialization_outcome_rejected() -> anyhow::Result<()> {
    use rio_auth::hmac::{ExecutorClaims, HmacKey};
    use rio_proto::ExecutorService;
    let (_db, mut grpc, _handle, _actor_task) = setup_grpc().await;
    let executor_key = std::sync::Arc::new(HmacKey::from_key(
        b"executor-key-32-bytes-long!!!!!!".to_vec(),
    ));
    let service_key = std::sync::Arc::new(HmacKey::from_key(
        b"service-key-32-bytes-long-here!!".to_vec(),
    ));
    grpc.hmac_key = Some(std::sync::Arc::clone(&executor_key));
    grpc.service_verifier = Some(std::sync::Arc::clone(&service_key));

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let executor_token = executor_key.sign(&ExecutorClaims {
        intent_id: "drv-cross-kind".into(),
        kind: rio_proto::types::ExecutorKind::Builder as i32,
        expiry_unix: now + 600,
    });

    // Executor-authenticated materialization report → PermissionDenied.
    let mut req = Request::new(rio_proto::types::ReportOutcomeRequest {
        exec_id: uuid::Uuid::now_v7().to_string(),
        report: None,
        materialization_outcome: Some(mat_success_outcome()),
    });
    req.metadata_mut()
        .insert(rio_proto::EXECUTOR_TOKEN_HEADER, executor_token.parse()?);
    let err = grpc
        .report_outcome(req)
        .await
        .expect_err("an executor token must not authorize a materialization outcome report");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);

    // The same executor token reporting a BUILD outcome still works
    // (unknown exec → acknowledged): the build path is untouched.
    let mut req = Request::new(rio_proto::types::ReportOutcomeRequest {
        exec_id: uuid::Uuid::now_v7().to_string(),
        report: Some(rio_proto::types::CompletionReport::default()),
        materialization_outcome: None,
    });
    req.metadata_mut()
        .insert(rio_proto::EXECUTOR_TOKEN_HEADER, executor_token.parse()?);
    grpc.report_outcome(req)
        .await
        .expect("build reports with executor tokens are unaffected");
    Ok(())
}

// r[verify sched.materialize.job+2]
/// The third dormant materialization RPC (the Phase-A ack-and-drop
/// progress stub) carries the same identity posture as its siblings —
/// dormant ≠ unprotected. With the HMAC posture configured:
/// credential-less → Unauthenticated (closed); an executor token (the
/// per-intent builder/fetcher credential) → PermissionDenied; the
/// kind-attested store-service credential → acknowledged (the Phase-A
/// ack-and-drop, unchanged after authentication). Full dev mode stays
/// open, like every other ExecutorService unary.
#[tokio::test]
async fn report_materialization_progress_requires_store_credential() -> anyhow::Result<()> {
    use rio_auth::hmac::{ExecutorClaims, HmacKey};
    use rio_proto::ExecutorService;
    let (_db, mut grpc, _handle, _actor_task) = setup_grpc().await;
    // The production posture: BOTH key families configured, distinct keys.
    let executor_key = std::sync::Arc::new(HmacKey::from_key(
        b"executor-key-32-bytes-long!!!!!!".to_vec(),
    ));
    let service_key = std::sync::Arc::new(HmacKey::from_key(
        b"service-key-32-bytes-long-here!!".to_vec(),
    ));
    grpc.hmac_key = Some(std::sync::Arc::clone(&executor_key));
    grpc.service_verifier = Some(std::sync::Arc::clone(&service_key));

    let progress_req = || rio_proto::types::ReportMaterializationProgressRequest {
        exec_id: uuid::Uuid::now_v7().to_string(),
        bytes_done: 1024,
        bytes_expected: 4096,
        upstream_uri: "https://cache.example.org/nar/0000000000000000".into(),
    };

    // 1. Credential-less in the configured posture → Unauthenticated
    //    (closed), same as the sibling materialization unaries.
    let err = grpc
        .report_materialization_progress(Request::new(progress_req()))
        .await
        .expect_err("a credential-less progress report must be rejected when HMAC is configured");
    assert_eq!(
        err.code(),
        tonic::Code::Unauthenticated,
        "got {err:?} instead of Unauthenticated"
    );

    // 2. An executor token (the per-intent builder credential) never
    //    authorizes a materialization progress report.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let executor_token = executor_key.sign(&ExecutorClaims {
        intent_id: "drv-progress-authz".into(),
        kind: rio_proto::types::ExecutorKind::Builder as i32,
        expiry_unix: now + 600,
    });
    let mut req = Request::new(progress_req());
    req.metadata_mut()
        .insert(rio_proto::EXECUTOR_TOKEN_HEADER, executor_token.parse()?);
    let err = grpc
        .report_materialization_progress(req)
        .await
        .expect_err("an executor token must not authorize a materialization progress report");
    assert_eq!(
        err.code(),
        tonic::Code::PermissionDenied,
        "got {err:?} instead of PermissionDenied"
    );

    // 3. The store-service credential → acknowledged (the Phase-A
    //    ack-and-drop, unchanged after authentication).
    let store_token = store_service_token(&service_key, "rio-store");
    let mut req = Request::new(progress_req());
    req.metadata_mut()
        .insert(rio_common::grpc::SERVICE_TOKEN_HEADER, store_token.parse()?);
    grpc.report_materialization_progress(req)
        .await
        .expect("the store-service credential authorizes the progress report");

    // 4. Full dev mode (neither key family configured) stays open —
    //    the Phase-A ack-and-drop needs no credential, like every
    //    other ExecutorService unary in dev mode.
    let (_db2, grpc2, _handle2, _actor_task2) = setup_grpc().await;
    grpc2
        .report_materialization_progress(Request::new(progress_req()))
        .await
        .expect("dev-mode progress reports stay open (ack-and-drop)");
    Ok(())
}

// ── T-6.2: the wire-level flag-on lifecycle (review finding dormancy-5;
//    PD-14 as amended) ────────────────────────────────────────────────────

// r[verify sched.materialize.job+2]
// r[verify store.materialize.executor+5]
/// The store-to-scheduler seam, flag-on, through the REAL wire: a real
/// in-process tonic `ExecutorService` server with BOTH HMAC key families
/// configured (the production posture), driven by a real tonic client
/// presenting the kind-attested store-service credential
/// (`ServiceClaims{caller="rio-store"}` signed with the service key,
/// carried on `x-rio-service-token` — stop condition 9's assumption
/// converted into checked fact):
///
///   ListMaterializationJobs → the flag-on job is listed
///   → PullAssignment(kind=MATERIALIZATION, executor_instance="store-test-0")
///     → the assignment delivers; the fenced mint persisted
///       attempt_kind='materialization'
///   → PullAssignment with EMPTY executor_instance → InvalidArgument
///     (the BC-1 identity-mandatory rule, proven flag-on)
///   → ReportOutcome(materialization_outcome: Success) → consumption
///     (node Completed, build Succeeded, job resolved_success)
///
/// PD-14 (amended by dormancy-5): the store's *role* is played by this
/// test through a real client — the real store executor against the
/// real scheduler is Phase B's VM matrix — but the wire and auth seam
/// Phase B depends on are real here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flag_on_materialization_lifecycle_through_grpc() -> anyhow::Result<()> {
    use crate::actor::tests::{
        barrier, expect_drv, merge_dag, query_status, setup_actor_configured, test_store_path, tick,
    };
    use crate::state::DerivationStatus;
    use rio_auth::hmac::HmacKey;

    // The flag-on actor + MockStore (the probe target the job-creation
    // gate consults).
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, _actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |_cfg, p| {
            p.service_signer = Some(std::sync::Arc::new(rio_auth::hmac::HmacSigner::from_key(
                b"mock-store-harness-service-key32".to_vec(),
            )));
        });

    // The production auth posture: BOTH key families, distinct keys.
    let executor_key = std::sync::Arc::new(HmacKey::from_key(
        b"executor-key-32-bytes-long!!!!!!".to_vec(),
    ));
    let service_key = std::sync::Arc::new(HmacKey::from_key(
        b"service-key-32-bytes-long-here!!".to_vec(),
    ));
    let mut grpc = SchedulerGrpc::new_for_tests(handle.clone());
    grpc.hmac_key = Some(executor_key);
    grpc.service_verifier = Some(std::sync::Arc::clone(&service_key));
    // T-5.1 (enumerated change): the credential is instance-bound to the
    // replica identity this test claims under ("store-test-0").
    let store_token = store_service_token_bound(&service_key, "rio-store", "store-test-0");

    // The REAL wire: in-process tonic server + connected client.
    let router = tonic::transport::Server::builder().add_service(ExecutorServiceServer::new(grpc));
    let (addr, _server) = rio_test_support::grpc::spawn_grpc_server(router).await;
    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let mut client = ExecutorServiceClient::new(channel);

    // Drive the scheduler to a claimable job through the actor (merge,
    // then the dispatch-probe partition against the substitutable
    // answer) — the same path the keystone actor test pins.
    let out = test_store_path("mat-wire-out");
    let mut node = make_node("mat-wire");
    node.expected_output_paths = vec![out.clone()];
    node.wanted_output_names = vec!["out".into()];
    let build_id = uuid::Uuid::new_v4();
    merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    barrier(&handle).await;
    store.state.substitutable.write().unwrap().push(out.clone());
    tick(&handle).await?;

    // 1. ListMaterializationJobs through the wire → the job is listed.
    let mut req = Request::new(rio_proto::types::ListMaterializationJobsRequest {
        service_token: String::new(),
        limit: 16,
    });
    req.metadata_mut()
        .insert(rio_common::grpc::SERVICE_TOKEN_HEADER, store_token.parse()?);
    let resp = client.list_materialization_jobs(req).await?.into_inner();
    assert_eq!(
        resp.jobs.len(),
        1,
        "the flag-on job is listed through the wire, got {:?}",
        resp.jobs
    );
    assert_eq!(resp.jobs[0].drv_hash, "mat-wire");
    assert_eq!(resp.jobs[0].origin, "cache_opportunity");

    // 2. The BC-1 identity-mandatory rule, proven flag-on through the
    //    wire: an EMPTY executor_instance is InvalidArgument.
    let mut req = Request::new(rio_proto::types::PullAssignmentRequest {
        executor_token: String::new(),
        intent_id: "mat-wire".into(),
        kind: rio_proto::types::AttemptKind::Materialization.into(),
        executor_instance: String::new(),
        resume_exec_id: String::new(),
        claim_nonce: String::new(),
        confirm_only: false,
    });
    req.metadata_mut()
        .insert(rio_common::grpc::SERVICE_TOKEN_HEADER, store_token.parse()?);
    let err = client
        .pull_assignment(req)
        .await
        .expect_err("an empty executor_instance must be rejected flag-on too");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    // 3. The claim through the wire → the assignment delivers; the
    //    fenced mint persisted the materialization work class.
    let mut req = Request::new(rio_proto::types::PullAssignmentRequest {
        executor_token: String::new(),
        intent_id: "mat-wire".into(),
        kind: rio_proto::types::AttemptKind::Materialization.into(),
        executor_instance: "store-test-0".into(),
        resume_exec_id: String::new(),
        claim_nonce: String::new(),
        confirm_only: false,
    });
    req.metadata_mut()
        .insert(rio_common::grpc::SERVICE_TOKEN_HEADER, store_token.parse()?);
    let resp = client.pull_assignment(req).await?.into_inner();
    let assignment = match resp.outcome {
        Some(rio_proto::types::pull_assignment_response::Outcome::Assignment(a)) => a,
        other => panic!("the flag-on claim through the wire must deliver, got {other:?}"),
    };
    let exec_id = assignment.exec_id.clone();
    let kind: String =
        sqlx::query_scalar("SELECT attempt_kind FROM drv_executions WHERE exec_id = $1")
            .bind(uuid::Uuid::parse_str(&exec_id)?)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        kind, "materialization",
        "the wire-level mint persists the work class"
    );

    // 4. ReportOutcome(Success) through the wire → consumption.
    store.seed_with_content(&out, b"materialized");
    let mut req = Request::new(rio_proto::types::ReportOutcomeRequest {
        exec_id: exec_id.clone(),
        report: None,
        materialization_outcome: Some(rio_proto::types::MaterializationOutcome {
            outcome: Some(rio_proto::types::materialization_outcome::Outcome::Success(
                rio_proto::types::materialization_outcome::Success {
                    ingested_paths: vec![out.clone()],
                    verified_paths: vec![],
                    verified_tenants: vec![],
                },
            )),
        }),
    });
    req.metadata_mut()
        .insert(rio_common::grpc::SERVICE_TOKEN_HEADER, store_token.parse()?);
    client
        .report_outcome(req)
        .await
        .expect("the store-credential Success report consumes the attempt");

    // 5. The consumption's effects, observed through the actor: node
    //    Completed, build Succeeded, job resolved_success.
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "mat-wire").await.status,
        DerivationStatus::Completed,
        "the wire-level Success consumption completes the node"
    );
    let st = query_status(&handle, build_id).await?;
    assert_eq!(
        st.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "the creating build succeeds through the wire-level lifecycle"
    );
    let job_state: String = sqlx::query_scalar("SELECT state FROM materialization_jobs")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        job_state, "resolved_success",
        "the job resolved through the wire-level lifecycle"
    );
    Ok(())
}

// ── T-1.2 (Phase B): the BC-4 progress relay through the wire ──────────────

// r[verify sched.materialize.job+2]
// r[verify gw.activity.subst-progress+4]
/// The Phase B progress relay (BC-4, replacing the PD-15b ack-and-drop
/// stub): `ReportMaterializationProgress` through the REAL wire — with
/// the store-service credential and a live materialization attempt —
/// resolves the exec_id to its derivation and re-emits the byte
/// progress as the same display-only `Event::SubstituteProgress` the
/// walk's progress path emitted, on the build's LOG broadcast ring,
/// with done ≤ expected. The relay is droppable end-to-end (try_send;
/// errors ignored) and the auth gate prologue is unchanged (T-1.9).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flag_on_progress_relay_reaches_build_events() -> anyhow::Result<()> {
    use crate::actor::tests::{
        barrier, merge_dag, setup_actor_configured, subscribe_log, test_store_path, tick,
    };
    use rio_auth::hmac::HmacKey;

    // Flag-on actor + MockStore + db-backed SchedulerGrpc (the relay
    // needs the exec_id → attempt lookup).
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, _actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |_cfg, p| {
            p.service_signer = Some(std::sync::Arc::new(rio_auth::hmac::HmacSigner::from_key(
                b"mock-store-harness-service-key32".to_vec(),
            )));
        });

    let executor_key = std::sync::Arc::new(HmacKey::from_key(
        b"executor-key-32-bytes-long!!!!!!".to_vec(),
    ));
    let service_key = std::sync::Arc::new(HmacKey::from_key(
        b"service-key-32-bytes-long-here!!".to_vec(),
    ));
    let mut grpc = SchedulerGrpc::new_for_tests_with_pool(handle.clone(), db.pool.clone());
    grpc.hmac_key = Some(executor_key);
    grpc.service_verifier = Some(std::sync::Arc::clone(&service_key));
    // T-5.1 (enumerated change): the credential is instance-bound to the
    // replica identity this test claims under ("store-test-0").
    let store_token = store_service_token_bound(&service_key, "rio-store", "store-test-0");

    let router = tonic::transport::Server::builder().add_service(ExecutorServiceServer::new(grpc));
    let (addr, _server) = rio_test_support::grpc::spawn_grpc_server(router).await;
    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let mut client = ExecutorServiceClient::new(channel);

    // A claimable job (merge → probe partition), then claim it through
    // the wire so a real materialization attempt (exec_id) exists.
    let out = test_store_path("mat-progress-out");
    let mut node = make_node("mat-progress");
    node.expected_output_paths = vec![out.clone()];
    node.wanted_output_names = vec!["out".into()];
    let build_id = uuid::Uuid::new_v4();
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    barrier(&handle).await;
    store.state.substitutable.write().unwrap().push(out.clone());
    tick(&handle).await?;

    let mut req = Request::new(rio_proto::types::PullAssignmentRequest {
        executor_token: String::new(),
        intent_id: "mat-progress".into(),
        kind: rio_proto::types::AttemptKind::Materialization.into(),
        executor_instance: "store-test-0".into(),
        resume_exec_id: String::new(),
        claim_nonce: String::new(),
        confirm_only: false,
    });
    req.metadata_mut()
        .insert(rio_common::grpc::SERVICE_TOKEN_HEADER, store_token.parse()?);
    let resp = client.pull_assignment(req).await?.into_inner();
    let assignment = match resp.outcome {
        Some(rio_proto::types::pull_assignment_response::Outcome::Assignment(a)) => a,
        other => panic!("the claim must deliver, got {other:?}"),
    };

    // Subscribe to the build's LOG ring (display-only events ride it).
    let mut log_rx = subscribe_log(&handle, build_id).await?;

    // The progress report through the wire (store credential).
    let mut req = Request::new(rio_proto::types::ReportMaterializationProgressRequest {
        exec_id: assignment.exec_id.clone(),
        bytes_done: 1024 * 1024,
        bytes_expected: 4 * 1024 * 1024,
        upstream_uri: "https://cache.example.org".into(),
    });
    req.metadata_mut()
        .insert(rio_common::grpc::SERVICE_TOKEN_HEADER, store_token.parse()?);
    client
        .report_materialization_progress(req)
        .await
        .expect("the authenticated progress report is acknowledged");

    // The relay emits Event::SubstituteProgress on the LOG ring with the
    // reported byte counts (done ≤ expected). The relay rides the actor
    // mailbox (try_send), so barrier on the actor before asserting.
    barrier(&handle).await;
    let mut saw_progress = None;
    while let Ok(event) = log_rx.try_recv() {
        if let Some(rio_proto::types::build_event::Event::SubstituteProgress(p)) = event.event {
            saw_progress = Some(p);
        }
    }
    let progress = saw_progress.expect(
        "the Phase B relay must re-emit ReportMaterializationProgress as \
         Event::SubstituteProgress on the build's log ring (BC-4)",
    );
    assert_eq!(progress.bytes_done, 1024 * 1024);
    assert_eq!(progress.bytes_expected, 4 * 1024 * 1024);
    assert!(
        progress.bytes_done <= progress.bytes_expected,
        "done ≤ expected"
    );
    assert_eq!(progress.upstream_uri, "https://cache.example.org");
    Ok(())
}

// ── T-1.9 (Phase B): the materialization-surface authentication sweep ──────

/// The complete materialization RPC surface (PD-B18's four-row table).
/// A future fifth surface is one variant + one match arm here — the
/// sweep below then covers it automatically.
#[derive(Debug, Clone, Copy)]
enum MatSurface {
    /// `PullAssignment` with kind=MATERIALIZATION (the claim arm).
    PullAssignment,
    /// `ReportOutcome` carrying a materialization_outcome payload.
    ReportOutcome,
    /// `ListMaterializationJobs` (the store poll).
    ListJobs,
    /// `ReportMaterializationProgress` (the BC-4 relay).
    ReportProgress,
}

const ALL_MAT_SURFACES: [MatSurface; 4] = [
    MatSurface::PullAssignment,
    MatSurface::ReportOutcome,
    MatSurface::ListJobs,
    MatSurface::ReportProgress,
];

/// Issue one call against `surface`, optionally decorated with an
/// executor token on the metadata carrier. Returns the call result
/// (Ok = the surface answered; the answer's content is irrelevant to
/// the authentication sweep).
async fn call_mat_surface(
    grpc: &SchedulerGrpc,
    surface: MatSurface,
    executor_token: Option<&str>,
) -> Result<(), tonic::Status> {
    use rio_proto::ExecutorService;
    macro_rules! decorated {
        ($inner:expr) => {{
            let mut req = Request::new($inner);
            if let Some(tok) = executor_token {
                req.metadata_mut()
                    .insert(rio_proto::EXECUTOR_TOKEN_HEADER, tok.parse().unwrap());
            }
            req
        }};
    }
    match surface {
        MatSurface::PullAssignment => grpc
            .pull_assignment(decorated!(rio_proto::types::PullAssignmentRequest {
                executor_token: String::new(),
                intent_id: "sweep-drv".into(),
                kind: rio_proto::types::AttemptKind::Materialization.into(),
                executor_instance: "store-replica-0".into(),
                resume_exec_id: String::new(),
                claim_nonce: String::new(),
                confirm_only: false,
            }))
            .await
            .map(|_| ()),
        MatSurface::ReportOutcome => grpc
            .report_outcome(decorated!(rio_proto::types::ReportOutcomeRequest {
                exec_id: uuid::Uuid::now_v7().to_string(),
                report: None,
                materialization_outcome: Some(mat_success_outcome()),
            }))
            .await
            .map(|_| ()),
        MatSurface::ListJobs => grpc
            .list_materialization_jobs(decorated!(
                rio_proto::types::ListMaterializationJobsRequest {
                    service_token: String::new(),
                    limit: 16,
                }
            ))
            .await
            .map(|_| ()),
        MatSurface::ReportProgress => grpc
            .report_materialization_progress(decorated!(
                rio_proto::types::ReportMaterializationProgressRequest {
                    exec_id: uuid::Uuid::now_v7().to_string(),
                    bytes_done: 1,
                    bytes_expected: 2,
                    upstream_uri: String::new(),
                }
            ))
            .await
            .map(|_| ()),
    }
}

// r[verify sched.materialize.job+2]
// r[verify sec.executor.identity-token+3]
/// T-1.9 / PD-B18: the materialization-surface authentication sweep —
/// the cross-surface structural pin that catches the NEXT handler
/// added or completed without the store-credential gate (the
/// `21955a450` class: ReportMaterializationProgress shipped its Phase A
/// stub without the gate and was closed by a post-integration fix
/// instead of CI).
///
/// Enforced posture (both key families configured): every surface ×
///   {no credential}    → Unauthenticated  (closed by default)
///   {executor token}   → PermissionDenied (the per-intent builder
///                         credential never authorizes the
///                         materialization work class)
/// Dev mode (neither key family): every surface stays open — the
/// documented uniform exception.
#[tokio::test]
async fn materialization_surface_rejects_unauthenticated_and_executor_credentials()
-> anyhow::Result<()> {
    use rio_auth::hmac::{ExecutorClaims, HmacKey};

    // ── The enforced posture: BOTH key families, distinct keys. ──
    let (_db, mut grpc, _handle, _actor_task) = setup_grpc().await;
    let executor_key = std::sync::Arc::new(HmacKey::from_key(
        b"executor-key-32-bytes-long!!!!!!".to_vec(),
    ));
    let service_key = std::sync::Arc::new(HmacKey::from_key(
        b"service-key-32-bytes-long-here!!".to_vec(),
    ));
    grpc.hmac_key = Some(std::sync::Arc::clone(&executor_key));
    grpc.service_verifier = Some(service_key);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let executor_token = executor_key.sign(&ExecutorClaims {
        intent_id: "sweep-drv".into(),
        kind: rio_proto::types::ExecutorKind::Builder as i32,
        expiry_unix: now + 600,
    });

    for surface in ALL_MAT_SURFACES {
        // (a) Credential-less → Unauthenticated (closed).
        let err = call_mat_surface(&grpc, surface, None)
            .await
            .expect_err(&format!(
                "{surface:?}: a credential-less call must be rejected in the enforced posture"
            ));
        assert_eq!(
            err.code(),
            tonic::Code::Unauthenticated,
            "{surface:?}: credential-less → Unauthenticated, got {err:?}"
        );

        // (b) A valid EXECUTOR token (the per-intent builder credential)
        //     → PermissionDenied (never authorizes the materialization
        //     work class, on any surface).
        let err = call_mat_surface(&grpc, surface, Some(&executor_token))
            .await
            .expect_err(&format!(
                "{surface:?}: an executor token must never authorize materialization operations"
            ));
        assert_eq!(
            err.code(),
            tonic::Code::PermissionDenied,
            "{surface:?}: executor token → PermissionDenied, got {err:?}"
        );
    }

    // ── Dev mode (neither key family): every surface stays open. ──
    let (_db2, grpc2, _handle2, _actor_task2) = setup_grpc().await;
    for surface in ALL_MAT_SURFACES {
        call_mat_surface(&grpc2, surface, None)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "{surface:?}: full dev mode must stay open (the uniform exception), got {e:?}"
                )
            });
    }
    Ok(())
}

// r[verify sec.executor.identity-token+3]
/// merged_bug_084 (A2): the credential FAMILY is selected by the
/// payload kind BEFORE verification. The half-configured deployment
/// {hmac: None, service: Some} must still gate materialization
/// reports: pre-fix the store gate was nested under require_executor's
/// Err arm, and with no executor key configured require_executor
/// answered Ok(None) — a credential-less materialization outcome was
/// consumed with auth_intent=None.
#[tokio::test]
async fn half_configured_deployment_still_gates_materialization_reports() -> anyhow::Result<()> {
    use rio_auth::hmac::HmacKey;
    use rio_proto::ExecutorService;
    let (_db, mut grpc, _handle, _actor_task) = setup_grpc().await;
    let service_key = std::sync::Arc::new(HmacKey::from_key(
        b"service-key-32-bytes-long-here!!".to_vec(),
    ));
    grpc.hmac_key = None; // no executor key family
    grpc.service_verifier = Some(std::sync::Arc::clone(&service_key));

    // Materialization outcome, NO credential on any carrier.
    let req = Request::new(rio_proto::types::ReportOutcomeRequest {
        exec_id: uuid::Uuid::now_v7().to_string(),
        report: None,
        materialization_outcome: Some(mat_success_outcome()),
    });
    let err = grpc
        .report_outcome(req)
        .await
        .expect_err("a credential-less materialization report must be rejected");
    assert_eq!(
        err.code(),
        tonic::Code::Unauthenticated,
        "the store-service family gates the report even with no executor key configured: {err:?}"
    );

    // The build shape stays open in the same half-config (no executor
    // key = dev-mode build identity, byte-identical to the as-built
    // posture).
    let req = Request::new(rio_proto::types::ReportOutcomeRequest {
        exec_id: uuid::Uuid::now_v7().to_string(),
        report: Some(rio_proto::types::CompletionReport::default()),
        materialization_outcome: None,
    });
    grpc.report_outcome(req)
        .await
        .expect("the build report family is unaffected by the service verifier");
    Ok(())
}

// r[verify sched.grpc.fence-retryable]
/// bug_362 (A2): EVERY ExecutorService method runs the standby gate.
/// Pre-fix report_materialization_progress had no ensure_leader (and no
/// check_actor_alive): a standby replica ACKed progress, defeating the
/// store client's UNAVAILABLE-based leader failover.
#[tokio::test]
async fn report_materialization_progress_rejected_on_standby() -> anyhow::Result<()> {
    use rio_proto::ExecutorService;
    let (_db, mut grpc, _handle, _actor_task) = setup_grpc().await;
    grpc.is_leader = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let err = grpc
        .report_materialization_progress(Request::new(
            rio_proto::types::ReportMaterializationProgressRequest {
                exec_id: uuid::Uuid::now_v7().to_string(),
                upstream_uri: String::new(),
                bytes_done: 1,
                bytes_expected: 2,
            },
        ))
        .await
        .expect_err("standby must reject progress reports");
    assert_eq!(err.code(), tonic::Code::Unavailable);
    Ok(())
}

// r[verify sched.grpc.fence-retryable]
/// The descriptor-driven standby sweep (bug_362's structural net):
/// every method of `rio.types.ExecutorService` — enumerated from the
/// compiled FILE_DESCRIPTOR_SET, so the NEXT RPC cannot be forgotten —
/// must answer UNAVAILABLE on a standby replica. A method added to the
/// proto without an arm here fails the match below.
#[tokio::test]
async fn every_executor_service_method_is_standby_gated() -> anyhow::Result<()> {
    use prost::Message;
    use rio_proto::ExecutorService;
    let fds = prost_types::FileDescriptorSet::decode(rio_proto::FILE_DESCRIPTOR_SET)?;
    let methods: Vec<String> = fds
        .file
        .iter()
        .flat_map(|f| f.service.iter())
        .filter(|s| s.name() == "ExecutorService")
        .flat_map(|s| s.method.iter().map(|m| m.name().to_string()))
        .collect();
    assert!(
        !methods.is_empty(),
        "ExecutorService missing from the descriptor set"
    );

    let (_db, mut grpc, _handle, _actor_task) = setup_grpc().await;
    grpc.is_leader = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    for method in &methods {
        let code = match method.as_str() {
            "PullAssignment" => grpc
                .pull_assignment(Request::new(rio_proto::types::PullAssignmentRequest {
                    executor_token: String::new(),
                    intent_id: "drv-standby".into(),
                    kind: rio_proto::types::AttemptKind::Build.into(),
                    executor_instance: String::new(),
                    resume_exec_id: String::new(),
                    claim_nonce: String::new(),
                    confirm_only: false,
                }))
                .await
                .expect_err("standby")
                .code(),
            "ReportOutcome" => grpc
                .report_outcome(Request::new(rio_proto::types::ReportOutcomeRequest {
                    exec_id: uuid::Uuid::now_v7().to_string(),
                    report: Some(rio_proto::types::CompletionReport::default()),
                    materialization_outcome: None,
                }))
                .await
                .expect_err("standby")
                .code(),
            "ListMaterializationJobs" => grpc
                .list_materialization_jobs(Request::new(
                    rio_proto::types::ListMaterializationJobsRequest {
                        service_token: String::new(),
                        limit: 1,
                    },
                ))
                .await
                .expect_err("standby")
                .code(),
            "ReportMaterializationProgress" => grpc
                .report_materialization_progress(Request::new(
                    rio_proto::types::ReportMaterializationProgressRequest {
                        exec_id: uuid::Uuid::now_v7().to_string(),
                        upstream_uri: String::new(),
                        bytes_done: 0,
                        bytes_expected: 0,
                    },
                ))
                .await
                .expect_err("standby")
                .code(),
            other => panic!(
                "ExecutorService method {other:?} has no standby-sweep arm — add the arm \
                 AND the executor_prologue to its handler"
            ),
        };
        assert_eq!(
            code,
            tonic::Code::Unavailable,
            "method {method} must answer UNAVAILABLE on standby"
        );
    }
    Ok(())
}

// r[verify sec.executor.identity-token+3]
/// merged_bug_084's exhaustive matrix: every {executor key} ×
/// {service verifier} × {payload kind} cell of the credential-less
/// `ReportOutcome`, through the `credential_for` chokepoint. The
/// family is selected by the PAYLOAD — never by which keys happen to
/// be configured — so the materialization column is gated whenever ANY
/// key family exists, and the build column keeps its as-built
/// dev/enforced split.
#[tokio::test]
async fn report_outcome_credential_matrix() -> anyhow::Result<()> {
    use rio_auth::hmac::HmacKey;
    use rio_proto::ExecutorService;
    let executor_key = || {
        std::sync::Arc::new(HmacKey::from_key(
            b"executor-key-32-bytes-long!!!!!!".to_vec(),
        ))
    };
    let service_key = || {
        std::sync::Arc::new(HmacKey::from_key(
            b"service-key-32-bytes-long-here!!".to_vec(),
        ))
    };
    // (hmac?, service?, mat-payload?, expected code; None = Ok)
    let cells: Vec<(bool, bool, bool, Option<tonic::Code>)> = vec![
        // Full dev mode: both payloads open (the flag gate / actor
        // ack-ignore handles unknown execs).
        (false, false, false, None),
        (false, false, true, None),
        // Executor-only config: build enforced; materialization closed
        // (no acceptable credential family exists for it).
        (true, false, false, Some(tonic::Code::Unauthenticated)),
        (true, false, true, Some(tonic::Code::Unauthenticated)),
        // Service-only half-config (THE 084 red cell): build stays
        // dev-open; credential-less materialization is rejected.
        (false, true, false, None),
        (false, true, true, Some(tonic::Code::Unauthenticated)),
        // Both configured: both columns enforced.
        (true, true, false, Some(tonic::Code::Unauthenticated)),
        (true, true, true, Some(tonic::Code::Unauthenticated)),
    ];
    for (hmac, service, mat, want) in cells {
        let (_db, mut grpc, _handle, _task) = setup_grpc().await;
        grpc.hmac_key = hmac.then(executor_key);
        grpc.service_verifier = service.then(service_key);
        let req = Request::new(rio_proto::types::ReportOutcomeRequest {
            exec_id: uuid::Uuid::now_v7().to_string(),
            report: (!mat).then(rio_proto::types::CompletionReport::default),
            materialization_outcome: mat.then(mat_success_outcome),
        });
        let got = grpc.report_outcome(req).await;
        match want {
            None => {
                got.unwrap_or_else(|e| {
                    panic!("cell (hmac={hmac}, service={service}, mat={mat}) must be open: {e:?}")
                });
            }
            Some(code) => {
                let err = got.expect_err(&format!(
                    "cell (hmac={hmac}, service={service}, mat={mat}) must be rejected"
                ));
                assert_eq!(
                    err.code(),
                    code,
                    "cell (hmac={hmac}, service={service}, mat={mat}): {err:?}"
                );
            }
        }
    }
    Ok(())
}

// r[verify sched.materialize.job+2]
/// bug_168 (round 3): the pull-rejection reason label classifies from
/// the ACTUAL credential failure, not the status code. A shape-valid
/// store-service token signed with the WRONG service key (the
/// store-fleet HMAC rotation-skew trace) is a verification failure and
/// must be counted `service_verification_failed` — pre-fix the
/// PermissionDenied blanket counted it `kind_unauthorized`,
/// mis-narrating key skew as a kind-authorization bug during the only
/// window where the operator needs the true signal.
#[tokio::test]
async fn pull_rejection_reason_classifies_verification_failure() -> anyhow::Result<()> {
    use rio_auth::hmac::HmacKey;
    use rio_proto::ExecutorService;
    let recorder = rio_test_support::metrics::CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let (_db, mut grpc, _handle, _actor_task) = setup_grpc().await;
    let executor_key = std::sync::Arc::new(HmacKey::from_key(
        b"executor-key-32-bytes-long!!!!!!".to_vec(),
    ));
    let service_key = std::sync::Arc::new(HmacKey::from_key(
        b"service-key-32-bytes-long-here!!".to_vec(),
    ));
    // The skewed key: what the store fleet signs with after a rotation
    // the scheduler has not yet picked up.
    let rotated_key = std::sync::Arc::new(HmacKey::from_key(
        b"rotated-away-key-32-bytes-long!!".to_vec(),
    ));
    grpc.hmac_key = Some(executor_key);
    grpc.service_verifier = Some(service_key);
    let skewed_token = store_service_token_bound(&rotated_key, "rio-store", "store-replica-0");

    let mut req = Request::new(rio_proto::types::PullAssignmentRequest {
        executor_token: String::new(),
        intent_id: "drv-rotation-skew".into(),
        kind: rio_proto::types::AttemptKind::Materialization.into(),
        executor_instance: "store-replica-0".into(),
        resume_exec_id: String::new(),
        claim_nonce: String::new(),
        confirm_only: false,
    });
    req.metadata_mut().insert(
        rio_common::grpc::SERVICE_TOKEN_HEADER,
        skewed_token.parse()?,
    );
    let err = grpc
        .pull_assignment(req)
        .await
        .expect_err("a wrong-key service token must be rejected");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert_eq!(
        recorder.get(
            "rio_scheduler_pull_rejected_total{reason=service_verification_failed,rpc=pull_assignment}"
        ),
        1,
        "verification failure must carry its own reason label; keys seen: {:?}",
        recorder.all_keys()
    );
    assert_eq!(
        recorder
            .get("rio_scheduler_pull_rejected_total{reason=kind_unauthorized,rpc=pull_assignment}"),
        0,
        "rotation skew must not be narrated as a kind-authorization rejection"
    );
    Ok(())
}

// r[verify sched.materialize.job+2]
/// merged_bug_049 (round 4): only 2 of the 4 `credential_for`
/// consumers counted rejections — the listing/progress sites routed a
/// bare `?` through `From<CredentialRejection> for Status`, which
/// kept the status and silently dropped the classified reason. During
/// a store-fleet service-HMAC rotation skew the store's poll loop
/// dies at the UNCOUNTED listing every pass, so the counted
/// pull_assignment/report_outcome surfaces are never reached:
/// materialization halts fleet-wide while the HELP-advertised
/// rotation-skew trace stays flat. The From impl is DELETED (compile
/// red: 4 sites stopped typechecking — the compiler-generated
/// consumer census) and every consumer routes through
/// `into_status_counted(rpc)`.
#[tokio::test]
async fn listing_and_progress_rejections_tick_their_rpc_labels() -> anyhow::Result<()> {
    use rio_auth::hmac::HmacKey;
    use rio_proto::ExecutorService;
    let recorder = rio_test_support::metrics::CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let (_db, mut grpc, _handle, _actor_task) = setup_grpc().await;
    let service_key = std::sync::Arc::new(HmacKey::from_key(
        b"service-key-32-bytes-long-here!!".to_vec(),
    ));
    let rotated_key = std::sync::Arc::new(HmacKey::from_key(
        b"rotated-away-key-32-bytes-long!!".to_vec(),
    ));
    grpc.hmac_key = Some(std::sync::Arc::new(HmacKey::from_key(
        b"executor-key-32-bytes-long!!!!!!".to_vec(),
    )));
    grpc.service_verifier = Some(service_key);
    let skewed_token = store_service_token_bound(&rotated_key, "rio-store", "store-replica-0");

    // The listing: the store poll loop's FIRST call each pass — the
    // uncounted hole pre-fix.
    let mut req = Request::new(rio_proto::types::ListMaterializationJobsRequest {
        service_token: String::new(),
        limit: 16,
    });
    req.metadata_mut().insert(
        rio_common::grpc::SERVICE_TOKEN_HEADER,
        skewed_token.parse()?,
    );
    let err = grpc
        .list_materialization_jobs(req)
        .await
        .expect_err("a wrong-key service token must be rejected");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert_eq!(
        recorder.get(
            "rio_scheduler_pull_rejected_total{reason=service_verification_failed,rpc=list_materialization_jobs}"
        ),
        1,
        "the listing rejection must tick its own rpc label; keys: {:?}",
        recorder.all_keys()
    );

    // The progress surface: same skewed credential.
    let mut req = Request::new(rio_proto::types::ReportMaterializationProgressRequest {
        exec_id: uuid::Uuid::now_v7().to_string(),
        bytes_done: 0,
        bytes_expected: 0,
        upstream_uri: String::new(),
    });
    req.metadata_mut().insert(
        rio_common::grpc::SERVICE_TOKEN_HEADER,
        skewed_token.parse()?,
    );
    let err = grpc
        .report_materialization_progress(req)
        .await
        .expect_err("a wrong-key service token must be rejected");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert_eq!(
        recorder.get(
            "rio_scheduler_pull_rejected_total{reason=service_verification_failed,rpc=report_materialization_progress}"
        ),
        1,
        "the progress rejection must tick its own rpc label; keys: {:?}",
        recorder.all_keys()
    );
    Ok(())
}

// ── Fence-key provenance (merged_bug_078) ──

// r[verify sched.executor.confirm-fence]
/// merged_bug_078: the confirm-fence key must be the SHA-256 of
/// exactly the carrier bytes that VERIFIED — never of whichever
/// carrier happened to be present. The attack cell: garbage
/// `x-rio-executor-token` metadata + a valid signed body token
/// authenticates as the BODY identity (`credential_for`'s Build arm
/// catches any `require_executor` error and falls back to the body),
/// so keying the fence on the unverified metadata bytes lets an
/// untrusted worker de-key its own fence write (and dodge the
/// DeliverNew screen) while staying authenticated.
///
/// Driven through the production chokepoint into the durable row: a
/// confirm-only pull for an absent drv answers `Gone`, whose
/// write-ahead fences the answering token — the row's key IS the
/// value the actor received on the command conduit, asserted one hop
/// deeper than a stubbed actor would.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fence_key_follows_the_verified_carrier() -> anyhow::Result<()> {
    use rio_auth::hmac::{ExecutorClaims, HmacKey};
    use sha2::Digest as _;
    let (db, mut grpc, _handle, _actor_task) = setup_grpc().await;
    let key = std::sync::Arc::new(HmacKey::from_key(
        b"test-key-32-bytes-long-here!!!!!".to_vec(),
    ));
    grpc.hmac_key = Some(std::sync::Arc::clone(&key));
    let router = tonic::transport::Server::builder().add_service(ExecutorServiceServer::new(grpc));
    let (addr, _server) = rio_test_support::grpc::spawn_grpc_server(router).await;
    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let mut client = ExecutorServiceClient::new(channel);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let body_token = key.sign(&ExecutorClaims {
        intent_id: "intent-fencekey".into(),
        kind: rio_proto::types::ExecutorKind::Builder as i32,
        expiry_unix: now + 600,
    });
    let garbage_metadata = "not-a-valid-token-but-present";

    // Unverifiable metadata + valid body: authenticates as the body
    // identity; the absent drv answers Gone; confirm_only fences the
    // exit-0 license ahead of the answer.
    let mut req = tonic::Request::new(rio_proto::types::PullAssignmentRequest {
        executor_token: body_token.clone(),
        intent_id: "intent-fencekey".into(),
        confirm_only: true,
        ..Default::default()
    });
    req.metadata_mut()
        .insert(rio_proto::EXECUTOR_TOKEN_HEADER, garbage_metadata.parse()?);
    let resp = client
        .pull_assignment(req)
        .await
        .expect("body-authenticated confirm pull is served")
        .into_inner();
    assert!(
        matches!(
            resp.outcome,
            Some(rio_proto::types::pull_assignment_response::Outcome::Gone(_))
        ),
        "absent drv answers the confirm probe Gone, got {resp:?}"
    );

    let row: Option<(String, String)> =
        sqlx::query_as("SELECT executor_token_sha256, intent_id FROM executor_confirm_fences")
            .fetch_optional(&db.pool)
            .await?;
    let (fence_key, intent) = row.expect("the confirm Gone must write exactly one fence row");
    assert_eq!(intent, "intent-fencekey");
    assert_eq!(
        fence_key,
        hex::encode(sha2::Sha256::digest(body_token.as_bytes())),
        "the fence key must hash the carrier that VERIFIED (the body token), \
         not whichever carrier was merely present (garbage metadata hashes to {})",
        hex::encode(sha2::Sha256::digest(garbage_metadata.as_bytes()))
    );
    Ok(())
}

/// merged_bug_013: the pull path's fence write-ahead NACK
/// (`ConsumptionNotDurable`) must tick the brownout counter exactly
/// like the report path's consumption NACK — pre-fix the rejection
/// arm lumped it into the uncounted retryable group under prose
/// claiming it "unreachable from the pull path" (falsified by the
/// confirm-fence write-ahead, and further by the totalized live-Gone
/// fence), so a PG brownout's pull-side NACK wave was invisible to
/// the advertised alert.
///
/// The brownout is injected at the DB layer (the fence table
/// vanishes); the request rides the production handler and the real
/// actor — the counter is asserted through the real recorder, no
/// synthetic pokes.
#[tokio::test]
async fn pull_assignment_fence_nack_ticks_the_brownout_counter() -> anyhow::Result<()> {
    use rio_auth::hmac::{ExecutorClaims, HmacKey};
    use rio_proto::ExecutorService;
    let recorder = rio_test_support::metrics::CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let (db, mut grpc, _handle, _actor_task) = setup_grpc().await;
    let key = std::sync::Arc::new(HmacKey::from_key(
        b"test-key-32-bytes-long-here!!!!!".to_vec(),
    ));
    grpc.hmac_key = Some(std::sync::Arc::clone(&key));

    // The PG brownout: the fence write-ahead cannot land.
    sqlx::query("DROP TABLE executor_confirm_fences")
        .execute(&db.pool)
        .await?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let token = key.sign(&ExecutorClaims {
        intent_id: "drv-brownout".into(),
        kind: rio_proto::types::ExecutorKind::Builder as i32,
        expiry_unix: now + 600,
    });
    let err = grpc
        .pull_assignment(Request::new(rio_proto::types::PullAssignmentRequest {
            executor_token: token,
            intent_id: "drv-brownout".into(),
            confirm_only: true,
            ..Default::default()
        }))
        .await
        .expect_err("a failed fence write-ahead withholds the exit-0 license");
    assert_eq!(
        err.code(),
        tonic::Code::Unavailable,
        "ConsumptionNotDurable rides the retryable NACK class"
    );
    assert_eq!(
        recorder.get(
            "rio_scheduler_pull_rejected_total{reason=consumption_not_durable,rpc=pull_assignment}"
        ),
        1,
        "the pull-side fence NACK must be visible on the brownout trace; keys: {:?}",
        recorder.all_keys()
    );
    Ok(())
}
