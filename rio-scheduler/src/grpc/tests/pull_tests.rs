//! Pull-unary identity posture (`pull_assignment` / `report_outcome`)
//! with the executor HMAC key configured: the metadata header is the
//! report's only carrier, the pull additionally accepts the body
//! token, and a missing/invalid credential is rejected closed.

use super::*;
use rio_proto::{ExecutorServiceClient, ExecutorServiceServer};

// r[verify sec.executor.identity-token+2]
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

// ── Materialization listing + kind intake (substitution-replacement
//    Phase A — flag-off dormancy pins + the BC-1 identity rule) ──

// r[verify sched.materialize.job]
/// Flag-off (the Phase A deployed state), ListMaterializationJobs
/// answers an empty list — never an error (the AS-6 mixed-flag
/// posture: a flag-on store polling a flag-off scheduler hangs
/// harmlessly on empty lists). Dev mode (no HMAC key): the call needs
/// no credential, same as the other ExecutorService unaries.
#[tokio::test]
async fn list_materialization_jobs_returns_empty_flag_off() -> anyhow::Result<()> {
    use rio_proto::ExecutorService;
    let (_db, grpc, _handle, _actor_task) = setup_grpc().await;
    let resp = grpc
        .list_materialization_jobs(Request::new(
            rio_proto::types::ListMaterializationJobsRequest {
                service_token: String::new(),
                limit: 16,
            },
        ))
        .await
        .expect("flag-off listing must answer, never error")
        .into_inner();
    assert!(
        resp.jobs.is_empty(),
        "flag-off ListMaterializationJobs must answer an empty list, got {:?}",
        resp.jobs
    );
    Ok(())
}

// r[verify sched.materialize.job]
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
        }))
        .await
        .expect_err("a materialization pull without executor_instance must be rejected");
    assert_eq!(
        err.code(),
        tonic::Code::InvalidArgument,
        "BC-1: the per-replica identity is mandatory for the materialization kind, got {err:?}"
    );

    // The same request as a BUILD pull (kind unset) is admitted and
    // answers Gone for the unknown intent — the as-built behavior,
    // bit-identical (the new fields are ignored for builds).
    let resp = grpc
        .pull_assignment(Request::new(rio_proto::types::PullAssignmentRequest {
            executor_token: String::new(),
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

// r[verify sched.materialize.job]
/// Flag-off, a materialization-kind pull (with a valid instance) parks
/// NotYetReady — the AS-6 posture: a flag-on store claiming against a
/// flag-off scheduler hangs harmlessly; it never errors, never mints,
/// and never receives work.
#[tokio::test]
async fn materialization_pull_parks_not_yet_ready_flag_off() -> anyhow::Result<()> {
    use rio_proto::ExecutorService;
    let (_db, grpc, _handle, _actor_task) = setup_grpc().await;
    let resp = grpc
        .pull_assignment(Request::new(rio_proto::types::PullAssignmentRequest {
            executor_token: String::new(),
            intent_id: "drv-mat-flag-off".into(),
            kind: rio_proto::types::AttemptKind::Materialization.into(),
            executor_instance: "store-replica-0".into(),
        }))
        .await
        .expect("flag-off materialization pulls are answered, never errored")
        .into_inner();
    assert!(
        matches!(
            resp.outcome,
            Some(rio_proto::types::pull_assignment_response::Outcome::NotYetReady(_))
        ),
        "flag-off materialization pull must park NotYetReady (AS-6), got {resp:?}"
    );
    Ok(())
}

// ── Materialization authorization (security review: dormant ≠ unprotected) ──

// r[verify sched.materialize.job]
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

// r[verify sched.materialize.job]
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

// r[verify sched.materialize.job]
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
            }))
            .await
            .expect_err("malformed executor_instance must be rejected");
        assert_eq!(
            err.code(),
            tonic::Code::InvalidArgument,
            "instance {bad:?} must be InvalidArgument, got {err:?}"
        );
    }
    // A valid DNS-1123 label is accepted (and parks NotYetReady
    // flag-off, dev mode — the AS-6 posture).
    let resp = grpc
        .pull_assignment(Request::new(rio_proto::types::PullAssignmentRequest {
            executor_token: String::new(),
            intent_id: "drv-instance-validation".into(),
            kind: rio_proto::types::AttemptKind::Materialization.into(),
            executor_instance: "store-replica-0".into(),
        }))
        .await
        .expect("a valid DNS-1123 instance is accepted")
        .into_inner();
    assert!(
        matches!(
            resp.outcome,
            Some(rio_proto::types::pull_assignment_response::Outcome::NotYetReady(_))
        ),
        "valid instance + flag-off parks NotYetReady, got {resp:?}"
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

/// Sign a `ServiceClaims` store credential with `key`.
fn store_service_token(key: &rio_auth::hmac::HmacKey, caller: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs();
    key.sign(&rio_auth::hmac::ServiceClaims {
        caller: caller.to_string(),
        expiry_unix: now + 60,
    })
}

/// An empty materialization Success outcome payload.
fn mat_success_outcome() -> rio_proto::types::MaterializationOutcome {
    rio_proto::types::MaterializationOutcome {
        outcome: Some(rio_proto::types::materialization_outcome::Outcome::Success(
            rio_proto::types::materialization_outcome::Success {
                ingested_paths: vec![],
                verified_paths: vec![],
            },
        )),
    }
}

// r[verify sched.materialize.job]
/// Wave-4 kind-attested credential: `ServiceClaims{caller="rio-store"}`
/// signed with the SERVICE HMAC key (the separate x-rio-service-token
/// family) authorizes exactly the materialization operations —
/// ListMaterializationJobs (both carriers), kind=MATERIALIZATION
/// PullAssignment, and materialization ReportOutcome — while the
/// executor HMAC posture stays fully enforced. The flag-off answers
/// stay dormant: empty list, NotYetReady, acknowledged-and-ignored.
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
    let store_token = store_service_token(&service_key, "rio-store");

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
    assert!(resp.jobs.is_empty(), "flag-off listing stays empty");

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
    //    flag-off NotYetReady (the AS-6 posture), never a rejection.
    let mut req = Request::new(rio_proto::types::PullAssignmentRequest {
        executor_token: String::new(),
        intent_id: "drv-store-credential".into(),
        kind: rio_proto::types::AttemptKind::Materialization.into(),
        executor_instance: "store-replica-0".into(),
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
            Some(rio_proto::types::pull_assignment_response::Outcome::NotYetReady(_))
        ),
        "flag-off materialization pull parks NotYetReady, got {resp:?}"
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

// r[verify sched.materialize.job]
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

// r[verify sched.materialize.job]
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

// ── T-6.2: the wire-level flag-on lifecycle (review finding dormancy-5;
//    PD-14 as amended) ────────────────────────────────────────────────────

// r[verify sched.materialize.job]
// r[verify store.materialize.executor]
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
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, _actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |cfg, _| {
            cfg.materialization.enabled = true;
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
    let store_token = store_service_token(&service_key, "rio-store");

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
