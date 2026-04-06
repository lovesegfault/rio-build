//! SubmitBuild latency: wall-clock from gRPC call start to response
//! headers received.
//!
//! What this measures: the scheduler's MergeDag path — validation,
//! in-memory DAG insert + BFS to seed the ready queue, and the
//! `builds`/`derivations` DB insert. The `.await` on a tonic
//! server-streaming call resolves when the server sends initial
//! metadata (the `x-rio-build-id` header), which the handler does
//! AFTER the actor's MergeDag reply comes back. So the measured
//! interval captures exactly: gRPC round-trip + validation + actor
//! queue + MergeDag.
//!
//! What this does NOT measure: event stream delivery, dispatch,
//! build execution. The stream is dropped immediately.
//!
//! Phase 4c target: p99 < 50ms @ N=100. Not a gate — the deliverable
//! is this harness. See docs/src/capacity-planning.md § Benchmarking
//! against Hydra.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use rio_bench::{BenchHarness, Dag, binary_tree, linear, shared_diamond};

/// Run one iteration: fresh DAG, submit, drop the stream.
///
/// Fresh DAG every iteration: the generators use `rand_hash()` so
/// drv_hashes are unique across calls. Reusing a pre-built DAG would
/// hit MergeDag's dedup path after the first iteration and measure
/// a cache hit (near-zero cost) instead of a cold insert.
async fn submit_once(harness: &BenchHarness, build: impl FnOnce() -> Dag) {
    let mut client = harness.client.clone();
    let req = build().into_request();
    // Timing point: this .await resolves when the scheduler sends
    // initial response headers (post-MergeDag). The returned stream
    // is the BuildEvent feed — we drop it; we're measuring latency
    // to acceptance, not event delivery.
    let _stream = client
        .submit_build(req)
        .await
        .expect("SubmitBuild should accept a valid synthetic DAG");
}

fn bench_submit_build(c: &mut Criterion) {
    // Multi-threaded runtime: the harness runs the actor task, the
    // gRPC server task, and the client connection all concurrently.
    // A current-thread runtime would serialize them and distort the
    // measurement.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    // ONE harness for the whole bench group. TestDb bootstrap (initdb
    // + spawn postgres + migrate) is ~2s — amortize it. Each iteration
    // inserts into the SAME database; rows accumulate (no per-iter
    // teardown) so DB size grows over the run. That's actually closer
    // to production conditions than a pristine DB every iteration.
    //
    // Criterion's --test mode runs 1 iteration so the smoke check
    // doesn't accumulate meaningful state.
    let harness = rt.block_on(BenchHarness::spawn()).expect("spawn harness");

    // --- linear chain ---
    {
        let mut group = c.benchmark_group("submit_build/linear");
        for n in [1usize, 10, 100, 1000] {
            group.throughput(Throughput::Elements(n as u64));
            group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
                b.to_async(&rt).iter(|| submit_once(&harness, || linear(n)));
            });
        }
        group.finish();
    }

    // --- binary tree: fan-out at merge (all leaves enter ready queue at once) ---
    {
        let mut group = c.benchmark_group("submit_build/binary_tree");
        // depth=10 → 1023 nodes. Comparable to linear(1000) but
        // with a very different edge structure (n-1 edges forming
        // a tree vs n-1 edges forming a chain). Comparing these two
        // at similar N isolates whether MergeDag's BFS cost is
        // shape-sensitive.
        for depth in [1usize, 4, 7, 10] {
            let n = (1usize << depth) - 1;
            group.throughput(Throughput::Elements(n as u64));
            group.bench_with_input(BenchmarkId::from_parameter(depth), &depth, |b, &depth| {
                b.to_async(&rt)
                    .iter(|| submit_once(&harness, || binary_tree(depth)));
            });
        }
        group.finish();
    }

    // --- shared diamond: many edges converging on one node ---
    {
        let mut group = c.benchmark_group("submit_build/shared_diamond");
        for &(n, frac) in &[(100usize, 0.1f64), (100, 0.5), (100, 0.9)] {
            group.throughput(Throughput::Elements((n + 1) as u64));
            group.bench_with_input(
                BenchmarkId::new("n100", format!("frac{frac}")),
                &(n, frac),
                |b, &(n, frac)| {
                    b.to_async(&rt)
                        .iter(|| submit_once(&harness, || shared_diamond(n, frac)));
                },
            );
        }
        group.finish();
    }
}

criterion_group!(benches, bench_submit_build);
criterion_main!(benches);
