//! AdminService test suite.
//!
//! Shared fixtures (`setup_svc`, `setup_svc_default`)
//! live here and are `pub(super)` for the per-domain submodules. Tests
//! for handlers that remain inline in `admin/mod.rs` post-P0383
//! (`ClusterStatus`, `DrainExecutor`, `ClearPoison`, `admin_rpcs_are_wired`
//! smoke test) stay in this file — everything else mirrors the
//! `admin/{gc,tenants,builds,workers,graph,sizeclass}.rs` submodule
//! seams.

use super::*;
use crate::actor::tests::setup_actor;
use rio_test_support::TestDb;
use tokio_stream::StreamExt;

mod builds_tests;
mod gc_tests;
mod graph_tests;
mod spawn_intents_tests;
mod tenants_tests;
mod workers_tests;

/// Set up `AdminServiceImpl` with a live actor.
///
/// `setup_actor` gives a real actor backed by the same PG — no
/// mocks needed. The `_task` keeps the actor task alive; dropping
/// the returned tuple drops the handle → channel closes → actor
/// shuts down cleanly.
///
/// Returns `(svc, actor_handle, task)`. The handle is separate so
/// ClusterStatus tests can also send actor commands directly.
pub(super) async fn setup_svc() -> (
    AdminServiceImpl,
    ActorHandle,
    tokio::task::JoinHandle<()>,
    TestDb,
) {
    let db = TestDb::new(&crate::MIGRATOR).await;
    let (actor, task) = setup_actor(db.pool.clone());
    let svc = AdminServiceImpl::new(
        db.pool.clone(),
        actor.clone(),
        // store_addr: unreachable in tests. TriggerGC would fail
        // the proxy connect with a clear error. Use :1 (fails
        // fast, never listened on) not a timeout-prone addr.
        "127.0.0.1:1".into(),
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        crate::lease::LeaderState::default(),
        rio_common::signal::Token::new(),
        String::new(),
        Arc::new(crate::sla::config::SlaConfig::test_default()),
        // service_verifier=None → dev-mode pass-through. Tests that
        // exercise the gate construct svc with Some(verifier) below.
        None,
        // §13c-3: in production, main.rs sets `resolved_global` before
        // passing the cost_table to the admin svc. Test wires it up
        // here from the test SlaConfig fixture (Some(16, 2GiB)).
        std::sync::Arc::new(parking_lot::RwLock::new({
            let mut t = crate::sla::cost::CostTable::default();
            let g = crate::sla::config::SlaConfig::test_default();
            t.set_resolved_global((g.max_cores.unwrap() as u32, g.max_mem.unwrap()));
            t
        })),
    );
    (svc, actor, task, db)
}

/// `setup_svc` with the common defaults.
pub(super) async fn setup_svc_default() -> (
    AdminServiceImpl,
    ActorHandle,
    tokio::task::JoinHandle<()>,
    TestDb,
) {
    setup_svc().await
}

/// `append_interrupt_sample` is idempotent on `event_uid`. The
/// controller's spot-interrupt watcher consumes `.applied_objects()`,
/// which re-yields every still-extant `SpotInterrupted` Event on
/// relist (controller restart, watch reconnect). Without dedup each
/// relist double-counts into λ's numerator (`refresh_lambda` SUMs
/// `kind='interrupt'` rows) → `solve_full` biases away from spot.
///
/// Exposure rows pass `event_uid=None` and are unconstrained — the
/// M_047 partial unique index only covers non-NULL uids.
#[tokio::test]
async fn append_interrupt_sample_idempotent_on_event_uid() {
    let (svc, _actor, _task, db) = setup_svc_default().await;

    let interrupt = |uid: Option<&str>| AppendInterruptSampleRequest {
        hw_class: "aws-8-nvme-hi".into(),
        kind: "interrupt".into(),
        value: 1.0,
        event_uid: uid.map(String::from),
    };

    // Same Event uid twice → one row.
    svc.append_interrupt_sample(Request::new(interrupt(Some("ev-abc"))))
        .await
        .unwrap();
    svc.append_interrupt_sample(Request::new(interrupt(Some("ev-abc"))))
        .await
        .unwrap();
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM interrupt_samples WHERE event_uid = 'ev-abc'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(n, 1, "same event_uid re-delivered → deduped to one row");

    // Distinct uid → second row.
    svc.append_interrupt_sample(Request::new(interrupt(Some("ev-def"))))
        .await
        .unwrap();
    // NULL uid (exposure path) twice → both inserted (partial index
    // only covers non-NULL).
    let exposure = AppendInterruptSampleRequest {
        hw_class: "aws-8-nvme-hi".into(),
        kind: "exposure".into(),
        value: 60.0,
        event_uid: None,
    };
    svc.append_interrupt_sample(Request::new(exposure.clone()))
        .await
        .unwrap();
    svc.append_interrupt_sample(Request::new(exposure))
        .await
        .unwrap();

    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM interrupt_samples")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(total, 4, "ev-abc(1) + ev-def(1) + 2×NULL exposure = 4 rows");
}

/// Defense-in-depth input validation: lands regardless of the
/// service-token gate (dev-mode pass-through here).
#[tokio::test]
async fn append_interrupt_sample_rejects_invalid_inputs() {
    let (svc, _actor, _task, _db) = setup_svc_default().await;

    let base = AppendInterruptSampleRequest {
        hw_class: "aws-8-nvme-hi".into(),
        kind: "interrupt".into(),
        value: 1.0,
        event_uid: None,
    };

    let bad_kind = AppendInterruptSampleRequest {
        kind: "garbage".into(),
        ..base.clone()
    };
    assert_eq!(
        svc.append_interrupt_sample(Request::new(bad_kind))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::InvalidArgument
    );

    let bad_value = AppendInterruptSampleRequest {
        value: f64::NAN,
        ..base.clone()
    };
    assert_eq!(
        svc.append_interrupt_sample(Request::new(bad_value))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::InvalidArgument
    );

    let neg_value = AppendInterruptSampleRequest {
        value: -1.0,
        ..base.clone()
    };
    assert_eq!(
        svc.append_interrupt_sample(Request::new(neg_value))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::InvalidArgument
    );

    for bad in ["", "Upper-Case", &"x".repeat(65)] {
        let bad_hw = AppendInterruptSampleRequest {
            hw_class: bad.into(),
            ..base.clone()
        };
        assert_eq!(
            svc.append_interrupt_sample(Request::new(bad_hw))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::InvalidArgument,
            "{bad:?} must be rejected"
        );
    }
}

/// Construct svc with a real `service_verifier`. Other defaults match
/// [`setup_svc`]. Returns the signer so tests can mint valid tokens.
async fn setup_svc_with_service_verifier() -> (
    AdminServiceImpl,
    rio_auth::hmac::HmacSigner,
    tokio::task::JoinHandle<()>,
    TestDb,
) {
    let key = b"test-service-hmac-key-32-bytes!!".to_vec();
    let signer = rio_auth::hmac::HmacSigner::from_key(key.clone());
    let verifier = Arc::new(rio_auth::hmac::HmacVerifier::from_key(key));
    let db = TestDb::new(&crate::MIGRATOR).await;
    let (actor, task) = setup_actor(db.pool.clone());
    let svc = AdminServiceImpl::new(
        db.pool.clone(),
        actor,
        "127.0.0.1:1".into(),
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        crate::lease::LeaderState::default(),
        rio_common::signal::Token::new(),
        String::new(),
        Arc::new(crate::sla::config::SlaConfig::test_default()),
        Some(verifier),
        Arc::default(),
    );
    (svc, signer, task, db)
}

fn req_with_token<T>(signer: &rio_auth::hmac::HmacSigner, caller: &str, body: T) -> Request<T> {
    let claims = rio_auth::hmac::ServiceClaims {
        caller: caller.into(),
        expiry_unix: rio_auth::now_unix().unwrap() + 60,
    };
    let mut req = Request::new(body);
    req.metadata_mut().insert(
        rio_proto::SERVICE_TOKEN_HEADER,
        signer.sign(&claims).parse().unwrap(),
    );
    req
}

/// EVERY AdminService RPC appears in exactly one of these. A new RPC
/// that's in neither fails [`admin_rpc_gate_coverage`]. The
/// `SERVICE_GATED` list is mutating + topology/credential-leaking
/// read-paths; `UNGATED_PUBLIC` is the dashboard/grpc-web surface.
///
/// **Payload-sensitivity rule** (`r[sched.sla.threat.read-path-auth]`):
/// an RPC's gate is determined by the most sensitive *field* it
/// returns, not its verb. Any response field that is a signed
/// credential (`*_token`, `*_claims`) or a tenant-scoped secret is
/// `rio-controller`-only regardless of read/write classification —
/// `GetSpawnIntents` was "intentionally ungated" here while it carried
/// `executor_token`, which let a compromised builder on port 9001
/// harvest another tenant's executor identity (bug_028).
/// [`admin_rpc_gate_coverage`] enforces both: every RPC is classified,
/// and no `UNGATED_PUBLIC` response transitively contains a
/// credential-shaped field.
const SERVICE_GATED: &[&str] = &[
    // Mutating ([`mutating_rpcs_require_service_token`]):
    "TriggerGC",
    "DrainExecutor",
    "CancelBuild",
    "ReportExecutorTermination",
    "ClearPoison",
    "CreateTenant",
    "DeleteTenant",
    "AckSpawnedIntents",
    "SetSlaOverride",
    "ClearSlaOverride",
    "ResetSlaModel",
    "ImportSlaCorpus",
    "InjectBuildSample",
    "AppendInterruptSample",
    "ReportAttemptOutcome",
    // Read-path gated ([`read_path_rpcs_require_service_token`]):
    "ListOpenAttempts",
    "ListPoisoned",
    "ListTenants",
    "GetBuildGraph",
    "GetSpawnIntents",
    "MintExecutorTokens",
    "InspectBuildDag",
    "ListSlaOverrides",
    "SlaStatus",
    "SlaExplain",
    "GetSlaDefaults",
    "GetSlaMispredictors",
    "ExportSlaCorpus",
    "HwClassSampled",
    "GetHwClassConfig",
];
const UNGATED_PUBLIC: &[&str] = &[
    "ClusterStatus",
    "ListExecutors",
    "ListBuilds",
    "DebugListExecutors",
];

/// Every mutating AdminService RPC is service-token gated. Builders
/// share port 9001 with this service (CCNP allows scheduler:9001 at L4
/// only) and the JWT interceptor is permissive-on-absent-header — so
/// without `ensure_service_caller` a compromised builder reaches every
/// handler. Per the threat model "the worker is NOT trusted".
///
/// **Adding a new RPC:** add it to [`SERVICE_GATED`] or
/// [`UNGATED_PUBLIC`] above — never neither.
/// [`admin_rpc_gate_coverage`] fails until classified (bug_013: 7 of
/// 11 mutating RPCs were ungated before the canonical list became a
/// test, not a comment).
// r[verify sec.authz.service-token]
#[tokio::test]
async fn mutating_rpcs_require_service_token() {
    let (svc, _signer, _task, _db) = setup_svc_with_service_verifier().await;
    let denied = tonic::Code::PermissionDenied;

    macro_rules! assert_gated {
        ($name:literal, $call:expr) => {{
            let code = $call.await.unwrap_err().code();
            assert_eq!(code, denied, "{} must be service-token gated", $name);
        }};
    }

    assert_gated!(
        "DrainExecutor",
        svc.drain_executor(Request::new(DrainExecutorRequest {
            executor_id: "victim".into(),
            force: true,
        }))
    );
    assert_gated!(
        "ReportExecutorTermination",
        svc.report_executor_termination(Request::new(ReportExecutorTerminationRequest {
            executor_id: "victim".into(),
            reason: TerminationReason::OomKilled as i32,
        }))
    );
    assert_gated!(
        "ReportAttemptOutcome",
        svc.report_attempt_outcome(Request::new(
            rio_proto::types::ReportAttemptOutcomeRequest {
                intent_id: "victim-drv".into(),
                job_name: String::new(),
                exec_id: String::new(),
                reason: rio_proto::types::AttemptTerminalReason::Reaped as i32,
                node_name: String::new(),
            }
        ))
    );
    assert_gated!(
        "AckSpawnedIntents",
        svc.ack_spawned_intents(Request::new(AckSpawnedIntentsRequest {
            spawned: vec![],
            unfulfillable_cells: vec![],
            registered_cells: vec![],
            observed_instance_types: vec![],
            bound_intents: vec![],
        }))
    );
    assert_gated!(
        "AppendInterruptSample",
        svc.append_interrupt_sample(Request::new(AppendInterruptSampleRequest {
            hw_class: "aws-8-nvme-hi".into(),
            kind: "interrupt".into(),
            value: 1.0,
            event_uid: None,
        }))
    );
    assert_gated!(
        "ClearPoison",
        svc.clear_poison(Request::new(ClearPoisonRequest {
            derivation_hash: "h".into(),
        }))
    );
    assert_gated!(
        "CreateTenant",
        svc.create_tenant(Request::new(CreateTenantRequest::default()))
    );
    assert_gated!(
        "DeleteTenant",
        svc.delete_tenant(Request::new(DeleteTenantRequest::default()))
    );
    assert_gated!(
        "SetSlaOverride",
        svc.set_sla_override(Request::new(SetSlaOverrideRequest::default()))
    );
    assert_gated!(
        "ClearSlaOverride",
        svc.clear_sla_override(Request::new(ClearSlaOverrideRequest::default()))
    );
    assert_gated!(
        "ResetSlaModel",
        svc.reset_sla_model(Request::new(ResetSlaModelRequest::default()))
    );
    assert_gated!(
        "ImportSlaCorpus",
        svc.import_sla_corpus(Request::new(ImportSlaCorpusRequest::default()))
    );
    assert_gated!(
        "InjectBuildSample",
        svc.inject_build_sample(Request::new(InjectBuildSampleRequest::default()))
    );
    assert_gated!(
        "CancelBuild",
        svc.cancel_build(Request::new(CancelBuildRequest {
            build_id: uuid::Uuid::nil().to_string(),
            reason: "test".into(),
        }))
    );

    // TriggerGC is server-streaming with the grpc-web Trailers-Only
    // convention: error is yielded IN-STREAM, not as handler-level Err.
    let mut stream = svc
        .trigger_gc(Request::new(GcRequest::default()))
        .await
        .expect("server-streaming handler returns Ok(stream)")
        .into_inner();
    let status = stream
        .next()
        .await
        .expect("one item")
        .expect_err("first item is Err(PermissionDenied)");
    assert_eq!(
        status.code(),
        denied,
        "TriggerGC must be service-token gated"
    );
}

/// Read-path RPCs that leak tenant/DAG/SLA topology are service-token
/// gated. Threat-model gap (a): builders share port 9001 and the JWT
/// interceptor is permissive-on-absent — without this gate a
/// compromised builder enumerates tenants, reads any build's DAG, and
/// learns which (pname,system) keys are under-/over-provisioned.
// r[verify sched.sla.threat.read-path-auth]
#[tokio::test]
async fn read_path_rpcs_require_service_token() {
    let (svc, _signer, _task, _db) = setup_svc_with_service_verifier().await;
    let denied = tonic::Code::PermissionDenied;

    macro_rules! assert_gated {
        ($name:literal, $call:expr) => {{
            let code = $call.await.unwrap_err().code();
            assert_eq!(code, denied, "{} must be service-token gated", $name);
        }};
    }

    assert_gated!("ListPoisoned", svc.list_poisoned(Request::new(())));
    assert_gated!(
        "ListOpenAttempts",
        svc.list_open_attempts(Request::new(rio_proto::types::ListOpenAttemptsRequest {}))
    );
    assert_gated!("ListTenants", svc.list_tenants(Request::new(())));
    assert_gated!(
        "GetBuildGraph",
        svc.get_build_graph(Request::new(GetBuildGraphRequest {
            build_id: Uuid::nil().to_string(),
        }))
    );
    assert_gated!(
        "InspectBuildDag",
        svc.inspect_build_dag(Request::new(InspectBuildDagRequest {
            build_id: Uuid::nil().to_string(),
        }))
    );
    assert_gated!(
        "ListSlaOverrides",
        svc.list_sla_overrides(Request::new(ListSlaOverridesRequest::default()))
    );
    assert_gated!(
        "SlaStatus",
        svc.sla_status(Request::new(SlaStatusRequest::default()))
    );
    assert_gated!(
        "SlaExplain",
        svc.sla_explain(Request::new(SlaExplainRequest::default()))
    );
    assert_gated!("GetSlaDefaults", svc.get_sla_defaults(Request::new(())));
    assert_gated!(
        "GetSlaMispredictors",
        svc.get_sla_mispredictors(Request::new(GetSlaMispredictorsRequest::default()))
    );
    assert_gated!(
        "ExportSlaCorpus",
        svc.export_sla_corpus(Request::new(ExportSlaCorpusRequest::default()))
    );
    assert_gated!(
        "HwClassSampled",
        svc.hw_class_sampled(Request::new(HwClassSampledRequest::default()))
    );
    assert_gated!(
        "GetHwClassConfig",
        svc.get_hw_class_config(Request::new(()))
    );
    assert_gated!(
        "GetSpawnIntents",
        svc.get_spawn_intents(Request::new(GetSpawnIntentsRequest::default()))
    );
    assert_gated!(
        "MintExecutorTokens",
        svc.mint_executor_tokens(Request::new(MintExecutorTokensRequest::default()))
    );
}

/// merged_bug_001: `HwClassSampledResponse.trust_threshold` is the
/// scheduler's `FLEET_MEDIAN_MIN_TENANTS` — single source of truth so
/// the controller's bench-needed gate compares against the SAME value
/// `cross_tenant_median` gates on. r3 R3B5 reconciled unit+granularity
/// but left the controller-side constant at `3` (vs `5` here),
/// deadlocking calibration in the 3..5 band. The
/// `fleet_median_min_tenants_is_5` tripwire (sla/hw.rs) guards the
/// value; this guards the wiring.
#[tokio::test]
async fn bench_threshold_is_fleet_min_tenants() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc_default().await;
    let resp = svc
        .hw_class_sampled(Request::new(HwClassSampledRequest::default()))
        .await?
        .into_inner();
    assert_eq!(
        resp.trust_threshold,
        Some(crate::sla::FLEET_MEDIAN_MIN_TENANTS as u32),
        "HwClassSampled.trust_threshold must emit FLEET_MEDIAN_MIN_TENANTS"
    );
    Ok(())
}

/// §13c-2 r[verify scheduler.sla.ceiling.controller-mirror]:
/// `GetHwClassConfig` ships `min(catalog, cfg)` per axis, each falling
/// to global when absent — the controller's `ceilings_for` reads it.
/// Wire is never zero: with NO catalog (Static / empty) and NO cfg
/// override, the wire carries the global ceiling, not 0.
#[tokio::test]
async fn get_hw_class_config_ships_catalog_or_global_ceilings() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc_default().await;
    let g = crate::sla::config::SlaConfig::test_default();
    // No catalog set on the default cost_table → falls to global.
    let resp = svc
        .get_hw_class_config(Request::new(()))
        .await?
        .into_inner();
    let hc = &resp.hw_classes["test-hw"];
    assert_eq!(
        hc.max_cores,
        g.max_cores.unwrap() as u32,
        "no catalog → global cores"
    );
    assert_eq!(hc.max_mem, g.max_mem.unwrap(), "no catalog → global mem");
    assert!(hc.max_cores > 0 && hc.max_mem > 0, "wire never zero");
    // Catalog says (8, 1GiB) — tighter than global (16, 2GiB). Wire MUST
    // carry the min.
    svc.cost_table
        .write()
        .set_catalog_ceilings(std::collections::HashMap::from([(
            "test-hw".to_owned(),
            (8u32, 1u64 << 30),
        )]));
    let resp = svc
        .get_hw_class_config(Request::new(()))
        .await?
        .into_inner();
    let hc = &resp.hw_classes["test-hw"];
    assert_eq!(hc.max_cores, 8, "catalog tightens below global");
    assert_eq!(hc.max_mem, 1 << 30);
    Ok(())
}

/// Exhaustive gate-coverage: every `AdminService` RPC is in exactly one
/// of [`SERVICE_GATED`] / [`UNGATED_PUBLIC`], and no `UNGATED_PUBLIC`
/// response type transitively contains a credential-shaped field.
///
/// Part (a) is the structural close for bug_013 / bug_028 — the
/// "intentionally ungated" comment was a list nobody enforced; this
/// test fails on a new RPC until it's classified. Part (b) is the
/// payload-sensitivity rule (`r[sched.sla.threat.read-path-auth]`): a
/// signed credential on a multi-caller read-path response is a
/// cross-tenant leak regardless of the verb's gating.
///
/// Walks `rio_proto::FILE_DESCRIPTOR_SET` (the prost binary descriptor
/// rio-proto already emits for rio-test-support's MockAdmin codegen) —
/// the rpc/message/field surface comes structured rather than from
/// regex-parsing the raw `.proto` text, so a `repeated` qualifier
/// moving or a oneof being introduced can't silently widen the grammar.
// r[verify sched.sla.threat.read-path-auth]
#[test]
fn admin_rpc_gate_coverage() {
    use prost::Message;
    use prost_types::{DescriptorProto, FileDescriptorSet};
    use std::collections::{HashMap, HashSet};

    let fds = FileDescriptorSet::decode(rio_proto::FILE_DESCRIPTOR_SET)
        .expect("rio_proto::FILE_DESCRIPTOR_SET decodes as a prost FileDescriptorSet");

    // ─── (a) every RPC is classified, partition is disjoint ──────────
    let admin_svc = fds
        .file
        .iter()
        .flat_map(|f| &f.service)
        .find(|s| s.name() == "AdminService")
        .expect("AdminService in descriptor set");
    let all_rpcs: HashSet<&str> = admin_svc.method.iter().map(|m| m.name()).collect();
    assert!(
        !all_rpcs.is_empty(),
        "AdminService descriptor has no methods"
    );

    let gated: HashSet<&str> = SERVICE_GATED.iter().copied().collect();
    let public: HashSet<&str> = UNGATED_PUBLIC.iter().copied().collect();
    let overlap: Vec<_> = gated.intersection(&public).collect();
    assert!(overlap.is_empty(), "gated ∩ public must be ∅: {overlap:?}");

    let classified: HashSet<&str> = gated.union(&public).copied().collect();
    let unclassified: Vec<_> = all_rpcs.difference(&classified).collect();
    assert!(
        unclassified.is_empty(),
        "every AdminService RPC must be in SERVICE_GATED or UNGATED_PUBLIC \
         (admin/tests/mod.rs); unclassified: {unclassified:?}"
    );
    let stale: Vec<_> = classified.difference(&all_rpcs).collect();
    assert!(
        stale.is_empty(),
        "SERVICE_GATED/UNGATED_PUBLIC names not in admin.proto: {stale:?}"
    );

    // ─── (b) no UNGATED_PUBLIC response transitively carries a
    //         credential-shaped field ───────────────────────────────────
    // Field-name suffixes that mark a credential. `_token`/`_secret`/
    // `_claims` are unambiguous; `_key` is too FP-prone (model_key,
    // cache_key) so it's matched only as the bare `key` in a
    // `*_secret_key`/`*_signing_key` compound — none exist today, and
    // map<K,V> entry field names (`key`) are scoped to the synthetic
    // `*Entry` message, not the parent.
    let bad_suffix = |name: &str| {
        name.ends_with("_token")
            || name.ends_with("_secret")
            || name.ends_with("_claims")
            || name.ends_with("_key")
    };

    // Method.output_type is `.`-qualified (`.rio.types.Foo`,
    // `.google.protobuf.Empty`); take the last segment.
    let response_of: HashMap<&str, &str> = admin_svc
        .method
        .iter()
        .map(|m| (m.name(), m.output_type().rsplit('.').next().unwrap()))
        .collect();

    // Flatten every `rio.types` message (admin_types/types/dag/
    // build_types all share that package) into
    // `name → [(field_type_last_segment, field_name)]`. Nested types
    // are registered by bare name too — proto3 has no nested message
    // declarations in this set, but `map<K,V>` synthesizes a `*Entry`
    // message whose `value` field carries the message type to recurse
    // into. Scalar fields have an empty `type_name`, so they leaf out
    // on the next traversal step (`messages.get("")` is never a hit).
    fn collect(msg: &DescriptorProto, out: &mut HashMap<String, Vec<(String, String)>>) {
        let fields: Vec<(String, String)> = msg
            .field
            .iter()
            .map(|f| {
                let ty = f.type_name().rsplit('.').next().unwrap().to_owned();
                (ty, f.name().to_owned())
            })
            .collect();
        out.entry(msg.name().to_owned()).or_default().extend(fields);
        for nested in &msg.nested_type {
            collect(nested, out);
        }
    }
    let mut messages: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for f in fds.file.iter().filter(|f| f.package() == "rio.types") {
        for msg in &f.message_type {
            collect(msg, &mut messages);
        }
    }
    assert!(
        messages.contains_key("SpawnIntent"),
        "descriptor walk found no rio.types messages"
    );

    // Transitive walk from each UNGATED_PUBLIC response type.
    for &rpc in UNGATED_PUBLIC {
        let resp = response_of
            .get(rpc)
            .unwrap_or_else(|| panic!("response type for {rpc} not in descriptor"));
        if *resp == "Empty" {
            continue;
        }
        let mut seen = HashSet::new();
        let mut stack = vec![resp.to_string()];
        while let Some(ty) = stack.pop() {
            if !seen.insert(ty.clone()) {
                continue;
            }
            let Some(fields) = messages.get(ty.as_str()) else {
                // Scalar / well-known (Timestamp, Empty) — leaf.
                continue;
            };
            for (fty, fname) in fields {
                assert!(
                    !bad_suffix(fname),
                    "UNGATED_PUBLIC rpc {rpc} → {ty}.{fname}: credential-shaped \
                     field on an ungated read-path response — gate the RPC \
                     (add to SERVICE_GATED) or move the field to a \
                     controller-only RPC (r[sched.sla.threat.read-path-auth])"
                );
                stack.push(fty.clone());
            }
        }
    }
}

/// Positive path: valid token with allowlisted `caller` passes the
/// gate; wrong caller is rejected. One representative RPC per
/// allowlist shape — the tokenless-reject coverage above is
/// per-RPC.
#[tokio::test]
async fn service_token_allowlist_enforced() {
    let (svc, signer, _task, db) = setup_svc_with_service_verifier().await;

    // controller-only allowlist (`["rio-controller"]`).
    let r = AppendInterruptSampleRequest {
        hw_class: "aws-8-nvme-hi".into(),
        kind: "interrupt".into(),
        value: 1.0,
        event_uid: None,
    };
    svc.append_interrupt_sample(req_with_token(&signer, "rio-controller", r.clone()))
        .await
        .expect("rio-controller allowed");
    let err = svc
        .append_interrupt_sample(req_with_token(&signer, "rio-gateway", r))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(err.message().contains("allowlist"));
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM interrupt_samples")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(n, 1, "exactly one accepted insert");

    // controller+cli allowlist (`["rio-controller","rio-cli"]`).
    // DrainExecutor is a retired no-op (Unimplemented) — the assertion
    // here is that an allowlisted caller gets PAST the token gate (the
    // retirement error, not PermissionDenied), and that the gate still
    // fronts the stub.
    let drain = DrainExecutorRequest {
        executor_id: "victim".into(),
        force: true,
    };
    let err = svc
        .drain_executor(req_with_token(&signer, "rio-controller", drain.clone()))
        .await
        .unwrap_err();
    assert_eq!(
        err.code(),
        tonic::Code::Unimplemented,
        "rio-controller passes the gate and reaches the retired-surface error"
    );
    let err = svc
        .drain_executor(req_with_token(&signer, "rio-cli", drain.clone()))
        .await
        .unwrap_err();
    assert_eq!(
        err.code(),
        tonic::Code::Unimplemented,
        "rio-cli passes the gate and reaches the retired-surface error"
    );
    let err = svc
        .drain_executor(req_with_token(&signer, "rio-gateway", drain))
        .await
        .unwrap_err();
    assert_eq!(
        err.code(),
        tonic::Code::PermissionDenied,
        "non-allowlisted caller is still rejected at the gate, before the stub"
    );

    // cli-only allowlist (`["rio-cli"]`).
    let cp = ClearPoisonRequest {
        derivation_hash: "h".into(),
    };
    svc.clear_poison(req_with_token(&signer, "rio-cli", cp.clone()))
        .await
        .expect("rio-cli allowed");
    let err = svc
        .clear_poison(req_with_token(&signer, "rio-controller", cp))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
}

/// `AdminService.CancelBuild` is service-token gated. Builders share
/// port 9001; without this gate a compromised builder could cancel
/// arbitrary builds. rio-cli reaches the actor with `caller_tenant:
/// None` (operator override) — this is the path `rio-cli cancel-build`
/// uses; `SchedulerService.CancelBuild` (tenant-JWT gated) is
/// unreachable from the CLI.
// r[verify admin.rpc.cancel-build]
#[tokio::test]
async fn admin_cancel_build_gated_on_service_token() {
    let (svc, signer, _task, _db) = setup_svc_with_service_verifier().await;
    let r = CancelBuildRequest {
        build_id: Uuid::new_v4().to_string(),
        reason: "test".into(),
    };

    // No token → PermissionDenied (NOT Unauthenticated — that's the
    // tenant-JWT gate on SchedulerService).
    let err = svc.cancel_build(Request::new(r.clone())).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(err.message().contains(rio_proto::SERVICE_TOKEN_HEADER));

    // Non-allowlisted caller (a builder identity) → PermissionDenied.
    let err = svc
        .cancel_build(req_with_token(&signer, "rio-builder", r.clone()))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(err.message().contains("allowlist"));

    // rio-cli → reaches the actor. Unknown build_id → actor returns
    // BuildNotFound, which maps to NotFound — proves the request got
    // PAST the gate and into `ActorCommand::CancelBuild{caller_tenant:
    // None}`.
    let err = svc
        .cancel_build(req_with_token(&signer, "rio-cli", r.clone()))
        .await
        .unwrap_err();
    assert_eq!(
        err.code(),
        tonic::Code::NotFound,
        "rio-cli token must reach actor (operator override path)"
    );

    // rio-controller also allowlisted (e.g. orphan-watcher sweep).
    let err = svc
        .cancel_build(req_with_token(&signer, "rio-controller", r))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn admin_rpcs_are_wired() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc_default().await;

    // All phase4a RPCs are wired (no remaining stubs). This test proves
    // each returns a non-Unimplemented error or success — NOT full
    // behavior coverage (see dedicated tests below).

    // ListExecutors is implemented — no workers → empty list, not error.
    let lw = svc
        .list_executors(Request::new(ListExecutorsRequest::default()))
        .await?
        .into_inner();
    assert!(
        lw.executors.is_empty(),
        "no workers registered → empty list"
    );

    // ListBuilds is implemented — no builds → empty list, not error.
    let lb = svc
        .list_builds(Request::new(ListBuildsRequest::default()))
        .await?
        .into_inner();
    assert!(lb.builds.is_empty(), "no builds → empty list");
    assert_eq!(lb.total_count, 0);
    // TriggerGC: now implemented (proxy to store). With the test
    // store_addr (127.0.0.1:1), proxy connect fails with Unavailable
    // (not Unimplemented) — proves it's wired. Error arrives IN-STREAM
    // (grpc-web Trailers-Only constraint).
    let mut gc_stream = svc
        .trigger_gc(Request::new(GcRequest::default()))
        .await?
        .into_inner();
    let gc_err = gc_stream.next().await.unwrap().unwrap_err();
    assert_eq!(
        gc_err.code(),
        tonic::Code::Unavailable,
        "TriggerGC implemented but store unreachable in tests"
    );
    // ClearPoison is now implemented. Default request has empty
    // derivation_hash → InvalidArgument. Proves it's wired (no
    // longer Unimplemented).
    assert_eq!(
        svc.clear_poison(Request::new(ClearPoisonRequest::default()))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::InvalidArgument
    );
    // Non-empty but non-poisoned → cleared=false (not an error).
    let resp = svc
        .clear_poison(Request::new(ClearPoisonRequest {
            derivation_hash: "nonexistent-drv-hash".into(),
        }))
        .await?
        .into_inner();
    assert!(!resp.cleared, "non-poisoned drv → cleared=false");
    // ListPoisoned on empty DAG → empty list (not an error).
    let resp = svc.list_poisoned(Request::new(())).await?.into_inner();
    assert!(resp.derivations.is_empty());
    Ok(())
}

// -----------------------------------------------------------------------
// ClusterStatus
// -----------------------------------------------------------------------

/// Tick (publish cached snapshot) then call `cluster_status`. I-163:
/// `cluster_status` reads the watch-cached snapshot, which is Default
/// until the actor's first Tick. Tests drive Tick explicitly.
async fn cluster_status_now(
    svc: &AdminServiceImpl,
    actor: &crate::actor::ActorHandle,
) -> anyhow::Result<ClusterStatusResponse> {
    actor.send_unchecked(ActorCommand::Tick).await?;
    crate::actor::tests::barrier(actor).await;
    Ok(svc.cluster_status(Request::new(())).await?.into_inner())
}

#[tokio::test]
async fn cluster_status_empty() -> anyhow::Result<()> {
    let (svc, actor, _task, _db) = setup_svc_default().await;

    let resp = cluster_status_now(&svc, &actor).await?;

    assert_eq!(resp.total_executors, 0);
    assert_eq!(resp.active_executors, 0);
    assert_eq!(resp.draining_executors, 0);
    assert_eq!(resp.pending_builds, 0);
    assert_eq!(resp.active_builds, 0);
    assert_eq!(resp.queued_derivations, 0);
    assert_eq!(resp.running_derivations, 0);
    assert_eq!(
        resp.store_size_bytes, 0,
        "store_size bg refresh not spawned in tests → stays at initial 0"
    );

    // uptime_since is "now minus elapsed since construction" → within
    // a few hundred ms of now. Wide tolerance (10s) to survive slow CI.
    let uptime = resp.uptime_since.expect("uptime_since always set");
    let now = prost_types::Timestamp::from(SystemTime::now());
    let delta = now.seconds - uptime.seconds;
    assert!(
        (0..10).contains(&delta),
        "uptime_since should be recent (within 10s), got delta={delta}s"
    );
    Ok(())
}

/// The executor counts are the busy view: one open pull-mode attempt
/// per in-flight derivation. (Successor of the stream-era
/// registered/draining census, which retired with the executors map —
/// spawned-but-not-yet-pulled pods are the controller's Job census,
/// not a scheduler count.)
#[tokio::test]
async fn cluster_status_counts_open_attempts() -> anyhow::Result<()> {
    use crate::actor::tests::{merge_single_node, pull_attempt};
    use crate::state::PriorityClass;

    let (svc, actor, _task, _db) = setup_svc_default().await;

    // Two single-node DAGs; only the first is taken by a pull attempt.
    let _ev1 = merge_single_node(
        &actor,
        uuid::Uuid::new_v4(),
        "cs-w1",
        PriorityClass::Scheduled,
    )
    .await?;
    let _ev2 = merge_single_node(
        &actor,
        uuid::Uuid::new_v4(),
        "cs-w2",
        PriorityClass::Scheduled,
    )
    .await?;
    let _assignment = pull_attempt(&actor, "cs-w1").await;

    let resp = cluster_status_now(&svc, &actor).await?;

    assert_eq!(
        resp.total_executors, 1,
        "one open attempt = one busy executor; the un-pulled Ready drv is queue, not fleet"
    );
    assert_eq!(resp.active_executors, 1);
    assert_eq!(
        resp.draining_executors, 0,
        "per-executor drain retired with the stream session; always 0"
    );
    Ok(())
}

#[tokio::test]
async fn cluster_status_counts_queued_and_running() -> anyhow::Result<()> {
    use crate::actor::tests::{merge_single_node, pull_attempt};
    use crate::state::PriorityClass;

    let (svc, actor, _task, _db) = setup_svc_default().await;

    // Two independent single-node DAGs. The first is taken by a
    // pull-mode attempt (Running); the second stays Ready.
    let _ev1 =
        merge_single_node(&actor, uuid::Uuid::new_v4(), "a", PriorityClass::Scheduled).await?;
    let _ev2 =
        merge_single_node(&actor, uuid::Uuid::new_v4(), "b", PriorityClass::Scheduled).await?;
    let _assignment = pull_attempt(&actor, "a").await;

    let resp = cluster_status_now(&svc, &actor).await?;

    // BOTH builds transition to Active on merge (merge.rs sets Active
    // as soon as derivations are tracked, not waiting for delivery).
    assert_eq!(
        resp.active_builds, 2,
        "both builds transitioned to Active on merge"
    );
    assert_eq!(resp.pending_builds, 0);
    // The DAG-status-derived counts are the pull-era truth: "b" is the
    // only Ready derivation, "a" is Running under its open attempt.
    // The scalar is counted from the same DAG pass as the per-system
    // split (the recorded ready-queue over-count is fixed), so the two
    // must agree exactly.
    assert_eq!(
        resp.queued_derivations, 1,
        "one Ready derivation waiting for a worker (DAG-status based; a pulled drv is \
         no longer queued)"
    );
    assert_eq!(
        resp.queued_by_system.values().sum::<u32>(),
        resp.queued_derivations,
        "per-system split sums to the scalar by construction"
    );
    assert_eq!(
        resp.running_derivations, 1,
        "first drv runs under its open pull attempt"
    );
    Ok(())
}

#[tokio::test]
async fn cluster_status_actor_dead_returns_unavailable() -> anyhow::Result<()> {
    // Set up, then drop the handle + abort the task → actor channel closes.
    // check_actor_alive() catches this before the oneshot would hang.
    let (svc, actor, task, _db) = setup_svc_default().await;
    drop(actor);
    task.abort();
    // Give tokio a tick to process the abort.
    tokio::task::yield_now().await;

    // The svc still holds an ActorHandle clone. is_alive() checks
    // tx.is_closed() which becomes true when the receiver drops
    // (actor task gone). Abort + drop(actor) both contribute — abort
    // kills the task (receiver drops), drop(actor) removes one of
    // the two senders. The svc's clone is the last sender.
    //
    // Poll is_closed via the svc's handle until it flips. Bounded
    // loop (if it never flips in 100 yields, something's wrong).
    for _ in 0..100 {
        if !svc.actor.is_alive() {
            break;
        }
        tokio::task::yield_now().await;
    }

    let result = svc.cluster_status(Request::new(())).await;
    let status = result.expect_err("should be Unavailable");
    assert_eq!(status.code(), tonic::Code::Unavailable);
    assert!(status.message().contains("actor"));
    Ok(())
}

// -----------------------------------------------------------------------
// DrainExecutor
// -----------------------------------------------------------------------

/// DrainExecutor is a retired no-op until the 1d proto sweep: it must
/// perform no action and return a clear error that names the successor
/// procedures (cordon + AD2 exclusion / cancel + Job deletion /
/// pool-level pause), never a silent success. The service-token gate
/// stays in front of it (covered by
/// [`mutating_rpcs_require_service_token`]).
#[tokio::test]
async fn drain_worker_retired_returns_unimplemented() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc_default().await;

    let status = svc
        .drain_executor(Request::new(DrainExecutorRequest {
            executor_id: "any-worker".into(),
            force: true,
        }))
        .await
        .expect_err("retired surface must not report success");
    assert_eq!(status.code(), tonic::Code::Unimplemented);
    assert!(
        status.message().contains("retired"),
        "the error explains the retirement: {}",
        status.message()
    );
    assert!(
        status.message().contains("cordon") && status.message().contains("CancelBuild"),
        "the error names the successor procedures: {}",
        status.message()
    );
    Ok(())
}

/// DebugListExecutors is likewise a retired no-op: there is no
/// in-memory executor map left to snapshot; the error points at
/// ListOpenAttempts and the Job/pod census.
#[tokio::test]
async fn debug_list_executors_retired_returns_unimplemented() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc_default().await;

    let status = svc
        .debug_list_executors(Request::new(()))
        .await
        .expect_err("retired surface must not report success");
    assert_eq!(status.code(), tonic::Code::Unimplemented);
    assert!(
        status.message().contains("ListOpenAttempts"),
        "the error names the successor view: {}",
        status.message()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// ClearPoison happy path
// ---------------------------------------------------------------------------

// r[verify sched.admin.clear-poison]
/// Poison a derivation via PermanentFailure, then ClearPoison it.
/// Verifies cleared=true, in-mem status reset, PG poisoned_at cleared.
#[tokio::test]
async fn test_clear_poison_happy_path() -> anyhow::Result<()> {
    use crate::actor::tests::{merge_single_node, pull_complete_failure, test_drv_path};
    use crate::state::PriorityClass;

    let (svc, actor, _task, db) = setup_svc_default().await;

    let _ev = merge_single_node(
        &actor,
        uuid::Uuid::new_v4(),
        "poison-me",
        PriorityClass::Scheduled,
    )
    .await?;

    // PermanentFailure → poisoned (both in-mem and PG). Delivered
    // through the pull report intake.
    pull_complete_failure(
        &actor,
        "poison-me",
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "test permanent failure",
    )
    .await?;

    // Verify poisoned (barrier via debug_query).
    let pre = actor
        .debug_query_derivation("poison-me")
        .await?
        .expect("exists");
    assert_eq!(pre.status, crate::state::DerivationStatus::Poisoned);
    // PG should have poisoned_at set (the as_bytes() bug would break this).
    let pg_poisoned: Option<f64> = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM poisoned_at)::float8 FROM derivations WHERE drv_hash=$1",
    )
    .bind("poison-me")
    .fetch_one(&db.pool)
    .await?;
    assert!(
        pg_poisoned.is_some(),
        "poisoned_at should be persisted to PG"
    );

    // r[verify sched.admin.list-poisoned]
    // ListPoisoned should now return it with the full .drv path
    // (what ClearPoison takes as input).
    let listed = svc.list_poisoned(Request::new(())).await?.into_inner();
    assert_eq!(listed.derivations.len(), 1);
    assert_eq!(listed.derivations[0].drv_path, test_drv_path("poison-me"));
    // PermanentFailure poisons immediately without appending to
    // failed_builders (that's the transient-retry path), so this can
    // be empty. Just check the field is wired.
    let _ = &listed.derivations[0].failed_executors;

    // ClearPoison via RPC.
    let resp = svc
        .clear_poison(Request::new(ClearPoisonRequest {
            derivation_hash: "poison-me".into(),
        }))
        .await?
        .into_inner();
    assert!(resp.cleared, "happy path → cleared=true");

    // In-mem: node removed from DAG (next submit re-inserts it fresh
    // with full proto fields — see Dag::remove_node rationale).
    let post = actor.debug_query_derivation("poison-me").await?;
    assert!(post.is_none(), "node removed from DAG on ClearPoison");

    // PG: poisoned_at cleared.
    let pg_poisoned: Option<f64> = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM poisoned_at)::float8 FROM derivations WHERE drv_hash=$1",
    )
    .bind("poison-me")
    .fetch_one(&db.pool)
    .await?;
    assert!(
        pg_poisoned.is_none(),
        "clear_poison should NULL poisoned_at"
    );

    Ok(())
}

/// ClearPoison ordering regression: PG clear must precede in-mem
/// reset so that a PG failure leaves in-mem Poisoned for retry.
/// The old order (in-mem first) left status=Created on PG blip →
/// retry hit the not-poisoned guard → permanent no-op.
#[tokio::test]
async fn test_clear_poison_pg_failure_leaves_inmem_poisoned_for_retry() -> anyhow::Result<()> {
    use crate::actor::tests::{merge_single_node, pull_complete_failure};
    use crate::state::PriorityClass;

    let (svc, actor, _task, db) = setup_svc_default().await;

    let _ev = merge_single_node(
        &actor,
        uuid::Uuid::new_v4(),
        "pg-blip",
        PriorityClass::Scheduled,
    )
    .await?;
    pull_complete_failure(
        &actor,
        "pg-blip",
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "test",
    )
    .await?;

    // Confirm poisoned.
    let pre = actor
        .debug_query_derivation("pg-blip")
        .await?
        .expect("exists");
    assert_eq!(pre.status, crate::state::DerivationStatus::Poisoned);

    // Simulate PG blip: close the pool. All subsequent queries fail.
    db.pool.close().await;

    // First attempt: PG fails → cleared=false, in-mem STILL Poisoned.
    let resp1 = svc
        .clear_poison(Request::new(ClearPoisonRequest {
            derivation_hash: "pg-blip".into(),
        }))
        .await?
        .into_inner();
    assert!(!resp1.cleared, "PG failure → cleared=false");

    let post1 = actor
        .debug_query_derivation("pg-blip")
        .await?
        .expect("exists");
    assert_eq!(
        post1.status,
        crate::state::DerivationStatus::Poisoned,
        "PG failure must leave in-mem Poisoned so retry can proceed"
    );

    // Second attempt with PG still down: same result (proves retry
    // is still hitting the PG path, not the not-poisoned early-return).
    let resp2 = svc
        .clear_poison(Request::new(ClearPoisonRequest {
            derivation_hash: "pg-blip".into(),
        }))
        .await?
        .into_inner();
    assert!(!resp2.cleared);
    let post2 = actor
        .debug_query_derivation("pg-blip")
        .await?
        .expect("exists");
    assert_eq!(post2.status, crate::state::DerivationStatus::Poisoned);

    Ok(())
}
