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
        })
        .await
        .expect_err("token-less ReportOutcome must be rejected when HMAC is configured");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);

    // 2. ReportOutcome with the metadata header → passes the identity
    //    gate and reaches the actor (unknown exec → acknowledged).
    let mut req = tonic::Request::new(rio_proto::types::ReportOutcomeRequest {
        exec_id: uuid::Uuid::now_v7().to_string(),
        report: None,
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
        })
        .await
        .expect_err("credential-less PullAssignment must be rejected when HMAC is configured");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    Ok(())
}
