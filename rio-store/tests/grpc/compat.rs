//! Stock-Nix binary-cache compat writer (ADR-022 §10 / P0566).
//!
//! End-to-end through the gRPC surface: PutPath/PutPathBatch a path
//! with compat enabled and assert the object pair a plain `nix` client
//! would read — `{hash-part}.narinfo`, the `nar/…` object it points
//! at, and the `nix-cache-info` bootstrap marker — plus the
//! `narinfo.compat_file_hash` bookkeeping column. The full
//! stock-`nix substitute` round-trip is the P0580 VM test; these tests
//! pin the object *contents* so that VM test has something correct to
//! find.

use super::*;

use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt as _;

use rio_nix::narinfo::NarInfo;
use rio_nix::store_path::{StorePath, nixbase32};
use rio_test_support::fixtures::TEST_HASH;

/// `sha256:<nixbase32>` spelling of a raw 32-byte digest — the format
/// narinfo `NarHash:`/`FileHash:` fields carry.
fn sha256_b32(digest: &[u8; 32]) -> String {
    format!("sha256:{}", nixbase32::encode(digest))
}

/// Decode a zstd blob with the same async-compression backend the
/// production substituter decoder uses.
async fn zstd_decode(blob: &[u8]) -> Vec<u8> {
    let mut decoder = async_compression::tokio::bufread::ZstdDecoder::new(blob);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .await
        .expect("published nar/ object must be a valid zstd frame");
    out
}

// r[verify store.compat.nar-on-put]
// r[verify store.compat.narinfo-on-put]
// r[verify store.compat.write-after-commit]
/// The full happy path: PutPath a multi-chunk path with compat ON →
/// the bucket holds a parseable narinfo whose URL/FileHash/FileSize
/// describe the published zstd NAR, the NAR decompresses back to the
/// uploaded bytes (NarHash matches), `nix-cache-info` exists, and
/// `narinfo.compat_file_hash` records the compressed digest.
#[tokio::test]
async fn compat_publishes_stock_nix_object_pair() -> TestResult {
    let (mut s, backend) = StoreSession::new_with_compat().await?;

    // ~600 KiB payload → well past CHUNK_MAX, so the rio-native side
    // stores several chunks while the compat side publishes ONE whole
    // NAR object.
    let (nar, mut info, store_path) = make_large_nar(3, 600 * 1024);
    info.references = vec![
        StorePath::parse(&test_store_path("compat-dep-a"))?,
        StorePath::parse(&test_store_path("compat-dep-b"))?,
    ];
    let created = put_path(&mut s.client, info.clone(), nar.clone()).await?;
    assert!(created, "fresh path must be created");

    // ── narinfo object ──────────────────────────────────────────────
    let hash_part = StorePath::parse(&store_path)?.hash_part();
    let narinfo_key = format!("{hash_part}.narinfo");
    let narinfo_blob = backend
        .get_blob(&narinfo_key)
        .await?
        .unwrap_or_else(|| panic!("compat ON must publish {narinfo_key}"));
    let parsed = NarInfo::parse(std::str::from_utf8(&narinfo_blob)?)?;

    assert_eq!(parsed.store_path, store_path);
    assert_eq!(parsed.compression, "zstd");
    let nar_hash: [u8; 32] = Sha256::digest(&nar).into();
    assert_eq!(parsed.nar_hash, sha256_b32(&nar_hash));
    assert_eq!(parsed.nar_size, nar.len() as u64);
    // References are basenames in the text format.
    assert_eq!(
        parsed.references,
        vec![
            format!("{TEST_HASH}-compat-dep-a"),
            format!("{TEST_HASH}-compat-dep-b"),
        ]
    );
    // The store signed the path at commit time; the compat narinfo
    // must carry that exact signature (no re-signing).
    assert_eq!(parsed.sigs.len(), 1, "cluster signature expected");
    assert!(
        parsed.sigs[0].starts_with("rio-test-1:"),
        "{:?}",
        parsed.sigs
    );

    // ── NAR object (URL ↔ FileHash ↔ bytes agreement) ───────────────
    assert!(
        parsed.url.starts_with("nar/") && parsed.url.ends_with(".nar.zst"),
        "URL must be nar/<filehash>.nar.zst, got {}",
        parsed.url
    );
    let nar_blob = backend
        .get_blob(&parsed.url)
        .await?
        .unwrap_or_else(|| panic!("narinfo URL {} must resolve in the bucket", parsed.url));
    let file_hash: [u8; 32] = Sha256::digest(&nar_blob).into();
    assert_eq!(
        parsed.file_hash.as_deref(),
        Some(sha256_b32(&file_hash).as_str()),
        "FileHash must be the digest of the compressed object"
    );
    assert_eq!(parsed.file_size, Some(nar_blob.len() as u64));
    assert_eq!(
        parsed.url,
        format!("nar/{}.nar.zst", nixbase32::encode(&file_hash)),
        "object key must embed the compressed digest"
    );
    // Decompressed object is byte-identical to the uploaded NAR.
    let decompressed = zstd_decode(&nar_blob).await;
    assert_eq!(decompressed, nar, "round-trip through the compat object");

    // ── nix-cache-info bootstrap ────────────────────────────────────
    let cache_info = backend
        .get_blob("nix-cache-info")
        .await?
        .expect("first compat write must bootstrap nix-cache-info");
    assert_eq!(
        std::str::from_utf8(&cache_info)?,
        "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n"
    );

    // ── compat_file_hash bookkeeping ────────────────────────────────
    let recorded: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT compat_file_hash FROM narinfo WHERE store_path = $1")
            .bind(&store_path)
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(
        recorded.as_deref(),
        Some(file_hash.as_slice()),
        "compat_file_hash must record the compressed-object digest"
    );

    Ok(())
}

// r[verify store.compat.runtime-toggle]
/// Compat OFF (no writer wired — exactly what main.rs does for
/// `enabled = false`): the upload succeeds and NO compat objects
/// appear; `compat_file_hash` stays NULL for the reconciler.
#[tokio::test]
async fn compat_off_writes_no_objects() -> TestResult {
    let (mut s, backend) = StoreSession::new_chunked().await?;

    let path = test_store_path("compat-off");
    let (nar, _) = make_nar(b"compat off body");
    let info = make_path_info_for_nar(&path, &nar);
    assert!(put_path(&mut s.client, info, nar).await?);

    let hash_part = StorePath::parse(&path)?.hash_part();
    assert!(
        backend
            .get_blob(&format!("{hash_part}.narinfo"))
            .await?
            .is_none(),
        "compat OFF must not publish a narinfo"
    );
    assert!(
        backend.get_blob("nix-cache-info").await?.is_none(),
        "compat OFF must not bootstrap nix-cache-info"
    );

    let recorded: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT compat_file_hash FROM narinfo WHERE store_path = $1")
            .bind(&path)
            .fetch_one(&s.db.pool)
            .await?;
    assert!(recorded.is_none(), "compat_file_hash must stay NULL");

    Ok(())
}

// r[verify store.compat.write-after-commit]
/// Failure injection: the compat blob target is broken (its blobs/
/// directory is a regular file, so every `put_blob` fails) while the
/// chunk backend is healthy. The PutPath still succeeds — the compat
/// layer must never fail the RPC — the path is servable, and
/// `compat_file_hash` stays NULL for the P0582 reconciler.
#[tokio::test]
async fn compat_failure_does_not_fail_put_path() -> TestResult {
    use rio_store::backend::FilesystemChunkBackend;

    // A blob target whose puts always fail: replace {root}/blobs with a
    // regular file so create_dir_all/File::create under it error out
    // regardless of uid.
    let tmp = tempfile::tempdir()?;
    let broken_target: Arc<dyn ChunkBackend> = Arc::new(FilesystemChunkBackend::new(tmp.path())?);
    std::fs::remove_dir_all(tmp.path().join("blobs"))?;
    std::fs::write(tmp.path().join("blobs"), b"not a directory")?;

    let backend = mem_backend();
    let cache = Arc::new(rio_store::cas::ChunkCache::new(
        Arc::clone(&backend) as Arc<dyn ChunkBackend>
    ));
    let s = StoreSession::build(|pool| {
        let compat = rio_store::compat::CompatWriter::new(
            pool.clone(),
            broken_target,
            rio_store::config::CompatCompression::Zstd,
        );
        StoreServiceImpl::new(pool)
            .with_chunk_cache(cache)
            .with_compat_writer(Arc::new(compat))
    })
    .await?;
    let mut client = s.client.clone();

    let path = test_store_path("compat-broken-target");
    let (nar, _) = make_nar(b"commit survives compat failure");
    let info = make_path_info_for_nar(&path, &nar);

    // The upload must succeed despite every compat put_blob failing.
    assert!(
        put_path(&mut client, info, nar).await?,
        "PutPath must succeed even when the compat write fails"
    );

    // Path is committed and servable.
    let qpi = client
        .query_path_info(QueryPathInfoRequest {
            store_path: path.clone(),
        })
        .await?
        .into_inner();
    assert_eq!(qpi.store_path, path);

    // Bookkeeping reflects "not yet written" so the reconciler picks
    // it up later.
    let recorded: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT compat_file_hash FROM narinfo WHERE store_path = $1")
            .bind(&path)
            .fetch_one(&s.db.pool)
            .await?;
    assert!(
        recorded.is_none(),
        "failed compat write must leave compat_file_hash NULL"
    );

    Ok(())
}

// r[verify store.compat.reconcile]
/// The reconciler backfills exactly the pending set: a path whose NAR
/// exists only as chunks (uploaded with compat OFF) gets its object
/// pair published from a chunk-store reassembly and its
/// `compat_file_hash` recorded; a path already marked written is left
/// alone; a second pass is idle.
#[tokio::test]
async fn reconciler_backfills_pending_paths() -> TestResult {
    use rio_store::compat::{CompatWriter, reconciler};

    // Compat OFF at upload time: chunks + narinfo land, no compat
    // objects, compat_file_hash stays NULL.
    let (mut s, backend) = StoreSession::new_chunked().await?;

    let path_a = format!("/nix/store/{}-reconcile-pending", "d".repeat(32));
    let path_b = format!("/nix/store/{}-reconcile-done", "g".repeat(32));
    let (nar_a, _) = make_nar(b"backfill me from chunks");
    let (nar_b, _) = make_nar(b"already published elsewhere");
    let info_a = make_path_info(&path_a, &nar_a, Sha256::digest(&nar_a).into());
    let info_b = make_path_info(&path_b, &nar_b, Sha256::digest(&nar_b).into());
    assert!(put_path(&mut s.client, info_a, nar_a.clone()).await?);
    assert!(put_path(&mut s.client, info_b, nar_b).await?);

    // Mark B as already written (as if the inline writer had covered
    // it) so the reconciler must skip it.
    sqlx::query("UPDATE narinfo SET compat_file_hash = $2 WHERE store_path = $1")
        .bind(&path_b)
        .bind([0xEEu8; 32].as_slice())
        .execute(&s.db.pool)
        .await?;

    // Reconciler components share the session's backend: the cache
    // reads the chunks PutPath stored, the writer publishes into the
    // same blob namespace the assertions read.
    let cache = Arc::new(rio_store::cas::ChunkCache::new(
        Arc::clone(&backend) as Arc<dyn ChunkBackend>
    ));
    let writer = CompatWriter::new(
        s.db.pool.clone(),
        Arc::clone(&backend) as Arc<dyn ChunkBackend>,
        rio_store::config::CompatCompression::Zstd,
    );

    let stats = reconciler::run_once(&s.db.pool, &cache, &writer).await?;
    assert_eq!(stats.backlog, 1, "only A is pending (B was marked written)");
    assert_eq!(stats.published, 1);
    assert_eq!(stats.failed, 0);

    // A's pair is now in the bucket and round-trips.
    let hash_part_a = StorePath::parse(&path_a)?.hash_part();
    let narinfo_blob = backend
        .get_blob(&format!("{hash_part_a}.narinfo"))
        .await?
        .expect("reconciler must publish A's narinfo");
    let parsed = NarInfo::parse(std::str::from_utf8(&narinfo_blob)?)?;
    assert_eq!(parsed.store_path, path_a);
    let nar_blob = backend
        .get_blob(&parsed.url)
        .await?
        .expect("A's narinfo URL must resolve");
    assert_eq!(zstd_decode(&nar_blob).await, nar_a);
    let recorded: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT compat_file_hash FROM narinfo WHERE store_path = $1")
            .bind(&path_a)
            .fetch_one(&s.db.pool)
            .await?;
    let file_hash: [u8; 32] = Sha256::digest(&nar_blob).into();
    assert_eq!(recorded.as_deref(), Some(file_hash.as_slice()));

    // B was skipped: no narinfo object was created for it.
    let hash_part_b = StorePath::parse(&path_b)?.hash_part();
    assert!(
        backend
            .get_blob(&format!("{hash_part_b}.narinfo"))
            .await?
            .is_none(),
        "already-written path must not be re-published"
    );

    // Steady state: nothing pending, second pass is idle.
    let stats = reconciler::run_once(&s.db.pool, &cache, &writer).await?;
    assert!(stats.idle(), "second pass must find nothing: {stats:?}");
    assert_eq!(stats.backlog, 0);

    Ok(())
}

// r[verify store.compat.reconcile]
/// Per-path failures don't poison the batch: a pending row whose
/// chunks are missing from the backend fails (and stays pending),
/// while a healthy pending path in the same batch is still published.
#[tokio::test]
async fn reconciler_failure_keeps_path_pending_and_loop_alive() -> TestResult {
    use rio_store::compat::{CompatWriter, reconciler};
    use rio_store::test_helpers::StoreSeed;

    let (mut s, backend) = StoreSession::new_chunked().await?;

    // Healthy pending path (real upload, chunks present).
    let good_path = format!("/nix/store/{}-reconcile-good", "h".repeat(32));
    let (good_nar, _) = make_nar(b"healthy backfill");
    let good_info = make_path_info(&good_path, &good_nar, Sha256::digest(&good_nar).into());
    assert!(put_path(&mut s.client, good_info, good_nar).await?);

    // Broken pending path: complete metadata pointing at chunks that
    // were never written to the backend → reassembly fails.
    let broken_path = format!("/nix/store/{}-reconcile-broken", "i".repeat(32));
    StoreSeed::raw_path(&broken_path)
        .with_chunk_manifest(&[([0xAB; 32], 64)])
        .with_nar_size(64)
        .seed(&s.db.pool)
        .await;

    let cache = Arc::new(rio_store::cas::ChunkCache::new(
        Arc::clone(&backend) as Arc<dyn ChunkBackend>
    ));
    let writer = CompatWriter::new(
        s.db.pool.clone(),
        Arc::clone(&backend) as Arc<dyn ChunkBackend>,
        rio_store::config::CompatCompression::Zstd,
    );

    let stats = reconciler::run_once(&s.db.pool, &cache, &writer).await?;
    assert_eq!(stats.backlog, 2);
    assert_eq!(stats.published, 1, "healthy path must still publish");
    assert_eq!(stats.failed, 1, "broken path must be counted, not fatal");

    // The healthy path is done; the broken one stays pending for a
    // later pass (NULL column, no objects).
    let good_hash: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT compat_file_hash FROM narinfo WHERE store_path = $1")
            .bind(&good_path)
            .fetch_one(&s.db.pool)
            .await?;
    assert!(good_hash.is_some());
    let broken_hash: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT compat_file_hash FROM narinfo WHERE store_path = $1")
            .bind(&broken_path)
            .fetch_one(&s.db.pool)
            .await?;
    assert!(broken_hash.is_none(), "failed path must stay pending");
    let broken_part = StorePath::parse(&broken_path)?.hash_part();
    assert!(
        backend
            .get_blob(&format!("{broken_part}.narinfo"))
            .await?
            .is_none()
    );

    Ok(())
}

// r[verify store.compat.reconcile]
/// The tick (the loop body the periodic wrapper sleeps between):
/// a batch of permanently-failing rows ends the tick after ONE pass —
/// no progress means back to the interval sleep, never an immediate
/// re-list — and the failed rows are stamped so a newer healthy path
/// arriving later is served first on the next tick instead of being
/// starved behind them.
#[tokio::test]
async fn reconciler_tick_idles_on_failures_without_starving_new_paths() -> TestResult {
    use rio_store::compat::{CompatWriter, reconciler};
    use rio_store::test_helpers::StoreSeed;

    let (mut s, backend) = StoreSession::new_chunked().await?;
    let shutdown = rio_common::signal::Token::new();

    // Two permanently-failing pending paths (chunks never written).
    let broken_a = format!("/nix/store/{}-tick-broken-a", "j".repeat(32));
    let broken_b = format!("/nix/store/{}-tick-broken-b", "k".repeat(32));
    for p in [&broken_a, &broken_b] {
        StoreSeed::raw_path(p)
            .with_chunk_manifest(&[([0xCD; 32], 32)])
            .with_nar_size(32)
            .seed(&s.db.pool)
            .await;
    }

    let cache = Arc::new(rio_store::cas::ChunkCache::new(
        Arc::clone(&backend) as Arc<dyn ChunkBackend>
    ));
    let writer = CompatWriter::new(
        s.db.pool.clone(),
        Arc::clone(&backend) as Arc<dyn ChunkBackend>,
        rio_store::config::CompatCompression::Zstd,
    );

    // Tick 1: everything fails → exactly one pass over the batch, then
    // the tick returns (the periodic wrapper would now sleep). If the
    // no-progress guard were missing this would loop forever and the
    // test would hang.
    let tick = reconciler::run_tick(&s.db.pool, &cache, &writer, &shutdown).await;
    assert_eq!(tick.published, 0);
    assert_eq!(
        tick.failed, 2,
        "one attempt per failing row, not a hot loop"
    );
    // Rotation stamp recorded for both failures.
    let stamped: i64 =
        sqlx::query_scalar("SELECT count(*) FROM narinfo WHERE compat_attempted_at IS NOT NULL")
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(stamped, 2, "failed rows must be stamped for rotation");

    // A newer healthy pending path arrives after the failures.
    let healthy = format!("/nix/store/{}-tick-healthy", "l".repeat(32));
    let (nar, _) = make_nar(b"new path behind a failing prefix");
    let info = make_path_info(&healthy, &nar, Sha256::digest(&nar).into());
    assert!(put_path(&mut s.client, info, nar).await?);

    // Tick 2: the healthy path sorts ahead of the previously-failed
    // rows (NULLS FIRST) and is published — no starvation.
    let tick = reconciler::run_tick(&s.db.pool, &cache, &writer, &shutdown).await;
    assert_eq!(
        tick.published, 1,
        "newer pending path must not be starved by older failing rows"
    );
    let recorded: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT compat_file_hash FROM narinfo WHERE store_path = $1")
            .bind(&healthy)
            .fetch_one(&s.db.pool)
            .await?;
    assert!(recorded.is_some(), "healthy path published on tick 2");

    Ok(())
}

// r[verify store.compat.narinfo-on-put]
/// PutPathBatch: every committed output gets its own compat pair, and
/// the published narinfo carries the batch-resolved signature.
#[tokio::test]
async fn compat_batch_publishes_every_output() -> TestResult {
    use rio_proto::types::{PutPathBatchRequest, PutPathRequest};

    let (s, backend) = StoreSession::new_with_compat().await?;
    let mut client = s.client.clone();

    // Distinct hash parts (test_store_path uses one fixed hash, which
    // would collide on the `{hash}.narinfo` key).
    let path_a = format!("/nix/store/{}-compat-batch-a", "b".repeat(32));
    let path_b = format!("/nix/store/{}-compat-batch-b", "c".repeat(32));
    let (nar_a, _) = make_nar(b"batch output a");
    let (nar_b, _) = make_nar(b"batch output b");
    let info_a = make_path_info(&path_a, &nar_a, Sha256::digest(&nar_a).into());
    let info_b = make_path_info(&path_b, &nar_b, Sha256::digest(&nar_b).into());

    // Send both outputs on one batch stream (metadata → chunk →
    // trailer each), mirroring chunked.rs's send_batch_output shape.
    let (tx, rx) = mpsc::channel(16);
    for (idx, info, nar) in [(0u32, info_a, nar_a.clone()), (1u32, info_b, nar_b.clone())] {
        let mut raw: PathInfo = info.into();
        let trailer = PutPathTrailer {
            nar_hash: std::mem::take(&mut raw.nar_hash),
            nar_size: std::mem::take(&mut raw.nar_size),
        };
        for msg in [
            put_path_request::Msg::Metadata(PutPathMetadata { info: Some(raw) }),
            put_path_request::Msg::NarChunk(nar),
            put_path_request::Msg::Trailer(trailer),
        ] {
            tx.send(PutPathBatchRequest {
                output_index: idx,
                inner: Some(PutPathRequest { msg: Some(msg) }),
            })
            .await
            .expect("fresh channel");
        }
    }
    drop(tx);
    let resp = client
        .put_path_batch(ReceiverStream::new(rx))
        .await?
        .into_inner();
    assert_eq!(resp.created, vec![true, true]);

    for (path, nar) in [(&path_a, &nar_a), (&path_b, &nar_b)] {
        let hash_part = StorePath::parse(path)?.hash_part();
        let blob = backend
            .get_blob(&format!("{hash_part}.narinfo"))
            .await?
            .unwrap_or_else(|| panic!("batch output {path} must get a narinfo object"));
        let parsed = NarInfo::parse(std::str::from_utf8(&blob)?)?;
        assert_eq!(&parsed.store_path, path);
        assert_eq!(parsed.nar_size, nar.len() as u64);
        assert!(
            parsed.sigs.iter().any(|s| s.starts_with("rio-test-1:")),
            "batch compat narinfo must carry the resolved signature: {:?}",
            parsed.sigs
        );
        // The NAR object it points at exists and decompresses to the
        // uploaded bytes.
        let nar_blob = backend
            .get_blob(&parsed.url)
            .await?
            .unwrap_or_else(|| panic!("URL {} must resolve", parsed.url));
        assert_eq!(&zstd_decode(&nar_blob).await, nar);
    }

    Ok(())
}
