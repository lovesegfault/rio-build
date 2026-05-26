//! P0575 streaming-open tests: the dispatch boundary, the during-fill
//! read path, chunk sourcing (node chunk cache vs `GetChunks`),
//! `PromoteChunks` batching, and the failure semantics. All of them
//! drive [`OpenPath::ensure_readable`] against the shared mock-castore
//! + fake-mountd harness — no kernel mount involved.

use super::*;
use crate::castore_fuse::open::Readable;
use crate::castore_fuse::stream::StreamFill;

/// `ensure_readable` blocks (condvar waits, `Handle::block_on` in the
/// fill thread), so call it the way production does: from a thread
/// that is allowed to block.
async fn ensure_readable_blocking(
    open_path: &Arc<OpenPath>,
    digest: [u8; 32],
    size: u64,
) -> Result<Readable, fuser::Errno> {
    let op = Arc::clone(open_path);
    tokio::task::spawn_blocking(move || op.ensure_readable(&digest, size))
        .await
        .expect("ensure_readable panicked")
}

/// A streaming `read()` may block until its range is filled — run it on
/// the blocking pool like the fuser thread it stands in for.
async fn read_blocking(
    fill: &Arc<StreamFill>,
    offset: u64,
    len: u32,
) -> Result<Vec<u8>, fuser::Errno> {
    let fill = Arc::clone(fill);
    tokio::task::spawn_blocking(move || fill.read_at(offset, len))
        .await
        .expect("read_at panicked")
}

fn unwrap_streaming(readable: Readable) -> (Arc<StreamFill>, OpenCase) {
    match readable {
        Readable::Streaming { fill, case } => (fill, case),
        Readable::Backing(case) => panic!("expected a streaming open, got Backing({case:?})"),
    }
}

fn unwrap_backing(readable: Readable) -> OpenCase {
    match readable {
        Readable::Backing(case) => case,
        Readable::Streaming { case, .. } => {
            panic!("expected a backing open, got Streaming({case:?})")
        }
    }
}

/// Poll for an on-disk condition the fill thread produces asynchronously
/// (fill completion or failure cleanup). Bounded; panics with `what` on
/// timeout so a hang reads as a clear failure.
async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Test content with enough structure that any slicing/offset bug shows
/// up as a content mismatch, not just a length mismatch.
fn patterned(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

// ─── Dispatch boundary ─────────────────────────────────────────────────

/// The `stream_threshold` decides the path: at or below it the open
/// pays the whole-file fetch (`ReadBlob`, complete backing entry on
/// return); above it the open goes streaming (`StatBlob` + chunk fill,
/// no `ReadBlob`).
// r[verify builder.fs.streaming-open-threshold]
#[tokio::test(flavor = "multi_thread")]
async fn dispatch_boundary_routes_by_stream_threshold() {
    let h = harness().await; // threshold = 1024
    let small = patterned(1024); // == threshold → whole-file
    let small_digest = seeded_blob(&h.mock, &small);
    let case = unwrap_backing(
        ensure_readable_blocking(&h.open_path, small_digest, small.len() as u64)
            .await
            .expect("small open"),
    );
    assert_eq!(case, OpenCase::MissSmall);
    assert_eq!(h.mock.read_blob_calls(), 1, "whole-file path uses ReadBlob");
    assert_eq!(h.mock.stat_blob_calls(), 0);
    assert_eq!(
        std::fs::read(h.open_path.cache_path(&small_digest)).unwrap(),
        small,
        "the small open returns only once the backing entry is complete"
    );

    let large = patterned(4096); // > threshold → streaming
    let (large_digest, _chunks) = h.mock.seed_chunked_blob(&large, 1000, 16);
    let (fill, case) = unwrap_streaming(
        ensure_readable_blocking(&h.open_path, large_digest, large.len() as u64)
            .await
            .expect("large open"),
    );
    assert_eq!(case, OpenCase::MissStream);
    assert_eq!(h.mock.stat_blob_calls(), 1, "streaming path uses StatBlob");
    assert_eq!(
        h.mock.read_blob_calls(),
        1,
        "the streaming path never falls back to a whole-file ReadBlob"
    );
    assert_eq!(
        read_blocking(&fill, 0, large.len() as u32).await.unwrap(),
        large,
        "the streamed bytes match the original content"
    );
}

// ─── The streaming window ──────────────────────────────────────────────

/// The core P0575 behavior: `open()` returns once the first chunk has
/// landed (the rest of the file is still held back by the test gate),
/// reads inside the filled prefix are served from the partial file,
/// reads beyond it wait for the fill, and after completion the file is
/// promoted so the next open is a plain backing-cache hit with zero
/// further fetches.
// r[verify builder.fs.streaming-open]
#[tokio::test(flavor = "multi_thread")]
async fn streaming_open_returns_after_the_first_chunk_and_promotes_on_completion() {
    let h = harness().await;
    let content = patterned(5000);
    let (digest, chunk_digests) = h.mock.seed_chunked_blob(&content, 1000, 16);
    assert_eq!(chunk_digests.len(), 5);
    // Only the first chunk may be served until the test releases more.
    let gate = h.mock.gate_chunks(1);

    let (fill, case) = unwrap_streaming(
        ensure_readable_blocking(&h.open_path, digest, content.len() as u64)
            .await
            .expect("streaming open"),
    );
    // Reaching this point with a 1-permit gate is the structural proof
    // that open() did not wait for the whole file: only one of five
    // chunks could possibly have been served.
    assert_eq!(case, OpenCase::MissStream);
    assert!(
        !h.open_path.cache_path(&digest).exists(),
        "the backing entry must not exist yet — the fill is still running"
    );

    // A read inside the filled prefix is served immediately.
    assert_eq!(
        read_blocking(&fill, 0, 100).await.unwrap(),
        content[..100],
        "prefix reads are served from the partial file during the fill"
    );

    // A read past the high-water mark stays blocked while the gate
    // holds the remaining chunks back.
    let tail_start = content.len() as u64 - 700;
    let tail = {
        let fill = Arc::clone(&fill);
        tokio::task::spawn_blocking(move || fill.read_at(tail_start, 700))
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !tail.is_finished(),
        "a read beyond the filled prefix must wait for its range"
    );

    // Release the rest of the file and let the fill finish.
    gate.add_permits(chunk_digests.len() - 1);
    assert_eq!(
        tail.await.expect("join").expect("tail read"),
        content[content.len() - 700..],
        "the blocked read resumes with the exact bytes once its range arrives"
    );

    // Fill completion promotes into the shared backing cache; the next
    // open of this digest is a plain hit and nothing was ever fetched
    // whole-file.
    wait_until("the fill to promote into the backing cache", || {
        h.open_path.cache_path(&digest).exists()
    })
    .await;
    assert_eq!(
        std::fs::read(h.open_path.cache_path(&digest)).unwrap(),
        content,
        "the promoted backing entry is the verified assembled content"
    );
    let case = unwrap_backing(
        ensure_readable_blocking(&h.open_path, digest, content.len() as u64)
            .await
            .expect("second open"),
    );
    assert_eq!(case, OpenCase::Hit, "the next open takes the hit path");
    assert_eq!(h.mock.read_blob_calls(), 0, "no whole-file fetch ever ran");
    assert_eq!(h.mock.stat_blob_calls(), 1, "and no second fill started");
}

/// Concurrent opens of one in-flight digest attach to the same fill
/// (no second StatBlob, no second `.partial`), and the attacher only
/// waits for the first chunk — not for completion.
// r[verify builder.fs.streaming-open]
#[tokio::test(flavor = "multi_thread")]
async fn a_second_open_attaches_to_the_inflight_fill() {
    let h = harness().await;
    let content = patterned(4000);
    let (digest, chunk_digests) = h.mock.seed_chunked_blob(&content, 1000, 8);
    let gate = h.mock.gate_chunks(1);

    let (first, case_first) = unwrap_streaming(
        ensure_readable_blocking(&h.open_path, digest, content.len() as u64)
            .await
            .expect("first open"),
    );
    assert_eq!(case_first, OpenCase::MissStream);

    // The fill is still gated after its first chunk; a second open must
    // come back without waiting for completion.
    let (second, case_second) = unwrap_streaming(
        ensure_readable_blocking(&h.open_path, digest, content.len() as u64)
            .await
            .expect("second open"),
    );
    assert_eq!(case_second, OpenCase::WaitFetching);
    assert!(
        Arc::ptr_eq(&first, &second),
        "both opens share one fill (one .partial, one fetch)"
    );
    assert_eq!(h.mock.stat_blob_calls(), 1, "one StatBlob for both opens");

    gate.add_permits(chunk_digests.len() - 1);
    assert_eq!(
        read_blocking(&second, 0, content.len() as u32)
            .await
            .unwrap(),
        content
    );
}

// ─── Chunk sourcing ────────────────────────────────────────────────────

/// Chunks already present in the node chunk cache are read locally —
/// only the misses go to `GetChunks` — and the assembled content is
/// still byte-exact.
// r[verify builder.fs.node-chunk-cache]
#[tokio::test(flavor = "multi_thread")]
async fn local_chunk_cache_hits_skip_the_remote_fetch() {
    let h = harness().await;
    let content = patterned(5000);
    let (digest, chunk_digests) = h.mock.seed_chunked_blob(&content, 1000, 16);

    // Pre-populate the node chunk cache with chunks 0 and 2 (what an
    // earlier build on this node would have left behind).
    for &local in &[chunk_digests[0], chunk_digests[2]] {
        let bytes = h.mock.state.chunks.lock().unwrap()[&local].clone();
        let hex = hex::encode(local);
        let shard = h.chunks.join(&hex[..2]);
        std::fs::create_dir_all(&shard).unwrap();
        std::fs::write(shard.join(&hex), bytes).unwrap();
    }

    let (fill, _case) = unwrap_streaming(
        ensure_readable_blocking(&h.open_path, digest, content.len() as u64)
            .await
            .expect("streaming open"),
    );
    assert_eq!(
        read_blocking(&fill, 0, content.len() as u32).await.unwrap(),
        content
    );

    let requested = h.mock.chunk_requests();
    assert_eq!(
        requested.len(),
        chunk_digests.len() - 2,
        "locally-cached chunks are never requested remotely: {requested:?}"
    );
    assert!(
        !requested.contains(&chunk_digests[0]) && !requested.contains(&chunk_digests[2]),
        "the pre-seeded chunks were served from the node chunk cache"
    );
    wait_until("the fill to promote into the backing cache", || {
        h.open_path.cache_path(&digest).exists()
    })
    .await;
    assert_eq!(
        std::fs::read(h.open_path.cache_path(&digest)).unwrap(),
        content
    );
}

/// Remotely-fetched chunks are staged and `PromoteChunks`-batched (≤32
/// per frame) so the next build on this node finds them in the shared
/// chunk cache; the staged copies are dropped once each batch is
/// promoted.
// r[verify builder.fs.node-chunk-cache]
#[tokio::test(flavor = "multi_thread")]
async fn remote_chunks_are_promoted_for_other_builds() {
    let h = harness().await;
    // 40 chunks of 100 B → one full 32-chunk batch plus an 8-chunk tail.
    let content = patterned(4000);
    let (digest, chunk_digests) = h.mock.seed_chunked_blob(&content, 100, 4);
    assert_eq!(chunk_digests.len(), 40);

    let (fill, _case) = unwrap_streaming(
        ensure_readable_blocking(&h.open_path, digest, content.len() as u64)
            .await
            .expect("streaming open"),
    );
    assert_eq!(
        read_blocking(&fill, 0, content.len() as u32).await.unwrap(),
        content
    );
    wait_until("the fill to promote into the backing cache", || {
        h.open_path.cache_path(&digest).exists()
    })
    .await;

    let batches = h.mountd_state.promoted_chunk_batches();
    assert!(
        batches.iter().all(|b| b.len() <= 32),
        "PromoteChunks batches stay within the 32-chunk flush size: {:?}",
        batches.iter().map(Vec::len).collect::<Vec<_>>()
    );
    let promoted: std::collections::HashSet<[u8; 32]> = batches.iter().flatten().copied().collect();
    assert_eq!(
        promoted,
        chunk_digests.iter().copied().collect(),
        "every remotely-fetched chunk is promoted for other builds"
    );
    for chunk in &chunk_digests {
        let hex = hex::encode(chunk);
        assert!(
            h.chunks.join(&hex[..2]).join(&hex).exists(),
            "chunk {hex} landed in the shared chunk cache"
        );
    }
    assert_eq!(
        std::fs::read_dir(h.staging.join("chunks")).unwrap().count(),
        0,
        "staged chunk copies are dropped after their batch is promoted"
    );
    assert!(
        !h.staging
            .join(format!("{}.partial", hex::encode(digest)))
            .exists(),
        "no .partial left behind after a successful fill"
    );
}

// ─── Failure semantics ─────────────────────────────────────────────────

/// A chunk whose bytes do not hash to its digest kills the fill: blocked
/// readers get EIO, nothing is promoted, the `.partial` is cleaned up,
/// and the next open starts a fresh fill that succeeds once the store
/// serves correct bytes.
// r[verify builder.fs.file-digest-integrity]
#[tokio::test(flavor = "multi_thread")]
async fn a_corrupt_chunk_fails_the_fill_and_the_next_open_retries() {
    let h = harness().await;
    let content = patterned(5000);
    let (digest, chunk_digests) = h.mock.seed_chunked_blob(&content, 1000, 16);

    // Corrupt the third chunk's stored bytes (its digest stays the
    // same, so the per-chunk verification must catch the mismatch).
    let original = h.mock.state.chunks.lock().unwrap()[&chunk_digests[2]].clone();
    h.mock
        .state
        .chunks
        .lock()
        .unwrap()
        .insert(chunk_digests[2], b"corrupted bytes".to_vec());

    // The first chunk is fine, so the open usually succeeds and the EIO
    // surfaces on a read past the corrupt chunk. On a loaded machine the
    // fill can hit the corrupt chunk and publish its failure before the
    // opener's first-chunk barrier acquires the lock, in which case
    // `wait_covering` (deliberately — fail-fast once the fill is dead)
    // reports the EIO at open() instead. Same outcome, different timing.
    match ensure_readable_blocking(&h.open_path, digest, content.len() as u64).await {
        Ok(readable) => {
            let (fill, _case) = unwrap_streaming(readable);
            let err = read_blocking(&fill, content.len() as u64 - 10, 10)
                .await
                .expect_err("a read past the corrupt chunk fails");
            assert_eq!(err.code(), fuser::Errno::EIO.code());
        }
        Err(errno) => assert_eq!(
            errno.code(),
            fuser::Errno::EIO.code(),
            "an open that lost the race to the failed fill surfaces the same EIO"
        ),
    }
    assert!(
        !h.open_path.cache_path(&digest).exists(),
        "corrupt content is never promoted into the shared cache"
    );
    wait_until("the failed fill to clean up its .partial", || {
        !h.staging
            .join(format!("{}.partial", hex::encode(digest)))
            .exists()
    })
    .await;
    // The corrupt chunk is rejected on arrival: it must be neither
    // staged for PromoteChunks nor included in any batch already sent —
    // a node-cache entry whose bytes don't hash to its name would
    // poison every later build on this node.
    assert!(
        !h.mountd_state
            .promoted_chunk_batches()
            .iter()
            .flatten()
            .any(|d| d == &chunk_digests[2]),
        "the corrupt chunk must never be PromoteChunks'd"
    );
    assert!(
        !h.staging
            .join("chunks")
            .join(hex::encode(chunk_digests[2]))
            .exists(),
        "the corrupt chunk must never be staged for promotion"
    );

    // Heal the store and open again: the dead fill was deregistered, so
    // this is a fresh fill that completes and promotes.
    h.mock
        .state
        .chunks
        .lock()
        .unwrap()
        .insert(chunk_digests[2], original);
    let (fill, case) = unwrap_streaming(
        ensure_readable_blocking(&h.open_path, digest, content.len() as u64)
            .await
            .expect("the retry open succeeds"),
    );
    assert_eq!(case, OpenCase::MissStream, "the retry is a fresh fill");
    assert_eq!(
        read_blocking(&fill, 0, content.len() as u32).await.unwrap(),
        content
    );
    wait_until("the retry fill to promote", || {
        h.open_path.cache_path(&digest).exists()
    })
    .await;
    assert_eq!(
        std::fs::read(h.open_path.cache_path(&digest)).unwrap(),
        content
    );
}

/// A fill that dies before its first chunk (StatBlob NotFound, or the
/// `GetChunks` stream erroring on the very first chunk) fails the
/// `open()` itself with EIO promptly — the failure propagates through
/// the first-chunk barrier instead of the open waiting out its budget.
// r[verify builder.fs.streaming-open]
#[tokio::test(flavor = "multi_thread")]
async fn a_fill_that_fails_before_the_first_chunk_fails_the_open() {
    // A long first-chunk budget so "failed fast" and "waited out the
    // budget" are unmistakably different outcomes; the slack below is
    // generous because the failure path is two local RPC round-trips.
    let h = harness_with(OpenConfig {
        jit_fetch_timeout: Duration::from_secs(30),
        mountd_request_timeout: FAST,
        stream_threshold: 1024,
    })
    .await;

    // (a) StatBlob answers NotFound: nothing about the digest is known.
    let unknown = *blake3::hash(b"never seeded for streaming").as_bytes();
    let started = std::time::Instant::now();
    let err = ensure_readable_blocking(&h.open_path, unknown, 4096)
        .await
        .expect_err("the open fails when the chunk window cannot be resolved");
    assert_eq!(err.code(), fuser::Errno::EIO.code());
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the failed open must return promptly, not wait out the 30s first-chunk budget \
         (took {:?})",
        started.elapsed()
    );
    assert!(
        !h.staging
            .join(format!("{}.partial", hex::encode(unknown)))
            .exists(),
        "the dead fill cleans up its .partial"
    );

    // (b) StatBlob succeeds but the very first GetChunks fetch aborts
    // (chunk bodies missing store-side): same outcome.
    let content = patterned(4096);
    let (digest, chunk_digests) = h.mock.seed_chunked_blob(&content, 1000, 8);
    for chunk in &chunk_digests {
        h.mock.state.chunks.lock().unwrap().remove(chunk);
    }
    let started = std::time::Instant::now();
    let err = ensure_readable_blocking(&h.open_path, digest, content.len() as u64)
        .await
        .expect_err("the open fails when the first chunk cannot be fetched");
    assert_eq!(err.code(), fuser::Errno::EIO.code());
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the failed open must return promptly (took {:?})",
        started.elapsed()
    );
    assert!(
        !h.open_path.cache_path(&digest).exists(),
        "nothing is promoted by a fill that never produced a verified byte"
    );
}

/// Individually-valid chunks that do not assemble to the claimed
/// `file_digest` are caught by the whole-file verification at fill
/// completion: nothing is renamed, nothing is promoted, and later reads
/// on the streaming handle fail.
// r[verify builder.fs.file-digest-integrity]
#[tokio::test(flavor = "multi_thread")]
async fn a_whole_file_digest_mismatch_is_never_promoted() {
    let h = harness().await;
    let content = patterned(4000);
    let (real_digest, _chunks) = h.mock.seed_chunked_blob(&content, 1000, 8);
    // Re-register the same chunk window under a digest the content does
    // NOT hash to.
    let claimed = *blake3::hash(b"a different file entirely").as_bytes();
    let plan = h
        .mock
        .state
        .stat_plans
        .lock()
        .unwrap()
        .remove(&real_digest)
        .unwrap();
    h.mock
        .state
        .stat_plans
        .lock()
        .unwrap()
        .insert(claimed, plan);

    // The open usually returns inside the fill window (per-chunk checks
    // pass), but on a loaded machine the whole 4-chunk fill plus its
    // failed whole-file verification can finish before the opener's
    // first-chunk barrier acquires the lock — and `wait_covering`
    // deliberately reports a dead fill's error even for ranges it
    // already covers (the §2.7/§13 fail-fast promise). Both orderings
    // are the same outcome; only how early the EIO surfaces differs.
    let opened = ensure_readable_blocking(&h.open_path, claimed, content.len() as u64).await;
    wait_until("the failed fill to clean up its .partial", || {
        !h.staging
            .join(format!("{}.partial", hex::encode(claimed)))
            .exists()
    })
    .await;
    assert!(
        !h.open_path.cache_path(&claimed).exists()
            && !h.staging.join(hex::encode(claimed)).exists(),
        "a file failing its whole-file digest is neither promoted nor left in staging"
    );
    match opened {
        Ok(readable) => {
            let (fill, _case) = unwrap_streaming(readable);
            // The verification failure is published right after the
            // cleanup; reads on the streaming handle turn into EIO from
            // that point on (ranges already assembled included).
            let mut last = read_blocking(&fill, 0, 16).await;
            for _ in 0..200 {
                if last.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
                last = read_blocking(&fill, 0, 16).await;
            }
            assert_eq!(
                last.expect_err("reads after the failed verification are EIO")
                    .code(),
                fuser::Errno::EIO.code()
            );
        }
        Err(errno) => assert_eq!(
            errno.code(),
            fuser::Errno::EIO.code(),
            "an open that lost the race to the failed fill surfaces the same EIO"
        ),
    }
}

/// A Promote failure after the fill has verified and renamed is
/// best-effort: reads on the open handle keep working (served from the
/// staging fd), nothing corrupt or partial reaches the shared cache,
/// and the failure stays observable (logged + counted, and the entry is
/// simply absent from the cache so later opens re-fetch). Deliberate
/// asymmetry with the whole-file path, where a Promote failure is fatal
/// because the open has nothing to serve without the cache entry.
// r[verify builder.fs.streaming-open]
#[tokio::test(flavor = "multi_thread")]
async fn a_promote_failure_does_not_poison_the_streaming_fill() {
    let h = harness().await;
    let content = patterned(4000);
    let (digest, _chunks) = h.mock.seed_chunked_blob(&content, 1000, 8);
    // The fill's own work succeeds; only the final Promote is rejected.
    h.mountd_state.script_promote(
        proto::ErrKind::Retryable("staging disk on fire".into()),
        false,
    );

    let (fill, case) = unwrap_streaming(
        ensure_readable_blocking(&h.open_path, digest, content.len() as u64)
            .await
            .expect("streaming open"),
    );
    assert_eq!(case, OpenCase::MissStream);
    assert_eq!(
        read_blocking(&fill, 0, content.len() as u32).await.unwrap(),
        content,
        "the fill itself completes"
    );

    wait_until("the (failing) Promote to be attempted", || {
        !h.mountd_state.promote_requests().is_empty()
    })
    .await;

    assert!(
        !h.open_path.cache_path(&digest).exists(),
        "a failed promote publishes nothing to the shared cache"
    );
    assert!(
        h.staging.join(hex::encode(digest)).exists(),
        "the verified staging file stays — it is what open handles read from"
    );
    assert_eq!(
        read_blocking(&fill, 100, 100).await.unwrap(),
        content[100..200],
        "reads on the open handle still succeed after the failed promote"
    );

    // The failed promote only costs future opens the cache hit: once the
    // dead fill deregisters, a fresh open starts its own fill and — with
    // no more scripted failures — publishes the entry.
    let mut recovered = None;
    for _ in 0..200 {
        match ensure_readable_blocking(&h.open_path, digest, content.len() as u64)
            .await
            .expect("re-open after the failed promote")
        {
            Readable::Streaming {
                fill,
                case: OpenCase::MissStream,
            } => {
                recovered = Some(fill);
                break;
            }
            // Attached to the finished first fill (not yet deregistered)
            // — harmless; try again shortly.
            _ => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
    let fill2 = recovered.expect("a fresh fill must start once the failed one deregisters");
    assert_eq!(
        read_blocking(&fill2, 0, content.len() as u32)
            .await
            .unwrap(),
        content
    );
    wait_until("the retry fill to promote", || {
        h.open_path.cache_path(&digest).exists()
    })
    .await;
    assert_eq!(
        std::fs::read(h.open_path.cache_path(&digest)).unwrap(),
        content
    );
}

/// A chunk window that repeats one digest (CDC plans legitimately do —
/// identical zero-page runs) assembles byte-exact, fetches the repeated
/// chunk over the wire exactly once, and `PromoteChunks`es it exactly
/// once.
// r[verify builder.fs.node-chunk-cache]
#[tokio::test(flavor = "multi_thread")]
async fn a_repeated_chunk_in_the_window_is_fetched_and_promoted_once() {
    let h = harness().await;
    // content = A ‖ B ‖ A, window [A, B, A] with no edge padding.
    let chunk_a = patterned(1000);
    let mut chunk_b = patterned(1000);
    chunk_b.reverse();
    let content: Vec<u8> = [chunk_a.as_slice(), chunk_b.as_slice(), chunk_a.as_slice()].concat();
    let digest = *blake3::hash(&content).as_bytes();
    let a = h.mock.seed_chunk(chunk_a.clone());
    let b = h.mock.seed_chunk(chunk_b.clone());
    h.mock.state.stat_plans.lock().unwrap().insert(
        digest,
        StatBlobResponse {
            chunks: vec![
                ChunkMeta {
                    digest: a.to_vec(),
                    size: 1000,
                },
                ChunkMeta {
                    digest: b.to_vec(),
                    size: 1000,
                },
                ChunkMeta {
                    digest: a.to_vec(),
                    size: 1000,
                },
            ],
            first_chunk_skip: 0,
            last_chunk_take: 1000,
        },
    );

    let (fill, _case) = unwrap_streaming(
        ensure_readable_blocking(&h.open_path, digest, content.len() as u64)
            .await
            .expect("streaming open"),
    );
    assert_eq!(
        read_blocking(&fill, 0, content.len() as u32).await.unwrap(),
        content,
        "the repeated chunk lands at both of its window positions"
    );
    wait_until("the fill to promote into the backing cache", || {
        h.open_path.cache_path(&digest).exists()
    })
    .await;
    assert_eq!(
        std::fs::read(h.open_path.cache_path(&digest)).unwrap(),
        content
    );

    let raw_requests = h.mock.state.chunk_requests.lock().unwrap().clone();
    assert_eq!(
        raw_requests.iter().filter(|d| **d == a).count(),
        1,
        "the repeated chunk goes over the wire once, not once per occurrence"
    );
    let promoted: Vec<[u8; 32]> = h
        .mountd_state
        .promoted_chunk_batches()
        .into_iter()
        .flatten()
        .collect();
    assert_eq!(
        promoted.iter().filter(|d| **d == a).count(),
        1,
        "the repeated chunk is PromoteChunks'd once"
    );
    assert!(promoted.contains(&b));
}

/// The integrity gate also covers the inline-manifest `ReadBlob`
/// fallback: a stream whose bytes do not hash to the claimed
/// `file_digest` fails the fill — nothing is renamed, nothing is
/// promoted, and the streaming handle turns to EIO.
// r[verify builder.fs.file-digest-integrity]
#[tokio::test(flavor = "multi_thread")]
async fn a_corrupt_read_blob_fallback_stream_fails_the_fill() {
    let h = harness().await;
    let content = patterned(3000);
    let claimed = *blake3::hash(&content).as_bytes();
    // Same length, different bytes: the per-frame overrun guard stays
    // quiet and the whole-file digest check is what must catch it.
    let mut corrupt = content.clone();
    corrupt[1500] ^= 0xFF;
    h.mock.seed_blob(claimed, corrupt);
    h.mock
        .state
        .stat_errors
        .lock()
        .unwrap()
        .insert(claimed, tonic::Code::FailedPrecondition);

    // The fill may fail before or after open()'s first-chunk barrier
    // (the corrupt stream is short); both surfaces are the same EIO.
    match ensure_readable_blocking(&h.open_path, claimed, content.len() as u64).await {
        Err(err) => assert_eq!(err.code(), fuser::Errno::EIO.code()),
        Ok(readable) => {
            let (fill, _case) = unwrap_streaming(readable);
            let mut last = read_blocking(&fill, 0, 16).await;
            for _ in 0..200 {
                if last.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
                last = read_blocking(&fill, 0, 16).await;
            }
            assert_eq!(
                last.expect_err("reads fail once the verification fails")
                    .code(),
                fuser::Errno::EIO.code()
            );
        }
    }

    wait_until("the failed fill to clean up its .partial", || {
        !h.staging
            .join(format!("{}.partial", hex::encode(claimed)))
            .exists()
    })
    .await;
    assert!(
        !h.open_path.cache_path(&claimed).exists()
            && !h.staging.join(hex::encode(claimed)).exists(),
        "a corrupt fallback stream is neither renamed into staging nor promoted"
    );
    assert!(
        h.mountd_state.promote_requests().is_empty(),
        "no Promote is even attempted for a failed fallback fill"
    );
    assert_eq!(
        h.mock.read_blob_calls(),
        1,
        "the fallback path was exercised"
    );
}

// ─── Robustness ────────────────────────────────────────────────────────

/// `StatBlob` answering `FailedPrecondition` (an inline manifest with no
/// chunk list) falls back to streaming the whole blob via `ReadBlob` —
/// the open still returns early and the file is still verified and
/// promoted.
#[tokio::test(flavor = "multi_thread")]
async fn streaming_open_falls_back_to_read_blob_for_inline_manifests() {
    let h = harness().await;
    let content = patterned(3000);
    let digest = seeded_blob(&h.mock, &content);
    h.mock
        .state
        .stat_errors
        .lock()
        .unwrap()
        .insert(digest, tonic::Code::FailedPrecondition);

    let (fill, case) = unwrap_streaming(
        ensure_readable_blocking(&h.open_path, digest, content.len() as u64)
            .await
            .expect("fallback open"),
    );
    assert_eq!(case, OpenCase::MissStream);
    assert_eq!(h.mock.read_blob_calls(), 1, "the fallback streams ReadBlob");
    assert_eq!(
        read_blocking(&fill, 0, content.len() as u32).await.unwrap(),
        content
    );
    wait_until("the fallback fill to promote", || {
        h.open_path.cache_path(&digest).exists()
    })
    .await;
    assert_eq!(
        std::fs::read(h.open_path.cache_path(&digest)).unwrap(),
        content
    );
}

/// An orphaned `.partial` from a fill that died without cleanup (no
/// flock holder) is reclaimed instead of wedging the digest, exactly
/// like the whole-file path.
#[tokio::test(flavor = "multi_thread")]
async fn streaming_open_reclaims_an_orphaned_partial() {
    let h = harness().await;
    let content = patterned(4000);
    let (digest, _chunks) = h.mock.seed_chunked_blob(&content, 1000, 8);
    std::fs::write(
        h.staging.join(format!("{}.partial", hex::encode(digest))),
        b"garbage from a dead fill",
    )
    .unwrap();

    let (fill, case) = unwrap_streaming(
        ensure_readable_blocking(&h.open_path, digest, content.len() as u64)
            .await
            .expect("reclaim and refill"),
    );
    assert_eq!(case, OpenCase::MissStream);
    assert_eq!(
        read_blocking(&fill, 0, content.len() as u32).await.unwrap(),
        content
    );
    wait_until("the fill to promote", || {
        h.open_path.cache_path(&digest).exists()
    })
    .await;
    assert_eq!(
        std::fs::read(h.open_path.cache_path(&digest)).unwrap(),
        content
    );
}

/// The streaming fill's RPCs (StatBlob for the chunk window, GetChunks
/// for the misses) carry the build's assignment token, same as the
/// whole-file path — the store's tenant gate applies to them equally.
#[tokio::test(flavor = "multi_thread")]
async fn streaming_fill_rpcs_carry_the_assignment_token() {
    let h = harness().await;
    h.mock.require_token();
    let content = patterned(5000);
    let (digest, _chunks) = h.mock.seed_chunked_blob(&content, 1000, 16);

    let (fill, _case) = unwrap_streaming(
        ensure_readable_blocking(&h.open_path, digest, content.len() as u64)
            .await
            .expect("a token-bearing streaming open succeeds"),
    );
    assert_eq!(
        read_blocking(&fill, 0, content.len() as u32).await.unwrap(),
        content
    );

    assert_eq!(
        h.mock.tokens_for("stat_blob"),
        vec![Some(HARNESS_TOKEN.to_string())],
        "StatBlob must carry x-rio-assignment-token"
    );
    let chunk_tokens = h.mock.tokens_for("get_chunks");
    assert!(
        !chunk_tokens.is_empty()
            && chunk_tokens
                .iter()
                .all(|t| t.as_deref() == Some(HARNESS_TOKEN)),
        "every GetChunks stream must carry x-rio-assignment-token, got {chunk_tokens:?}"
    );
}
