//! Ingestion / upload throughput benchmark for rio-store.
//!
//! Measures the `PutPath` gRPC streaming write path end-to-end: NAR
//! framing → 256 KiB stream chunks → server-side trailer hash check →
//! placeholder claim → inline blob OR FastCDC chunking → CAS write →
//! manifest commit. Reports criterion `Throughput::Bytes` so the
//! summary line reads in MiB/s.
//!
//! Both groups use **real network clients** against **real running
//! servers** — no in-process shims, no `MemoryChunkBackend`. The bench
//! does NOT spin up the targets; bring them up first (see
//! `process-compose-bench.yaml`).
//!
//! Two groups:
//!
//!   - **`ingest`** (`RIO_STORE_ADDR=host:port`): full PutPath path —
//!     gRPC stream → trailer hash check → placeholder claim → FastCDC
//!     chunking → S3 PutObject per chunk → PG manifest commit.
//!
//!   - **`s3-put`** (`BENCH_S3_BUCKET` + standard `AWS_*`): the same
//!     NAR-framed payloads pushed straight to S3 with one `PutObject`
//!     each. No chunking, no PG, no gRPC. This is the *floor* of what
//!     the object store can do; the gap to `ingest` is rio-store's
//!     overhead (protocol + chunker + metadata).
//!
//! Point both at the same minio so the object store is held constant
//! and the diff isolates rio-store. Optional env:
//!
//!   - `BENCH_S3_ENDPOINT` — non-AWS endpoint; forces path-style.
//!   - `BENCH_S3_PREFIX`  — key prefix, default `rio-bench/`.
//!
//! Each iteration uploads a NAR under a *fresh random* store path so
//! the placeholder/dedup short-circuit never fires — every iter
//! exercises the full write path. Objects are NOT cleaned up; use a
//! scratch bucket and a throwaway PG.
//!
//! ## Usage
//!
//! ```text
//! # 1. bring up postgres + minio + rio-store-with-s3-backend
//! process-compose -f process-compose-bench.yaml up
//!
//! # 2. in another terminal
//! RIO_STORE_ADDR=127.0.0.1:9002 \
//! BENCH_S3_BUCKET=rio-bench-baseline \
//! BENCH_S3_ENDPOINT=http://127.0.0.1:19000 \
//! AWS_ACCESS_KEY_ID=bench AWS_SECRET_ACCESS_KEY=benchbench AWS_REGION=us-east-1 \
//!   cargo bench -p rio-store --features test-utils --bench ingest
//!
//! # subset by name
//! ... --bench ingest -- '16MiB'
//! ```
//!
//! Inspired by <https://github.com/Mic92/cache-shootout>, which
//! measures the *download* side of binary caches; this is the upload
//! mirror for rio-store.

use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use futures_util::stream::{self, StreamExt};
use rand::RngExt;
use tonic::transport::Channel;

use rio_proto::StoreServiceClient;
use rio_proto::client::chunk_nar_for_put;
use rio_proto::validated::ValidatedPathInfo;
use rio_test_support::fixtures::{make_nar, make_path_info, pseudo_random_bytes, rand_store_hash};

// ---------------------------------------------------------------------------
// Workload definitions
// ---------------------------------------------------------------------------

/// (label, payload bytes). Spans the inline/chunked boundary
/// (`INLINE_THRESHOLD = 256 KiB`):
///   - 4 KiB / 64 KiB → inline blob (single PG row write)
///   - 1 MiB / 16 MiB → FastCDC chunked into the CAS backend
///   - 128 MiB → many chunks; exercises streaming backpressure & the
///     NAR byte budget semaphore
const NAR_SIZES: &[(&str, usize)] = &[
    ("4KiB", 4 * 1024),
    ("64KiB", 64 * 1024),
    ("1MiB", 1024 * 1024),
    ("16MiB", 16 * 1024 * 1024),
    ("128MiB", 128 * 1024 * 1024),
];

/// Concurrent uploaders. 1 = latency baseline; >1 shows whether the
/// server's PG pool / CAS backend / chunker scale.
const CONCURRENCIES: &[usize] = &[1, 4, 16];

/// Target wall-clock spend per (size × concurrency) cell. Criterion
/// estimates iter count from this; large NARs at low concurrency would
/// otherwise run for minutes per cell.
const MEASUREMENT_TIME: Duration = Duration::from_secs(8);
const SAMPLE_SIZE: usize = 10;

// ---------------------------------------------------------------------------
// rio-store target
// ---------------------------------------------------------------------------

/// gRPC client to a running rio-store. `StoreServiceClient<Channel>`
/// is cheap to clone (h2 multiplexes over one TCP connection);
/// concurrent uploaders each clone so they can `.put_path()`
/// independently without `&mut` fights.
struct RioTarget {
    client: StoreServiceClient<Channel>,
    addr: String,
}

async fn build_rio_target() -> Option<RioTarget> {
    let addr = std::env::var("RIO_STORE_ADDR").ok()?;
    let client = rio_proto::client::connect_single(&addr)
        .await
        .unwrap_or_else(|e| panic!("connect to RIO_STORE_ADDR={addr}: {e}"));
    Some(RioTarget { client, addr })
}

// ---------------------------------------------------------------------------
// Upload primitive
// ---------------------------------------------------------------------------

/// Wrap a payload in NAR framing under a fresh random store path.
///
/// Random hash, NOT the `/nix/store/aaaa…` test fixture default —
/// criterion iterates and the store dedupes on store-path hash, so a
/// fixed path would short-circuit every iter after the first into the
/// "already exists" branch and we'd be benchmarking PG SELECT.
fn fresh_nar(seed: u64, size: usize) -> (Arc<[u8]>, ValidatedPathInfo) {
    let payload = pseudo_random_bytes(seed, size);
    let (nar, hash) = make_nar(&payload);
    let path = format!("/nix/store/{}-bench-{seed:016x}", rand_store_hash());
    let info = make_path_info(&path, &nar, hash);
    (nar.into(), info)
}

/// One `PutPath` round-trip. Panics on a non-OK status so a misconfigured
/// target (HMAC enabled, store down) fails loudly instead of recording
/// "fast" timings for rejected requests.
async fn put_one(mut client: StoreServiceClient<Channel>, info: ValidatedPathInfo, nar: Arc<[u8]>) {
    let stream = chunk_nar_for_put(info, nar);
    let resp = client.put_path(stream).await.expect("PutPath");
    // `created == false` means the path already existed — should never
    // happen with random hashes, and would mean we measured a no-op.
    assert!(resp.into_inner().created, "PutPath dedup'd a random path");
}

/// Upload `concurrency` NARs of `size` bytes in parallel, fresh path
/// each. Used as the iter body under `iter_custom` so we time exactly
/// the upload, not the NAR build.
async fn upload_batch(target: &RioTarget, size: usize, concurrency: usize) -> Duration {
    // Build all NARs *before* the clock starts. Random seeds → distinct
    // payloads so FastCDC produces distinct chunks (no cross-iter CAS
    // dedup masking the chunker cost).
    let mut rng = rand::rng();
    let work: Vec<_> = (0..concurrency)
        .map(|_| fresh_nar(rng.random(), size))
        .collect();

    let start = Instant::now();
    stream::iter(work)
        .for_each_concurrent(concurrency, |(nar, info)| {
            let client = target.client.clone();
            async move { put_one(client, info, nar).await }
        })
        .await;
    start.elapsed()
}

// ---------------------------------------------------------------------------
// Raw S3 baseline (opt-in)
// ---------------------------------------------------------------------------

/// `aws_sdk_s3::Client` + bucket + key prefix. Built once; the SDK
/// client is `Clone` and pools HTTP connections internally, mirroring
/// what `S3ChunkBackend` does in production.
struct S3Target {
    client: aws_sdk_s3::Client,
    bucket: String,
    prefix: String,
}

async fn build_s3_target() -> Option<S3Target> {
    let bucket = std::env::var("BENCH_S3_BUCKET").ok()?;
    let prefix = std::env::var("BENCH_S3_PREFIX").unwrap_or_else(|_| "rio-bench/".into());

    let base = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let mut cfg = aws_sdk_s3::config::Builder::from(&base);
    if let Ok(endpoint) = std::env::var("BENCH_S3_ENDPOINT") {
        // minio/rustfs need path-style; virtual-hosted would resolve
        // `<bucket>.127.0.0.1` and fail DNS.
        cfg = cfg.endpoint_url(endpoint).force_path_style(true);
    }
    Some(S3Target {
        client: aws_sdk_s3::Client::from_conf(cfg.build()),
        bucket,
        prefix,
    })
}

/// Single-shot `PutObject`. Fresh key per call (mirrors the fresh
/// store path in [`put_one`]) so server-side dedup / overwrite
/// fast-paths can't fire. Body is the *NAR-framed* payload, not the
/// raw bytes — apples-to-apples with what rio-store actually receives.
async fn s3_put_one(t: &S3Target, key: String, nar: Arc<[u8]>) {
    // SDK takes `Bytes`; `Arc<[u8]>` → `Vec<u8>` copy is unavoidable
    // here. Same copy exists on the rio-store side (gRPC chunking),
    // so the comparison stays fair.
    let body = aws_sdk_s3::primitives::ByteStream::from(nar.to_vec());
    t.client
        .put_object()
        .bucket(&t.bucket)
        .key(key)
        .body(body)
        .send()
        .await
        .expect("S3 PutObject");
}

/// S3 sibling of [`upload_batch`]: `concurrency` parallel `PutObject`s
/// of `size` bytes each, fresh keys.
async fn s3_upload_batch(t: &S3Target, size: usize, concurrency: usize) -> Duration {
    let mut rng = rand::rng();
    let work: Vec<_> = (0..concurrency)
        .map(|_| {
            let seed: u64 = rng.random();
            let (nar, _info) = fresh_nar(seed, size);
            let key = format!("{}{seed:016x}.nar", t.prefix);
            (key, nar)
        })
        .collect();

    let start = Instant::now();
    stream::iter(work)
        .for_each_concurrent(concurrency, |(key, nar)| async move {
            s3_put_one(t, key, nar).await;
        })
        .await;
    start.elapsed()
}

fn bench_s3_put(c: &mut Criterion, rt: &tokio::runtime::Runtime) {
    let Some(t) = rt.block_on(build_s3_target()) else {
        eprintln!("BENCH_S3_BUCKET unset — skipping s3-put baseline");
        return;
    };
    eprintln!(
        "s3-put bench target: bucket={} prefix={}",
        t.bucket, t.prefix
    );

    let mut group = c.benchmark_group("s3-put");
    group.measurement_time(MEASUREMENT_TIME);
    group.sample_size(SAMPLE_SIZE);

    for &(size_label, size) in NAR_SIZES {
        for &conc in CONCURRENCIES {
            group.throughput(Throughput::Bytes((size * conc) as u64));
            let id = BenchmarkId::new(size_label, format!("c{conc}"));
            group.bench_function(id, |b| {
                b.to_async(rt).iter_custom(|iters| {
                    let t = &t;
                    async move {
                        let mut total = Duration::ZERO;
                        for _ in 0..iters {
                            total += s3_upload_batch(t, size, conc).await;
                        }
                        total
                    }
                });
            });
        }
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Criterion entrypoint
// ---------------------------------------------------------------------------

fn bench_rio_ingest(c: &mut Criterion, rt: &tokio::runtime::Runtime) {
    let Some(target) = rt.block_on(build_rio_target()) else {
        eprintln!("RIO_STORE_ADDR unset — skipping ingest group");
        return;
    };
    eprintln!("ingest bench target: {}", target.addr);

    let mut group = c.benchmark_group("ingest");
    group.measurement_time(MEASUREMENT_TIME);
    group.sample_size(SAMPLE_SIZE);

    for &(size_label, size) in NAR_SIZES {
        for &conc in CONCURRENCIES {
            // Throughput = total payload bytes pushed per criterion
            // iter (one batch of `conc` parallel uploads).
            group.throughput(Throughput::Bytes((size * conc) as u64));
            let id = BenchmarkId::new(size_label, format!("c{conc}"));
            group.bench_function(id, |b| {
                b.to_async(rt).iter_custom(|iters| {
                    let target = &target;
                    async move {
                        let mut total = Duration::ZERO;
                        for _ in 0..iters {
                            total += upload_batch(target, size, conc).await;
                        }
                        total
                    }
                });
            });
        }
    }
    group.finish();
}

fn bench_main(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let have_rio = std::env::var("RIO_STORE_ADDR").is_ok();
    let have_s3 = std::env::var("BENCH_S3_BUCKET").is_ok();
    assert!(
        have_rio || have_s3,
        "set RIO_STORE_ADDR and/or BENCH_S3_BUCKET — see module docs.\n\
         For both, run `process-compose -f process-compose-bench.yaml up` first."
    );

    bench_s3_put(c, &rt);
    bench_rio_ingest(c, &rt);
}

criterion_group!(benches, bench_main);
criterion_main!(benches);
