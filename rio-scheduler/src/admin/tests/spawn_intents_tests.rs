//! `GetSpawnIntents` RPC tests.
//!
//! Mirrors the `admin/spawn_intents.rs` submodule seam. The
//! actor-command level (`GetSpawnIntents`) is covered in
//! `actor/tests/misc.rs`; this file exercises the gRPC-level wiring:
//! the `AdminServiceImpl` handler, leader/actor-alive gates, and the
//! actor→proto conversion.

use super::*;

// r[verify sched.admin.spawn-intents]
/// `[sla]` on: each Ready derivation emits one intent. Proves the gRPC
/// handler threads `ActorCommand::Admin(AdminQuery::GetSpawnIntents)`
/// through to the proto response, and that proto3's `optional
/// ExecutorKind` round-trips (None on the wire = unfiltered).
#[tokio::test]
async fn test_get_spawn_intents_reports_ready() -> anyhow::Result<()> {
    use crate::actor::tests::{make_node, merge_dag, setup_actor_configured, test_sla_config};

    let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
    let (actor, task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        c.sla = test_sla_config();
    });
    let svc = AdminServiceImpl::new(
        Arc::new(LogBuffers::new()),
        None,
        db.pool.clone(),
        actor.clone(),
        "127.0.0.1:1".into(),
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        crate::lease::LeaderState::default(),
        rio_common::signal::Token::new(),
        String::new(),
        Arc::new(crate::sla::config::SlaConfig::test_default()),
        None,
    );

    let mut fod = make_node("fod-a");
    fod.is_fixed_output = true;
    merge_dag(
        &actor,
        uuid::Uuid::new_v4(),
        vec![make_node("a"), fod],
        vec![],
        false,
    )
    .await?;

    // Unfiltered (kind=None on the wire) → both.
    let resp = svc
        .get_spawn_intents(Request::new(GetSpawnIntentsRequest::default()))
        .await?
        .into_inner();
    assert_eq!(resp.intents.len(), 2, "FOD + non-FOD both emit (D2)");
    assert_eq!(resp.queued_by_system.get("x86_64-linux"), Some(&2));

    // kind=Builder → non-FOD only.
    let resp = svc
        .get_spawn_intents(Request::new(GetSpawnIntentsRequest {
            kind: Some(rio_proto::types::ExecutorKind::Builder.into()),
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert_eq!(resp.intents.len(), 1, "kind=Builder excludes FOD");
    assert_eq!(
        resp.intents[0].kind,
        i32::from(rio_proto::types::ExecutorKind::Builder)
    );

    drop(actor);
    drop(task);
    Ok(())
}

// r[verify sec.executor.identity-token+2]
/// `MintExecutorTokens` mints a verifiable per-intent `ExecutorClaims`
/// token; `GetSpawnIntents` does NOT carry one. Proves the credential
/// lives on a controller-only surface and `SpawnIntent` is plain data
/// (the bug_028 split).
#[tokio::test]
async fn test_mint_executor_tokens_signs_per_intent() -> anyhow::Result<()> {
    use crate::actor::tests::{make_node, merge_dag, setup_actor_configured, test_sla_config};
    use rio_auth::hmac::{ExecutorClaims, HmacSigner, HmacVerifier};

    let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
    let key = b"test-mint-hmac-key-32-bytes!!!!!".to_vec();
    let (actor, task) = setup_actor_configured(db.pool.clone(), None, {
        let key = key.clone();
        move |c, p| {
            c.sla = test_sla_config();
            p.hmac_signer = Some(Arc::new(HmacSigner::from_key(key)));
        }
    });
    let svc = AdminServiceImpl::new(
        Arc::new(LogBuffers::new()),
        None,
        db.pool.clone(),
        actor.clone(),
        "127.0.0.1:1".into(),
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        crate::lease::LeaderState::default(),
        rio_common::signal::Token::new(),
        String::new(),
        Arc::new(crate::sla::config::SlaConfig::test_default()),
        // service_verifier=None (dev-mode pass-through): the gate is
        // covered by `read_path_rpcs_require_service_token`; this test
        // exercises the actor-side mint.
        None,
    );

    let mut fod = make_node("fod-a");
    fod.is_fixed_output = true;
    merge_dag(
        &actor,
        uuid::Uuid::new_v4(),
        vec![make_node("a"), fod],
        vec![],
        false,
    )
    .await?;

    // GetSpawnIntents → plain data (no `executor_token` field exists
    // on the proto; the compile-time proof is `roundtrip.rs`).
    let intents = svc
        .get_spawn_intents(Request::new(GetSpawnIntentsRequest::default()))
        .await?
        .into_inner()
        .intents;
    assert_eq!(intents.len(), 2);

    // MintExecutorTokens for both → verifiable claims with the right
    // `kind` per intent.
    let tokens = svc
        .mint_executor_tokens(Request::new(MintExecutorTokensRequest {
            intent_ids: intents.iter().map(|i| i.intent_id.clone()).collect(),
        }))
        .await?
        .into_inner()
        .tokens;
    assert_eq!(tokens.len(), 2, "one token per requested intent");

    let verifier = HmacVerifier::from_key(key);
    let now = rio_auth::now_unix().unwrap();
    for intent in &intents {
        let tok = tokens
            .get(&intent.intent_id)
            .unwrap_or_else(|| panic!("token for {}", intent.intent_id));
        let claims: ExecutorClaims = verifier.verify(tok).expect("token verifies with same key");
        assert_eq!(claims.intent_id, intent.intent_id);
        assert_eq!(
            claims.kind, intent.kind,
            "kind binds to the FOD/non-FOD arm"
        );
        assert!(
            claims.expiry_unix > now,
            "expiry = now + deadline + eta + 300 > now"
        );
    }

    // Unknown intent_id → omitted (NOT an error).
    let tokens = svc
        .mint_executor_tokens(Request::new(MintExecutorTokensRequest {
            intent_ids: vec!["nonexistent".into()],
        }))
        .await?
        .into_inner()
        .tokens;
    assert!(tokens.is_empty(), "unknown intent_id → omitted from map");

    drop(actor);
    drop(task);
    Ok(())
}
