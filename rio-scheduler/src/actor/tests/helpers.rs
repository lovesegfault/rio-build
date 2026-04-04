//! Shared test helpers: actor setup, fixture builders, synchronization barrier.

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
    setup_actor_configured(pool, store_client, |a| a)
}

/// Bundle of handles for CA-compare test scenarios. See [`setup_ca_fixture`].
pub(crate) struct CaFixture {
    /// MockStore handle — arm fault flags or seed paths for
    /// FindMissingPaths BEFORE driving the actor to the CA-compare
    /// callsite.
    pub store: rio_test_support::grpc::MockStore,
    /// Actor handle — send commands, await replies.
    pub actor: ActorHandle,
    /// Connected worker's message receiver — `recv_assignment(&mut f.rx)`
    /// to get the dispatched assignment after the single-node merge.
    pub rx: mpsc::Receiver<rio_proto::types::SchedulerMessage>,
    /// The single CA derivation's path (`test_drv_path(key)`).
    pub drv_path: String,
    /// The connected worker's id (`"w-{key}"`). Pass to [`complete_ca`].
    pub executor_id: String,
    /// Build id for the merged single-node DAG.
    pub build_id: Uuid,
    /// The CA node's modular hash (set on the proto node so the
    /// compare gate `state.ca_modular_hash.is_some()` passes).
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
/// client, connect a worker, merge a single `is_content_addressed=true`
/// node, return all handles bundled as [`CaFixture`].
///
/// Absorbs the 8-copy boilerplate that had accrued across `completion.rs`
/// (5 added by P0311, 3 pre-existing): `TestDb::new` +
/// `spawn_mock_store_with_client` + `setup_actor_with_store` +
/// `connect_executor` + `make_test_node(ca=true)` + `merge_dag`. Each test
/// drops from ~15L setup to 1-2L.
///
/// The fixture returns with the actor holding the node in Ready/Assigned
/// — the CA-compare callsite at `completion.rs` only fires on
/// `ProcessCompletion`, so tests can arm fault flags or seed the store
/// AFTER this returns and BEFORE calling [`complete_ca`]. Verified by
/// `setup_ca_fixture_does_not_race_past_ca_compare`.
///
/// Tests that need a configured actor (e.g. `with_grpc_timeout`) should
/// use [`setup_ca_fixture_configured`].
pub(crate) async fn setup_ca_fixture(key: &str) -> anyhow::Result<CaFixture> {
    setup_ca_fixture_configured(key, |a| a).await
}

/// Like [`setup_ca_fixture`] but applies a configurator closure to the
/// actor before spawn — for tests that need `.with_grpc_timeout()` or
/// other `DagActor` builder methods.
pub(crate) async fn setup_ca_fixture_configured(
    key: &str,
    configure: impl FnOnce(DagActor) -> DagActor,
) -> anyhow::Result<CaFixture> {
    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (actor, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), configure);

    let executor_id = format!("w-{key}");
    let rx = connect_executor(&actor, &executor_id, "x86_64-linux", 4).await?;

    // Deterministic modular_hash per key — the CA-compare gate
    // requires `state.ca_modular_hash.is_some()`. Real flow: the
    // gateway's populate_ca_modular_hashes fills this from
    // hash_derivation_modulo; test fixture fakes it with a
    // key-derived hash so tests can seed matching PG rows.
    let modular_hash: [u8; 32] = {
        use sha2::{Digest, Sha256};
        Sha256::digest(format!("ca-fixture:{key}").as_bytes()).into()
    };
    let mut node = make_test_node(key, "x86_64-linux");
    node.is_content_addressed = true;
    node.ca_modular_hash = modular_hash.to_vec();
    let drv_path = node.drv_path.clone();
    let build_id = Uuid::new_v4();
    let _ev = merge_dag(&actor, build_id, vec![node], vec![], false).await?;

    Ok(CaFixture {
        store,
        actor,
        rx,
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

/// Set up an actor with a configurator closure applied before spawn.
/// For tests that need `.with_size_classes()` / `.with_event_persister()`
/// etc — avoids reimplementing the full spawn boilerplate inline.
pub(crate) fn setup_actor_configured(
    pool: sqlx::PgPool,
    store_client: Option<StoreServiceClient<Channel>>,
    configure: impl FnOnce(DagActor) -> DagActor,
) -> (ActorHandle, tokio::task::JoinHandle<()>) {
    let db = SchedulerDb::new(pool);
    let (tx, rx) = mpsc::channel(ACTOR_CHANNEL_CAPACITY);
    let actor = configure(DagActor::new(db, store_client));
    let backpressure = actor.backpressure_flag();
    let generation = actor.generation_reader();
    let snapshot_rx = actor.snapshot_receiver();
    let self_tx = tx.downgrade();
    let task = tokio::spawn(actor.run_with_self_tx(rx, self_tx));
    (
        ActorHandle {
            tx,
            backpressure,
            generation,
            snapshot_rx,
        },
        task,
    )
}

/// Bootstrap an ephemeral PG + actor. The returned `TestDb` MUST be held
/// for the test duration — `TestDb::Drop` tears down the database.
/// Most callers: `let (_db, handle, _task) = setup().await;`
pub(crate) async fn setup() -> (TestDb, ActorHandle, tokio::task::JoinHandle<()>) {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, task) = setup_actor(db.pool.clone());
    (db, handle, task)
}

/// Bootstrap PG + actor + one fully-registered worker.
/// Returns the scheduler→worker message receiver alongside the standard triple.
pub(crate) async fn setup_with_worker(
    executor_id: &str,
    system: &str,
    max_builds: u32,
) -> anyhow::Result<(
    TestDb,
    ActorHandle,
    tokio::task::JoinHandle<()>,
    mpsc::Receiver<rio_proto::types::SchedulerMessage>,
)> {
    let (db, handle, task) = setup().await;
    let rx = connect_executor(&handle, executor_id, system, max_builds).await?;
    Ok((db, handle, task, rx))
}

/// Connect a worker (stream + heartbeat) WITHOUT the automatic
/// `PrefetchComplete` ACK. For warm-gate tests that need to observe
/// the initial `PrefetchHint` arrival and/or prove dispatch blocks
/// until the ACK. Shared `Heartbeat` field list — when
/// `ActorCommand::Heartbeat` grows a field, one edit not two.
pub(crate) async fn connect_executor_no_ack(
    handle: &ActorHandle,
    executor_id: &str,
    system: &str,
    max_builds: u32,
) -> anyhow::Result<mpsc::Receiver<rio_proto::types::SchedulerMessage>> {
    connect_executor_no_ack_kind(
        handle,
        executor_id,
        system,
        max_builds,
        rio_proto::types::ExecutorKind::Builder,
    )
    .await
}

/// [`connect_executor_no_ack`] with explicit executor kind. For FOD
/// routing tests that need a fetcher-kind executor (ADR-019). The
/// builder-default wrapper above keeps the 60+ existing call sites
/// unchanged.
pub(crate) async fn connect_executor_no_ack_kind(
    handle: &ActorHandle,
    executor_id: &str,
    system: &str,
    // P0537 stage 1: ignored (always-1). Kept so 60+ callers don't
    // churn in this commit; stage 3 deletes it.
    _max_builds: u32,
    kind: rio_proto::types::ExecutorKind,
) -> anyhow::Result<mpsc::Receiver<rio_proto::types::SchedulerMessage>> {
    let (stream_tx, stream_rx) = mpsc::channel(256);
    handle
        .send_unchecked(ActorCommand::ExecutorConnected {
            executor_id: executor_id.into(),
            stream_tx,
        })
        .await?;
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: false,
            draining: false,
            kind,
            resources: None,
            bloom: None,
            size_class: None,
            executor_id: executor_id.into(),
            systems: vec![system.into()],
            supported_features: vec![],
            running_builds: vec![],
        })
        .await?;
    Ok(stream_rx)
}

/// I-170: connect a Fetcher-kind executor with a size_class. For
/// `r[sched.fod.size-class-reactive]` tests — the FOD floor walk
/// matches against `ExecutorState.size_class`. Includes the warm-
/// gate ACK (same as `connect_executor`) so callers can merge then
/// `recv_assignment` directly.
pub(crate) async fn connect_fetcher_classed(
    handle: &ActorHandle,
    executor_id: &str,
    system: &str,
    size_class: &str,
) -> anyhow::Result<mpsc::Receiver<rio_proto::types::SchedulerMessage>> {
    let (stream_tx, stream_rx) = mpsc::channel(256);
    handle
        .send_unchecked(ActorCommand::ExecutorConnected {
            executor_id: executor_id.into(),
            stream_tx,
        })
        .await?;
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: false,
            draining: false,
            kind: rio_proto::types::ExecutorKind::Fetcher,
            resources: None,
            bloom: None,
            size_class: Some(size_class.into()),
            executor_id: executor_id.into(),
            systems: vec![system.into()],
            supported_features: vec![],
            running_builds: vec![],
        })
        .await?;
    handle
        .send_unchecked(ActorCommand::PrefetchComplete {
            executor_id: executor_id.into(),
            paths_fetched: 0,
        })
        .await?;
    Ok(stream_rx)
}

/// Send a default heartbeat (no bloom, no size_class, empty
/// running_builds, store_degraded=false), then a `Tick`. For the
/// "extra heartbeat to trigger dispatch" pattern where the test
/// doesn't care about heartbeat field values, only that dispatch runs.
///
/// I-163: Heartbeat alone now sets `dispatch_dirty` instead of
/// dispatching inline; the trailing `Tick` drains it. Callers that
/// specifically need Heartbeat-without-dispatch construct
/// `ActorCommand::Heartbeat` directly (the field-churn cost is on
/// those few sites, not the dozen "trigger dispatch" callers).
///
/// Shared field list with [`connect_executor_no_ack`] — when
/// `ActorCommand::Heartbeat` grows a field, this is the second edit
/// site (not 20+ scattered across test modules).
pub(crate) async fn send_heartbeat(
    handle: &ActorHandle,
    executor_id: &str,
    system: &str,
    // P0537 stage 1: ignored (always-1). Stage 3 deletes it.
    _max_builds: u32,
) -> anyhow::Result<()> {
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: false,
            draining: false,
            kind: rio_proto::types::ExecutorKind::Builder,
            resources: None,
            bloom: None,
            size_class: None,
            executor_id: executor_id.into(),
            systems: vec![system.into()],
            supported_features: vec![],
            running_builds: vec![],
        })
        .await?;
    handle.send_unchecked(ActorCommand::Tick).await?;
    Ok(())
}

/// Send `ActorCommand::Tick` and barrier on it. For tests driving the
/// `dispatch_dirty` → dispatch path or refreshing the cached
/// `ClusterSnapshot` without faking a heartbeat.
pub(crate) async fn tick(handle: &ActorHandle) -> anyhow::Result<()> {
    handle.send_unchecked(ActorCommand::Tick).await?;
    barrier(handle).await;
    Ok(())
}

/// I-063: heartbeat with `draining=true`. For tests that verify the
/// worker-authoritative drain semantics — heartbeat is the only
/// reader/writer of a worker's own drain state.
pub(crate) async fn send_heartbeat_draining(
    handle: &ActorHandle,
    executor_id: &str,
    system: &str,
    _max_builds: u32,
) -> anyhow::Result<()> {
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: false,
            draining: true,
            kind: rio_proto::types::ExecutorKind::Builder,
            resources: None,
            bloom: None,
            size_class: None,
            executor_id: executor_id.into(),
            systems: vec![system.into()],
            supported_features: vec![],
            running_builds: vec![],
        })
        .await?;
    handle.send_unchecked(ActorCommand::Tick).await?;
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
    Ok(reply_rx.await??)
}

/// Connect a worker (stream + heartbeat) so it becomes fully registered.
/// Returns the mpsc::Receiver for scheduler→worker messages.
pub(crate) async fn connect_executor(
    handle: &ActorHandle,
    executor_id: &str,
    system: &str,
    max_builds: u32,
) -> anyhow::Result<mpsc::Receiver<rio_proto::types::SchedulerMessage>> {
    let stream_rx = connect_executor_no_ack(handle, executor_id, system, max_builds).await?;
    // Warm-gate: unconditionally ACK so the worker flips warm=true
    // regardless of whether on_worker_registered sent an initial
    // PrefetchHint (it only does so when the ready queue is non-
    // empty at registration time). Idempotent: if the queue was
    // empty, warm was already flipped true by the registration hook;
    // the ACK is a no-op re-set. Either way, the worker is warm by
    // the time subsequent dispatch_ready calls run — existing tests'
    // "connect then merge then recv_assignment" flow is preserved.
    //
    // Tests that merge FIRST then connect will see the initial
    // PrefetchHint on stream_rx before any Assignment. Such tests
    // must drain the hint themselves (recv + match Prefetch) or
    // use a recv loop that skips Prefetch variants.
    handle
        .send_unchecked(ActorCommand::PrefetchComplete {
            executor_id: executor_id.into(),
            paths_fetched: 0,
        })
        .await?;
    Ok(stream_rx)
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
                tenant_id: None,
                priority_class,
                nodes: vec![make_test_node(tag, "x86_64-linux")],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: false,
                traceparent: String::new(),
                jti: None,
                jwt_token: None,
            },
            reply: reply_tx,
        })
        .await?;
    Ok(reply_rx.await??)
}

/// Merge a multi-node DAG with default options (tenant=None,
/// priority=Scheduled, options=default). Generalization of [`merge_single_node`].
/// Returns the broadcast receiver for build events.
pub(crate) async fn merge_dag(
    handle: &ActorHandle,
    build_id: Uuid,
    nodes: Vec<rio_proto::dag::DerivationNode>,
    edges: Vec<rio_proto::dag::DerivationEdge>,
    keep_going: bool,
) -> anyhow::Result<broadcast::Receiver<rio_proto::types::BuildEvent>> {
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            req: MergeDagRequest {
                build_id,
                tenant_id: None,
                priority_class: PriorityClass::Scheduled,
                nodes,
                edges,
                options: BuildOptions::default(),
                keep_going,
                traceparent: String::new(),
                jti: None,
                jwt_token: None,
            },
            reply: reply_tx,
        })
        .await?;
    Ok(reply_rx.await??)
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
            reply: tx,
        })
        .await?;
    Ok(rx.await?)
}

/// Receive the next SchedulerMessage and unwrap it as a WorkAssignment.
/// Panics on timeout (2s), channel close, or wrong message variant.
///
/// Extracts the common `match msg.msg { Some(Msg::Assignment(a)) => a, _ => panic! }`
/// pattern repeated across coverage/wiring/grpc tests.
pub(crate) async fn recv_assignment(
    rx: &mut mpsc::Receiver<rio_proto::types::SchedulerMessage>,
) -> rio_proto::types::WorkAssignment {
    // Skip PrefetchHint messages: send_prefetch_hint fires before each
    // Assignment when approx_input_closure is non-empty. Since
    // approx_input_closure now reads realized output_paths (not just
    // expected_output_paths), any dispatch following a completed
    // dependency may be preceded by a hint. Tests asserting on
    // assignment ORDER don't care about the hint (it's a cache-warming
    // side-channel); drain until the actual Assignment arrives.
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("recv_assignment: timeout waiting for SchedulerMessage")
            .expect("recv_assignment: channel closed");
        match msg.msg {
            Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => return a,
            Some(rio_proto::types::scheduler_message::Msg::Prefetch(_)) => continue,
            other => panic!("recv_assignment: expected Assignment, got {other:?}"),
        }
    }
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
            result: rio_proto::build_types::BuildResult {
                status: rio_proto::build_types::BuildResultStatus::Built.into(),
                built_outputs: vec![rio_proto::types::BuiltOutput {
                    output_name: "out".into(),
                    output_path: output_path.into(),
                    output_hash: vec![0u8; 32],
                }],
                ..Default::default()
            },
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
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
            result: rio_proto::build_types::BuildResult {
                status: rio_proto::build_types::BuildResultStatus::Built.into(),
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
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
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
            result: rio_proto::build_types::BuildResult {
                status: rio_proto::build_types::BuildResultStatus::Built.into(),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        })
        .await?;
    Ok(())
}

/// Send a failed completion with the given status and error message.
pub(crate) async fn complete_failure(
    handle: &ActorHandle,
    executor_id: &str,
    drv_key: &str,
    status: rio_proto::build_types::BuildResultStatus,
    error_msg: &str,
) -> anyhow::Result<()> {
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            executor_id: executor_id.into(),
            drv_key: drv_key.into(),
            result: rio_proto::build_types::BuildResult {
                status: status.into(),
                error_msg: error_msg.into(),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        })
        .await?;
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
    let _ = handle.debug_query_workers().await;
}

// Re-export for test modules. Canonical impl moved to rio-test-support
// (P0330) — was 3× copied with method-name drift before that.
pub(crate) use rio_test_support::metrics::CountingRecorder;

#[tokio::test]
async fn test_actor_starts_and_stops() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, task) = setup_actor(db.pool.clone());
    // Query should succeed (actor is running). Also acts as a barrier.
    let workers = handle.debug_query_workers().await?;
    assert!(workers.is_empty());
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
