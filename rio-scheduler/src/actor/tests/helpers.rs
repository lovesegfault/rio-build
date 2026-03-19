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
    let self_tx = tx.downgrade();
    let task = tokio::spawn(actor.run_with_self_tx(rx, self_tx));
    (
        ActorHandle {
            tx,
            backpressure,
            generation,
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
    worker_id: &str,
    system: &str,
    max_builds: u32,
) -> anyhow::Result<(
    TestDb,
    ActorHandle,
    tokio::task::JoinHandle<()>,
    mpsc::Receiver<rio_proto::types::SchedulerMessage>,
)> {
    let (db, handle, task) = setup().await;
    let rx = connect_worker(&handle, worker_id, system, max_builds).await?;
    Ok((db, handle, task, rx))
}

/// Connect a worker (stream + heartbeat) so it becomes fully registered.
/// Returns the mpsc::Receiver for scheduler→worker messages.
pub(crate) async fn connect_worker(
    handle: &ActorHandle,
    worker_id: &str,
    system: &str,
    max_builds: u32,
) -> anyhow::Result<mpsc::Receiver<rio_proto::types::SchedulerMessage>> {
    let (stream_tx, stream_rx) = mpsc::channel(256);
    handle
        .send_unchecked(ActorCommand::WorkerConnected {
            worker_id: worker_id.into(),
            stream_tx,
        })
        .await?;
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            resources: None,
            bloom: None,
            size_class: None,
            worker_id: worker_id.into(),
            systems: vec![system.into()],
            supported_features: vec![],
            max_builds,
            running_builds: vec![],
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
    let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("recv_assignment: timeout waiting for SchedulerMessage")
        .expect("recv_assignment: channel closed");
    match msg.msg {
        Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => a,
        other => panic!("recv_assignment: expected Assignment, got {other:?}"),
    }
}

/// Send a successful completion (Built) with a single `out` output.
/// Uses a placeholder output_hash; override via inline construction if
/// the test asserts on hash contents.
pub(crate) async fn complete_success(
    handle: &ActorHandle,
    worker_id: &str,
    drv_key: &str,
    output_path: &str,
) -> anyhow::Result<()> {
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: worker_id.into(),
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
    worker_id: &str,
    drv_key: &str,
) -> anyhow::Result<()> {
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: worker_id.into(),
            drv_key: drv_key.into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
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
    worker_id: &str,
    drv_key: &str,
    status: rio_proto::types::BuildResultStatus,
    error_msg: &str,
) -> anyhow::Result<()> {
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: worker_id.into(),
            drv_key: drv_key.into(),
            result: rio_proto::types::BuildResult {
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

// ===========================================================================
// Metric-increment capture (CountingRecorder)
// ===========================================================================

/// Recorder that captures counter increments into a shared map keyed by
/// `name{sorted,labels}`. Used for metric-delta assertions.
///
/// Unlike `with_local_recorder` (sync closure only — fine for the gateway's
/// `handle_session_error`), actor tests need the recorder visible to the
/// *spawned actor task* across `.await` points. Use
/// `metrics::set_default_local_recorder(&recorder)`, which holds the
/// thread-local for the guard's lifetime. `#[tokio::test]` uses a
/// current-thread runtime, so the spawned actor runs on the same OS thread
/// and sees the thread-local when it calls `counter!()`.
///
/// Mirrors `rio-gateway/tests/ssh_hardening.rs` — a follow-up could lift
/// both into `rio-test-support`.
#[derive(Default)]
pub(crate) struct CountingRecorder {
    // `metrics` provides `impl CounterFn for AtomicU64` (atomics.rs), so
    // `Counter::from_arc(Arc<AtomicU64>)` is a valid counter handle.
    counters: std::sync::Mutex<HashMap<String, Arc<AtomicU64>>>,
    // Gauge touch-set: names only, no values. `gauge!(name).set()`
    // expands to `recorder.register_gauge(key, _).set(v)` — the
    // register call fires on EVERY `gauge!()` invocation, so tracking
    // the key here captures "gauge was touched" regardless of value.
    // Used for absence-checks (leader-gate: standby must NOT set).
    gauges: std::sync::Mutex<HashSet<String>>,
}

impl CountingRecorder {
    fn counter_key(key: &metrics::Key) -> String {
        let mut labels: Vec<_> = key
            .labels()
            .map(|l| format!("{}={}", l.key(), l.value()))
            .collect();
        labels.sort();
        format!("{}{{{}}}", key.name(), labels.join(","))
    }

    /// Returns the current value for `rendered_key` (as produced by
    /// [`counter_key`]), or 0 if never incremented. A counter with no
    /// labels has key `"name{}"`.
    pub(crate) fn get(&self, rendered_key: &str) -> u64 {
        self.counters
            .lock()
            .unwrap()
            .get(rendered_key)
            .map(|a| a.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// All counter keys seen so far. For assertion-failure diagnostics:
    /// if the expected key is absent, seeing the ACTUAL keys pinpoints
    /// a wrong-name regression ("_sent_total" vs "_signals_total").
    pub(crate) fn all_keys(&self) -> Vec<String> {
        let mut keys: Vec<_> = self.counters.lock().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    }

    /// True if any `gauge!()` invocation has been observed for `name`
    /// (unlabeled name only — sufficient for the handle_tick gauges,
    /// which carry no labels).
    pub(crate) fn gauge_touched(&self, name: &str) -> bool {
        self.gauges.lock().unwrap().contains(name)
    }

    /// All gauge names seen so far (sorted). For assertion-failure
    /// diagnostics: when an absence-check fails, this shows what DID
    /// get touched.
    pub(crate) fn gauge_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.gauges.lock().unwrap().iter().cloned().collect();
        names.sort();
        names
    }
}

impl metrics::Recorder for CountingRecorder {
    fn describe_counter(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn describe_gauge(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn describe_histogram(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }

    fn register_counter(&self, key: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Counter {
        let rendered = Self::counter_key(key);
        let atomic = self
            .counters
            .lock()
            .unwrap()
            .entry(rendered)
            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .clone();
        metrics::Counter::from_arc(atomic)
    }
    fn register_gauge(&self, key: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Gauge {
        self.gauges.lock().unwrap().insert(key.name().to_string());
        metrics::Gauge::noop()
    }
    fn register_histogram(
        &self,
        _: &metrics::Key,
        _: &metrics::Metadata<'_>,
    ) -> metrics::Histogram {
        metrics::Histogram::noop()
    }
}

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
