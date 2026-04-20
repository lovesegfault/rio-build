use super::*;

/// I-189 Option A: jitter actually varies and stays in
/// `[0.5×base, 1.5×base)`. Under herd, lockstep retry IS the herd;
/// this proves the lockstep is broken.
// r[verify builder.fuse.retry-jitter]
#[test]
fn test_jitter_range_and_variance() {
    let base = RETRY_BACKOFF[0];
    let lo = base.mul_f64(0.5);
    let hi = base.mul_f64(1.5);
    let samples: Vec<Duration> = (0..100).map(|_| jitter(base)).collect();
    for s in &samples {
        assert!(
            *s >= lo && *s <= hi,
            "jitter({base:?}) = {s:?} outside [{lo:?}, {hi:?}]"
        );
    }
    // Not all identical — jitter actually varied. P(100 identical
    // f64 draws) ≈ 0; if this fires, jitter() is a no-op.
    assert!(
        samples.iter().any(|s| *s != samples[0]),
        "jitter produced 100 identical samples"
    );
}

/// I-178: per-path JIT fetch timeout = max(base, nar_size /
/// MIN_THROUGHPUT). A 2 GB NAR at 15 MiB/s ≈ 128 s; the base 60 s
/// would have aborted it mid-stream → daemon ENOENT →
/// PermanentFailure poison. A 1 KB NAR keeps the base.
// r[verify builder.fuse.jit-lookup]
#[test]
fn test_jit_fetch_timeout_scales_with_nar_size() {
    let base = Duration::from_secs(60);

    // Small input: floor at base.
    assert_eq!(jit_fetch_timeout(base, 1024), base);
    assert_eq!(jit_fetch_timeout(base, 0), base);

    // 2 GB input: ceil(2_000_000_000 / 15_728_640) = 128 s > 60 s.
    let two_gb = jit_fetch_timeout(base, 2_000_000_000);
    assert!(
        two_gb >= Duration::from_secs(127),
        "2 GB @ 15 MiB/s floor must get ≥127 s, got {two_gb:?}"
    );
    assert!(
        two_gb < Duration::from_secs(200),
        "sanity upper bound (catches MIN_THROUGHPUT being lowered \
         without revisiting this test): {two_gb:?}"
    );

    // The 1.9 GB NAR from the I-178 incident.
    let i178 = jit_fetch_timeout(base, 1_901_554_624);
    assert!(
        i178 > base,
        "I-178's 1.9 GB input must exceed the 60 s base that poisoned it"
    );
}

// ========================================================================
// fetch_extract_insert tests via prefetch_path_blocking
// ========================================================================
//
// fetch_extract_insert is module-private; we test it through
// prefetch_path_blocking (its public caller). prefetch is SYNC with
// internal block_on — it MUST be called from spawn_blocking to avoid
// nested-runtime panic.
//
// Multi-thread runtime required: spawn_blocking runs the closure on a
// separate thread pool; that thread's block_on needs a worker thread
// free on the main runtime to actually process the gRPC futures.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use rio_test_support::fixtures::{make_nar, make_path_info, test_store_basename};
use rio_test_support::grpc::{MockStore, spawn_mock_store};

/// Short fetch timeout for tests — MockStore either responds instantly
/// or is gated via Notify; no test needs the full 60s.
const TEST_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Harness: spawn MockStore + Cache in a tempdir. Returns everything the
/// tests need, including the runtime handle for prefetch's block_on calls
/// and a fresh `CircuitBreaker` (default config, closed).
async fn setup_fetch_harness() -> (
    Arc<Cache>,
    StoreClients,
    MockStore,
    tempfile::TempDir,
    Handle,
    Arc<CircuitBreaker>,
    tokio::task::JoinHandle<()>,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = Arc::new(Cache::new(dir.path().to_path_buf()).expect("Cache::new"));
    let (store, addr, server_handle) = spawn_mock_store().await.expect("spawn mock store");
    let ch = rio_proto::client::connect_channel(&addr.to_string())
        .await
        .expect("connect");
    let clients = StoreClients::from_channel(ch);
    let rt = Handle::current();
    let circuit = Arc::new(CircuitBreaker::default());
    (cache, clients, store, dir, rt, circuit, server_handle)
}

/// Seed MockStore with a valid single-file NAR → prefetch fetches,
/// spools to a `.nar-*` tempfile, `restore_path_streaming`s to cache_dir,
/// inserts into the in-memory index → Ok(None) ("fetched"). Verify the extracted
/// file exists on disk with the right contents and the spool is gone.
// r[verify builder.fuse.fetch-bounded-memory]
#[tokio::test(flavor = "multi_thread")]
async fn test_prefetch_success_roundtrip() {
    let (cache, clients, store, dir, rt, circuit, _srv) = setup_fetch_harness().await;

    // Seed: single-file NAR containing "hello". Basename must be a valid
    // nixbase32 store path basename (32-char hash + name).
    let basename = test_store_basename("fetchtest");
    let store_path = format!("/nix/store/{basename}");
    let (nar, hash) = make_nar(b"hello");
    store.seed(make_path_info(&store_path, &nar, hash), nar);

    // Call prefetch via spawn_blocking — Cache methods use block_on
    // internally, nested-runtime panics if called from async context.
    let cache_cl = Arc::clone(&cache);
    let clients_cl = clients.clone();
    let basename_cl = basename.clone();
    let result = tokio::task::spawn_blocking(move || {
        prefetch_path_blocking(
            &cache_cl,
            &clients_cl,
            &rt,
            &circuit,
            TEST_FETCH_TIMEOUT,
            &basename_cl,
        )
    })
    .await
    .expect("spawn_blocking join");

    // Ok(None) means "fetched successfully" (not skipped).
    assert!(
        matches!(result, Ok(None)),
        "expected Ok(None) (fetched), got: {result:?}"
    );

    // The extracted NAR should be on disk: single-file NARs extract to a
    // plain file (not a directory) at cache_dir/basename.
    let local = dir.path().join(&basename);
    assert!(local.exists(), "extracted path should exist: {local:?}");
    let content = std::fs::read(&local).expect("read extracted file");
    assert_eq!(content, b"hello");

    // And the cache index should know about it.
    assert!(
        cache.contains(&basename),
        "cache index should record the path"
    );

    // I-180: the `.nar-*` spool tempfile must be removed post-extract
    // (scopeguard) — only the extracted tree remains in cache_dir.
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.contains(".nar-")))
        .collect();
    assert!(
        leftovers.is_empty(),
        "spool file should be removed after extract, found: {leftovers:?}"
    );
}

/// Directory NAR (multiple files + nested + symlink) round-trips
/// through the spool→restore_path_streaming path. The single-file
/// test above only exercises the regular-file branch of restore_node.
#[tokio::test(flavor = "multi_thread")]
async fn test_prefetch_directory_nar() {
    use rio_nix::nar::{NarEntry, NarNode, serialize};
    use sha2::Digest;

    let (cache, clients, store, dir, rt, circuit, _srv) = setup_fetch_harness().await;

    let basename = test_store_basename("dirfetch");
    let store_path = format!("/nix/store/{basename}");
    // A small directory tree: file + executable + nested + symlink.
    let node = NarNode::Directory {
        entries: vec![
            NarEntry {
                name: "bin".into(),
                node: NarNode::Directory {
                    entries: vec![NarEntry {
                        name: "tool".into(),
                        node: NarNode::Regular {
                            executable: true,
                            contents: b"#!/bin/sh\necho ok\n".to_vec(),
                        },
                    }],
                },
            },
            NarEntry {
                name: "data.txt".into(),
                node: NarNode::Regular {
                    executable: false,
                    contents: b"payload bytes".to_vec(),
                },
            },
            NarEntry {
                name: "link".into(),
                node: NarNode::Symlink {
                    target: "data.txt".into(),
                },
            },
        ],
    };
    let mut nar = Vec::new();
    serialize(&mut nar, &node).unwrap();
    let hash: [u8; 32] = sha2::Sha256::digest(&nar).into();
    store.seed(make_path_info(&store_path, &nar, hash), nar);

    let cache_cl = Arc::clone(&cache);
    let clients_cl = clients.clone();
    let basename_cl = basename.clone();
    let result = tokio::task::spawn_blocking(move || {
        prefetch_path_blocking(
            &cache_cl,
            &clients_cl,
            &rt,
            &circuit,
            TEST_FETCH_TIMEOUT,
            &basename_cl,
        )
    })
    .await
    .expect("spawn_blocking join");
    assert!(
        matches!(result, Ok(None)),
        "expected fetched, got {result:?}"
    );

    let local = dir.path().join(&basename);
    assert_eq!(
        std::fs::read(local.join("data.txt")).unwrap(),
        b"payload bytes"
    );
    assert_eq!(
        std::fs::read_link(local.join("link")).unwrap(),
        std::path::Path::new("data.txt")
    );
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(local.join("bin/tool"))
        .unwrap()
        .permissions()
        .mode();
    assert_ne!(mode & 0o111, 0, "executable bit must survive restore");
}

/// I-055: nixpkgs bootstrap placeholder hash (`eeee…` — `e` is not in
/// nixbase32) must short-circuit to ENOENT WITHOUT calling the store.
/// `fail_get_path=true` arms the proof: if gRPC were called we'd see
/// Unavailable→retry→EIO with ≥RETRY_BACKOFF total elapsed. Layer 1's
/// pre-gRPC StorePath::parse rejects the basename → immediate ENOENT.
/// ensure_cached:166 then records this as a breaker SUCCESS (path
/// definitively absent = store gave a healthy answer, even though we
/// never asked it).
// r[verify builder.fuse.circuit-breaker+3]
#[tokio::test(flavor = "multi_thread")]
async fn test_prefetch_invalid_basename_enoent_no_grpc() {
    let (cache, clients, store, _dir, rt, circuit, _srv) = setup_fetch_harness().await;
    // Arm Unavailable: if we DO call GetPath, we get retry→EIO not ENOENT.
    store.faults.fail_get_path.store(true, Ordering::SeqCst);

    // The actual nixpkgs bootstrap placeholder. 32 `e`s — every byte
    // outside nixbase32's alphabet.
    let placeholder = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-libidn2-2.3.8";
    let start = std::time::Instant::now();
    let result = tokio::task::spawn_blocking(move || {
        prefetch_path_blocking(
            &cache,
            &clients,
            &rt,
            &circuit,
            TEST_FETCH_TIMEOUT,
            placeholder,
        )
    })
    .await
    .expect("spawn_blocking join");

    let err = result.expect_err("expected Err(ENOENT)");
    assert_eq!(
        err.code(),
        Errno::ENOENT.code(),
        "expected ENOENT (local-validation reject), got: {err:?}. \
         EIO here means we hit the gRPC path despite the parse failure."
    );

    // No retry backoff observed → never entered the retry loop → never
    // called gRPC. test-cfg RETRY_BACKOFF totals ~760ms; sub-100ms is
    // unambiguously the pre-gRPC return.
    let elapsed = start.elapsed();
    let backoff_floor: Duration = RETRY_BACKOFF.iter().sum();
    assert!(
        elapsed < backoff_floor,
        "expected immediate return (<{backoff_floor:?}), got {elapsed:?} — \
         suggests we entered the gRPC retry loop"
    );
}

/// MockStore has no seeded paths → GetPath returns NotFound → ENOENT.
#[tokio::test(flavor = "multi_thread")]
async fn test_prefetch_not_found_returns_enoent() {
    let (cache, clients, _store, _dir, rt, circuit, _srv) = setup_fetch_harness().await;

    let basename = test_store_basename("missing");
    let result = tokio::task::spawn_blocking(move || {
        prefetch_path_blocking(
            &cache,
            &clients,
            &rt,
            &circuit,
            TEST_FETCH_TIMEOUT,
            &basename,
        )
    })
    .await
    .expect("spawn_blocking join");

    // fuser::Errno doesn't implement PartialEq — compare via .code().
    let err = result.expect_err("expected Err(ENOENT)");
    assert_eq!(
        err.code(),
        Errno::ENOENT.code(),
        "expected ENOENT, got: {err:?}"
    );
}

/// MockStore.fail_get_path = true → GetPath returns Unavailable →
/// retried RETRY_BACKOFF.len() times → still Unavailable → EIO.
/// Covers the retry-exhausted arm in fetch_extract_insert.
#[tokio::test(flavor = "multi_thread")]
async fn test_prefetch_store_unavailable_returns_eio() {
    let (cache, clients, store, _dir, rt, circuit, _srv) = setup_fetch_harness().await;
    store.faults.fail_get_path.store(true, Ordering::SeqCst);

    let basename = test_store_basename("unavail");
    let start = std::time::Instant::now();
    let result = tokio::task::spawn_blocking(move || {
        prefetch_path_blocking(
            &cache,
            &clients,
            &rt,
            &circuit,
            TEST_FETCH_TIMEOUT,
            &basename,
        )
    })
    .await
    .expect("spawn_blocking join");

    let err = result.expect_err("expected Err(EIO)");
    assert_eq!(err.code(), Errno::EIO.code(), "expected EIO, got: {err:?}");

    // Total backoff should have been observed (cfg(test): ~760ms ×
    // jitter ∈ [0.5, 1.5) per step → floor ~380ms). Lower-bound
    // check — proves retries happened, not just immediate EIO.
    let elapsed = start.elapsed();
    let min: Duration = RETRY_BACKOFF.iter().sum::<Duration>().mul_f64(0.5);
    assert!(
        elapsed >= min,
        "expected ≥{min:?} total backoff (proves retries fired), got {elapsed:?}"
    );
}

/// I-039: store pod restarts mid-build → transient Unavailable →
/// retry recovers → build survives. MockStore.fail_get_path starts
/// true; the test flips it false mid-retry, simulating the new pod
/// coming Ready. Prefetch should complete successfully (Ok(None),
/// not EIO).
#[tokio::test(flavor = "multi_thread")]
async fn test_prefetch_transient_unavailable_recovers() {
    let (cache, clients, store, dir, rt, circuit, _srv) = setup_fetch_harness().await;

    // Seed valid path so the post-recovery fetch has something to return.
    let basename = test_store_basename("transient");
    let store_path = format!("/nix/store/{basename}");
    let (nar, hash) = make_nar(b"survived");
    store.seed(make_path_info(&store_path, &nar, hash), nar);

    // Store starts "down". The first attempt + RETRY_BACKOFF[0] (10ms)
    // backoff + second attempt all hit Unavailable.
    store.faults.fail_get_path.store(true, Ordering::SeqCst);

    let cache_cl = Arc::clone(&cache);
    let clients_cl = clients.clone();
    let basename_cl = basename.clone();
    let task = tokio::task::spawn_blocking(move || {
        prefetch_path_blocking(
            &cache_cl,
            &clients_cl,
            &rt,
            &circuit,
            TEST_FETCH_TIMEOUT,
            &basename_cl,
        )
    });

    // Sleep past the first two backoffs (10ms+50ms cfg(test)) so at
    // least two retries land on the failing store, then "restart" it.
    // The third attempt (after 200ms backoff) sees the recovered store.
    tokio::time::sleep(RETRY_BACKOFF[0] + RETRY_BACKOFF[1] + Duration::from_millis(20)).await;
    store.faults.fail_get_path.store(false, Ordering::SeqCst);

    let result = task.await.expect("spawn_blocking join");
    assert!(
        matches!(result, Ok(None)),
        "expected Ok(None) (recovered + fetched), got: {result:?}"
    );

    // The NAR should be on disk — full roundtrip completed.
    let local = dir.path().join(&basename);
    let content = std::fs::read(&local).expect("read extracted file");
    assert_eq!(content, b"survived");
}

/// MockStore.get_path_garbage = true → GetPath returns valid PathInfo
/// but garbage NAR bytes → `restore_path_streaming` fails → EIO.
/// Covers the NAR parse-error arm in fetch_extract_insert.
#[tokio::test(flavor = "multi_thread")]
async fn test_prefetch_nar_parse_error_returns_eio() {
    let (cache, clients, store, _dir, rt, circuit, _srv) = setup_fetch_harness().await;

    // Seed a valid PathInfo (so the MockStore lookup finds it) but
    // enable garbage mode so the NAR bytes are malformed.
    let basename = test_store_basename("garbage");
    let store_path = format!("/nix/store/{basename}");
    let (nar, hash) = make_nar(b"real");
    store.seed(make_path_info(&store_path, &nar, hash), nar);
    store.faults.get_path_garbage.store(true, Ordering::SeqCst);

    let result = tokio::task::spawn_blocking(move || {
        prefetch_path_blocking(
            &cache,
            &clients,
            &rt,
            &circuit,
            TEST_FETCH_TIMEOUT,
            &basename,
        )
    })
    .await
    .expect("spawn_blocking join");

    let err = result.expect_err("expected Err(EIO) from NAR parse failure");
    assert_eq!(err.code(), Errno::EIO.code(), "expected EIO, got: {err:?}");
}

/// I-211: a fetch whose total wall-clock exceeds `fetch_timeout`
/// completes as long as every inter-chunk gap is below the timeout.
/// 5 chunks × 200ms = 1s total against a 500ms idle bound — pre-I-211
/// the 500ms wall-clock wrapper aborted at chunk 2-3 → EIO.
// r[verify builder.fuse.fetch-progress-timeout+2]
#[tokio::test(flavor = "multi_thread")]
async fn test_prefetch_idle_timeout_slow_but_progressing_ok() {
    let (cache, clients, store, dir, rt, circuit, _srv) = setup_fetch_harness().await;

    // 320 KiB payload → 5+ NarChunks at MockStore's 64 KiB stride.
    let payload = vec![0xab; 320 * 1024];
    let basename = test_store_basename("i211-slow");
    let store_path = format!("/nix/store/{basename}");
    let (nar, hash) = make_nar(&payload);
    store.seed(make_path_info(&store_path, &nar, hash), nar);
    store
        .faults
        .get_path_chunk_delay_ms
        .store(200, Ordering::SeqCst);

    let idle = Duration::from_millis(500);
    let started = std::time::Instant::now();
    let result = tokio::task::spawn_blocking(move || {
        prefetch_path_blocking(&cache, &clients, &rt, &circuit, idle, &basename)
    })
    .await
    .expect("spawn_blocking join");

    assert!(
        matches!(result, Ok(None)),
        "I-211: total > idle timeout but per-chunk gap < idle timeout MUST succeed; got: {result:?}"
    );
    assert!(
        started.elapsed() > idle,
        "test precondition: total fetch time ({:?}) must exceed idle timeout ({idle:?}) \
         or this isn't proving the wall-clock bound is gone",
        started.elapsed()
    );
    let local = dir.path().join(test_store_basename("i211-slow"));
    let content = std::fs::read(&local).expect("read extracted file");
    assert_eq!(content.len(), payload.len());
}

/// I-211: a stream that goes silent for longer than `fetch_timeout`
/// trips the idle bound on the FIRST stalled gap → DeadlineExceeded
/// (non-transient) → EIO without retry. This is the I-165 stuck-store
/// behavior the idle bound preserves.
// r[verify builder.fuse.fetch-progress-timeout+2]
#[tokio::test(flavor = "multi_thread")]
async fn test_prefetch_idle_timeout_stalled_chunk_eio() {
    let (cache, clients, store, _dir, rt, circuit, _srv) = setup_fetch_harness().await;

    let basename = test_store_basename("i211-stall");
    let store_path = format!("/nix/store/{basename}");
    let (nar, hash) = make_nar(&vec![0xcd; 128 * 1024]);
    store.seed(make_path_info(&store_path, &nar, hash), nar);
    // 800ms gap > 300ms idle bound → first NarChunk after Info trips.
    store
        .faults
        .get_path_chunk_delay_ms
        .store(800, Ordering::SeqCst);

    let idle = Duration::from_millis(300);
    let started = std::time::Instant::now();
    let result = tokio::task::spawn_blocking(move || {
        prefetch_path_blocking(&cache, &clients, &rt, &circuit, idle, &basename)
    })
    .await
    .expect("spawn_blocking join");

    let err = result.expect_err("expected Err(EIO) from idle timeout");
    assert_eq!(err.code(), Errno::EIO.code(), "expected EIO, got: {err:?}");
    // Tripped on the first gap, not after multiple — bound is per-chunk.
    // Allow slack for CI variance but assert it's well under the 800ms
    // gap (i.e., the receiver gave up, not the sender).
    assert!(
        started.elapsed() < Duration::from_millis(700),
        "idle timeout should trip near 300ms, took {:?}",
        started.elapsed()
    );
}

/// Second prefetch of the same path returns PrefetchSkip::AlreadyCached
/// (fast path hits cache.get_path() → Some).
#[tokio::test(flavor = "multi_thread")]
async fn test_prefetch_already_cached_skip() {
    let (cache, clients, store, _dir, rt, circuit, _srv) = setup_fetch_harness().await;

    let basename = test_store_basename("twice");
    let store_path = format!("/nix/store/{basename}");
    let (nar, hash) = make_nar(b"x");
    store.seed(make_path_info(&store_path, &nar, hash), nar);

    // First fetch: Ok(None) — actually fetched.
    let (c1, cl1, r1, cb1, b1) = (
        Arc::clone(&cache),
        clients.clone(),
        rt.clone(),
        Arc::clone(&circuit),
        basename.clone(),
    );
    let first = tokio::task::spawn_blocking(move || {
        prefetch_path_blocking(&c1, &cl1, &r1, &cb1, TEST_FETCH_TIMEOUT, &b1)
    })
    .await
    .expect("join");
    assert!(matches!(first, Ok(None)), "first fetch: {first:?}");

    // Second fetch: Ok(Some(AlreadyCached)) — fast-path skip.
    let (c2, cl2, r2, cb2, b2) = (
        Arc::clone(&cache),
        clients.clone(),
        rt.clone(),
        Arc::clone(&circuit),
        basename,
    );
    let second = tokio::task::spawn_blocking(move || {
        prefetch_path_blocking(&c2, &cl2, &r2, &cb2, TEST_FETCH_TIMEOUT, &b2)
    })
    .await
    .expect("join");
    assert!(
        matches!(second, Ok(Some(PrefetchSkip::AlreadyCached))),
        "second fetch: {second:?}"
    );
}

// ========================================================================
// ensure_cached tests (remediation 16: loop-wait + self-heal + semaphore)
// ========================================================================
//
// ensure_cached is a method on NixStoreFs; unlike prefetch_path_blocking
// it needs a full fs instance (for self.fetch_sem). NixStoreFs is Send+Sync
// (all fields are sync primitives / Arc / atomics) so Arc-wrapping lets
// spawn_blocking share it across the test's worker threads.

/// Build a NixStoreFs wrapped in Arc for cross-thread ensure_cached tests.
/// `fuse_threads` controls the fetch semaphore permits (threads - 1, min 1).
fn make_fs(
    cache: Arc<Cache>,
    clients: StoreClients,
    rt: Handle,
    fuse_threads: u32,
) -> Arc<NixStoreFs> {
    Arc::new(NixStoreFs::new(
        cache,
        clients,
        rt,
        false,
        fuse_threads,
        TEST_FETCH_TIMEOUT,
    ))
}

/// Concurrent `ensure_cached` calls for the same path during a slow fetch
/// all succeed — none get EAGAIN. Before the loop-wait fix, waiters timed
/// out at WAIT_TIMEOUT=30s while the fetcher was still healthy.
///
/// Mechanism: MockStore's get_path is gated on a Notify. One ensure_cached
/// wins Fetch and parks in block_on(GetPath) at the gate. The other N-1 get
/// WaitFor and park on the condvar. We sleep past one WAIT_SLICE (200ms in
/// cfg(test)) so each waiter does at least one heartbeat loop iteration —
/// the point where the OLD code returned EAGAIN. Then we open the gate;
/// the fetcher completes, guard drops, notify_all wakes all waiters, and
/// every call returns Ok(path).
///
// r[verify builder.fuse.lookup-caches+2]
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_concurrent_waiters_no_eagain_during_slow_fetch() {
    let (cache, clients, store, dir, rt, _circuit, _srv) = setup_fetch_harness().await;

    let basename = test_store_basename("slowfetch");
    let store_path = format!("/nix/store/{basename}");
    let (nar, hash) = make_nar(b"slow-payload");
    store.seed(make_path_info(&store_path, &nar, hash), nar);
    store
        .faults
        .get_path_gate_armed
        .store(true, Ordering::SeqCst);

    // N=5 concurrent ensure_cached. One wins Fetch, four get WaitFor.
    // fuse_threads=N → permits = N-1 = 4 ≥ 1 fetcher, so the semaphore
    // doesn't interfere (only one thread fetches this singleflight path).
    const N: usize = 5;
    let fs = make_fs(Arc::clone(&cache), clients, rt, N as u32);

    let mut handles = Vec::with_capacity(N);
    for _ in 0..N {
        let fs = Arc::clone(&fs);
        let bn = basename.clone();
        handles.push(tokio::task::spawn_blocking(move || fs.ensure_cached(&bn)));
    }

    // Let the fetcher reach the gate and the waiters park on the condvar.
    // Sleep past one WAIT_SLICE so waiters do ≥1 heartbeat iteration —
    // the OLD code returned EAGAIN here. 2× slice + margin for CI jitter.
    tokio::time::sleep(WAIT_SLICE * 2 + std::time::Duration::from_millis(100)).await;

    // Release the fetcher.
    store.faults.get_path_gate.notify_waiters();

    // All N must succeed with the same path. Zero EAGAIN.
    let mut paths = Vec::with_capacity(N);
    for h in handles {
        let r = h.await.expect("join");
        let p = r.expect("ensure_cached must succeed (no EAGAIN)");
        paths.push(p);
    }
    assert!(
        paths.iter().all(|p| p == &paths[0]),
        "all waiters see same path: {paths:?}"
    );
    assert!(paths[0].exists(), "fetched path on disk: {:?}", paths[0]);
    assert_eq!(std::fs::read(&paths[0]).expect("read"), b"slow-payload");
    drop(dir); // keep tempdir alive to here
}

/// I-179: when the singleflight guard drops with the cache STILL empty
/// (fetcher errored or panicked), `wait_for_fetcher` MUST return EIO,
/// not ENOENT. ENOENT would be negative-cached by overlayfs above the
/// FUSE mount → daemon's retry never reaches FUSE again → permanent
/// "input does not exist" until remount. EIO propagates without a
/// negative dentry, so the daemon's retry re-asks FUSE.
///
/// Mechanism: take the Fetch claim manually (so we control guard
/// lifetime), spawn a `wait_for_fetcher` waiter, drop the guard
/// without ever inserting into the cache → waiter wakes, finds
/// `cache.get_path == Ok(None)`, returns EIO.
///
// r[verify builder.fuse.jit-lookup]
#[tokio::test(flavor = "multi_thread")]
async fn test_wait_for_fetcher_guard_drop_cache_empty_is_eio() {
    let (cache, clients, _store, dir, rt, _circuit, _srv) = setup_fetch_harness().await;
    let fs = make_fs(Arc::clone(&cache), clients, rt, 4);

    let basename = test_store_basename("guard-drop-eio");

    // Take the Fetch claim ourselves; we will NOT populate the cache.
    let FetchClaim::Fetch(guard) = cache.try_start_fetch(&basename) else {
        panic!("first claim must be Fetch");
    };
    // Second claim is the waiter's InflightEntry.
    let FetchClaim::WaitFor(entry) = cache.try_start_fetch(&basename) else {
        panic!("second claim must be WaitFor");
    };

    // Park a waiter on the condvar via spawn_blocking (wait_for_fetcher
    // is sync). wait_deadline is irrelevant to the path under test —
    // the guard drop wakes the waiter long before staleness matters.
    let waiter = {
        let fs = Arc::clone(&fs);
        let bn = basename.clone();
        tokio::task::spawn_blocking(move || {
            fs.wait_for_fetcher(&entry, &bn, TEST_FETCH_TIMEOUT + WAIT_SLOP)
        })
    };

    // Let the waiter reach the condvar (one WAIT_SLICE tick is plenty).
    tokio::time::sleep(WAIT_SLICE / 2).await;

    // Simulate fetcher failure: drop the guard WITHOUT inserting into
    // the cache. FetchGuard::drop flips `done` and notify_all()s.
    drop(guard);

    let result = waiter.await.expect("join");
    let err = result.expect_err("guard dropped with cache empty ⇒ Err");
    assert_eq!(
        err.code(),
        Errno::EIO.code(),
        "I-179: must be EIO (overlay-safe), NOT ENOENT \
         (overlay would negative-cache); got {err:?}"
    );
    // And specifically NOT the old behavior:
    assert_ne!(err.code(), Errno::ENOENT.code());

    // Sanity: cache really is empty for this basename.
    let cached = cache.get_path(&basename);
    assert!(cached.is_none(), "cache must be empty: {cached:?}");
    drop(dir);
}

/// bug_134: `wait_for_fetcher` bounds STALENESS (time since the fetcher
/// last heartbeat), not total elapsed. A fetcher in its retry loop —
/// healthy, recovering through a store rolling-restart — can run for
/// several × `fetch_timeout`; concurrent `WaitFor` threads MUST NOT
/// abandon it as long as it keeps heartbeating. Before the fix, the
/// waiter checked `started.elapsed()` and gave up at `wait_deadline`
/// regardless of fetcher progress.
///
/// Mechanism: take the Fetch claim manually, spawn a waiter with a
/// short `wait_deadline`, sleep past it in TOTAL but heartbeat every
/// slice so staleness stays under it, then commit + drop guard. Waiter
/// must return `Ok`.
// r[verify builder.fuse.fetch-progress-timeout+2]
#[tokio::test(flavor = "multi_thread")]
async fn test_wait_for_fetcher_survives_retry_heartbeat() {
    let (cache, clients, _store, dir, rt, _circuit, _srv) = setup_fetch_harness().await;
    let fs = make_fs(Arc::clone(&cache), clients, rt, 4);

    let basename = test_store_basename("heartbeat");

    let FetchClaim::Fetch(guard) = cache.try_start_fetch(&basename) else {
        panic!("first claim must be Fetch");
    };
    let FetchClaim::WaitFor(entry) = cache.try_start_fetch(&basename) else {
        panic!("second claim must be WaitFor");
    };

    // wait_deadline = 400ms. WAIT_SLICE in cfg(test) is 200ms, so the
    // waiter checks staleness at ~200ms, ~400ms, ~600ms.
    let wait_deadline = Duration::from_millis(400);
    let waiter = {
        let fs = Arc::clone(&fs);
        let bn = basename.clone();
        tokio::task::spawn_blocking(move || fs.wait_for_fetcher(&entry, &bn, wait_deadline))
    };

    // Total ~700ms > wait_deadline, but heartbeat every 300ms keeps
    // staleness ≤ ~300ms < 400ms at every check.
    let started = std::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(300)).await;
    guard.heartbeat();
    tokio::time::sleep(Duration::from_millis(300)).await;
    guard.heartbeat();
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Commit + drop: waiter wakes via condvar, finds cache populated.
    cache.insert(&basename);
    drop(guard);

    let result = waiter.await.expect("join");
    let elapsed = started.elapsed();
    assert!(
        elapsed > wait_deadline,
        "test precondition: total elapsed ({elapsed:?}) must exceed \
         wait_deadline ({wait_deadline:?}) or this isn't proving the \
         total-elapsed bound is gone"
    );
    let path = result.unwrap_or_else(|e| {
        panic!(
            "waiter MUST survive a heartbeating fetcher past wait_deadline; \
             before bug_134 fix it returned EAGAIN at ~{wait_deadline:?}: {e:?}"
        )
    });
    assert_eq!(path, dir.path().join(&basename));
}

/// bug_134 negative case: a fetcher that NEVER heartbeats (genuinely
/// wedged) IS abandoned once staleness exceeds `wait_deadline`. The
/// staleness bound preserves the wedged-fetcher escape; only the
/// healthy-retry case is what changed.
#[tokio::test(flavor = "multi_thread")]
async fn test_wait_for_fetcher_wedged_no_heartbeat_eagain() {
    let (cache, clients, _store, _dir, rt, _circuit, _srv) = setup_fetch_harness().await;
    let fs = make_fs(Arc::clone(&cache), clients, rt, 4);

    let basename = test_store_basename("wedged");
    let FetchClaim::Fetch(guard) = cache.try_start_fetch(&basename) else {
        panic!("first claim must be Fetch");
    };
    let FetchClaim::WaitFor(entry) = cache.try_start_fetch(&basename) else {
        panic!("second claim must be WaitFor");
    };

    // wait_deadline = 300ms; WAIT_SLICE = 200ms → waiter checks staleness
    // at ~200ms (300 > 200, not yet) and ~400ms (stale ≈ 400 ≥ 300).
    let wait_deadline = Duration::from_millis(300);
    let waiter = {
        let fs = Arc::clone(&fs);
        let bn = basename.clone();
        tokio::task::spawn_blocking(move || fs.wait_for_fetcher(&entry, &bn, wait_deadline))
    };

    let result = waiter.await.expect("join");
    let err = result.expect_err("wedged fetcher (no heartbeat) → EAGAIN");
    assert_eq!(err.code(), Errno::EAGAIN.code());
    drop(guard);
}

/// `ensure_cached` with a stale index row (file rm'd, SQLite row intact)
/// detects the divergence, purges the row, re-fetches, and succeeds.
/// Before the self-heal fix, this returned Ok(path-that-doesn't-exist)
/// forever — every subsequent lookup would ENOENT in the caller's stat.
#[tokio::test(flavor = "multi_thread")]
async fn test_ensure_cached_self_heals_index_disk_divergence() {
    let (cache, clients, store, dir, rt, _circuit, _srv) = setup_fetch_harness().await;

    let basename = test_store_basename("diverge");
    let store_path = format!("/nix/store/{basename}");
    let (nar, hash) = make_nar(b"heal-me");
    store.seed(make_path_info(&store_path, &nar, hash), nar);

    let fs = make_fs(Arc::clone(&cache), clients, rt, 4);

    // First fetch: populates both disk and index.
    let p1 = {
        let fs = Arc::clone(&fs);
        let bn = basename.clone();
        tokio::task::spawn_blocking(move || fs.ensure_cached(&bn))
            .await
            .expect("join")
            .expect("first fetch")
    };
    assert!(p1.exists(), "first fetch materialized on disk: {p1:?}");

    // Simulate external rm: delete the file, leave the index row intact.
    // Single-file NARs extract to a plain file (not a dir) — see
    // test_prefetch_success_roundtrip.
    std::fs::remove_file(&p1).expect("rm cache file");
    assert!(!p1.exists(), "precondition: file gone from disk");

    // Index still says present.
    assert!(
        cache.contains(&basename),
        "precondition: index entry survives external rm"
    );

    // Second ensure_cached: should stat the fast-path return, detect
    // ENOENT, purge the row, re-fetch, and return a VALID path.
    let p2 = {
        let fs = Arc::clone(&fs);
        let bn = basename.clone();
        tokio::task::spawn_blocking(move || fs.ensure_cached(&bn))
            .await
            .expect("join")
            .expect("second fetch (self-heal)")
    };
    assert!(p2.exists(), "self-healed path exists: {p2:?}");
    assert_eq!(std::fs::read(&p2).expect("read"), b"heal-me");

    // The index entry was re-inserted by fetch_extract_insert (so the
    // self-heal's remove_stale was followed by a fresh insert).
    assert!(
        cache.contains(&basename),
        "index entry re-inserted after self-heal fetch"
    );
    drop(dir);
}

// ========================================================================
// Circuit-breaker integration with prefetch (bug_207) and TOCTOU (bug_363)
// ========================================================================

/// bug_207: prefetch-owned fetch failures MUST feed `circuit.record(false)`.
/// Under singleflight, when prefetch wins the claim and a FUSE thread lands
/// in `WaitFor`, the FUSE thread does NOT record (per the "fetcher records"
/// contract). If prefetch ALSO doesn't record, the breaker is blind to up
/// to 100 warm-gate failures. Five prefetch failures against a down store
/// MUST open the circuit.
// r[verify builder.fuse.circuit-breaker+3]
#[tokio::test(flavor = "multi_thread")]
async fn test_prefetch_failure_records_circuit() {
    let (cache, clients, store, _dir, rt, circuit, _srv) = setup_fetch_harness().await;
    store.faults.fail_get_path.store(true, Ordering::SeqCst);

    assert!(!circuit.is_open(), "precondition: breaker closed");

    // Five distinct paths (singleflight is per-path) → five Fetch claims →
    // five record(false) → threshold reached → open.
    for i in 0..5 {
        let basename = test_store_basename(&format!("circuit-fail-{i}"));
        let (c, cl, r, cb) = (
            Arc::clone(&cache),
            clients.clone(),
            rt.clone(),
            Arc::clone(&circuit),
        );
        let result = tokio::task::spawn_blocking(move || {
            prefetch_path_blocking(&c, &cl, &r, &cb, TEST_FETCH_TIMEOUT, &basename)
        })
        .await
        .expect("join");
        let err = result.expect_err("store unavailable → Err");
        assert_eq!(err.code(), Errno::EIO.code());
    }

    assert!(
        circuit.is_open(),
        "5 prefetch-owned failures MUST open the circuit; \
         before bug_207 fix prefetch never called record() → breaker blind"
    );
}

/// bug_207: when the breaker is open, prefetch MUST fail fast with EIO
/// WITHOUT contacting the store (no retry backoff observed).
#[tokio::test(flavor = "multi_thread")]
async fn test_prefetch_skipped_when_circuit_open() {
    let (cache, clients, store, _dir, rt, circuit, _srv) = setup_fetch_harness().await;
    // Arm Unavailable: if circuit.check() were skipped, we'd see retry→EIO
    // taking ≥ RETRY_BACKOFF total.
    store.faults.fail_get_path.store(true, Ordering::SeqCst);

    // Open the breaker directly (5× record(false)).
    for _ in 0..5 {
        circuit.record(false);
    }
    assert!(circuit.is_open(), "precondition: breaker open");

    let basename = test_store_basename("circuit-open");
    let (c, cl, r, cb) = (
        Arc::clone(&cache),
        clients.clone(),
        rt.clone(),
        Arc::clone(&circuit),
    );
    let start = std::time::Instant::now();
    let result = tokio::task::spawn_blocking(move || {
        prefetch_path_blocking(&c, &cl, &r, &cb, TEST_FETCH_TIMEOUT, &basename)
    })
    .await
    .expect("join");

    let err = result.expect_err("circuit open → Err(EIO)");
    assert_eq!(err.code(), Errno::EIO.code());
    // No retry backoff → never reached the gRPC loop.
    let backoff_floor: Duration = RETRY_BACKOFF.iter().sum::<Duration>().mul_f64(0.5);
    assert!(
        start.elapsed() < backoff_floor,
        "expected immediate EIO from circuit.check(), got {:?} \
         (≥ {backoff_floor:?} suggests gRPC retry loop ran)",
        start.elapsed()
    );
}

/// bug_363 observable behavior at the `prefetch_path_blocking` boundary:
/// a path already in `cached` MUST short-circuit to `AlreadyCached`
/// without contacting gRPC or touching the breaker. NOTE: this exercises
/// the mod.rs:446 fast-path, NOT the `try_start_fetch` TOCTOU re-check
/// (the synchronous `cache.insert` below means `get_path()` hits before
/// `try_start_fetch` is reached). The re-check itself is regression-
/// covered by `cache::tests::test_try_start_fetch_after_insert_returns_already_cached`
/// — do not delete that test as "redundant with this one".
#[tokio::test(flavor = "multi_thread")]
async fn test_concurrent_commit_then_claim_no_refetch() {
    let (cache, clients, store, _dir, rt, circuit, _srv) = setup_fetch_harness().await;
    // Arm the store to fail: if prefetch DID reach gRPC, we'd see retry
    // backoff. AlreadyCached skips gRPC entirely.
    store.faults.fail_get_path.store(true, Ordering::SeqCst);

    let basename = test_store_basename("toctou");
    // Path already committed; prefetch must fast-path skip.
    cache.insert(&basename);

    let (c, cl, r, cb, bn) = (
        Arc::clone(&cache),
        clients.clone(),
        rt.clone(),
        Arc::clone(&circuit),
        basename.clone(),
    );
    let start = std::time::Instant::now();
    let result = tokio::task::spawn_blocking(move || {
        prefetch_path_blocking(&c, &cl, &r, &cb, TEST_FETCH_TIMEOUT, &bn)
    })
    .await
    .expect("join");

    // Got AlreadyCached via the fast-path, NOT Err(EIO).
    assert!(
        matches!(result, Ok(Some(PrefetchSkip::AlreadyCached))),
        "already-cached path must short-circuit to AlreadyCached, got {result:?}"
    );
    // And it was immediate (no gRPC contact).
    assert!(
        start.elapsed() < Duration::from_millis(100),
        "AlreadyCached must skip gRPC, got {:?}",
        start.elapsed()
    );
    // Breaker untouched.
    assert!(!circuit.is_open());
}

/// bug_313: commit_to_cache's error-path cleanup must remove a regular-
/// file `tmp_path` (single-file NAR root: `toFile`, `writeText`, flat
/// `fetchurl`). Before the fix, `let _ = remove_dir_all(&tmp_path)`
/// returned `ENOTDIR` (swallowed) → the partial file leaked. On the
/// ENOSPC path that's a multi-GB leak worsening disk pressure.
///
/// Mechanism: write a single-file NAR to a spool, pre-create
/// `local_path` as a NON-EMPTY directory so the rename hits
/// `ENOTEMPTY` → the cleanup path runs. Assert no `*.tmp-*` files
/// remain in cache_dir.
#[test]
fn test_commit_to_cache_cleanup_removes_file_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = Cache::new(dir.path().to_path_buf()).expect("Cache::new");

    let basename = test_store_basename("file-root-cleanup");
    let local_path = dir.path().join(&basename);
    // Force rename failure: local_path is a non-empty directory.
    std::fs::create_dir(&local_path).unwrap();
    std::fs::write(local_path.join("blocker"), b"x").unwrap();

    // Spool a valid single-file NAR (root is a regular file, NOT a dir).
    let (nar, _hash) = make_nar(b"single-file payload");
    let spool_path = dir.path().join(format!("{basename}.nar-test"));
    std::fs::write(&spool_path, &nar).unwrap();

    let result = commit_to_cache(
        &cache,
        &spool_path,
        &local_path,
        &format!("/nix/store/{basename}"),
        &basename,
    );
    let err = result.expect_err("rename onto non-empty dir → Err");
    assert_eq!(err.code(), Errno::EIO.code());

    // The `*.tmp-*` extraction path (a regular file, since the NAR root
    // is a regular file) MUST be gone. Before the fix, remove_dir_all
    // failed ENOTDIR and the file leaked.
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.contains(".tmp-")))
        .collect();
    assert!(
        leftovers.is_empty(),
        "tmp file (single-file NAR root) must be removed on error path; \
         found: {leftovers:?}"
    );
}
