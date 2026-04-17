//! `GetSpawnIntents` RPC tests.
//!
//! Mirrors the `admin/spawn_intents.rs` submodule seam. The
//! actor-command level (`GetSpawnIntents`) is covered in
//! `actor/tests/misc.rs`; this file exercises the gRPC-level wiring:
//! the `AdminServiceImpl` handler, leader/actor-alive gates, and the
//! actor→proto conversion.

use super::*;

/// Static-mode: no `[sla]` configured → empty `intents`, not an error.
/// `queued_by_system` is still populated (the ComponentScaler reads it
/// independent of intent emission).
#[tokio::test]
async fn test_get_spawn_intents_empty_when_sla_off() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc_default().await;

    let resp = svc
        .get_spawn_intents(Request::new(GetSpawnIntentsRequest::default()))
        .await?
        .into_inner();

    assert!(
        resp.intents.is_empty(),
        "[sla] unconfigured → empty intents (not error)"
    );
    Ok(())
}

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
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        rio_common::signal::Token::new(),
        String::new(),
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
