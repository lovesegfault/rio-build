//! Shared test helpers: actor setup, fixture builders, synchronization barrier.
//!
//! Work delivery in tests goes through the production pull surface
//! (`pull_attempt` / `merge_*` + the report intake); the stream-era
//! connect/heartbeat helpers retired with the session machinery and
//! the operator surfaces that were their last drivers.

use super::*;
use tokio::sync::mpsc;

// Re-exports: fixtures imported once here, used by sibling test modules
// via `use super::*` and by grpc/tests.rs via `crate::actor::tests::*`.
// `pub(crate)` (not `pub(super)`) so grpc/tests.rs can reach them through
// the tests/mod.rs `pub(crate) use helpers::*;` re-export.
pub(crate) use rio_test_support::fixtures::{
    make_derivation_node as make_test_node, make_edge as make_test_edge, test_drv_path,
    test_store_path,
};

/// [`make_test_node`] with `system = "x86_64-linux"` (the default for
/// the overwhelming majority of scheduler tests). Use the two-arg form
/// directly only when the test exercises arch-aware routing.
pub(crate) fn make_node(tag: &str) -> rio_proto::types::DerivationNode {
    make_test_node(tag, "x86_64-linux")
}
pub(super) use rio_test_support::{TestDb, TestResult};
pub(super) use std::time::Duration;

pub(super) use crate::MIGRATOR;

/// Set up an actor with the given PgPool and return (handle, task).
/// The caller should drop the handle to shut down the actor.
pub(crate) fn setup_actor(pool: sqlx::PgPool) -> (ActorHandle, tokio::task::JoinHandle<()>) {
    setup_actor_with_store(pool, None)
}

/// Set up an actor with an optional store client for cache-check tests.
pub(crate) fn setup_actor_with_store(
    pool: sqlx::PgPool,
    store_client: Option<StoreServiceClient<Channel>>,
) -> (ActorHandle, tokio::task::JoinHandle<()>) {
    setup_actor_configured(pool, store_client, |_, _| {})
}

/// Bundle of handles for CA-compare test scenarios. See [`setup_ca_fixture`].
pub(crate) struct CaFixture {
    /// MockStore handle — arm fault flags or seed paths for
    /// FindMissingPaths BEFORE driving the actor to the CA-compare
    /// callsite.
    pub store: rio_test_support::grpc::MockStore,
    /// Actor handle — send commands, await replies.
    pub actor: ActorHandle,
    /// The single CA derivation's path (`test_drv_path(key)`).
    pub drv_path: String,
    /// Synthetic executor id (`"w-{key}"`). Pass to [`complete_ca`].
    pub executor_id: String,
    /// Build id for the merged single-node DAG.
    pub build_id: Uuid,
    /// The CA node's modular hash (set on the proto node so the
    /// compare gate `state.ca.modular_hash.is_some()` passes).
    /// Deterministic per-key: `Sha256("ca-fixture:" + key)`.
    pub modular_hash: [u8; 32],
    /// PG pool — seed realisations directly (the compare hits PG,
    /// not the store gRPC).
    pub pool: sqlx::PgPool,
    /// PG test database — keep alive for the actor's pool.
    pub _db: TestDb,
    /// MockStore tokio task guard — keep alive for the gRPC server.
    pub _store_task: tokio::task::JoinHandle<()>,
    /// Actor tokio task guard — keep alive for the actor loop.
    pub _actor_task: tokio::task::JoinHandle<()>,
}

/// Seed a realisation row in PG. The CA-compare's
/// `query_prior_realisation` reads this table (NOT content_index —
/// the compare moved from gRPC to PG when the self-exclusion
/// mechanism was fixed for CA).
pub(crate) async fn seed_realisation(
    pool: &sqlx::PgPool,
    modular_hash: &[u8; 32],
    output_name: &str,
    output_path: &str,
    output_hash: &[u8; 32],
) -> anyhow::Result<()> {
    crate::ca::insert_realisation(pool, modular_hash, output_name, output_path, output_hash)
        .await?;
    Ok(())
}

/// Standard CA-compare test setup: spawn MockStore, actor with store
/// client, merge a single `is_content_addressed=true` node, return all
/// handles bundled as [`CaFixture`].
///
/// Absorbs the 8-copy boilerplate that had accrued across `completion.rs`
/// (5 added by P0311, 3 pre-existing): `TestDb::new` +
/// `spawn_mock_store_with_client` + `setup_actor_with_store` +
/// `make_test_node(ca=true)` + `merge_dag`. Each test drops from ~15L
/// setup to 1-2L.
///
/// The fixture returns with the actor holding the node in Ready/Assigned
/// — the CA-compare callsite at `completion.rs` only fires on
/// `ProcessCompletion`, so tests can arm fault flags or seed the store
/// AFTER this returns and BEFORE calling [`complete_ca`]. Verified by
/// `setup_ca_fixture_does_not_race_past_ca_compare`.
///
/// Tests that need a configured actor (e.g. `grpc_timeout`) should use
/// [`setup_ca_fixture_configured`].
pub(crate) async fn setup_ca_fixture(key: &str) -> anyhow::Result<CaFixture> {
    setup_ca_fixture_configured(key, |_, _| {}).await
}

/// Like [`setup_ca_fixture`] but lets the caller mutate
/// `DagActorConfig`/`DagActorPlumbing` before spawn.
pub(crate) async fn setup_ca_fixture_configured(
    key: &str,
    configure: impl FnOnce(&mut DagActorConfig, &mut DagActorPlumbing),
) -> anyhow::Result<CaFixture> {
    let db = TestDb::new(&MIGRATOR).await;
    seed_default_tenant(&db.pool).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (actor, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), configure);

    let executor_id = format!("w-{key}");

    // Deterministic modular_hash per key — the CA-compare gate
    // requires `state.ca.modular_hash.is_some()`. Real flow: the
    // gateway's populate_ca_modular_hashes fills this from
    // hash_derivation_modulo; test fixture fakes it with a
    // key-derived hash so tests can seed matching PG rows.
    let modular_hash: [u8; 32] = {
        use sha2::{Digest, Sha256};
        Sha256::digest(format!("ca-fixture:{key}").as_bytes()).into()
    };
    let mut node = make_node(key);
    node.is_content_addressed = true;
    node.ca_modular_hash = modular_hash.to_vec();
    let drv_path = node.drv_path.clone();
    let build_id = Uuid::new_v4();
    let _ev = merge_dag(&actor, build_id, vec![node], vec![], false).await?;

    Ok(CaFixture {
        store,
        actor,
        drv_path,
        executor_id,
        build_id,
        modular_hash,
        pool: db.pool.clone(),
        _db: db,
        _store_task: store_task,
        _actor_task: actor_task,
    })
}

/// Set up an actor with a configurator closure that mutates
/// `DagActorConfig`/`DagActorPlumbing` before spawn. For tests that
/// need custom `retry_policy`, `leader`, etc.
pub(crate) fn setup_actor_configured(
    pool: sqlx::PgPool,
    store_client: Option<StoreServiceClient<Channel>>,
    configure: impl FnOnce(&mut DagActorConfig, &mut DagActorPlumbing),
) -> (ActorHandle, tokio::task::JoinHandle<()>) {
    let db = SchedulerDb::new(pool);
    let (tx, rx) = mpsc::channel(ACTOR_CHANNEL_CAPACITY);
    let mut cfg = DagActorConfig::default();
    // Test default: ZERO transient-retry backoff. The bug_282
    // pull-admission gate holds fresh mints for the real window;
    // dozens of tests drive back-to-back failure→re-pull cycles whose
    // subject is row accounting / poison thresholds, not timing.
    // Tests exercising the window itself (the 282 battery) configure a
    // real backoff explicitly in their `configure` closure (which runs
    // AFTER this default and overrides it).
    cfg.retry_policy.backoff_base_secs = 0.0;
    cfg.retry_policy.jitter_fraction = 0.0;
    let mut plumbing = DagActorPlumbing {
        store_client,
        ..Default::default()
    };
    configure(&mut cfg, &mut plumbing);
    let actor = DagActor::new(db, cfg, plumbing);
    let backpressure = actor.backpressure_flag();
    let generation = actor.generation_reader();
    let snapshot_rx = actor.snapshot_receiver();
    let admin_fast_tx = actor.admin_fast_sender();
    let self_tx = tx.downgrade();
    let task = tokio::spawn(actor.run_with_self_tx(rx, self_tx));
    (
        ActorHandle {
            tx,
            admin_fast_tx,
            backpressure,
            generation,
            snapshot_rx,
        },
        task,
    )
}

/// Construct a bare (unspawned) actor for tests that exercise `&self`
/// snapshot methods directly.
pub(crate) fn bare_actor(pool: sqlx::PgPool) -> DagActor {
    bare_actor_cfg(pool, DagActorConfig::default())
}

pub(crate) fn bare_actor_cfg(pool: sqlx::PgPool, cfg: DagActorConfig) -> DagActor {
    DagActor::new(SchedulerDb::new(pool), cfg, DagActorPlumbing::default())
}

/// Bootstrap an ephemeral PG + actor. The returned `TestDb` MUST be held
/// for the test duration — `TestDb::Drop` tears down the database.
/// Most callers: `let (_db, handle, _task) = setup().await;`
pub(crate) async fn setup() -> (TestDb, ActorHandle, tokio::task::JoinHandle<()>) {
    let db = TestDb::new(&MIGRATOR).await;
    seed_default_tenant(&db.pool).await;
    let (handle, task) = setup_actor(db.pool.clone());
    (db, handle, task)
}

/// Bootstrap PG + [`MockStore`](rio_test_support::grpc::MockStore) +
/// actor wired with the store client. Absorbs the `TestDb::new` →
/// `spawn_mock_store_with_client` → `setup_actor_with_store` preamble
/// repeated across cache-check / CA / FOD-substitution tests.
///
/// The actor is spawned BEFORE the caller can arm fault flags or seed
/// store paths — that's fine: the actor only talks to the store when a
/// command drives it (`MergeDag` / `Tick`), so callers arm
/// `store.faults.*` or seed `store.paths` after this returns.
///
/// The two background task guards (store server + actor loop) are
/// returned bundled — bind as `_tasks` to keep both alive.
pub(crate) async fn setup_with_mock_store() -> anyhow::Result<(
    TestDb,
    rio_test_support::grpc::MockStore,
    ActorHandle,
    (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>),
)> {
    let db = TestDb::new(&MIGRATOR).await;
    seed_default_tenant(&db.pool).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    // merged_bug_003 (Q3): tenant-scoped probing is the ONLY probing
    // that reports substitutable paths — the store (real and mock
    // alike) answers an anonymous probe with empty substitutable and
    // echoes probe_ran_tenant_scoped=false, and the scheduler only
    // attaches the probe header under service auth. The default
    // harness therefore signs like a configured deployment; the
    // pre-fix mock reported substitutable paths to anonymous probes,
    // a wire state the real store never produces.
    let (handle, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |_cfg, p| {
            p.service_signer = Some(std::sync::Arc::new(rio_auth::hmac::HmacSigner::from_key(
                b"mock-store-harness-service-key32".to_vec(),
            )));
        });
    Ok((db, store, handle, (store_task, actor_task)))
}

/// Send `ActorCommand::Tick` and barrier on it. For tests driving the
/// `dispatch_dirty` → dispatch path or refreshing the cached
/// `ClusterSnapshot` without faking a heartbeat.
pub(crate) async fn tick(handle: &ActorHandle) -> anyhow::Result<()> {
    handle.send_unchecked(ActorCommand::Tick).await?;
    barrier(handle).await;
    Ok(())
}

/// Merge a DAG with a caller-supplied [`MergeDagRequest`]. Thin wrapper
/// over the `ActorCommand::MergeDag` + oneshot-reply boilerplate for
/// tests that need custom `options`/`priority_class`/`traceparent` but
/// don't need to inspect the reply channel directly.
pub(crate) async fn merge_dag_req(
    handle: &ActorHandle,
    req: MergeDagRequest,
) -> anyhow::Result<broadcast::Receiver<rio_proto::types::BuildEvent>> {
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            req,
            reply: reply_tx,
        })
        .await?;
    // Tests assert on state events; the log channel is dropped here.
    Ok(reply_rx.await??.state)
}

/// The deterministic tenant every default-merge helper stamps on its
/// builds (merged_bug_003 / Q3): tenanted builds are the ONLY builds
/// the substitution lane serves — `tenant_upstreams` is per-tenant,
/// the dispatch probe is scoped under service auth, and the store
/// reports substitutable paths only to a verified scope. The pre-Q3
/// harness merged tenant-less builds against a mock that answered
/// anonymous probes with substitutable paths — a wire state the real
/// store never produces. Seeded idempotently by the async setup
/// helpers ([`seed_default_tenant`]); tests that need DISTINCT
/// tenants keep seeding their own.
pub(crate) const DEFAULT_TEST_TENANT: Uuid =
    Uuid::from_u128(0xD0FA_17DE_0000_4000_8000_0000_0000_D0FA);

/// Idempotently seed [`DEFAULT_TEST_TENANT`]. Called by every async
/// setup helper that owns a pool; safe to call repeatedly.
pub(crate) async fn seed_default_tenant(pool: &sqlx::PgPool) {
    sqlx::query(
        "INSERT INTO tenants (tenant_id, tenant_name) VALUES ($1, 'harness-default-tenant') \
         ON CONFLICT (tenant_id) DO NOTHING",
    )
    .bind(DEFAULT_TEST_TENANT)
    .execute(pool)
    .await
    .expect("seed_default_tenant INSERT failed");
}

/// Merge a single-node DAG and return the event receiver.
///
/// `drv_path` is auto-generated from `tag` via [`test_drv_path`].
pub(crate) async fn merge_single_node(
    handle: &ActorHandle,
    build_id: Uuid,
    tag: &str,
    priority_class: PriorityClass,
) -> anyhow::Result<broadcast::Receiver<rio_proto::types::BuildEvent>> {
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            req: MergeDagRequest {
                build_id,
                tenant_id: Some(DEFAULT_TEST_TENANT),
                priority_class,
                nodes: vec![make_node(tag)],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: false,
                traceparent: String::new(),
                jti: None,
                jwt_token: Some("harness-tenant-jwt".into()),
            },
            reply: reply_tx,
        })
        .await?;
    Ok(reply_rx.await??.state)
}

/// Merge a multi-node DAG with default options
/// (tenant=[`DEFAULT_TEST_TENANT`], priority=Scheduled,
/// options=default). Generalization of [`merge_single_node`].
/// Returns the broadcast receiver for build events.
pub(crate) async fn merge_dag(
    handle: &ActorHandle,
    build_id: Uuid,
    nodes: Vec<rio_proto::types::DerivationNode>,
    edges: Vec<rio_proto::types::DerivationEdge>,
    keep_going: bool,
) -> anyhow::Result<broadcast::Receiver<rio_proto::types::BuildEvent>> {
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            req: MergeDagRequest {
                build_id,
                tenant_id: Some(DEFAULT_TEST_TENANT),
                priority_class: PriorityClass::Scheduled,
                nodes,
                edges,
                options: BuildOptions::default(),
                keep_going,
                traceparent: String::new(),
                jti: None,
                jwt_token: Some("harness-tenant-jwt".into()),
            },
            reply: reply_tx,
        })
        .await?;
    Ok(reply_rx.await??.state)
}

/// Subscribe to a build's LOG broadcast ring (display-only events:
/// `Event::Log`, `Event::SubstituteProgress`). Tests asserting on
/// state-transition events use [`merge_dag`]'s return (state ring).
pub(crate) async fn subscribe_log(
    handle: &ActorHandle,
    build_id: Uuid,
) -> anyhow::Result<broadcast::Receiver<rio_proto::types::BuildEvent>> {
    let (tx, rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::WatchBuild {
            build_id,
            caller_tenant: None,
            reply: tx,
        })
        .await?;
    Ok(rx.await??.0.log)
}

/// Query build status. Propagates BuildNotFound as an error.
pub(crate) async fn query_status(
    handle: &ActorHandle,
    build_id: Uuid,
) -> anyhow::Result<rio_proto::types::BuildStatus> {
    let (tx, rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            caller_tenant: None,
            reply: tx,
        })
        .await?;
    Ok(rx.await??)
}

/// Query build status, returning the inner Result (for tests that expect
/// BuildNotFound). Propagates send/recv failures; caller inspects ActorError.
pub(crate) async fn try_query_status(
    handle: &ActorHandle,
    build_id: Uuid,
) -> anyhow::Result<Result<rio_proto::types::BuildStatus, ActorError>> {
    let (tx, rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            caller_tenant: None,
            reply: tx,
        })
        .await?;
    Ok(rx.await?)
}

/// Cancel a build through the production `CancelBuild` command and
/// await acceptance. Panics if the cancel is rejected (terminal build /
/// not found) — that is a test-sequencing bug.
pub(crate) async fn cancel_build(handle: &ActorHandle, build_id: Uuid) -> TestResult {
    let (tx, rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id,
            caller_tenant: None,
            reason: "test cancel".into(),
            reply: tx,
        })
        .await?;
    assert!(rx.await??, "CancelBuild must be accepted");
    Ok(())
}

/// Send a successful completion (Built) with a single `out` output.
/// Uses a placeholder output_hash; override via inline construction if
/// the test asserts on hash contents.
pub(crate) async fn complete_success(
    handle: &ActorHandle,
    executor_id: &str,
    drv_key: &str,
    output_path: &str,
) -> anyhow::Result<()> {
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            executor_id: executor_id.into(),
            drv_key: drv_key.into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                built_outputs: vec![rio_proto::types::BuiltOutput {
                    output_name: "out".into(),
                    output_path: output_path.into(),
                    output_hash: vec![0u8; 32],
                }],
                ..Default::default()
            },
            peak_memory_bytes: 0,
            peak_cpu_cores: 0.0,
            node_name: None,
            hw_class: None,
            final_line_count: 0,
            final_resources: None,
        })
        .await?;
    Ok(())
}

/// Send a successful completion (Built) with caller-controlled per-output
/// hash bytes.
///
/// CA-compare tests need specific hash values: `[0xAB; 32]` for a valid
/// hash the MockStore can be seeded to match, `[0xCD; 16]` for a
/// malformed length that triggers the 32-byte guard, or a store-seeded
/// real hash for compare-match scenarios.
///
/// [`complete_success`] hardcodes `vec![0u8; 32]` — fine for IA tests
/// where the hash is opaque, wrong for CA tests where the hash IS the
/// test subject. Each `outputs` entry is `(output_name, output_path,
/// output_hash)`; hash can be any length (the malformed-hash test sends
/// 16 bytes to exercise the len-guard at the CA-compare callsite).
///
/// Passing `&[]` is valid — it mirrors [`complete_success_empty`] but
/// for a CA context where the zero-outputs edge is explicitly under test.
pub(crate) async fn complete_ca(
    handle: &ActorHandle,
    executor_id: &str,
    drv_key: &str,
    outputs: &[(&str, &str, Vec<u8>)],
) -> anyhow::Result<()> {
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            executor_id: executor_id.into(),
            drv_key: drv_key.into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                built_outputs: outputs
                    .iter()
                    .map(|(name, path, hash)| rio_proto::types::BuiltOutput {
                        output_name: (*name).into(),
                        output_path: (*path).into(),
                        output_hash: hash.clone(),
                    })
                    .collect(),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            peak_cpu_cores: 0.0,
            node_name: None,
            hw_class: None,
            final_line_count: 0,
            final_resources: None,
        })
        .await?;
    Ok(())
}

/// Send a successful completion (Built) with NO built_outputs.
/// Many tests don't care about output paths and just need the state transition.
pub(crate) async fn complete_success_empty(
    handle: &ActorHandle,
    executor_id: &str,
    drv_key: &str,
) -> anyhow::Result<()> {
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            executor_id: executor_id.into(),
            drv_key: drv_key.into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            peak_cpu_cores: 0.0,
            node_name: None,
            hw_class: None,
            final_line_count: 0,
            final_resources: None,
        })
        .await?;
    Ok(())
}

/// Send a failed completion with the given status and error message.
pub(crate) async fn complete_failure(
    handle: &ActorHandle,
    executor_id: &str,
    drv_key: &str,
    status: rio_proto::types::BuildResultStatus,
    error_msg: &str,
) -> anyhow::Result<()> {
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            executor_id: executor_id.into(),
            drv_key: drv_key.into(),
            result: rio_proto::types::BuildResult {
                status: status.into(),
                error_msg: error_msg.into(),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            peak_cpu_cores: 0.0,
            node_name: None,
            hw_class: None,
            final_line_count: 0,
            final_resources: None,
        })
        .await?;
    Ok(())
}

/// Query a derivation by hash and unwrap the `Some`. Replaces the 136×
/// open-coded `expect_drv(&handle, k).await`.
/// Panics with the hash on missing — better than a bare "exists".
pub(crate) async fn expect_drv(handle: &ActorHandle, hash: &str) -> DebugDerivationInfo {
    handle
        .debug_query_derivation(hash)
        .await
        .expect("actor alive")
        .unwrap_or_else(|| panic!("derivation {hash:?} should exist in DAG"))
}

/// `[sla]` config for tests that exercise the solve/explore branch
/// with realistic ceilings (the `SlaConfig::test_default()` ceilings
/// are tiny — sized for VM-test pools). One tier, probe = 4c, 64-core
/// / 256 GiB / 200 GiB ceilings.
pub(crate) fn test_sla_config() -> crate::sla::config::SlaConfig {
    use crate::sla::{config, solve};
    config::SlaConfig {
        tiers: vec![solve::Tier {
            name: "normal".into(),
            p50: None,
            p90: Some(1200.0),
            p99: None,
        }],
        probe: config::ProbeShape {
            cpu: 4.0,
            mem_per_core: 1 << 30,
            mem_base: 4 << 30,
            deadline_secs: 3600,
        },
        max_cores: Some(64.0),
        max_mem: Some(256 << 30),
        max_disk: 200 << 30,
        default_disk: 20 << 30,
        ..config::SlaConfig::test_default()
    }
}

/// Bare (unspawned) actor with the realistic-ceiling `[sla]` config.
/// For `compute_spawn_intents` / solve-branch tests.
pub(crate) fn bare_actor_sla(pool: sqlx::PgPool) -> DagActor {
    bare_actor_cfg(
        pool,
        DagActorConfig {
            sla: test_sla_config(),
            ..Default::default()
        },
    )
}

/// 3-class builders-only `[sla]` config: `intel-6/7/8`, no
/// `fetcher-*`. ε_h=0 so per-dispatch results are deterministic
/// (set explicitly in ε_h tests).
///
/// **Why a separate fixture exists (§13e cleanup):** several tests
/// assert on `cfg.hw_classes` cardinality or pinned-explore pool
/// counts (e.g. `assert_eq!(h_all.len(), 3)`). Featureless drvs never
/// route to `fetcher-*` cells (∅-guard), so any non-builder class in
/// the fixture inflates `h_all` without ever appearing in a solve —
/// the assertion goes red without a routing change. Building this
/// fixture SMALL — instead of filtering [`test_hw_sla_config`] down by
/// prefix — means the next class added there (e.g. a `metal-*` band
/// from the helm chart) stays out of these tests automatically.
pub(crate) fn test_hw_sla_config_builders_only() -> crate::sla::config::SlaConfig {
    use crate::sla::config::{HwClassDef, NodeLabelMatch};
    let mut cfg = test_sla_config();
    cfg.hw_explore_epsilon = 0.0;
    cfg.hw_classes.clear();
    for h in ["intel-6", "intel-7", "intel-8"] {
        cfg.hw_classes.insert(
            h.into(),
            HwClassDef {
                labels: vec![NodeLabelMatch {
                    key: "rio.build/hw-class".into(),
                    value: h.into(),
                }],
                max_cores: Some(cfg.max_cores.unwrap() as u32),
                max_mem: Some(cfg.max_mem.unwrap()),
                ..Default::default()
            },
        );
    }
    cfg
}

/// `[sla]` config with 3 builder + 2 fetcher hw_classes +
/// `hw_cost_source=Static` so the admissible-set solve_full path is
/// reachable. Builds on [`test_hw_sla_config_builders_only`].
pub(crate) fn test_hw_sla_config() -> crate::sla::config::SlaConfig {
    use crate::sla::config::{HwClassDef, NodeLabelMatch, NodeTaint};
    let mut cfg = test_hw_sla_config_builders_only();
    // §13e: fetcher hwClasses. FODs route here via
    // `effective_features(state) = [fetcher]` — the bidirectional
    // ∅-guard means featureless builders never see these (and FODs
    // never see `intel-*`). Mirrors the helm `values.yaml` shape so
    // the unit fixture and the deployed chart route identically.
    for (h, arch) in [("fetcher-x86", "amd64"), ("fetcher-arm", "arm64")] {
        cfg.hw_classes.insert(
            h.into(),
            HwClassDef {
                labels: vec![
                    NodeLabelMatch {
                        key: rio_common::k8s::FETCHER_TAINT_KEY.into(),
                        value: "true".into(),
                    },
                    NodeLabelMatch {
                        key: crate::sla::config::ARCH_LABEL.into(),
                        value: arch.into(),
                    },
                ],
                node_class: "rio-default".into(),
                max_cores: Some(cfg.max_cores.unwrap() as u32),
                max_mem: Some(cfg.max_mem.unwrap()),
                taints: vec![NodeTaint {
                    key: rio_common::k8s::FETCHER_TAINT_KEY.into(),
                    value: "true".into(),
                    effect: "NoSchedule".into(),
                }],
                provides_features: vec![rio_common::k8s::FETCHER_FEATURE.into()],
                ..Default::default()
            },
        );
    }
    cfg
}

/// One fitted Amdahl key (`S=30, P=2000`) for `pname`. Pair with
/// [`seed_fit`] or `sla_estimator.insert(..)` directly.
pub(crate) fn make_fit(pname: &str) -> crate::sla::types::FittedParams {
    use crate::sla::types::*;
    FittedParams {
        key: ModelKey {
            pname: pname.into(),
            system: "x86_64-linux".into(),
            tenant: String::new(),
        },
        fit: DurationFit::Amdahl {
            s: RefSeconds(30.0),
            p: RefSeconds(2000.0),
        },
        mem: MemFit::Independent {
            p90: MemBytes(6 << 30),
        },
        disk_p90: Some(DiskBytes(10 << 30)),
        sigma_resid: 0.1,
        log_residuals: Vec::new(),
        n_eff_ring: RingNEff(10.0),
        fit_df: FitDf(10.0),
        n_distinct_c: 5,
        sum_w: 10.0,
        span: 8.0,
        explore: ExploreState {
            distinct_c: 3,
            min_c: RawCores(1.0),
            max_c: RawCores(32.0),
            saturated: false,
            last_wall: WallSeconds(0.0),
        },
        t_min_ci: None,
        ci_computed_at: None,
        tier: None,
        hw_bias: Default::default(),
        alpha: crate::sla::alpha::UNIFORM,
        prior_source: None,
        is_fod: false,
    }
}

/// Seed one fitted Amdahl key (`S=30, P=2000`) on `actor`.
pub(crate) fn seed_fit(actor: &DagActor, pname: &str) {
    actor.sla_estimator.seed(make_fit(pname));
}

/// Snapshot `(hw, cost, inputs_gen)` and solve. Convenience for tests
/// that don't care about the snapshot threading; tests asserting
/// determinism / `inputs_gen` call `solve_inputs` + `solve_intent_for`
/// directly so they can pin or vary `inputs_gen`.
pub(crate) fn solve_intent(
    actor: &DagActor,
    state: &crate::state::DerivationState,
) -> crate::state::SolvedIntent {
    let (hw, cost, ig) = actor.solve_inputs();
    actor.solve_intent_for(state, &hw, &cost, ig)
}

/// Shared post-config actor seeding for [`bare_actor_hw`] /
/// [`bare_actor_hw_builders_only`]: Spot-sourced cost table, resolved
/// ceilings, builder hw factors (`intel-6/7/8`), one fitted key
/// `"test-pkg"`. The hw-factor map covers builders only — fetcher
/// classes (when present in `sla_config`) have no fit key and are
/// excluded by the ∅-guard regardless.
fn seed_hw_actor(mut actor: DagActor) -> DagActor {
    // `set_price` no longer upgrades source; tests that probe per-h
    // price discrimination need a Spot-sourced table.
    *actor.cost_table.write() =
        crate::sla::cost::CostTable::seeded("", crate::sla::cost::HwCostSource::Spot);
    actor.sla_tiers = actor.sla_config.solve_tiers();
    actor.cost_table.write().set_resolved_global((
        actor.sla_config.max_cores.unwrap() as u32,
        actor.sla_config.max_mem.unwrap(),
    ));
    actor.sla_ceilings = crate::sla::solve::Ceilings::from_resolved(
        &actor.sla_config,
        actor.cost_table.read().resolved_global(),
    );
    let mut m = std::collections::HashMap::new();
    m.insert("intel-6".into(), 1.0);
    m.insert("intel-7".into(), 1.4);
    m.insert("intel-8".into(), 2.0);
    actor
        .sla_estimator
        .seed_hw(crate::sla::hw::HwTable::from_map(m));
    seed_fit(&actor, "test-pkg");
    actor
}

/// Bare (unspawned) actor with [`test_hw_sla_config`] (3 builder + 2
/// fetcher classes) + populated builder hw table + one fitted key
/// `"test-pkg"`. For admissible-set / ε_h / ICE-mask tests.
pub(crate) fn bare_actor_hw(pool: sqlx::PgPool) -> DagActor {
    seed_hw_actor(bare_actor_cfg(
        pool,
        DagActorConfig {
            sla: test_hw_sla_config(),
            ..Default::default()
        },
    ))
}

/// [`bare_actor_hw`] with [`test_hw_sla_config_builders_only`]'s
/// 3-class fixture — no `fetcher-*`. For tests that assert on
/// `cfg.hw_classes` cardinality or pinned-explore pool counts; see the
/// fixture's doc for why these need a builders-only set.
pub(crate) fn bare_actor_hw_builders_only(pool: sqlx::PgPool) -> DagActor {
    seed_hw_actor(bare_actor_cfg(
        pool,
        DagActorConfig {
            sla: test_hw_sla_config_builders_only(),
            ..Default::default()
        },
    ))
}

/// Bootstrap PG + spawned actor with the realistic-ceiling `[sla]`
/// config. For end-to-end `GetSpawnIntents` tests via [`ActorHandle`].
pub(crate) async fn setup_with_big_ceilings() -> (TestDb, ActorHandle, tokio::task::JoinHandle<()>)
{
    let db = TestDb::new(&MIGRATOR).await;
    seed_default_tenant(&db.pool).await;
    let (handle, task) =
        setup_actor_configured(db.pool.clone(), None, |c, _| c.sla = test_sla_config());
    (db, handle, task)
}

/// Background driver for `LeaderState` confirmation rounds:
/// `begin_renew_round` + `confirm_leading_round` every ~50ms, simulating
/// a healthy lease loop that keeps completing Leading rounds. Recoveries
/// whose claim target the durable PG floor cannot vouch for — a target
/// above the entry generation, or a retained entry generation more than
/// one above the floor (or above an empty floor) — wait for one
/// post-claim confirmed round before completing
/// (`sched.recovery.bump-confirm`); tests that drive `LeaderAcquired` by
/// hand on such a PG state need this running or they would wait
/// out the confirmation cap and discard. The returned guard aborts the
/// task on drop.
pub(crate) struct ConfirmationLoop(tokio::task::JoinHandle<()>);

impl Drop for ConfirmationLoop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Spawn a [`ConfirmationLoop`] driving `leader`. See the struct doc.
pub(crate) fn spawn_leading_confirmations(leader: crate::lease::LeaderState) -> ConfirmationLoop {
    ConfirmationLoop(tokio::spawn(async move {
        loop {
            let round = leader.begin_renew_round();
            leader.confirm_leading_round(round);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }))
}

/// Recovery test fixture. Absorbs the 19× phase-1/phase-2 boilerplate
/// in `recovery.rs`: `TestDb::new` → spawn first actor → seed via
/// closure → `drop(handle)` + join → spawn fresh actor →
/// `LeaderAcquired` + barrier.
///
/// Phase-1 closure receives `(handle, pool)` and runs whatever
/// merge/backdate the test needs. Phase-2 spawns a fresh actor on the
/// same PG (with optional store client) and sends `LeaderAcquired`.
pub(crate) struct RecoveryFixture {
    pub db: TestDb,
    pub handle: ActorHandle,
    /// Phase-2 `LeaderState` — exposed so tests can drive lease-side
    /// transitions against the same instance the actor holds.
    pub leader: crate::lease::LeaderState,
    /// Keeps the phase-2 confirmation loop alive for the fixture's
    /// lifetime (aborts on drop). Phase-1 dispatches leave assignment
    /// rows at the entry generation with no claim row, which makes the
    /// phase-2 recovery a claim target the floor cannot vouch for as
    /// its own — it waits for a post-claim Leading round
    /// (`sched.recovery.bump-confirm`).
    pub _confirmations: ConfirmationLoop,
    pub _task: tokio::task::JoinHandle<()>,
}

impl RecoveryFixture {
    /// Full recovery cycle: run `seed` against a phase-1 actor, drop it,
    /// spawn a fresh phase-2 actor, send `LeaderAcquired`, barrier.
    pub(crate) async fn run<F, Fut>(seed: F) -> anyhow::Result<Self>
    where
        F: FnOnce(ActorHandle, sqlx::PgPool) -> Fut,
        Fut: Future<Output = anyhow::Result<()>>,
    {
        Self::run_with_store(None, seed).await
    }

    /// [`Self::run`] with an optional store client for the phase-2 actor
    /// (orphan-completion / reconcile tests need a store for the
    /// FindMissingPaths check).
    pub(crate) async fn run_with_store<F, Fut>(
        store: Option<StoreServiceClient<Channel>>,
        seed: F,
    ) -> anyhow::Result<Self>
    where
        F: FnOnce(ActorHandle, sqlx::PgPool) -> Fut,
        Fut: Future<Output = anyhow::Result<()>>,
    {
        Self::run_configured(store, |_| {}, seed).await
    }

    /// [`Self::run_with_store`] with a config hook applied to BOTH the
    /// phase-1 (seeding) and phase-2 (recovering) actors — for tests
    /// whose recovered-fold semantics depend on retry/poison knobs.
    pub(crate) async fn run_configured<C, F, Fut>(
        store: Option<StoreServiceClient<Channel>>,
        configure: C,
        seed: F,
    ) -> anyhow::Result<Self>
    where
        C: Fn(&mut DagActorConfig) + Clone + Send + 'static,
        F: FnOnce(ActorHandle, sqlx::PgPool) -> Fut,
        Fut: Future<Output = anyhow::Result<()>>,
    {
        let db = TestDb::new(&MIGRATOR).await;
        crate::actor::tests::seed_default_tenant(&db.pool).await;
        // Phase 1: first "leader" writes state.
        {
            let phase1_configure = configure.clone();
            let (handle, task) =
                setup_actor_configured(db.pool.clone(), None, move |c, _| phase1_configure(c));
            seed(handle, db.pool.clone()).await?;
            // handle dropped at end of seed's scope (moved in); join
            // the task so PG writes are flushed.
            let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        }
        // Phase 2: fresh actor recovers. The LeaderState mirrors the
        // always-leader default but is constructed explicitly so the
        // confirmation loop can drive its renew-round counters.
        let leader = crate::lease::LeaderState::always_leader(std::sync::Arc::new(
            std::sync::atomic::AtomicU64::new(1),
        ));
        let confirmations = spawn_leading_confirmations(leader.clone());
        let phase2_leader = leader.clone();
        let (handle, task) = setup_actor_configured(db.pool.clone(), store, move |c, p| {
            configure(c);
            p.leader = phase2_leader;
        });
        handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
        barrier(&handle).await;
        Ok(Self {
            db,
            handle,
            leader,
            _confirmations: confirmations,
            _task: task,
        })
    }
}

/// Phase-1 helper for [`RecoveryFixture`]: poison `drv_hash` via a
/// PermanentFailure completion. Three recovery tests share this exact
/// 10-line sequence.
pub(crate) async fn seed_poisoned(handle: &ActorHandle, drv_hash: &str) -> anyhow::Result<()> {
    let _ev = merge_single_node(handle, Uuid::new_v4(), drv_hash, PriorityClass::Scheduled).await?;
    pull_complete_failure(
        handle,
        drv_hash,
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "permanent",
    )
    .await?;
    barrier(handle).await;
    Ok(())
}

/// Force-assign + send `status` failure for `drv_hash` on each of
/// `workers` in sequence. Absorbs the 12-line `for (i, w) in workers
/// { force_assign; complete_failure }` loop repeated across the
/// poison-threshold matrix in `completion.rs`.
pub(crate) async fn fail_on_workers(
    handle: &ActorHandle,
    drv_hash: &str,
    status: rio_proto::types::BuildResultStatus,
    workers: &[&str],
) -> anyhow::Result<()> {
    let drv_path = test_drv_path(drv_hash);
    for (i, w) in workers.iter().enumerate() {
        assert!(
            handle.debug_force_assign(drv_hash, w).await?,
            "force-assign {drv_hash} → {w} (iter {i})"
        );
        complete_failure(handle, w, &drv_path, status, &format!("failure {i}")).await?;
    }
    Ok(())
}

/// Awaits actor quiescence by round-tripping a no-op query.
///
/// Unlike the old `settle()` (sleep-based), this is a **true barrier**:
/// when it returns, the actor has processed all messages sent before
/// this call. Works because the actor processes messages serially from
/// one mpsc — a request-reply guarantees everything ahead of it in the
/// channel has been handled.
///
/// ## When you DON'T need this
///
/// Most call sites don't need an explicit barrier at all, because the
/// next line is already a request-reply:
///   - `debug_query_*` / `query_status` / `try_query_status`
///   - `merge_single_node` / `merge_dag` (awaits MergeDag reply, which
///     is sent AFTER `dispatch_ready()` runs inline)
///   - any `reply_rx.await`
///
/// You only need `barrier()` when the next assertion is on state that
/// isn't mediated by the actor channel — e.g. `logs_contain()` (checks
/// captured tracing output) or assertions on shared Arc state that the
/// actor mutates as a side effect.
pub(crate) async fn barrier(handle: &ActorHandle) {
    // Any request-reply round-trip flushes everything queued ahead of
    // it; GcRoots is the cheapest read-only admin query.
    let _ = handle
        .query_unchecked(|reply| ActorCommand::Admin(AdminQuery::GcRoots { reply }))
        .await;
}

/// Poll until `hash` reaches `want` — i.e. the actor has ENTERED the
/// target status. Bounded (10ms × 100). For tests that need to observe
/// a node IN a transient status before flipping a knob.
pub(crate) async fn wait_for_status(
    handle: &ActorHandle,
    hash: &str,
    want: crate::state::DerivationStatus,
) {
    for _ in 0..100 {
        tokio::task::yield_now().await;
        barrier(handle).await;
        if let Ok(Some(d)) = handle.debug_query_derivation(hash).await
            && d.status == want
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("wait_for_status: timed out waiting for {hash:?} to reach {want:?}");
}

// Re-export for test modules. Canonical impl moved to rio-test-support
// (P0330) — was 3× copied with method-name drift before that.
pub(crate) use rio_test_support::metrics::CountingRecorder;

// ─────────────────────────────────────────────────────────────────────────
// Pull-mode delivery helpers.
//
// The only delivery vehicle: an attempt is opened through the same
// `admit_pull` + fenced-mint transaction production uses
// (`ActorCommand::PullAssignment`), outcomes land through the report intake
// (`ReportPullOutcome` → the same `handle_completion` entry point), and
// pod-terminal/fault injection goes through `ReportAttemptOutcome` or the
// establishment sweep — never by writing rows directly. (The stream-session
// helpers that used to sit above — `connect_executor` → `recv_assignment` →
// `complete_*` — retired with the session machinery; `complete_*` survive
// as thin report-intake wrappers.)
//
// Identity convention (mirrors actor/pull.rs): a pull attempt's executor
// identity IS the attested intent id (the drv hash). Exclusion/budget keys
// come from the controller-authoritative node binding
// ([`bind_intent_node`]) ONLY (decision P12) -- an unbound attempt charges
// flat counters but contributes no exclusion key.
// ─────────────────────────────────────────────────────────────────────────

pub(crate) use crate::actor::pull::{PullOutcome, PullRejection, PullReportPayload};

/// Send one `PullAssignment` for `drv_hash` and return the raw outcome.
/// The auth token is bound to the same intent (the production pod-token
/// shape); use this directly when the test asserts `Gone` / `NotYetReady`
/// / a rejection rather than a delivery.
pub(crate) async fn try_pull_attempt(
    handle: &ActorHandle,
    drv_hash: &str,
) -> Result<PullOutcome, PullRejection> {
    handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: drv_hash.into(),
            auth_intent: Some(drv_hash.into()),
            // Mechanical flag-off defaults (carve-out 1c).
            kind: rio_evidence_kernel::pull::PullKind::Build,
            executor_instance: None,
            resume_exec_id: None,
            claim_nonce: None,
            confirm_only: false,
            executor_token_sha256: Some("tokhash-pod-a".into()),
            reply,
        })
        .await
        .expect("actor alive")
}

/// Mint (or idempotently re-deliver) the open pull attempt for `drv_hash`
/// and return its `WorkAssignment`. The pull-mode replacement for
/// `recv_assignment`: when this returns, an open attempt exists for the
/// drv on its intent identity, the durable mint has committed, and the
/// node is Running. Panics when the pull does not deliver (drv not Ready /
/// not wanted / open on another executor) — that is a test-sequencing bug,
/// the same way a missing `recv_assignment` message was.
pub(crate) async fn pull_attempt(
    handle: &ActorHandle,
    drv_hash: &str,
) -> rio_proto::types::WorkAssignment {
    match try_pull_attempt(handle, drv_hash).await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("pull_attempt({drv_hash:?}): expected Deliver, got {other:?}"),
    }
}

/// The exec_id of the open pull attempt for `drv_hash`, minting one if
/// none is open yet.
pub(crate) async fn open_pull_exec(handle: &ActorHandle, drv_hash: &str) -> Uuid {
    pull_attempt(handle, drv_hash)
        .await
        .exec_id
        .parse()
        .expect("pull-delivered exec_id is a uuid")
}

/// Wrap a `BuildResult` in the zeroed-readings report payload shape the
/// stream-era `complete_*` helpers used (no resource readings, no node /
/// hw-class attribution, no line count).
pub(crate) fn pull_payload(result: rio_proto::types::BuildResult) -> PullReportPayload {
    PullReportPayload {
        result,
        peak_memory_bytes: 0,
        peak_cpu_cores: 0.0,
        node_name: None,
        hw_class: None,
        final_resources: None,
        final_line_count: 0,
        // Mechanical flag-off default (carve-out 1c).
        materialization_outcome: None,
    }
}

/// Send one `ReportPullOutcome` for an explicit exec_id (token bound to
/// `intent`). Use when the test must re-report a specific attempt (e.g.
/// duplicate-report idempotency after the drv went terminal — a fresh pull
/// could not return that exec any more).
pub(crate) async fn pull_report_exec(
    handle: &ActorHandle,
    exec_id: Uuid,
    intent: &str,
    payload: PullReportPayload,
) -> anyhow::Result<()> {
    handle
        .query_unchecked(|reply| ActorCommand::ReportPullOutcome {
            exec_id,
            auth_intent: Some(intent.into()),
            payload,
            reply,
        })
        .await?
        .map_err(|e| anyhow::anyhow!("ReportPullOutcome rejected: {e:?}"))?;
    Ok(())
}

/// Report an outcome for the open pull attempt of `drv_hash`: pull (mint
/// or re-deliver) to resolve the exec_id, then drive the report intake.
/// When this returns the report has been fully folded (the reply is sent
/// after `handle_completion` returns), so no extra barrier is needed.
pub(crate) async fn pull_report(
    handle: &ActorHandle,
    drv_hash: &str,
    payload: PullReportPayload,
) -> anyhow::Result<()> {
    let exec_id = open_pull_exec(handle, drv_hash).await;
    pull_report_exec(handle, exec_id, drv_hash, payload).await
}

/// Pull-mode `complete_success`: open attempt + Built report with a single
/// `out` output (placeholder hash).
pub(crate) async fn pull_complete_success(
    handle: &ActorHandle,
    drv_hash: &str,
    output_path: &str,
) -> anyhow::Result<()> {
    pull_report(
        handle,
        drv_hash,
        pull_payload(rio_proto::types::BuildResult {
            status: rio_proto::types::BuildResultStatus::Built.into(),
            built_outputs: vec![rio_proto::types::BuiltOutput {
                output_name: "out".into(),
                output_path: output_path.into(),
                output_hash: vec![0u8; 32],
            }],
            ..Default::default()
        }),
    )
    .await
}

/// Pull-mode `complete_success_empty`: Built report with no outputs.
pub(crate) async fn pull_complete_success_empty(
    handle: &ActorHandle,
    drv_hash: &str,
) -> anyhow::Result<()> {
    pull_report(
        handle,
        drv_hash,
        pull_payload(rio_proto::types::BuildResult {
            status: rio_proto::types::BuildResultStatus::Built.into(),
            ..Default::default()
        }),
    )
    .await
}

/// Pull-mode `complete_ca`: Built report with caller-controlled per-output
/// hash bytes. Each entry is `(output_name, output_path, output_hash)`.
pub(crate) async fn pull_complete_ca(
    handle: &ActorHandle,
    drv_hash: &str,
    outputs: &[(&str, &str, Vec<u8>)],
) -> anyhow::Result<()> {
    pull_report(
        handle,
        drv_hash,
        pull_payload(rio_proto::types::BuildResult {
            status: rio_proto::types::BuildResultStatus::Built.into(),
            built_outputs: outputs
                .iter()
                .map(|(name, path, hash)| rio_proto::types::BuiltOutput {
                    output_name: (*name).into(),
                    output_path: (*path).into(),
                    output_hash: hash.clone(),
                })
                .collect(),
            ..Default::default()
        }),
    )
    .await
}

/// Pull-mode `complete_failure`: open attempt + failure report with the
/// given status and error message.
pub(crate) async fn pull_complete_failure(
    handle: &ActorHandle,
    drv_hash: &str,
    status: rio_proto::types::BuildResultStatus,
    error_msg: &str,
) -> anyhow::Result<()> {
    pull_report(
        handle,
        drv_hash,
        pull_payload(rio_proto::types::BuildResult {
            status: status.into(),
            error_msg: error_msg.into(),
            ..Default::default()
        }),
    )
    .await
}

/// [`pull_complete_failure`] with a caller-built `BuildResult` — for
/// reports that carry more than (status, error_msg), e.g. bug_408's
/// `store_degraded` flag.
pub(crate) async fn pull_complete_failure_result(
    handle: &ActorHandle,
    drv_hash: &str,
    result: rio_proto::types::BuildResult,
) -> anyhow::Result<()> {
    pull_report(handle, drv_hash, pull_payload(result)).await
}

/// Record the controller-authoritative pod→node binding for `intent`
/// (`AckSpawnedIntents.bound_intents` — the same Model J surface the
/// controller drives). Subsequent attempt rows for the intent carry
/// `source_node = node`, so the exclusion fold keys them by node exactly
/// as production pull attempts are keyed. Re-binding the same intent to a
/// new node overwrites (the respawn-on-a-different-node shape).
pub(crate) async fn bind_intent_node(
    handle: &ActorHandle,
    intent: &str,
    node: &str,
) -> anyhow::Result<()> {
    handle
        .send_unchecked(ActorCommand::AckSpawnedIntents {
            // merged_bug_005 reply: receiver intentionally dropped —
            // these tests assert via actor state, not the ack path.
            rejected: vec![],
            reply: tokio::sync::oneshot::channel().0,
            binding_snapshot: None,
            spawned: vec![],
            unfulfillable_cells: vec![],
            registered_cells: vec![],
            observed_instance_types: vec![],
            bound_intents: vec![rio_proto::types::BoundIntent {
                intent_id: intent.into(),
                node_name: node.into(),
                deadline_secs: 0,
            }],
        })
        .await?;
    barrier(handle).await;
    Ok(())
}

/// Pull-mode `fail_on_workers`: for each node in sequence, bind the
/// intent to that node, open a pull attempt, and report `status`. Each
/// iteration therefore charges a distinct source-node exclusion key —
/// the pull-path equivalent of failing on N distinct stream workers.
pub(crate) async fn pull_fail_on_nodes(
    handle: &ActorHandle,
    drv_hash: &str,
    status: rio_proto::types::BuildResultStatus,
    nodes: &[&str],
) -> anyhow::Result<()> {
    for (i, node) in nodes.iter().enumerate() {
        bind_intent_node(handle, drv_hash, node).await?;
        pull_complete_failure(handle, drv_hash, status, &format!("failure {i}")).await?;
    }
    Ok(())
}

/// Send one `ReportAttemptOutcome` (the unified pod-terminal intake):
/// controller-synthesized verdicts (Cancelled/Preempted/Reaped), the
/// second-installment reason fill, or the spawn-gate NoEligibleSource.
/// The pull-mode fault-injection entry point for "the pod died".
pub(crate) async fn report_attempt_terminal(
    handle: &ActorHandle,
    intent_id: Option<&str>,
    exec_id: Option<Uuid>,
    reason: rio_proto::types::AttemptTerminalReason,
    node: Option<&str>,
) -> Result<crate::actor::pull::AttemptResolution, PullRejection> {
    handle
        .query_unchecked(|reply| ActorCommand::ReportAttemptOutcome {
            identity: crate::actor::pull::AttemptIdentity {
                intent_id: intent_id.map(Into::into),
                job_name: None,
                exec_id,
            },
            reason,
            node_name: node.map(Into::into),
            resubmit_cycle: 0,
            reply,
        })
        .await
        .expect("actor alive")
}

/// Backdate the assignment row of one pull attempt past any establishment
/// window (deadline + slack), so the next `Tick` runs the establishment
/// sweep over it. The pull-mode injection point for "the executor crashed
/// without ever reporting".
pub(crate) async fn backdate_pull_attempt(
    pool: &sqlx::PgPool,
    exec_id: Uuid,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE assignments SET assigned_at = now() - interval '100 days' WHERE exec_id = $1",
    )
    .bind(exec_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Bundle of handles for pull-mode CA-compare test scenarios. The
/// pull-protocol sibling of [`CaFixture`]: no stream receiver and no
/// worker identity — the attempt is opened by [`pull_complete_ca`] /
/// [`pull_attempt`] on the intent identity itself.
pub(crate) struct PullCaFixture {
    /// MockStore handle — arm fault flags or seed paths BEFORE driving
    /// the report intake to the CA-compare callsite.
    pub store: rio_test_support::grpc::MockStore,
    /// Actor handle.
    pub actor: ActorHandle,
    /// The single CA derivation's hash (the fixture key / intent id).
    pub drv_hash: String,
    /// The single CA derivation's path (`test_drv_path(key)`).
    pub drv_path: String,
    /// Build id for the merged single-node DAG.
    pub build_id: Uuid,
    /// The CA node's modular hash (see [`CaFixture::modular_hash`]).
    pub modular_hash: [u8; 32],
    /// PG pool — seed realisations directly.
    pub pool: sqlx::PgPool,
    /// PG test database — keep alive for the actor's pool.
    pub _db: TestDb,
    /// MockStore tokio task guard.
    pub _store_task: tokio::task::JoinHandle<()>,
    /// Actor tokio task guard.
    pub _actor_task: tokio::task::JoinHandle<()>,
}

/// Pull-mode CA-compare setup: spawn MockStore, actor with store client,
/// merge a single `is_content_addressed=true` node, return the bundle.
/// No worker is registered and nothing is dispatched — the node is Ready
/// when this returns, and the CA-compare only fires when the test drives
/// the report intake ([`pull_complete_ca`]), so realisations/faults can be
/// seeded after setup exactly as with [`setup_ca_fixture`].
pub(crate) async fn setup_pull_ca_fixture(key: &str) -> anyhow::Result<PullCaFixture> {
    setup_pull_ca_fixture_configured(key, |_, _| {}).await
}

/// Like [`setup_pull_ca_fixture`] but lets the caller mutate
/// `DagActorConfig`/`DagActorPlumbing` before spawn.
pub(crate) async fn setup_pull_ca_fixture_configured(
    key: &str,
    configure: impl FnOnce(&mut DagActorConfig, &mut DagActorPlumbing),
) -> anyhow::Result<PullCaFixture> {
    let db = TestDb::new(&MIGRATOR).await;
    seed_default_tenant(&db.pool).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (actor, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), configure);

    let modular_hash: [u8; 32] = {
        use sha2::{Digest, Sha256};
        Sha256::digest(format!("ca-fixture:{key}").as_bytes()).into()
    };
    let mut node = make_node(key);
    node.is_content_addressed = true;
    node.ca_modular_hash = modular_hash.to_vec();
    let drv_path = node.drv_path.clone();
    let build_id = Uuid::new_v4();
    let _ev = merge_dag(&actor, build_id, vec![node], vec![], false).await?;

    Ok(PullCaFixture {
        store,
        actor,
        drv_hash: key.to_string(),
        drv_path,
        build_id,
        modular_hash,
        pool: db.pool.clone(),
        _db: db,
        _store_task: store_task,
        _actor_task: actor_task,
    })
}

#[tokio::test]
async fn test_actor_starts_and_stops() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (handle, task) = setup_actor(db.pool.clone());
    // Query should succeed (actor is running). Also acts as a barrier.
    let roots = handle
        .query_unchecked(|reply| ActorCommand::Admin(AdminQuery::GcRoots { reply }))
        .await?;
    assert!(roots.is_empty());
    // Drop handle to close channel
    drop(handle);
    // Actor task should exit
    tokio::time::timeout(Duration::from_secs(5), task).await??;
    Ok(())
}

/// is_alive() should detect actor death (channel closed = receiver dropped).
#[tokio::test]
async fn test_actor_is_alive_detection() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (handle, task) = setup_actor(db.pool.clone());

    // Actor should be alive after spawn. is_alive() is just !tx.is_closed()
    // — no message processing needed for this check.
    assert!(handle.is_alive(), "actor should be alive after spawn");

    // Abort the actor task to simulate a panic/crash.
    task.abort();
    // Await the JoinHandle — after abort() it returns Err(Cancelled)
    // immediately once the task drops. No timed sleep needed.
    let _ = task.await;

    // is_alive() should now report false (channel closed).
    assert!(
        !handle.is_alive(),
        "is_alive should report false after actor task dies"
    );
}

/// One `drv_attempts` row joined back to its derivation — the shape the
/// 1a attempt-ledger assertions consume. Loaded by [`ledger_rows`].
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct LedgerRow {
    pub event_kind: String,
    pub outcome_class: String,
    pub executor_id: Option<String>,
    pub exec_id: Option<Uuid>,
    pub termination_reason: Option<String>,
    pub error_msg: Option<String>,
    pub final_line_count: Option<i64>,
    pub exempt: bool,
    pub floor_promoted: bool,
    pub floor_at_cap: bool,
    pub resubmit_cycle: i32,
}

/// Every attempt-ledger row for `drv_hash`, in append order. The 1a
/// appends run inside the handler turn (not fire-and-forget), so after
/// the actor has processed the triggering command the rows are visible
/// without polling.
pub(crate) async fn ledger_rows(pool: &sqlx::PgPool, drv_hash: &str) -> Vec<LedgerRow> {
    sqlx::query_as(
        "SELECT a.event_kind, a.outcome_class, a.executor_id, a.exec_id, \
                a.termination_reason, a.error_msg, a.final_line_count, \
                a.exempt, a.floor_promoted, a.floor_at_cap, a.resubmit_cycle \
         FROM drv_attempts a \
         JOIN derivations d ON d.derivation_id = a.derivation_id \
         WHERE d.drv_hash = $1 \
         ORDER BY a.recorded_at, a.attempt_id",
    )
    .bind(drv_hash)
    .fetch_all(pool)
    .await
    .expect("drv_attempts query")
}

/// The outcome classes of every ledger row for `drv_hash`, in append
/// order — the compact shape most attempt-ledger assertions want.
pub(crate) async fn ledger_classes(pool: &sqlx::PgPool, drv_hash: &str) -> Vec<String> {
    ledger_rows(pool, drv_hash)
        .await
        .into_iter()
        .map(|r| r.outcome_class)
        .collect()
}
