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
// >256 KB drv_content — defensive bound (gateway caps at 64 KB; hostile client may bypass).
#[case::oversized_drv_content(
    |r: &mut Req| r.nodes[0].drv_content = vec![b'a'; 256 * 1024 + 1],
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

// r[verify sched.tenant.authz]
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

// r[verify sched.tenant.authz]
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

// r[verify sched.tenant.authz]
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
