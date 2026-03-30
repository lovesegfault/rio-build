//! `GetSizeClassStatus` RPC tests.
//!
//! Mirrors the `admin/sizeclass.rs` submodule seam. The actor-command
//! level (`GetSizeClassSnapshot`) is covered in `actor/tests/misc.rs`;
//! this file exercises the gRPC-level wiring: the `AdminServiceImpl`
//! handler, leader/actor-alive gates, and the actor→proto conversion.

use super::*;

/// Feature-off: no `with_size_classes` configured → empty response, not
/// an error. CLI renders "size-class routing disabled."
#[tokio::test]
async fn test_get_size_class_status_empty_when_unconfigured() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc_default().await;

    let resp = svc
        .get_size_class_status(Request::new(GetSizeClassStatusRequest::default()))
        .await?
        .into_inner();

    assert!(
        resp.classes.is_empty(),
        "size_classes unconfigured → empty response (not error)"
    );
    Ok(())
}

// r[verify sched.admin.sizeclass-status]
/// Feature-on: actor configured with 2 classes → response has 2 entries
/// with effective/configured cutoffs matching the config. Proves the
/// gRPC handler threads `ActorCommand::GetSizeClassSnapshot` through to
/// the proto response.
#[tokio::test]
async fn test_get_size_class_status_reports_configured_classes() -> anyhow::Result<()> {
    use crate::actor::tests::setup_actor_configured;
    use crate::assignment::SizeClassConfig;

    let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
    let (actor, task) = setup_actor_configured(db.pool.clone(), None, |a| {
        a.with_size_classes(vec![
            SizeClassConfig {
                name: "small".into(),
                cutoff_secs: 60.0,
                mem_limit_bytes: u64::MAX,
                cpu_limit_cores: None,
            },
            SizeClassConfig {
                name: "large".into(),
                cutoff_secs: 3600.0,
                mem_limit_bytes: u64::MAX,
                cpu_limit_cores: None,
            },
        ])
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
    );

    let resp = svc
        .get_size_class_status(Request::new(GetSizeClassStatusRequest::default()))
        .await?
        .into_inner();

    assert_eq!(
        resp.classes.len(),
        2,
        "two configured classes → two entries"
    );

    // Sorted ascending by effective cutoff (actor sorts before return).
    assert_eq!(resp.classes[0].name, "small");
    assert_eq!(resp.classes[0].effective_cutoff_secs, 60.0);
    assert_eq!(resp.classes[0].configured_cutoff_secs, 60.0);
    assert_eq!(resp.classes[0].queued, 0);
    assert_eq!(resp.classes[0].running, 0);
    // sample_count: empty build_samples table → 0.
    assert_eq!(resp.classes[0].sample_count, 0);

    assert_eq!(resp.classes[1].name, "large");
    assert_eq!(resp.classes[1].effective_cutoff_secs, 3600.0);

    drop(actor);
    drop(task);
    Ok(())
}
