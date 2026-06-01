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
