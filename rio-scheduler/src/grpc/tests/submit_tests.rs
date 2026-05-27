//! `SubmitBuild` validation + tenant-resolve + jti-revocation tests.
//!
//! Split from the 1682L monolithic `grpc/tests.rs` (P0395) to mirror
//! the `grpc/scheduler_service.rs` seam (P0356). Covers the ingress
//! validation chain: empty fields, oversized payloads, priority class,
//! tenant name→UUID resolve, trace-id metadata, jti revocation.

use super::*;
use rio_store::test_helpers::seed_tenant;
use rstest::rstest;

type Req = rio_proto::types::SubmitBuildRequest;

/// Ingress-validation table: each `#[case]` mutates one field on a
/// well-formed [`SubmitBuildRequest`](Req), then asserts the handler
/// rejects with `InvalidArgument` and `expected_field` in the message.
/// Per-case comments capture WHY the validation exists — most guard
/// silent-stuck modes (Ready-forever, PG CHECK leak), not crashes.
///
/// `unknown_tenant` / `resolves_known_tenant` stay separate (need
/// `setup_grpc_with_pool()` + multi-part assertions).
#[rstest]
// Empty drv_hash would become a DAG primary key (proto types carry no validation).
#[case::empty_drv_hash(|r: &mut Req| r.nodes[0].drv_hash = String::new(), "drv_hash")]
// Empty drv_path → StorePath::parse fails; would break reverse-lookup if accepted.
#[case::empty_drv_path(|r: &mut Req| r.nodes[0].drv_path = String::new(), "drv_path")]
// Empty system never matches any worker → sits Ready forever with no feedback.
#[case::empty_system(|r: &mut Req| r.nodes[0].system = String::new(), "system")]
// > MAX_DRV_CONTENT_BYTES drv_content — the shared gateway/scheduler bound
// (1 MiB; the gateway's hook-fallback cap aliases the same constant). The
// scheduler re-checks it as defense in depth against direct submitters.
#[case::oversized_drv_content(
    |r: &mut Req| r.nodes[0].drv_content =
        vec![b'a'; rio_common::limits::MAX_DRV_CONTENT_BYTES + 1],
    "drv_content"
)]
// Unrecognized priority_class would leak as a PG CHECK violation in Status::internal.
#[case::invalid_priority_class(|r: &mut Req| r.priority_class = "urgent".into(), "priority_class")]
// > MAX_DAG_EDGES — DoS guard (O(edges) merge loop). Content irrelevant; len-check fires first.
#[case::too_many_edges(
    |r: &mut Req| r.edges = vec![Default::default(); rio_common::limits::MAX_DAG_EDGES + 1],
    "edges"
)]
// bug_155: duplicate drv_hash → batch_upsert_derivations' UNNEST hits PG 21000
// (cardinality_violation) → opaque Internal. Reject at the boundary so the error
// names the offending hash.
#[case::duplicate_drv_hash(
    |r: &mut Req| r.nodes.push(make_node("h")),
    "duplicate drv_hash"
)]
#[tokio::test]
async fn test_submit_build_rejects(#[case] mutate: fn(&mut Req), #[case] expected_field: &str) {
    let (_db, grpc, _handle, _task) = setup_grpc().await;
    let mut req = Req {
        nodes: vec![make_node("h")],
        edges: vec![],
        ..Default::default()
    };
    mutate(&mut req);
    let status = grpc.submit_build(Request::new(req)).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains(expected_field),
        "error should mention {expected_field}: {}",
        status.message()
    );
}

// r[verify sched.tenant.resolve+2]
/// SubmitBuild with a tenant name not in the tenants table → InvalidArgument.
/// Proto field carries tenant NAME (from gateway's authorized_keys comment);
/// scheduler resolves to UUID via PG lookup.
#[tokio::test]
async fn test_submit_build_rejects_unknown_tenant() {
    let (_db, grpc, _handle, _task) = setup_grpc_with_pool().await;

    let req = Request::new(rio_proto::types::SubmitBuildRequest {
        nodes: vec![make_node("h")],
        edges: vec![],
        tenant_name: "nonexistent-team".into(),
        ..Default::default()
    });

    let result = grpc.submit_build(req).await;
    assert!(result.is_err(), "unknown tenant should be rejected");
    let status = result.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("unknown tenant"),
        "error should mention 'unknown tenant': {}",
        status.message()
    );
    assert!(
        status.message().contains("nonexistent-team"),
        "error should include the tenant name: {}",
        status.message()
    );
}

/// SubmitBuild with a tenant name that IS in the tenants table → resolves
/// to the UUID and the build is submitted successfully.
#[tokio::test]
async fn test_submit_build_resolves_known_tenant() {
    let (db, grpc, _handle, _task) = setup_grpc_with_pool().await;

    // Seed the tenants table.
    let tenant_uuid = seed_tenant(&db.pool, "team-alpha").await;

    let req = Request::new(rio_proto::types::SubmitBuildRequest {
        nodes: vec![make_node("resolve-tenant-drv")],
        edges: vec![],
        tenant_name: "team-alpha".into(),
        ..Default::default()
    });

    let result = grpc.submit_build(req).await;
    assert!(
        result.is_ok(),
        "known tenant should be accepted: {result:?}"
    );

    // Verify the build row has the resolved UUID.
    let db_tenant: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM builds ORDER BY submitted_at DESC LIMIT 1")
            .fetch_one(&db.pool)
            .await
            .expect("build lookup");
    assert_eq!(db_tenant, Some(tenant_uuid));
}

/// Regression for the (256 KiB, 1 MiB] window: a node whose `drv_content`
/// is exactly the shared bound (`MAX_DRV_CONTENT_BYTES`, i.e. the
/// gateway's content-bound hook-fallback cap) is accepted at ingress —
/// the scheduler bound aliases the gateway producer cap, so nothing the
/// gateway emits is size-rejected here.
#[tokio::test]
async fn test_submit_build_accepts_drv_content_at_the_shared_bound() {
    let (db, grpc, _handle, _task) = setup_grpc_with_pool().await;
    seed_tenant(&db.pool, "team-bound").await;

    let mut node = make_node("at-bound-drv");
    node.drv_content = vec![b'a'; rio_common::limits::MAX_DRV_CONTENT_BYTES];
    let req = Request::new(rio_proto::types::SubmitBuildRequest {
        nodes: vec![node],
        edges: vec![],
        tenant_name: "team-bound".into(),
        ..Default::default()
    });

    let result = grpc.submit_build(req).await;
    assert!(
        result.is_ok(),
        "drv_content at the shared bound must be accepted: {result:?}"
    );
}

// r[verify sched.tenant.authz+2]
/// merged_bug_057: in JWT mode, SchedulerService rejects token-less
/// calls (the permissive interceptor's third state would otherwise let
/// a builder reach SubmitBuild/CancelBuild/WatchBuild unauthenticated).
#[tokio::test]
async fn test_submit_build_jwt_mode_rejects_tokenless() {
    let (_db, mut grpc, _handle, _task) = setup_grpc_with_pool().await;
    grpc.jwt_mode = true;

    let req = Request::new(rio_proto::types::SubmitBuildRequest {
        nodes: vec![make_node("h")],
        ..Default::default()
    });
    let status = grpc.submit_build(req).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unauthenticated);

    // Same gate on CancelBuild.
    let status = grpc
        .cancel_build(Request::new(rio_proto::types::CancelBuildRequest {
            build_id: uuid::Uuid::new_v4().to_string(),
            reason: "x".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
}

// r[verify sched.tenant.authz+2]
/// merged_bug_057: `claims.sub` is the authoritative tenant identity.
/// A caller holding tenant-A's claims with body `tenant_name="B"`
/// MUST be attributed to A, not B.
#[tokio::test]
async fn test_submit_build_claims_sub_overrides_body_tenant_name() {
    let (db, grpc, _handle, _task) = setup_grpc_with_pool().await;
    let tenant_a = seed_tenant(&db.pool, "team-a").await;
    let _tenant_b = seed_tenant(&db.pool, "team-b").await;

    let mut req = Request::new(rio_proto::types::SubmitBuildRequest {
        nodes: vec![make_node("override")],
        tenant_name: "team-b".into(),
        ..Default::default()
    });
    req.extensions_mut().insert(rio_auth::jwt::TenantClaims {
        sub: tenant_a,
        iat: 0,
        exp: i64::MAX,
        jti: "test".into(),
    });
    grpc.submit_build(req)
        .await
        .expect("submit with claims should succeed");

    let db_tenant: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM builds ORDER BY submitted_at DESC LIMIT 1")
            .fetch_one(&db.pool)
            .await
            .expect("build lookup");
    assert_eq!(
        db_tenant,
        Some(tenant_a),
        "claims.sub MUST override body tenant_name"
    );
}

// r[verify sched.tenant.authz+2]
/// merged_bug_057: cross-tenant Cancel/Watch is rejected with
/// PERMISSION_DENIED. Submit as A, attempt cancel/watch as B.
#[tokio::test]
async fn test_cancel_watch_cross_tenant_denied() {
    let (db, grpc, handle, _task) = setup_grpc_with_pool().await;
    let tenant_a = seed_tenant(&db.pool, "team-a").await;
    let tenant_b = seed_tenant(&db.pool, "team-b").await;

    // Submit as A.
    let mut req = Request::new(rio_proto::types::SubmitBuildRequest {
        nodes: vec![make_node("xtenant")],
        ..Default::default()
    });
    req.extensions_mut().insert(rio_auth::jwt::TenantClaims {
        sub: tenant_a,
        iat: 0,
        exp: i64::MAX,
        jti: "a".into(),
    });
    let resp = grpc.submit_build(req).await.expect("submit as A");
    let build_id = resp
        .metadata()
        .get(rio_proto::BUILD_ID_HEADER)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();

    let claims_b = rio_auth::jwt::TenantClaims {
        sub: tenant_b,
        iat: 0,
        exp: i64::MAX,
        jti: "b".into(),
    };

    // Cancel as B → PermissionDenied.
    let mut cancel = Request::new(rio_proto::types::CancelBuildRequest {
        build_id: build_id.clone(),
        reason: "evil".into(),
    });
    cancel.extensions_mut().insert(claims_b.clone());
    let status = grpc.cancel_build(cancel).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::PermissionDenied);

    // Watch as B → PermissionDenied.
    let mut watch = Request::new(rio_proto::types::WatchBuildRequest {
        build_id: build_id.clone(),
        since_sequence: 0,
    });
    watch.extensions_mut().insert(claims_b);
    let status = grpc.watch_build(watch).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::PermissionDenied);

    // Cancel as A → succeeds.
    let mut cancel_a = Request::new(rio_proto::types::CancelBuildRequest {
        build_id,
        reason: "owner".into(),
    });
    cancel_a
        .extensions_mut()
        .insert(rio_auth::jwt::TenantClaims {
            sub: tenant_a,
            iat: 0,
            exp: i64::MAX,
            jti: "a".into(),
        });
    grpc.cancel_build(cancel_a)
        .await
        .expect("owner cancel should succeed");
    drop(handle);
}

// r[verify obs.trace.scheduler-id-in-metadata]
/// SubmitBuild sets `x-rio-trace-id` in response metadata to the handler
/// span's trace_id, which DIFFERS from any injected `traceparent` (proving
/// the #[instrument]+link_parent combination produces a LINKED orphan, not
/// a child — the scheduler span keeps its own trace_id).
///
/// Requires the tracing→OTel bridge so #[instrument] spans get real
/// TraceIds. Scoped via `set_default` drop-guard so other tests on the
/// same thread are unaffected.
#[tokio::test]
async fn test_submit_build_sets_trace_id_header() {
    use opentelemetry::trace::TracerProvider;
    use tracing_subscriber::layer::SubscriberExt;

    // Bridge tracing→OTel so tracing::Span::current().context() yields a
    // real OTel SpanContext. Bare SdkTracerProvider (no exporter) gives
    // real 128-bit IDs without any network.
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
    let tracer = provider.tracer("test");
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = tracing_subscriber::registry().with(otel_layer);

    // W3C propagator so the injected traceparent is parsed by link_parent.
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    let (_db, grpc, _handle, _task) = setup_grpc().await;

    // Synthesize a W3C traceparent with a known trace_id. Format:
    // 00-{32-hex trace_id}-{16-hex span_id}-{2-hex flags}. Use non-zero
    // sampled flag (01) so the propagator doesn't drop it.
    let injected_tid = "abcdef0123456789abcdef0123456789";
    let traceparent = format!("00-{injected_tid}-0123456789abcdef-01");

    let mut req = Request::new(rio_proto::types::SubmitBuildRequest {
        nodes: vec![make_node("trace-id-drv")],
        edges: vec![],
        ..Default::default()
    });
    req.metadata_mut()
        .insert("traceparent", traceparent.parse().unwrap());

    // Scope the OTel bridge around the handler call. set_default installs
    // for the current thread and returns a drop-guard; the single-thread
    // tokio test runtime keeps the handler's await chain on this thread.
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);
    let resp = grpc
        .submit_build(req)
        .await
        .expect("SubmitBuild should succeed");
    drop(_subscriber_guard);

    let header = resp
        .metadata()
        .get(rio_proto::TRACE_ID_HEADER)
        .expect("x-rio-trace-id should be set under the OTel bridge");
    let header_tid = header.to_str().expect("ASCII hex");
    assert_eq!(header_tid.len(), 32, "trace_id is 32-hex: {header_tid}");
    assert!(
        header_tid
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "trace_id is lowercase hex: {header_tid}"
    );
    // The LOAD-BEARING assertion: the scheduler span's trace_id is NOT the
    // injected one. #[instrument] created the span BEFORE link_parent ran,
    // so it kept its own trace_id. link_parent added a LINK, not a parent.
    // This is documented, not a bug — see the obs.trace spec.
    assert_ne!(
        header_tid, injected_tid,
        "scheduler span must have its OWN trace_id (LINKED to gateway's, \
         not parented). If this fails, #[instrument]+link_parent semantics \
         changed and the x-rio-trace-id mechanism needs revisiting."
    );
}

/// SubmitBuild WITHOUT an OTel tracer does NOT set `x-rio-trace-id`
/// (empty-guard: current_trace_id_hex → "" for TraceId::INVALID → no
/// header, not a junk "invalid" string).
#[tokio::test]
async fn test_submit_build_no_otel_no_trace_id_header() {
    let (_db, grpc, _handle, _task) = setup_grpc().await;

    let req = Request::new(rio_proto::types::SubmitBuildRequest {
        nodes: vec![make_node("no-otel-drv")],
        edges: vec![],
        ..Default::default()
    });

    let resp = grpc.submit_build(req).await.expect("submit should succeed");

    // No OTel subscriber → TraceId::INVALID → empty → header skipped.
    // x-rio-build-id IS still set (UUID doesn't need OTel).
    assert!(
        resp.metadata().get(rio_proto::TRACE_ID_HEADER).is_none(),
        "no-OTel path must not set x-rio-trace-id (no junk 'invalid' string)"
    );
    assert!(
        resp.metadata().get(rio_proto::BUILD_ID_HEADER).is_some(),
        "x-rio-build-id should always be set"
    );
}

/// SubmitBuild with empty tenant_name (single-tenant mode) → None, no PG lookup.
/// This is the common case and must work even without a pool.
#[tokio::test]
async fn test_submit_build_empty_tenant_is_none() {
    // Intentionally pool-less to assert no PG hit for empty tenant_name.
    let (db, grpc, _handle, _task) = setup_grpc().await;

    let req = Request::new(rio_proto::types::SubmitBuildRequest {
        nodes: vec![make_node("no-tenant-drv")],
        edges: vec![],
        tenant_name: String::new(), // empty = single-tenant mode
        ..Default::default()
    });

    let result = grpc.submit_build(req).await;
    assert!(
        result.is_ok(),
        "empty tenant_name should succeed without PG: {result:?}"
    );

    // Verify tenant_id is NULL in the build row.
    let db_tenant: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM builds ORDER BY submitted_at DESC LIMIT 1")
            .fetch_one(&db.pool)
            .await
            .expect("build lookup");
    assert_eq!(db_tenant, None);
}

// r[verify sched.tenant.resolve+2]
/// ResolveTenant RPC: known name → UUID string, unknown → InvalidArgument,
/// empty → InvalidArgument (caller error). Exercises the RPC path the
/// gateway calls during JWT mint — same `resolve_tenant_name` helper as
/// SubmitBuild's inline resolve, but different empty-name contract (RPC
/// rejects empty; SubmitBuild treats it as single-tenant Ok(None)).
#[tokio::test]
async fn test_resolve_tenant_rpc() {
    let (db, grpc, _handle, _task) = setup_grpc_with_pool().await;

    // Seed one tenant so we know the ground-truth UUID.
    let expected = seed_tenant(&db.pool, "team-resolve").await;

    // Known → Ok. tenant_id is UUID hyphenated-string form — assert we
    // can PARSE it back (not just string-compare) to catch any future
    // format drift between the handler's .to_string() and uuid's parse.
    let resp = grpc
        .resolve_tenant(Request::new(rio_proto::scheduler::ResolveTenantRequest {
            tenant_name: "team-resolve".into(),
        }))
        .await
        .expect("known tenant resolves");
    let got: uuid::Uuid = resp
        .into_inner()
        .tenant_id
        .parse()
        .expect("tenant_id must be parseable UUID");
    assert_eq!(got, expected);

    // Unknown → InvalidArgument with the name in the message (same
    // diagnostics contract as SubmitBuild's inline resolve).
    let err = grpc
        .resolve_tenant(Request::new(rio_proto::scheduler::ResolveTenantRequest {
            tenant_name: "no-such-team".into(),
        }))
        .await
        .expect_err("unknown → Err");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains("no-such-team"),
        "error should name the tenant: {}",
        err.message()
    );

    // Empty → InvalidArgument. This differs from SubmitBuild (where
    // empty → Ok(None) single-tenant). The RPC contract is: gateway
    // gates empty-comment BEFORE calling (single-tenant mode skips JWT
    // mint entirely), so empty here = caller bug.
    let err = grpc
        .resolve_tenant(Request::new(rio_proto::scheduler::ResolveTenantRequest {
            tenant_name: String::new(),
        }))
        .await
        .expect_err("empty → Err (caller error, not single-tenant)");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains("empty"),
        "error should say 'empty': {}",
        err.message()
    );
}

/// ResolveTenant is NOT leader-gated. A standby replica can answer —
/// it's a read-only PG query, no actor interaction. Gating on
/// leadership would make SSH auth latency depend on leader-election
/// state (bad for the gateway).
#[tokio::test]
async fn test_resolve_tenant_works_on_standby() {
    let (db, grpc, _handle, _task) = setup_grpc_with_pool().await;

    // Flip to standby. Internal field access (same-module test).
    grpc.is_leader
        .store(false, std::sync::atomic::Ordering::Relaxed);

    let expected = seed_tenant(&db.pool, "standby-resolve").await;

    // SubmitBuild WOULD fail here (leader-gated). ResolveTenant doesn't.
    let resp = grpc
        .resolve_tenant(Request::new(rio_proto::scheduler::ResolveTenantRequest {
            tenant_name: "standby-resolve".into(),
        }))
        .await
        .expect("standby still resolves — not leader-gated");
    assert_eq!(
        resp.into_inner().tenant_id.parse::<uuid::Uuid>().unwrap(),
        expected
    );
}

// ---------------------------------------------------------------------------
// r[verify gw.jwt.verify] — jti revocation check in SubmitBuild
//
// These tests bypass the interceptor and attach Claims to the
// request extensions DIRECTLY. That's deliberate: the interceptor's
// sign→verify→attach path is covered by rio-common's jwt_interceptor
// unit tests (invalid/expired/hot-swap). Here we test only the
// REVOCATION query — a pure PG lookup of `claims.jti` against
// `jwt_revoked`. Testing the two layers separately means a failure
// localizes: interceptor bugs show up in rio-common, revocation bugs
// show up here.
// ---------------------------------------------------------------------------

/// Build a Claims with the given jti. Other fields don't matter for
/// the revocation check — it only reads `claims.jti`.
/// Fixed `sub` for jti-revocation tests. Seeded into the tenants
/// table per test (`seed_jti_tenant`) — `claims.sub` is now the
/// authoritative `tenant_id` (`r[sched.tenant.authz]`), so an
/// un-seeded UUID would FK-fail on the build INSERT.
const JTI_TEST_SUB: uuid::Uuid = uuid::Uuid::from_u128(0xFEED);

fn claims_with_jti(jti: &str) -> rio_auth::jwt::TenantClaims {
    rio_auth::jwt::TenantClaims {
        sub: JTI_TEST_SUB,
        iat: 1_700_000_000,
        exp: 9_999_999_999, // far future — expiry is interceptor's job, not ours
        jti: jti.into(),
    }
}

async fn seed_jti_tenant(pool: &sqlx::PgPool) {
    sqlx::query(
        "INSERT INTO tenants (tenant_id, tenant_name) VALUES ($1, 'jti-t') ON CONFLICT DO NOTHING",
    )
    .bind(JTI_TEST_SUB)
    .execute(pool)
    .await
    .expect("seed jti tenant");
}

/// A SubmitBuildRequest that would PASS all the pre-revocation
/// validation (non-empty drv_hash/drv_path/system, valid store path,
/// DAG bounds). We want the revocation check to be the FIRST thing
/// that fails in the negative test — if the request is malformed, we
/// get InvalidArgument instead of Unauthenticated and the test proves
/// nothing about revocation.
fn valid_request_with_claims(jti: &str) -> Request<rio_proto::types::SubmitBuildRequest> {
    let mut req = Request::new(rio_proto::types::SubmitBuildRequest {
        nodes: vec![make_node("revoke-test")],
        edges: vec![],
        ..Default::default()
    });
    // Attach Claims exactly as the interceptor would. The handler
    // reads this via `request.extensions().get::<Claims>()` BEFORE
    // into_inner(). If we put it on a separate struct or skip the
    // attach, the handler's `if let Some(claims)` branch never
    // fires and the test silently passes the no-JWT path.
    req.extensions_mut().insert(claims_with_jti(jti));
    req
}

/// jti IN jwt_revoked → UNAUTHENTICATED "token revoked".
///
/// Self-precondition: we assert the INSERT actually landed (rowcount
/// == 1) before calling submit_build. Without that, a botched INSERT
/// (typo'd table name, whatever) would make the revocation check
/// pass, and the test would fail for the WRONG reason — we'd chase
/// a non-bug in the handler. Same "proves nothing" guard as
/// rio-store/src/nar_roundtrip.rs:85.
#[tokio::test]
async fn revoked_jti_rejected_by_scheduler() {
    // with_pool — the revocation check NEEDS the pool. setup_grpc()
    // (pool=None) would hit the failed_precondition branch instead,
    // testing the wrong thing.
    let (db, grpc, _handle, _task) = setup_grpc_with_pool().await;
    seed_jti_tenant(&db.pool).await;

    let jti = "revoked-session-abc123";
    let inserted = sqlx::query("INSERT INTO jwt_revoked (jti, reason) VALUES ($1, $2)")
        .bind(jti)
        .bind("test: simulated session compromise")
        .execute(&db.pool)
        .await
        .expect("insert into jwt_revoked");
    assert_eq!(
        inserted.rows_affected(),
        1,
        "self-precondition: jti must be in jwt_revoked BEFORE we test the check"
    );

    let status = grpc
        .submit_build(valid_request_with_claims(jti))
        .await
        .expect_err("revoked jti → submit_build must fail");

    assert_eq!(
        status.code(),
        tonic::Code::Unauthenticated,
        "revoked token gets the same code as bad-sig/expired — \
         from the client's view it's one failure mode"
    );
    assert!(
        status.message().contains("revoked"),
        "message should say revoked so operators don't chase \
         signature/expiry red herrings: {}",
        status.message()
    );
}

/// jti NOT in jwt_revoked → the revocation check passes. The
/// request continues into the actor (and actually succeeds — it's
/// a valid 1-node DAG). Positive control: without this, the
/// negative test above could be passing because we broke
/// submit_build entirely.
#[tokio::test]
async fn unrevoked_jti_passes_through() {
    let (db, grpc, _handle, _task) = setup_grpc_with_pool().await;
    seed_jti_tenant(&db.pool).await;

    // Stronger self-precondition than "don't insert": populate
    // jwt_revoked with OTHER jtis, then assert OURS isn't among
    // them. Proves the EXISTS query is actually filtering on jti,
    // not doing `SELECT EXISTS(SELECT 1 FROM jwt_revoked)` (which
    // would be true for ANY non-empty table and reject everything).
    for other in ["some-other-session", "yet-another", "not-this-one"] {
        sqlx::query("INSERT INTO jwt_revoked (jti) VALUES ($1)")
            .bind(other)
            .execute(&db.pool)
            .await
            .expect("insert decoy jti");
    }
    let jti = "clean-session-xyz789";
    let present: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM jwt_revoked WHERE jti = $1)")
            .bind(jti)
            .fetch_one(&db.pool)
            .await
            .expect("precondition query");
    assert!(
        !present,
        "self-precondition: jti must NOT be in jwt_revoked (table has \
         {} decoy rows but not ours)",
        3
    );

    let result = grpc.submit_build(valid_request_with_claims(jti)).await;
    // We don't assert Ok — the actor might reject for unrelated
    // reasons in a future refactor. We assert it's NOT the
    // revocation failure. A "token revoked" error here would mean
    // the query is matching on something other than jti.
    if let Err(status) = &result {
        assert!(
            !status.message().contains("revoked"),
            "unrevoked jti wrongly rejected as revoked: {}",
            status.message()
        );
    }
    // But with the current handler, a valid 1-node DAG DOES
    // succeed, so assert that too — stronger check while it holds.
    assert!(
        result.is_ok(),
        "valid request + unrevoked jti should pass: {:?}",
        result.err()
    );
}

/// No Claims attached → revocation check skipped (the `if let Some`
/// branch never fires). Dev mode / dual-mode fallback path. The
/// request succeeds without ever touching jwt_revoked.
///
/// Regression guard: if someone changes the handler from
/// `if let Some(claims)` to `.ok_or_else(Status::internal(...))?`
/// (as an earlier draft of this plan specified), THIS test catches
/// it — dev mode would be bricked.
#[tokio::test]
async fn no_claims_skips_revocation_check() {
    let (db, grpc, _handle, _task) = setup_grpc_with_pool().await;

    // Populate jwt_revoked so we know a stray lookup WOULD find
    // something. If the handler somehow invented a jti out of
    // thin air and looked it up, a populated table makes that more
    // likely to show up as a false reject.
    sqlx::query("INSERT INTO jwt_revoked (jti) VALUES ('irrelevant')")
        .execute(&db.pool)
        .await
        .expect("insert");

    // No Claims in extensions — the normal state for dev/VM tests.
    let req = Request::new(rio_proto::types::SubmitBuildRequest {
        nodes: vec![make_node("no-jwt")],
        edges: vec![],
        ..Default::default()
    });

    let result = grpc.submit_build(req).await;
    assert!(
        result.is_ok(),
        "no-Claims path must not fail — this is every key-unset deploy: {:?}",
        result.err()
    );
}

/// bug_124 / `r[gw.jwt.verify]`: jti revocation MUST cover every
/// SchedulerService ingress RPC, not just SubmitBuild. A revoked-but-
/// unexpired token (≤8h window per `r[gw.jwt.claims]`) reaching
/// `cancel_build` / `watch_build` / `query_build_status` previously
/// passed `require_tenant` (synchronous, never touched PG) and could
/// cancel the tenant's builds + stream their log output until natural
/// expiry. Hoisting the lookup into `require_tenant` closes all four.
// r[verify gw.jwt.verify]
// r[verify sched.tenant.authz+2]
#[tokio::test]
async fn revoked_jti_rejected_by_cancel_watch_query() {
    let (db, grpc, _handle, _task) = setup_grpc_with_pool().await;
    seed_jti_tenant(&db.pool).await;

    let jti = "revoked-session-cwq";
    let inserted = sqlx::query("INSERT INTO jwt_revoked (jti, reason) VALUES ($1, 'test')")
        .bind(jti)
        .execute(&db.pool)
        .await
        .expect("insert into jwt_revoked");
    assert_eq!(
        inserted.rows_affected(),
        1,
        "self-precondition: jti revoked"
    );

    let build_id = uuid::Uuid::new_v4().to_string();

    // CancelBuild — destructive. Pre-fix: reached the actor's
    // tenant-ownership check (which would PASS for the token's own
    // tenant — the leak scenario).
    let mut cancel = Request::new(rio_proto::types::CancelBuildRequest {
        build_id: build_id.clone(),
        reason: "evil".into(),
    });
    cancel.extensions_mut().insert(claims_with_jti(jti));
    let s = grpc
        .cancel_build(cancel)
        .await
        .expect_err("revoked jti → cancel_build must fail");
    assert_eq!(s.code(), tonic::Code::Unauthenticated);
    assert!(s.message().contains("revoked"), "got: {}", s.message());

    // WatchBuild — leaks build progress + log output.
    let mut watch = Request::new(rio_proto::types::WatchBuildRequest {
        build_id: build_id.clone(),
        since_sequence: 0,
    });
    watch.extensions_mut().insert(claims_with_jti(jti));
    let s = grpc
        .watch_build(watch)
        .await
        .expect_err("revoked jti → watch_build must fail");
    assert_eq!(s.code(), tonic::Code::Unauthenticated);
    assert!(s.message().contains("revoked"), "got: {}", s.message());

    // QueryBuildStatus — leaks build state.
    let mut query = Request::new(rio_proto::types::QueryBuildRequest { build_id });
    query.extensions_mut().insert(claims_with_jti(jti));
    let s = grpc
        .query_build_status(query)
        .await
        .expect_err("revoked jti → query_build_status must fail");
    assert_eq!(s.code(), tonic::Code::Unauthenticated);
    assert!(s.message().contains("revoked"), "got: {}", s.message());
}

// ── Authoritative inline drv_content: ingress identity binding ──────────
//
// `drv_content_authoritative` means "persist these bytes and rebuild them
// verbatim after failover", so the scheduler must not take a submitter's
// word for them: the bytes must describe a content-bound derivation
// consistent with the node's claimed identity, and the flag is only valid
// for the single-node hook-fallback shape.
// r[verify sched.recovery.inline-drv-durability+2]

/// Helper: a floating-CA ATerm + the node fields that legitimately
/// describe it (what the gateway's content-bound fallback produces).
fn authoritative_ca_node(tag: &str) -> rio_proto::types::DerivationNode {
    let aterm = r#"Derive([("out","","r:sha256","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("out","")])"#;
    let mut node = make_node(tag);
    let drv = rio_nix::derivation::Derivation::parse(aterm).unwrap();
    let hash = rio_nix::derivation::hash_derivation_modulo(
        &drv,
        &node.drv_path,
        &|_| None,
        &mut std::collections::HashMap::new(),
    )
    .unwrap();
    node.drv_content = aterm.as_bytes().to_vec();
    node.drv_content_authoritative = true;
    node.expected_output_paths = vec![String::new()];
    node.is_content_addressed = true;
    node.needs_resolve = true;
    node.ca_modular_hash = hash.to_vec();
    node
}

#[tokio::test]
async fn test_submit_build_rejects_authoritative_in_multi_node_dag() {
    let (_db, grpc, _handle, _task) = setup_grpc().await;
    let req = Req {
        nodes: vec![
            authoritative_ca_node("auth-multi-a"),
            make_node("auth-multi-b"),
        ],
        edges: vec![],
        ..Default::default()
    };
    let status = grpc.submit_build(Request::new(req)).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("single-node"),
        "should name the single-node constraint: {}",
        status.message()
    );
}

#[tokio::test]
async fn test_submit_build_rejects_authoritative_identity_mismatch() {
    let (_db, grpc, _handle, _task) = setup_grpc().await;
    // Fixed-output ATerm whose declared path is NOT what its declared
    // hash derives to — the canonical poisoning shape.
    let aterm = r#"Derive([("out","/nix/store/ffffffffffffffffffffffffffffffff-victim","sha256","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("out","/nix/store/ffffffffffffffffffffffffffffffff-victim")])"#;
    let mut node = make_node("auth-mismatch");
    node.drv_content = aterm.as_bytes().to_vec();
    node.drv_content_authoritative = true;
    node.is_fixed_output = true;
    node.is_content_addressed = true;
    node.expected_output_paths = vec!["/nix/store/ffffffffffffffffffffffffffffffff-victim".into()];
    let status = grpc
        .submit_build(Request::new(Req {
            nodes: vec![node],
            edges: vec![],
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("derives to"),
        "should name the path/hash mismatch: {}",
        status.message()
    );
}

#[tokio::test]
async fn test_submit_build_rejects_authoritative_input_addressed_content() {
    let (_db, grpc, _handle, _task) = setup_grpc().await;
    // Plain IA-shaped output (declared path, no hash): never a
    // legitimate hook fallback, and exactly the shape that could squat
    // another derivation's output paths after a failover.
    let aterm = r#"Derive([("out","/nix/store/ffffffffffffffffffffffffffffffff-victim","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("out","/nix/store/ffffffffffffffffffffffffffffffff-victim")])"#;
    let mut node = make_node("auth-ia");
    node.drv_content = aterm.as_bytes().to_vec();
    node.drv_content_authoritative = true;
    node.expected_output_paths = vec!["/nix/store/ffffffffffffffffffffffffffffffff-victim".into()];
    let status = grpc
        .submit_build(Request::new(Req {
            nodes: vec![node],
            edges: vec![],
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("content-bound"),
        "should require content-bound outputs: {}",
        status.message()
    );
}

#[tokio::test]
async fn test_submit_build_accepts_authoritative_fod_fallback() {
    let (db, grpc, _handle, _task) = setup_grpc_with_pool().await;
    seed_tenant(&db.pool, "team-fod-hook").await;

    let mut node = make_node("auth-fod-ok");
    // Honest FOD: derive the path from the declared hash exactly like
    // the gateway/builder do.
    let digest =
        hex::decode("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855").unwrap();
    let nix_hash = rio_nix::hash::NixHash::new("sha256".parse().unwrap(), digest).unwrap();
    let drv_name = {
        let sp = rio_nix::store_path::StorePath::parse(&node.drv_path).unwrap();
        sp.name()
            .strip_suffix(".drv")
            .unwrap_or(sp.name())
            .to_owned()
    };
    let honest = rio_nix::store_path::StorePath::make_fixed_output(&drv_name, &nix_hash, true, &[])
        .unwrap()
        .as_str()
        .to_owned();
    let aterm = format!(
        r#"Derive([("out","{honest}","r:sha256","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("out","{honest}")])"#
    );
    node.drv_content = aterm.into_bytes();
    node.drv_content_authoritative = true;
    node.is_fixed_output = true;
    node.is_content_addressed = true;
    node.expected_output_paths = vec![honest];

    let result = grpc
        .submit_build(Request::new(Req {
            nodes: vec![node],
            edges: vec![],
            tenant_name: "team-fod-hook".into(),
            ..Default::default()
        }))
        .await;
    assert!(result.is_ok(), "honest FOD fallback accepted: {result:?}");
}

#[tokio::test]
async fn test_submit_build_accepts_authoritative_floating_ca_fallback() {
    let (db, grpc, _handle, _task) = setup_grpc_with_pool().await;
    seed_tenant(&db.pool, "team-ca-hook").await;
    let node = authoritative_ca_node("auth-ca-ok");
    let result = grpc
        .submit_build(Request::new(Req {
            nodes: vec![node],
            edges: vec![],
            tenant_name: "team-ca-hook".into(),
            ..Default::default()
        }))
        .await;
    assert!(
        result.is_ok(),
        "honest floating-CA fallback accepted: {result:?}"
    );
}

#[tokio::test]
async fn test_submit_build_rejects_authoritative_modular_hash_mismatch() {
    let (_db, grpc, _handle, _task) = setup_grpc().await;
    let mut node = authoritative_ca_node("auth-ca-badhash");
    node.ca_modular_hash = vec![0u8; 32];
    let status = grpc
        .submit_build(Request::new(Req {
            nodes: vec![node],
            edges: vec![],
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("ca_modular_hash"),
        "should name the hash mismatch: {}",
        status.message()
    );
}

// ── Merge-time authoritative-content protection ─────────────────────────
//
// Ingress validation binds authoritative bytes to the SUBMITTER's claims;
// the merge-time rules bind them across submissions: a second submitter
// cannot redefine an in-flight authoritative node, and a joining
// submission cannot rewrite or clear its persisted recovery row.

// r[verify sched.merge.authoritative-conflict+2]
#[tokio::test]
async fn test_submit_build_authoritative_conflict_is_failed_precondition() {
    let (db, grpc, _handle, _task) = setup_grpc_with_pool().await;
    seed_tenant(&db.pool, "team-auth-a").await;
    seed_tenant(&db.pool, "team-auth-b").await;

    // Tenant A establishes the in-flight authoritative node.
    let node_a = authoritative_ca_node("auth-conflict");
    let drv_hash = node_a.drv_hash.clone();
    let original_bytes = node_a.drv_content.clone();
    grpc.submit_build(Request::new(Req {
        nodes: vec![node_a],
        edges: vec![],
        tenant_name: "team-auth-a".into(),
        ..Default::default()
    }))
    .await
    .expect("first authoritative submission accepted");

    // Tenant B claims the same drv_path with DIFFERENT authoritative
    // bytes — self-consistent (so ingress identity validation passes),
    // but conflicting with the in-flight node.
    let mut node_b = authoritative_ca_node("auth-conflict");
    let aterm_b = r#"Derive([("out","","r:sha256","")],[],[],"x86_64-linux","/bin/sh",["-c","echo poisoned"],[("out","")])"#;
    node_b.drv_content = aterm_b.as_bytes().to_vec();
    node_b.ca_modular_hash = {
        let drv = rio_nix::derivation::Derivation::parse(aterm_b).unwrap();
        rio_nix::derivation::hash_derivation_modulo(
            &drv,
            &node_b.drv_path,
            &|_| None,
            &mut std::collections::HashMap::new(),
        )
        .unwrap()
        .to_vec()
    };
    let status = grpc
        .submit_build(Request::new(Req {
            nodes: vec![node_b],
            edges: vec![],
            tenant_name: "team-auth-b".into(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(
        status.message().contains("authoritative"),
        "should name the authoritative-content conflict: {}",
        status.message()
    );

    // The persisted recovery row still carries tenant A's bytes.
    let row: (Option<Vec<u8>>,) =
        sqlx::query_as("SELECT drv_content FROM derivations WHERE drv_hash = $1")
            .bind(&drv_hash)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(row.0.as_deref(), Some(original_bytes.as_slice()));
}

// r[verify sched.persist.creation-scoped]
#[tokio::test]
async fn test_submit_build_join_does_not_clear_authoritative_row() {
    let (db, grpc, _handle, _task) = setup_grpc_with_pool().await;
    seed_tenant(&db.pool, "team-auth-keep").await;

    let node_a = authoritative_ca_node("auth-keep");
    let drv_hash = node_a.drv_hash.clone();
    let original_bytes = node_a.drv_content.clone();
    grpc.submit_build(Request::new(Req {
        nodes: vec![node_a],
        edges: vec![],
        tenant_name: "team-auth-keep".into(),
        ..Default::default()
    }))
    .await
    .expect("authoritative submission accepted");

    // A later store-backed submission with the SAME verifiable identity
    // (including the matching CA modular hash the gateway computes for
    // this no-inputDrvs derivation — the merge gate's content evidence)
    // joins the live node. Before creation-scoped persistence this
    // re-upserted the row and cleared the authoritative bytes.
    let mut joiner = make_node("auth-keep");
    joiner.is_content_addressed = true;
    joiner.needs_resolve = true;
    joiner.expected_output_paths = vec![String::new()];
    joiner.ca_modular_hash = authoritative_ca_node("auth-keep").ca_modular_hash;
    grpc.submit_build(Request::new(Req {
        nodes: vec![joiner],
        edges: vec![],
        tenant_name: "team-auth-keep".into(),
        ..Default::default()
    }))
    .await
    .expect("store-backed join accepted");

    let row: (Option<Vec<u8>>,) =
        sqlx::query_as("SELECT drv_content FROM derivations WHERE drv_hash = $1")
            .bind(&drv_hash)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        row.0.as_deref(),
        Some(original_bytes.as_slice()),
        "joining submission must not clear the creating submission's authoritative bytes"
    );
}

/// Non-authoritative drv_content is dispatch payload only and is not
/// subject to the authoritative identity binding; the scheduler must keep
/// accepting it without binding. The surviving producers of this shape are
/// the gateway's inline-.drv optimization on store-backed nodes (a small
/// cached .drv inlined alongside a fetchable store copy) and direct
/// submitters — the gateway's hook fallback always claims the
/// authoritative copy, and an unverifiable-algo offender without a
/// resolvable .drv is rejected at the gateway rather than forwarded.
#[tokio::test]
async fn test_submit_build_accepts_non_authoritative_md5_fod_content() {
    let (db, grpc, _handle, _task) = setup_grpc_with_pool().await;
    seed_tenant(&db.pool, "team-md5-exempt").await;

    let mut node = make_node("md5-exempt");
    let out_path = "/nix/store/ffffffffffffffffffffffffffffffff-fetched";
    let aterm = format!(
        r#"Derive([("out","{out_path}","md5","deadbeefdeadbeefdeadbeefdeadbeef")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("out","{out_path}")])"#
    );
    let drv = rio_nix::derivation::Derivation::parse(&aterm).unwrap();
    node.ca_modular_hash = rio_nix::derivation::hash_derivation_modulo(
        &drv,
        &node.drv_path,
        &|_| None,
        &mut std::collections::HashMap::new(),
    )
    .unwrap()
    .to_vec();
    node.drv_content = aterm.into_bytes();
    node.drv_content_authoritative = false;
    node.is_fixed_output = true;
    node.is_content_addressed = true;
    node.expected_output_paths = vec![out_path.into()];

    let result = grpc
        .submit_build(Request::new(Req {
            nodes: vec![node],
            edges: vec![],
            tenant_name: "team-md5-exempt".into(),
            ..Default::default()
        }))
        .await;
    assert!(
        result.is_ok(),
        "non-authoritative md5-FOD inline content accepted: {result:?}"
    );
}

#[tokio::test]
async fn test_submit_build_rejects_authoritative_ca_flag_mismatch() {
    let (_db, grpc, _handle, _task) = setup_grpc().await;
    // Floating-CA bytes but the node claims is_content_addressed=false:
    // the merge-time conflict gate compares that flag, so it must be bound
    // to the bytes at ingress.
    let mut node = authoritative_ca_node("auth-ca-flag");
    node.is_content_addressed = false;
    let status = grpc
        .submit_build(Request::new(Req {
            nodes: vec![node],
            edges: vec![],
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("is_content_addressed"),
        "should name the content-addressed flag mismatch: {}",
        status.message()
    );
}

#[tokio::test]
async fn test_submit_build_rejects_authoritative_missing_expected_paths() {
    let (_db, grpc, _handle, _task) = setup_grpc().await;
    // The expected_output_paths vec must carry exactly one entry per
    // output — a short (or absent) vec previously truncated the zip and
    // skipped the per-output binding silently.
    let mut node = authoritative_ca_node("auth-ca-nopaths");
    node.expected_output_paths = vec![];
    let status = grpc
        .submit_build(Request::new(Req {
            nodes: vec![node],
            edges: vec![],
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("expected_output_paths"),
        "should name the missing expected_output_paths entries: {}",
        status.message()
    );
}

#[tokio::test]
async fn test_submit_build_rejects_authoritative_fod_without_expected_path() {
    let (_db, grpc, _handle, _task) = setup_grpc().await;
    // Honest FOD bytes, but the node declares an EMPTY expected path for
    // the fixed output: the path is the merge gate's content evidence, so
    // its presence is mandatory (not merely checked when present).
    let mut node = make_node("auth-fod-nopath");
    let digest =
        hex::decode("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855").unwrap();
    let nix_hash = rio_nix::hash::NixHash::new("sha256".parse().unwrap(), digest).unwrap();
    let drv_name = {
        let sp = rio_nix::store_path::StorePath::parse(&node.drv_path).unwrap();
        sp.name()
            .strip_suffix(".drv")
            .unwrap_or(sp.name())
            .to_owned()
    };
    let honest = rio_nix::store_path::StorePath::make_fixed_output(&drv_name, &nix_hash, true, &[])
        .unwrap()
        .as_str()
        .to_owned();
    let aterm = format!(
        r#"Derive([("out","{honest}","r:sha256","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("out","{honest}")])"#
    );
    node.drv_content = aterm.into_bytes();
    node.drv_content_authoritative = true;
    node.is_fixed_output = true;
    node.is_content_addressed = true;
    node.expected_output_paths = vec![String::new()];
    let status = grpc
        .submit_build(Request::new(Req {
            nodes: vec![node],
            edges: vec![],
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status
            .message()
            .contains("must declare the fixed-output path"),
        "should require the fixed-output expected path: {}",
        status.message()
    );
}
