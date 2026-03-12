//! End-to-end tests for rio-push upload functions.
//!
//! Spins up an ephemeral postgres + real `StoreServiceImpl` (same pattern
//! as `rio-store/tests/grpc/main.rs`), then exercises `find_missing` and
//! `push_path` against the live server.

use tonic::transport::{Channel, Server};

use rio_proto::StoreServiceClient;
use rio_proto::StoreServiceServer;
use rio_proto::types::{GetPathRequest, get_path_response};
use rio_proto::validated::ValidatedPathInfo;
use rio_push::upload::{find_missing, push_path};
use rio_store::grpc::StoreServiceImpl;
use rio_test_support::fixtures::{make_nar, make_path_info_for_nar, test_store_path};
use rio_test_support::{TestDb, TestResult};

// sqlx::migrate! resolves relative to CARGO_MANIFEST_DIR (rio-push/).
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

/// Test harness: ephemeral PG + in-process store server + connected client.
/// Drop aborts the server and drops the DB.
struct StoreSession {
    #[allow(dead_code)]
    db: TestDb,
    client: StoreServiceClient<Channel>,
    server: tokio::task::JoinHandle<()>,
}

impl StoreSession {
    async fn new() -> anyhow::Result<Self> {
        let db = TestDb::new(&MIGRATOR).await;
        let service = StoreServiceImpl::new(db.pool.clone());
        let router = Server::builder().add_service(StoreServiceServer::new(service));
        let (addr, server) = rio_test_support::grpc::spawn_grpc_server(router).await;
        let channel = Channel::from_shared(format!("http://{addr}"))?
            .connect()
            .await?;
        let client = StoreServiceClient::new(channel);
        Ok(Self { db, client, server })
    }
}

impl Drop for StoreSession {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// Drain a GetPath response stream, returning (PathInfo, nar_bytes).
async fn get_path_nar(
    client: &mut StoreServiceClient<Channel>,
    store_path: &str,
) -> anyhow::Result<(rio_proto::types::PathInfo, Vec<u8>)> {
    let mut stream = client
        .get_path(GetPathRequest {
            store_path: store_path.to_string(),
        })
        .await?
        .into_inner();

    let mut info = None;
    let mut nar = Vec::new();
    while let Some(msg) = stream.message().await? {
        match msg.msg {
            Some(get_path_response::Msg::Info(i)) => info = Some(i),
            Some(get_path_response::Msg::NarChunk(chunk)) => nar.extend_from_slice(&chunk),
            None => {}
        }
    }

    let info = info.ok_or_else(|| anyhow::anyhow!("GetPath stream had no PathInfo"))?;
    Ok((info, nar))
}

/// Build a ValidatedPathInfo with a distinct store path name.
fn make_test_path(name: &str, contents: &[u8]) -> (ValidatedPathInfo, Vec<u8>, String) {
    let (nar, _hash) = make_nar(contents);
    let store_path = test_store_path(name);
    let info = make_path_info_for_nar(&store_path, &nar);
    (info, nar, store_path)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Push a path, pull it back via GetPath, verify NAR bytes and metadata match.
#[tokio::test]
async fn push_then_pull_roundtrip() -> TestResult {
    let mut s = StoreSession::new().await?;

    let (info, nar, store_path) = make_test_path("push-roundtrip", b"hello world");

    let created = push_path(&mut s.client, info.clone(), nar.clone(), None).await?;
    assert!(created, "first push should create");

    let (got_info, got_nar) = get_path_nar(&mut s.client, &store_path).await?;
    assert_eq!(got_nar, nar, "NAR bytes should roundtrip exactly");
    assert_eq!(got_info.nar_hash, info.nar_hash);
    assert_eq!(got_info.nar_size, info.nar_size);
    assert_eq!(got_info.store_path, store_path);

    Ok(())
}

/// Pushing the same path twice: first returns created=true, second created=false.
#[tokio::test]
async fn push_idempotent() -> TestResult {
    let mut s = StoreSession::new().await?;

    let (info, nar, store_path) = make_test_path("push-idempotent", b"idempotent content");

    let created1 = push_path(&mut s.client, info.clone(), nar.clone(), None).await?;
    assert!(created1, "first push should create");

    let created2 = push_path(&mut s.client, info, nar.clone(), None).await?;
    assert!(!created2, "second push should return created=false");

    // Data should still be correct.
    let (_got_info, got_nar) = get_path_nar(&mut s.client, &store_path).await?;
    assert_eq!(got_nar, nar);

    Ok(())
}

/// find_missing with one present and one absent path returns only the absent.
#[tokio::test]
async fn find_missing_filters_present_paths() -> TestResult {
    let mut s = StoreSession::new().await?;

    let (info_a, nar_a, path_a) = make_test_path("fm-present", b"present path");
    push_path(&mut s.client, info_a, nar_a, None).await?;

    let path_b = test_store_path("fm-absent");

    let missing = find_missing(&mut s.client, vec![path_a.clone(), path_b.clone()]).await?;
    assert_eq!(missing, vec![path_b], "only absent path should be missing");

    Ok(())
}

/// find_missing on an empty store returns all paths.
#[tokio::test]
async fn find_missing_all_missing() -> TestResult {
    let mut s = StoreSession::new().await?;

    let paths: Vec<String> = (0..3)
        .map(|i| test_store_path(&format!("all-missing-{i}")))
        .collect();

    let missing = find_missing(&mut s.client, paths.clone()).await?;
    assert_eq!(missing, paths, "all paths should be missing on empty store");

    Ok(())
}

/// push_path rejects a NAR whose hash doesn't match the declared nar_hash
/// (client-side check, before any server interaction).
#[tokio::test]
async fn push_hash_mismatch_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;

    // Build NAR from "good" content but PathInfo from "bad" content.
    let (good_nar, _hash) = make_nar(b"good content");
    let store_path = test_store_path("hash-mismatch");
    let bad_info = make_path_info_for_nar(&store_path, &make_nar(b"bad content").0);

    let result = push_path(&mut s.client, bad_info, good_nar, None).await;
    assert!(result.is_err(), "hash mismatch should be rejected");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("NAR hash mismatch"),
        "error should mention hash mismatch: {err_msg}"
    );

    // Path should not exist in the store.
    let get_result = s
        .client
        .get_path(GetPathRequest {
            store_path: store_path.clone(),
        })
        .await;
    assert!(
        get_result.is_err(),
        "path should not exist after rejected push"
    );

    Ok(())
}

/// After pushing a path, find_missing for that path returns empty.
#[tokio::test]
async fn push_then_find_missing_empty() -> TestResult {
    let mut s = StoreSession::new().await?;

    let (info, nar, path) = make_test_path("push-then-fm", b"present now");
    push_path(&mut s.client, info, nar, None).await?;

    let missing = find_missing(&mut s.client, vec![path]).await?;
    assert!(missing.is_empty(), "pushed path should not be missing");

    Ok(())
}

/// Push paths with dependency references, verify roundtrip including references.
#[tokio::test]
async fn push_with_references() -> TestResult {
    let mut s = StoreSession::new().await?;

    // Create dep path.
    let (dep_nar, _) = make_nar(b"dependency content");
    let dep_path = test_store_path("ref-dep");
    let dep_info = make_path_info_for_nar(&dep_path, &dep_nar);
    push_path(&mut s.client, dep_info, dep_nar.clone(), None).await?;

    // Create main path that references dep.
    let (main_nar, _) = make_nar(b"main content with ref");
    let main_path = test_store_path("ref-main");
    let mut main_info = make_path_info_for_nar(&main_path, &main_nar);
    main_info.references =
        vec![rio_nix::store_path::StorePath::parse(&dep_path).expect("dep_path is valid")];
    push_path(&mut s.client, main_info, main_nar.clone(), None).await?;

    // Verify main path's references survived roundtrip.
    let (got_info, got_nar) = get_path_nar(&mut s.client, &main_path).await?;
    assert_eq!(got_nar, main_nar);
    assert_eq!(got_info.references, vec![dep_path.clone()]);

    // Verify dep path also roundtrips.
    let (_dep_got_info, dep_got_nar) = get_path_nar(&mut s.client, &dep_path).await?;
    assert_eq!(dep_got_nar, dep_nar);

    Ok(())
}

/// push_path propagates errors when the server is unreachable.
#[tokio::test]
async fn push_server_failure_propagates() -> TestResult {
    let mut s = StoreSession::new().await?;

    // Kill the server before pushing.
    s.server.abort();
    // Wait for the abort to take effect.
    let _ = (&mut s.server).await;

    let (info, nar, _path) = make_test_path("server-dead", b"will fail");
    let result = push_path(&mut s.client, info, nar, None).await;
    assert!(result.is_err(), "push to dead server should fail");

    Ok(())
}
