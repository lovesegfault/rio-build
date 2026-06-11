//! `GetSpawnIntents` RPC tests.
//!
//! Mirrors the `admin/spawn_intents.rs` submodule seam. The
//! actor-command level (`GetSpawnIntents`) is covered in
//! `actor/tests/misc.rs`; this file exercises the gRPC-level wiring:
//! the `AdminServiceImpl` handler, leader/actor-alive gates, and the
//! actor→proto conversion.

use super::*;

// r[verify sched.admin.spawn-intents+2]
/// `[sla]` on: each Ready derivation emits one intent. Proves the gRPC
/// handler threads `ActorCommand::Admin(AdminQuery::GetSpawnIntents)`
/// through to the proto response, and that proto3's `optional
/// ExecutorKind` round-trips (None on the wire = unfiltered).
#[tokio::test]
async fn test_get_spawn_intents_reports_ready() -> anyhow::Result<()> {
    use crate::actor::tests::{make_node, merge_dag, setup_actor_configured, test_sla_config};

    let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (actor, task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        c.sla = test_sla_config();
    });
    let svc = AdminServiceImpl::new(
        db.pool.clone(),
        actor.clone(),
        "127.0.0.1:1".into(),
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        crate::lease::LeaderState::default(),
        rio_common::signal::Token::new(),
        String::new(),
        Arc::new(crate::sla::config::SlaConfig::test_default()),
        None,
        Arc::default(),
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

// r[verify sec.executor.identity-token+3]
/// `MintExecutorTokens` mints a verifiable per-intent `ExecutorClaims`
/// token; `GetSpawnIntents` does NOT carry one. Proves the credential
/// lives on a controller-only surface and `SpawnIntent` is plain data
/// (the bug_028 split).
#[tokio::test]
async fn test_mint_executor_tokens_signs_per_intent() -> anyhow::Result<()> {
    use crate::actor::tests::{make_node, merge_dag, setup_actor_configured, test_sla_config};
    use rio_auth::hmac::{ExecutorClaims, HmacSigner, HmacVerifier};

    let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let key = b"test-mint-hmac-key-32-bytes!!!!!".to_vec();
    let (actor, task) = setup_actor_configured(db.pool.clone(), None, {
        let key = key.clone();
        move |c, p| {
            c.sla = test_sla_config();
            p.hmac_signer = Some(Arc::new(HmacSigner::from_key(key)));
        }
    });
    let svc = AdminServiceImpl::new(
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
        Arc::default(),
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
    let resp = svc
        .mint_executor_tokens(Request::new(MintExecutorTokensRequest {
            intent_ids: intents.iter().map(|i| i.intent_id.clone()).collect(),
        }))
        .await?
        .into_inner();
    assert!(
        !resp.keyless,
        "bug_121: an HMAC-configured scheduler reports keyless=false"
    );
    let tokens = resp.tokens;
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

    // Unknown intent_id → omitted (NOT an error). bug_121: omitted +
    // keyless=false is the wire face of the Omitted letter — the
    // controller skips the intent this tick instead of spawning a
    // token-less Job that is unauthenticatable by construction.
    let resp = svc
        .mint_executor_tokens(Request::new(MintExecutorTokensRequest {
            intent_ids: vec!["nonexistent".into()],
        }))
        .await?
        .into_inner();
    assert!(
        resp.tokens.is_empty(),
        "unknown intent_id → omitted from map"
    );
    assert!(
        !resp.keyless,
        "bug_121: whole-batch omission under HMAC is NOT conflated \
         with keyless dev mode (Ok(empty) is no longer ambiguous)"
    );

    drop(actor);
    drop(task);
    Ok(())
}

// r[verify sec.executor.identity-token+3]
/// bug_121 keyless face: a scheduler with NO HMAC key reports
/// `keyless=true` with an empty map — the controller's Keyless letter
/// (spawn token-less, dev parity, knob-free). Distinct by wire value
/// from the HMAC-mode omission face above (`keyless=false` + absent
/// ids), so `Ok(empty)` carries which law applies.
#[tokio::test]
async fn mint_keyless_discriminator_distinguishes_dev_from_omission() -> anyhow::Result<()> {
    use crate::actor::tests::{setup_actor_configured, test_sla_config};

    let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    // No `hmac_signer`: the keyless dev posture.
    let (actor, task) = setup_actor_configured(db.pool.clone(), None, |c, _p| {
        c.sla = test_sla_config();
    });
    let svc = AdminServiceImpl::new(
        db.pool.clone(),
        actor.clone(),
        "127.0.0.1:1".into(),
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        crate::lease::LeaderState::default(),
        rio_common::signal::Token::new(),
        String::new(),
        Arc::new(crate::sla::config::SlaConfig::test_default()),
        None,
        Arc::default(),
    );

    let resp = svc
        .mint_executor_tokens(Request::new(MintExecutorTokensRequest {
            intent_ids: vec!["anything".into()],
        }))
        .await?
        .into_inner();
    assert!(resp.tokens.is_empty(), "keyless mode signs nothing");
    assert!(
        resp.keyless,
        "bug_121: the keyless scheduler DECLARES itself — the \
         controller spawns token-less instead of skipping"
    );

    drop(actor);
    drop(task);
    Ok(())
}

// r[verify sched.admin.spawn-intents+2]
// r[verify sched.admission.mint-uncapped]
/// **W9-AD (round-9 B3, Banner A-1/A-2)** — *the priority-head window:
/// at N ≫ limit the served slice is bounded, `truncated` is honest,
/// and the aggregates carry the UNCAPPED demand truth* — the
/// cover-deficit trap's unit-level inverse: a consumer deriving its
/// deficit from `queued_by_system` computes the same number windowed
/// or not (the controller-side consumer constituent is REGISTERED
/// here and lands with the round-9 S4 work).
///
/// W9-AE rides the same drive: a request WITHOUT the limit field (the
/// old-client shape — proto3 default 0) gets the full pre-window
/// behavior, and an un-truncated response omits field 5 on the wire
/// (proto3 default elision) so old readers see nothing new.
#[tokio::test]
async fn spawn_intents_window_bounds_the_page_not_the_truth() -> anyhow::Result<()> {
    use crate::actor::tests::{make_node, merge_dag, setup_actor_configured, test_sla_config};

    let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (actor, task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        c.sla = test_sla_config();
    });
    let svc = AdminServiceImpl::new(
        db.pool.clone(),
        actor.clone(),
        "127.0.0.1:1".into(),
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        crate::lease::LeaderState::default(),
        rio_common::signal::Token::new(),
        String::new(),
        Arc::new(crate::sla::config::SlaConfig::test_default()),
        None,
        Arc::default(),
    );

    const N: usize = 8;
    let nodes: Vec<_> = (0..N).map(|i| make_node(&format!("w-{i}"))).collect();
    merge_dag(&actor, uuid::Uuid::new_v4(), nodes, vec![], false).await?;

    // Old-client shape (no limit): the full set — pre-window behavior.
    let full = svc
        .get_spawn_intents(Request::new(GetSpawnIntentsRequest::default()))
        .await?
        .into_inner();
    assert_eq!(full.intents.len(), N, "limit-absent = unbounded (W9-AE)");
    assert!(!full.truncated, "nothing truncated without a window");
    let full_demand: u64 = full.queued_by_system.values().sum();
    assert_eq!(full_demand, N as u64);

    // Windowed: the page is bounded, the truth is not.
    let windowed = svc
        .get_spawn_intents(Request::new(GetSpawnIntentsRequest {
            limit: 3,
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert_eq!(
        windowed.intents.len(),
        3,
        "the served slice is the request's window (W9-AD)"
    );
    assert!(
        windowed.truncated,
        "truncation honesty: more Ready work existed than the window"
    );
    let windowed_demand: u64 = windowed.queued_by_system.values().sum();
    assert_eq!(
        windowed_demand, full_demand,
        "the aggregate is the UNCAPPED demand truth (A-2) — a deficit \
         derived from it equals the full-set deficit (the cover trap's \
         inverse)"
    );
    // The page is the priority HEAD: every served intent also appears
    // in the full set's first 3 (priority-sorted descending contract).
    let head: std::collections::BTreeSet<_> =
        full.intents.iter().take(3).map(|i| &i.intent_id).collect();
    for i in &windowed.intents {
        assert!(
            head.contains(&i.intent_id),
            "served intent {} is not in the priority head",
            i.intent_id
        );
    }
    // An exactly-fitting window is NOT truncated (the boundary).
    let exact = svc
        .get_spawn_intents(Request::new(GetSpawnIntentsRequest {
            limit: N as u32,
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert_eq!(exact.intents.len(), N);
    assert!(!exact.truncated, "an exact fit is not a truncation");

    drop(task);
    Ok(())
}
