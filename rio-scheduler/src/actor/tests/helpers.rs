//! Shared test helpers: actor setup, fixture builders, synchronization barrier.

use super::*;
use tokio::sync::mpsc;

/// Per-stream-epoch source for tests. Mirrors the production
/// `STREAM_EPOCH_SEQ` in `grpc/executor_service.rs`. Process-global so
/// every `connect_executor*` (and inline `ExecutorConnected{..}`
/// construction) draws from the same monotonic sequence regardless of
/// which test or helper allocated it.
static STREAM_EPOCH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Last epoch allocated per executor_id. `disconnect()` and inline
/// `ExecutorDisconnected{..}` sites read this so the epoch matches the
/// connect's. Tests run in parallel but use distinct executor names,
/// so per-key races are not a concern.
static STREAM_EPOCHS: std::sync::LazyLock<dashmap::DashMap<String, u64>> =
    std::sync::LazyLock::new(dashmap::DashMap::new);

/// Throwaway `ExecutorConnected.reply` for test sites that don't care
/// about the accept/reject result. The receiver is dropped immediately;
/// the actor's `let _ = reply.send(..)` ignores send failure.
pub(crate) fn noop_connect_reply() -> oneshot::Sender<Result<(), &'static str>> {
    oneshot::channel().0
}

/// Allocate a fresh stream epoch for `executor_id` and record it.
/// Call at every `ExecutorConnected{..}` construction site (helper or
/// inline).
pub(crate) fn next_stream_epoch_for(executor_id: &str) -> u64 {
    let epoch = STREAM_EPOCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    STREAM_EPOCHS.insert(executor_id.to_string(), epoch);
    epoch
}

/// Current recorded epoch for `executor_id`. Call at every
/// `ExecutorDisconnected{..}` construction site. Panics if no connect
/// preceded — that's a test bug (production never sends disconnect
/// without a prior connect from the same reader task).
pub(crate) fn stream_epoch_for(executor_id: &str) -> u64 {
    STREAM_EPOCHS
        .get(executor_id)
        .map(|e| *e)
        .unwrap_or_else(|| panic!("stream_epoch_for({executor_id:?}): no prior connect recorded"))
}

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
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (actor, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), configure);

    let executor_id = format!("w-{key}");
    let rx = connect_executor(&actor, &executor_id, "x86_64-linux").await?;

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

/// Set up an actor with a configurator closure that mutates
/// `DagActorConfig`/`DagActorPlumbing` before spawn. For tests that
/// need custom `retry_policy`, `event_persist_tx`,
/// `leader`, etc.
pub(crate) fn setup_actor_configured(
    pool: sqlx::PgPool,
    store_client: Option<StoreServiceClient<Channel>>,
    configure: impl FnOnce(&mut DagActorConfig, &mut DagActorPlumbing),
) -> (ActorHandle, tokio::task::JoinHandle<()>) {
    let db = SchedulerDb::new(pool);
    let (tx, rx) = mpsc::channel(ACTOR_CHANNEL_CAPACITY);
    let mut cfg = DagActorConfig::default();
    let mut plumbing = DagActorPlumbing {
        store_client,
        ..Default::default()
    };
    configure(&mut cfg, &mut plumbing);
    let actor = DagActor::new(db, cfg, plumbing);
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
    let (handle, task) = setup_actor(db.pool.clone());
    (db, handle, task)
}

/// Bootstrap PG + actor + one fully-registered worker.
/// Returns the scheduler→worker message receiver alongside the standard triple.
pub(crate) async fn setup_with_worker(
    executor_id: &str,
    system: &str,
) -> anyhow::Result<(
    TestDb,
    ActorHandle,
    tokio::task::JoinHandle<()>,
    mpsc::Receiver<rio_proto::types::SchedulerMessage>,
)> {
    let (db, handle, task) = setup().await;
    let rx = connect_executor(&handle, executor_id, system).await?;
    Ok((db, handle, task, rx))
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
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, actor_task) = setup_actor_with_store(db.pool.clone(), Some(store_client));
    Ok((db, store, handle, (store_task, actor_task)))
}

/// Overridable fields of an `ActorCommand::Heartbeat` for
/// [`send_heartbeat_with`]. `executor_id` and `systems` are NOT here —
/// every caller passes those explicitly. `Default` is "idle builder,
/// no class, nothing running".
pub(crate) struct HeartbeatFields {
    pub store_degraded: bool,
    pub draining: bool,
    pub kind: rio_proto::types::ExecutorKind,
    pub resources: Option<rio_proto::types::ResourceUsage>,
    pub supported_features: Vec<String>,
    pub running_build: Option<String>,
    pub intent_id: Option<String>,
}

impl Default for HeartbeatFields {
    fn default() -> Self {
        Self {
            store_degraded: false,
            draining: false,
            kind: rio_proto::types::ExecutorKind::Builder,
            resources: None,
            supported_features: vec![],
            running_build: None,
            intent_id: None,
        }
    }
}

/// Send an `ActorCommand::Heartbeat` with default fields, after `f`
/// mutates the 1-2 the test cares about. THE single test-side
/// construction site for the variant — when `ActorCommand::Heartbeat`
/// grows a field, this is the only edit (not 20+ scattered across
/// test modules). No trailing `Tick` — callers that need the
/// dispatch-dirty drain compose with [`tick`] or use [`send_heartbeat`].
pub(crate) async fn send_heartbeat_with(
    handle: &ActorHandle,
    executor_id: &str,
    system: &str,
    f: impl FnOnce(&mut HeartbeatFields),
) -> anyhow::Result<()> {
    let mut hb = HeartbeatFields::default();
    f(&mut hb);
    handle
        .send_unchecked(ActorCommand::Heartbeat(HeartbeatPayload {
            executor_id: executor_id.into(),
            systems: vec![system.into()],
            store_degraded: hb.store_degraded,
            draining: hb.draining,
            kind: hb.kind,
            resources: hb.resources,
            supported_features: hb.supported_features,
            running_build: hb.running_build,
            intent_id: hb.intent_id,
        }))
        .await?;
    Ok(())
}

/// Connect a worker (stream + heartbeat) WITHOUT the automatic
/// `PrefetchComplete` ACK. For warm-gate tests that need to observe
/// the initial `PrefetchHint` arrival and/or prove dispatch blocks
/// until the ACK.
pub(crate) async fn connect_executor_no_ack(
    handle: &ActorHandle,
    executor_id: &str,
    system: &str,
) -> anyhow::Result<mpsc::Receiver<rio_proto::types::SchedulerMessage>> {
    connect_executor_no_ack_kind(
        handle,
        executor_id,
        system,
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
    kind: rio_proto::types::ExecutorKind,
) -> anyhow::Result<mpsc::Receiver<rio_proto::types::SchedulerMessage>> {
    connect_executor_with(handle, executor_id, system, false, |hb| hb.kind = kind).await
}

/// Single body backing every `connect_executor*` variant: stream
/// connect, first heartbeat (with caller-mutated [`HeartbeatFields`]),
/// and optional warm-gate `PrefetchComplete` ACK. The named wrappers
/// below pass the 1-2 fields they care about; when
/// `ActorCommand::ExecutorConnected` / `Heartbeat` grow a field, this
/// is the only edit.
pub(crate) async fn connect_executor_with(
    handle: &ActorHandle,
    executor_id: &str,
    system: &str,
    ack: bool,
    f: impl FnOnce(&mut HeartbeatFields),
) -> anyhow::Result<mpsc::Receiver<rio_proto::types::SchedulerMessage>> {
    let (stream_tx, stream_rx) = mpsc::channel(256);
    handle
        .send_unchecked(ActorCommand::ExecutorConnected {
            executor_id: executor_id.into(),
            stream_tx,
            stream_epoch: next_stream_epoch_for(executor_id),
            auth_intent: None,
            reply: noop_connect_reply(),
        })
        .await?;
    send_heartbeat_with(handle, executor_id, system, f).await?;
    if ack {
        handle
            .send_unchecked(ActorCommand::PrefetchComplete {
                executor_id: executor_id.into(),
                paths_fetched: 0,
            })
            .await?;
    }
    Ok(stream_rx)
}

/// Connect a kind-typed executor (stream + heartbeat + warm-gate ACK).
/// Callers can merge then `recv_assignment` directly.
pub(crate) async fn connect_executor_kind(
    handle: &ActorHandle,
    executor_id: &str,
    system: &str,
    kind: rio_proto::types::ExecutorKind,
) -> anyhow::Result<mpsc::Receiver<rio_proto::types::SchedulerMessage>> {
    connect_executor_with(handle, executor_id, system, true, |hb| {
        hb.kind = kind;
    })
    .await
}

/// [`connect_executor_kind`] with `kind = Builder`.
pub(crate) async fn connect_builder(
    handle: &ActorHandle,
    executor_id: &str,
    system: &str,
) -> anyhow::Result<mpsc::Receiver<rio_proto::types::SchedulerMessage>> {
    connect_executor_kind(
        handle,
        executor_id,
        system,
        rio_proto::types::ExecutorKind::Builder,
    )
    .await
}

/// Send a default heartbeat then a `Tick`. For the "extra heartbeat to
/// trigger dispatch" pattern where the test doesn't care about
/// heartbeat field values, only that dispatch runs.
///
/// I-163: Heartbeat alone now sets `dispatch_dirty` instead of
/// dispatching inline; the trailing `Tick` drains it. Callers that
/// need a raw heartbeat-without-Tick use [`send_heartbeat_with`].
pub(crate) async fn send_heartbeat(
    handle: &ActorHandle,
    executor_id: &str,
    system: &str,
) -> anyhow::Result<()> {
    send_heartbeat_with(handle, executor_id, system, |_| {}).await?;
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

/// Connect a worker (stream + heartbeat) so it becomes fully registered.
/// Returns the mpsc::Receiver for scheduler→worker messages.
pub(crate) async fn connect_executor(
    handle: &ActorHandle,
    executor_id: &str,
    system: &str,
) -> anyhow::Result<mpsc::Receiver<rio_proto::types::SchedulerMessage>> {
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
    connect_executor_with(handle, executor_id, system, true, |_| {}).await
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
                nodes: vec![make_node(tag)],
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
    Ok(reply_rx.await??.state)
}

/// Merge a multi-node DAG with default options (tenant=None,
/// priority=Scheduled, options=default). Generalization of [`merge_single_node`].
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
            since_sequence: 0,
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

/// Query workers and find one by id. Replaces the 29×
/// `debug_query_workers().await?; ...iter().find(|w| w.executor_id == id).expect(...)`.
pub(crate) async fn expect_worker(handle: &ActorHandle, id: &str) -> DebugExecutorInfo {
    handle
        .debug_query_workers()
        .await
        .expect("actor alive")
        .into_iter()
        .find(|w| w.executor_id == id)
        .unwrap_or_else(|| panic!("executor {id:?} should be registered"))
}

/// Send `ExecutorDisconnected` and barrier. Replaces the ~40×
/// open-coded `send_unchecked(ExecutorDisconnected{...}) + barrier()`.
pub(crate) async fn disconnect(handle: &ActorHandle, id: &str) -> anyhow::Result<()> {
    handle
        .send_unchecked(ActorCommand::ExecutorDisconnected {
            executor_id: id.into(),
            stream_epoch: stream_epoch_for(id),
            seen_drvs: vec![],
        })
        .await?;
    barrier(handle).await;
    Ok(())
}

/// Send `ReportExecutorTermination` and return whether the floor was
/// promoted. Simulates the controller's follow-up after observing
/// k8s Pod-status. Barriered.
pub(crate) async fn report_termination(
    handle: &ActorHandle,
    id: &str,
    reason: rio_proto::types::TerminationReason,
) -> anyhow::Result<bool> {
    let promoted = handle
        .query_unchecked(|reply| ActorCommand::ReportExecutorTermination {
            executor_id: id.into(),
            reason,
            reply,
        })
        .await?;
    barrier(handle).await;
    Ok(promoted)
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
        max_cores: 64.0,
        max_mem: 256 << 30,
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

/// `[sla]` config with 3 hw_classes + `hw_cost_source=Static` so the
/// admissible-set solve_full path is reachable. ε_h=0 so per-dispatch
/// results are deterministic (set explicitly in ε_h tests).
pub(crate) fn test_hw_sla_config() -> crate::sla::config::SlaConfig {
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
                max_cores: Some(cfg.max_cores as u32),
                max_mem: Some(cfg.max_mem),
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

/// Bare (unspawned) actor with [`test_hw_sla_config`] + populated
/// 3-class hw table + one fitted key `"test-pkg"`. For
/// admissible-set / ε_h / ICE-mask tests.
pub(crate) fn bare_actor_hw(pool: sqlx::PgPool) -> DagActor {
    let mut actor = bare_actor_cfg(
        pool,
        DagActorConfig {
            sla: test_hw_sla_config(),
            ..Default::default()
        },
    );
    // `set_price` no longer upgrades source; tests that probe per-h
    // price discrimination need a Spot-sourced table.
    *actor.cost_table.write() =
        crate::sla::cost::CostTable::seeded("", crate::sla::cost::HwCostSource::Spot);
    actor.sla_tiers = actor.sla_config.solve_tiers();
    actor.sla_ceilings = actor.sla_config.ceilings();
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

/// Bootstrap PG + spawned actor with the realistic-ceiling `[sla]`
/// config. For end-to-end `GetSpawnIntents` tests via [`ActorHandle`].
pub(crate) async fn setup_with_big_ceilings() -> (TestDb, ActorHandle, tokio::task::JoinHandle<()>)
{
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, task) =
        setup_actor_configured(db.pool.clone(), None, |c, _| c.sla = test_sla_config());
    (db, handle, task)
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
        let db = TestDb::new(&MIGRATOR).await;
        // Phase 1: first "leader" writes state.
        {
            let (handle, task) = setup_actor(db.pool.clone());
            seed(handle, db.pool.clone()).await?;
            // handle dropped at end of seed's scope (moved in); join
            // the task so PG writes are flushed.
            let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        }
        // Phase 2: fresh actor recovers.
        let (handle, task) = setup_actor_with_store(db.pool.clone(), store);
        handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
        barrier(&handle).await;
        Ok(Self {
            db,
            handle,
            _task: task,
        })
    }
}

/// Phase-1 helper for [`RecoveryFixture`]: poison `drv_hash` via a
/// PermanentFailure completion. Three recovery tests share this exact
/// 10-line sequence.
pub(crate) async fn seed_poisoned(handle: &ActorHandle, drv_hash: &str) -> anyhow::Result<()> {
    let mut rx = connect_executor(handle, "seed-poison-w", "x86_64-linux").await?;
    let _ev = merge_single_node(handle, Uuid::new_v4(), drv_hash, PriorityClass::Scheduled).await?;
    let _ = rx.recv().await.expect("assignment");
    complete_failure(
        handle,
        "seed-poison-w",
        &test_drv_path(drv_hash),
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
    let _ = handle.debug_query_workers().await;
}

/// Poll until none of `hashes` is `Substituting` — i.e. every detached
/// fetch task has posted `SubstituteComplete` and the actor handled it.
/// Bounded (10ms × 100) so a hung task fails the test instead of
/// hanging it. r[sched.substitute.detached+2]
pub(crate) async fn settle_substituting(handle: &ActorHandle, hashes: &[&str]) {
    use crate::state::DerivationStatus;
    for _ in 0..100 {
        tokio::task::yield_now().await;
        barrier(handle).await;
        let mut any = false;
        for h in hashes {
            if let Ok(Some(d)) = handle.debug_query_derivation(h).await
                && d.status == DerivationStatus::Substituting
            {
                any = true;
                break;
            }
        }
        if !any {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("settle_substituting: timed out waiting for SubstituteComplete on {hashes:?}");
}

/// Poll until `hash` reaches `want` — i.e. the actor has ENTERED the
/// target status. Inverse of [`settle_substituting`] (which waits for
/// exit). Same 10ms × 100 bound. For tests that need to observe a node
/// IN a transient status (e.g. `Substituting`) before flipping a knob.
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
