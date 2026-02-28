//! Shared test helpers: actor setup, fixture builders, settle/timing.

use super::*;
use tokio::sync::mpsc;

pub(super) use rio_test_support::TestDb;
pub(super) use std::time::Duration;

pub(super) static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

/// Set up an actor with the given PgPool and return (handle, task).
/// The caller should drop the handle to shut down the actor.
pub(crate) async fn setup_actor(pool: sqlx::PgPool) -> (ActorHandle, tokio::task::JoinHandle<()>) {
    setup_actor_with_store(pool, None).await
}

/// Set up an actor with an optional store client for cache-check tests.
pub(crate) async fn setup_actor_with_store(
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
            build_id,
            tenant_id: None,
            priority_class,
            nodes: vec![make_test_node(hash, path, "x86_64-linux")],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            reply: reply_tx,
        })
        .await
        .unwrap();
    reply_rx.await.unwrap().unwrap()
}

/// Give the actor time to process commands.
pub(crate) async fn settle() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[tokio::test]
async fn test_actor_starts_and_stops() {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, task) = setup_actor(db.pool.clone()).await;
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
    let (handle, task) = setup_actor(db.pool.clone()).await;
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
