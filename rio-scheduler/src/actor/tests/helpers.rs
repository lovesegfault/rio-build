//! Shared test helpers: actor setup, fixture builders, settle/timing.

use super::*;
use tokio::sync::mpsc;

pub(super) use rio_test_support::TestDb;
pub(super) use std::time::Duration;

pub(super) static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

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
    let db = SchedulerDb::new(pool);
    let (tx, rx) = mpsc::channel(ACTOR_CHANNEL_CAPACITY);
    let actor = DagActor::new(db, store_client);
    let backpressure = actor.backpressure_flag();
    let self_tx = tx.downgrade();
    let task = tokio::spawn(actor.run_with_self_tx(rx, self_tx));
    (ActorHandle { tx, backpressure }, task)
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
) -> (
    TestDb,
    ActorHandle,
    tokio::task::JoinHandle<()>,
    mpsc::Receiver<rio_proto::types::SchedulerMessage>,
) {
    let (db, handle, task) = setup().await;
    let rx = connect_worker(&handle, worker_id, system, max_builds).await;
    (db, handle, task, rx)
}

/// Create a minimal test DerivationNode.
pub(crate) fn make_test_node(
    hash: &str,
    path: &str,
    system: &str,
) -> rio_proto::types::DerivationNode {
    rio_proto::types::DerivationNode {
        drv_hash: hash.into(),
        drv_path: path.into(),
        pname: "test-pkg".into(),
        system: system.into(),
        required_features: vec![],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        expected_output_paths: vec![],
    }
}

/// Create a minimal test DerivationEdge.
pub(crate) fn make_test_edge(parent: &str, child: &str) -> rio_proto::types::DerivationEdge {
    rio_proto::types::DerivationEdge {
        parent_drv_path: parent.into(),
        child_drv_path: child.into(),
    }
}

/// Connect a worker (stream + heartbeat) so it becomes fully registered.
/// Returns the mpsc::Receiver for scheduler→worker messages.
pub(crate) async fn connect_worker(
    handle: &ActorHandle,
    worker_id: &str,
    system: &str,
    max_builds: u32,
) -> mpsc::Receiver<rio_proto::types::SchedulerMessage> {
    let (stream_tx, stream_rx) = mpsc::channel(256);
    handle
        .send_unchecked(ActorCommand::WorkerConnected {
            worker_id: worker_id.into(),
            stream_tx,
        })
        .await
        .unwrap();
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            worker_id: worker_id.into(),
            system: system.into(),
            supported_features: vec![],
            max_builds,
            running_builds: vec![],
        })
        .await
        .unwrap();
    stream_rx
}

/// Merge a single-node DAG and return the event receiver.
pub(crate) async fn merge_single_node(
    handle: &ActorHandle,
    build_id: Uuid,
    hash: &str,
    path: &str,
    priority_class: PriorityClass,
) -> broadcast::Receiver<rio_proto::types::BuildEvent> {
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            req: MergeDagRequest {
                build_id,
                tenant_id: None,
                priority_class,
                nodes: vec![make_test_node(hash, path, "x86_64-linux")],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: false,
            },
            reply: reply_tx,
        })
        .await
        .unwrap();
    reply_rx.await.unwrap().unwrap()
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
) -> broadcast::Receiver<rio_proto::types::BuildEvent> {
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
            },
            reply: reply_tx,
        })
        .await
        .unwrap();
    reply_rx.await.unwrap().unwrap()
}

/// Query build status (unwraps the Result; panics on BuildNotFound).
pub(crate) async fn query_status(
    handle: &ActorHandle,
    build_id: Uuid,
) -> rio_proto::types::BuildStatus {
    let (tx, rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().unwrap()
}

/// Query build status, returning the raw Result (for tests that expect
/// BuildNotFound).
pub(crate) async fn try_query_status(
    handle: &ActorHandle,
    build_id: Uuid,
) -> Result<rio_proto::types::BuildStatus, ActorError> {
    let (tx, rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::QueryBuildStatus {
            build_id,
            reply: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap()
}

/// Send a successful completion (Built) with a single `out` output.
/// Uses a placeholder output_hash; override via inline construction if
/// the test asserts on hash contents.
pub(crate) async fn complete_success(
    handle: &ActorHandle,
    worker_id: &str,
    drv_key: &str,
    output_path: &str,
) {
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
        })
        .await
        .unwrap();
}

/// Send a successful completion (Built) with NO built_outputs.
/// Many tests don't care about output paths and just need the state transition.
pub(crate) async fn complete_success_empty(handle: &ActorHandle, worker_id: &str, drv_key: &str) {
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: worker_id.into(),
            drv_key: drv_key.into(),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();
}

/// Send a failed completion with the given status and error message.
pub(crate) async fn complete_failure(
    handle: &ActorHandle,
    worker_id: &str,
    drv_key: &str,
    status: rio_proto::types::BuildResultStatus,
    error_msg: &str,
) {
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: worker_id.into(),
            drv_key: drv_key.into(),
            result: rio_proto::types::BuildResult {
                status: status.into(),
                error_msg: error_msg.into(),
                ..Default::default()
            },
        })
        .await
        .unwrap();
}

/// Give the actor time to process commands.
pub(crate) async fn settle() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[tokio::test]
async fn test_actor_starts_and_stops() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, task) = setup_actor(db.pool.clone());
    settle().await;
    // Query should succeed (actor is running)
    let workers = handle.debug_query_workers().await.unwrap();
    assert!(workers.is_empty());
    // Drop handle to close channel
    drop(handle);
    // Actor task should exit
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("actor should shut down")
        .expect("actor should not panic");
}

/// is_alive() should detect actor death (channel closed = receiver dropped).
#[tokio::test]
async fn test_actor_is_alive_detection() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, task) = setup_actor(db.pool.clone());
    settle().await;

    // Actor should be alive after spawn.
    assert!(handle.is_alive(), "actor should be alive after spawn");

    // Abort the actor task to simulate a panic/crash.
    task.abort();
    // Give the abort time to propagate and the receiver to drop.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // is_alive() should now report false (channel closed).
    assert!(
        !handle.is_alive(),
        "is_alive should report false after actor task dies"
    );
}
