//! Transitive input-closure walk tests (P0588).

use rio_test_support::TestDb;
use sha2::Digest as _;

use crate::db::SchedulerDb;

/// Insert a `narinfo` row with the given references.
async fn put_narinfo(pool: &sqlx::PgPool, path: &str, refs: &[&str]) {
    let h = sha2::Sha256::digest(path.as_bytes()).to_vec();
    let refs: Vec<String> = refs.iter().map(|s| s.to_string()).collect();
    sqlx::query(
        "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size, \"references\") \
         VALUES ($1, $2, $1, 0, $3)",
    )
    .bind(&h)
    .bind(path)
    .bind(&refs)
    .execute(pool)
    .await
    .unwrap();
    // nar_index FK-references manifests.
    sqlx::query("INSERT INTO manifests (store_path_hash, status) VALUES ($1, 'complete')")
        .bind(&h)
        .execute(pool)
        .await
        .unwrap();
}

/// Insert a `nar_index` row with an opaque `root_node` blob.
async fn put_root_node(pool: &sqlx::PgPool, path: &str, root_node: &[u8]) {
    let h = sha2::Sha256::digest(path.as_bytes()).to_vec();
    sqlx::query(
        "INSERT INTO nar_index (store_path_hash, entries, root_node) VALUES ($1, ''::bytea, $2)",
    )
    .bind(&h)
    .bind(root_node)
    .execute(pool)
    .await
    .unwrap();
}

/// Three-level closure root → middle → leaf, each with a `root_node`.
// r[verify sched.dispatch.input-roots+2]
#[tokio::test]
async fn closure_walk_resolves_root_nodes() {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let root = "/nix/store/cccc-root";
    let middle = "/nix/store/bbbb-middle";
    let leaf = "/nix/store/aaaa-leaf";
    put_narinfo(&test_db.pool, root, &[middle]).await;
    put_narinfo(&test_db.pool, middle, &[leaf]).await;
    put_narinfo(&test_db.pool, leaf, &[]).await;
    put_root_node(&test_db.pool, root, b"R").await;
    put_root_node(&test_db.pool, middle, b"M").await;
    put_root_node(&test_db.pool, leaf, b"L").await;

    let rows = db.compute_input_roots(&[root.to_string()]).await.unwrap();
    let got: Vec<(&str, Option<&[u8]>)> = rows
        .iter()
        .map(|r| (r.store_path.as_str(), r.root_node.as_deref()))
        .collect();
    assert_eq!(
        got,
        vec![
            (leaf, Some(b"L".as_slice())),
            (middle, Some(b"M".as_slice())),
            (root, Some(b"R".as_slice())),
        ],
        "sorted closure with root_node resolved"
    );
}

/// Missing `nar_index` row (indexer hasn't caught up): the path stays
/// in the closure with `root_node = None`. A seed not in `narinfo` at
/// all (still uploading) also survives.
// r[verify sched.dispatch.input-roots+2]
#[tokio::test]
async fn closure_walk_tolerates_unindexed_and_unknown_paths() {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let known = "/nix/store/bbbb-known"; // narinfo, no nar_index
    let unknown = "/nix/store/aaaa-unknown"; // not in narinfo at all
    put_narinfo(&test_db.pool, known, &[]).await;

    let rows = db
        .compute_input_roots(&[known.to_string(), unknown.to_string()])
        .await
        .unwrap();
    let got: Vec<(&str, bool)> = rows
        .iter()
        .map(|r| (r.store_path.as_str(), r.root_node.is_some()))
        .collect();
    assert_eq!(
        got,
        vec![(unknown, false), (known, false)],
        "unindexed/unknown paths survive with root_node=None"
    );
}

/// Cyclic references (Nix self-ref outputs) terminate.
// r[verify sched.dispatch.input-roots+2]
#[tokio::test]
async fn closure_walk_terminates_on_cycle() {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let a = "/nix/store/aaaa-a";
    let b = "/nix/store/bbbb-b";
    put_narinfo(&test_db.pool, a, &[a, b]).await; // self-ref + cycle
    put_narinfo(&test_db.pool, b, &[a]).await;

    let rows = db.compute_input_roots(&[a.to_string()]).await.unwrap();
    assert_eq!(rows.len(), 2);
}
