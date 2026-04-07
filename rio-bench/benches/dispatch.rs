//! Dispatch throughput: how fast the scheduler drains a ready queue
//! when workers are infinitely fast (mocked: instant completion).
//!
//! What this measures: the scheduler's ready-scan + dispatch + DB
//! update loop, isolated from actual build execution. A mock worker
//! task receives `SchedulerMessage::Assignment` and immediately
//! loops back a `ProcessCompletion` — so the end-to-end clock is
//! pure scheduler overhead: ready queue pop, assignment write to
//! the worker stream, completion handler, DAG state transition,
//! dependent-becomes-ready cascade, DB write.
//!
//! This bypasses gRPC and drives the actor directly via
//! `ActorCommand`: the dispatch path IS the actor command loop;
//! gRPC is just a thin shell around `ActorCommand::MergeDag`.
//! Keeping gRPC out removes connection overhead from the number
//! we care about.
//!
//! Seeded with `binary_tree(depth)`: at depth=10, 512 leaves
//! enter ready-at-once, then completions cascade up level by
//! level. Time-to-drain is from MergeDag-accepted to the
//! `BuildCompleted` event on the build's broadcast channel.

use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tokio::sync::{mpsc, oneshot};

use rio_bench::{BenchHarness, Dag, binary_tree};
use rio_proto::types::{
    BuildEvent, BuildResult, BuildResultStatus, BuiltOutput, SchedulerMessage, build_event,
    scheduler_message,
};
use rio_scheduler::actor::{ActorCommand, ActorHandle, MergeDagRequest};
use rio_scheduler::state::{BuildOptions, PriorityClass};

/// A registered infinitely-fast mock worker. Holds the scheduler→
/// worker channel receiver; [`run_until_drained`] consumes it.
struct InstantWorker {
    id: String,
    stream_rx: mpsc::Receiver<SchedulerMessage>,
}

impl InstantWorker {
    /// Connect + heartbeat a worker. P0537: capacity is binary (one
    /// build per pod), so "unlimited slots" is no longer expressible.
    /// The bench compensates with the instant-ack `recv_loop` below —
    /// completion frees the slot before the next dispatch tick, so
    /// the scheduler's ceiling is still what's measured.
    async fn connect(actor: &ActorHandle, executor_id: &str) -> anyhow::Result<Self> {
        // Deep channel: binary_tree(10) has 512 leaves ready at once.
        // The actor sends all 512 assignments in one dispatch burst
        // before the mock worker's recv loop gets a chance to run
        // (both are on the same runtime; the actor task holds the
        // scheduler until it awaits). 2048 covers any realistic
        // bench depth.
        let (stream_tx, stream_rx) = mpsc::channel(2048);
        actor
            .send_unchecked(ActorCommand::ExecutorConnected {
                executor_id: executor_id.into(),
                stream_tx,
            })
            .await?;
        actor
            .send_unchecked(ActorCommand::Heartbeat {
                executor_id: executor_id.into(),
                systems: vec!["x86_64-linux".into()],
                supported_features: vec![],
                running_builds: vec![],
                size_class: None,
                resources: None,
                store_degraded: false,
                draining: false,
                kind: rio_proto::types::ExecutorKind::Builder,
            })
            .await?;
        Ok(Self {
            id: executor_id.into(),
            stream_rx,
        })
    }

    /// Drain: receive assignments, loop back instant completions,
    /// until `n_completions` have been sent.
    ///
    /// Reads `stream_rx` in a loop; for each `Assignment` received,
    /// synthesizes a `BuildResultStatus::Built` completion and
    /// sends it straight back to the actor. Once `n_completions`
    /// have fired, disconnects the worker (so subsequent
    /// iterations can re-register the same ID).
    ///
    /// `n_completions` should equal the node count of the DAG
    /// being drained — the loop terminates there. An early return
    /// on channel close is defensive (shouldn't happen under
    /// normal bench operation).
    async fn run_until_drained(mut self, actor: &ActorHandle, n_completions: usize) {
        let mut done = 0usize;
        while done < n_completions {
            let Some(msg) = self.stream_rx.recv().await else {
                // Scheduler dropped the stream — bench is over.
                break;
            };
            let Some(scheduler_message::Msg::Assignment(a)) = msg.msg else {
                // CancelSignal / PrefetchHint — ignore, not a unit of work.
                continue;
            };
            // Instant completion. built_outputs: one synthetic output
            // per output_name, valid-shaped store path so downstream
            // StorePath::parse (if any) doesn't reject. output_hash
            // is 32 zero bytes — the scheduler stores it, never
            // verifies it.
            let result = BuildResult {
                status: BuildResultStatus::Built.into(),
                error_msg: String::new(),
                times_built: 1,
                start_time: None,
                stop_time: None,
                built_outputs: a
                    .output_names
                    .iter()
                    .map(|name| BuiltOutput {
                        output_name: name.clone(),
                        output_path: format!("/nix/store/{}-out", &a.drv_path[11..43]),
                        output_hash: vec![0u8; 32],
                    })
                    .collect(),
            };
            // send_unchecked: completions bypass backpressure the
            // same way the real gRPC BuildExecution recv loop does
            // (grpc/mod.rs uses send_unchecked for ProcessCompletion).
            // Fire-and-forget: no reply channel on ProcessCompletion.
            if actor
                .send_unchecked(ActorCommand::ProcessCompletion {
                    executor_id: self.id.clone().into(),
                    drv_key: a.drv_path,
                    result,
                    peak_memory_bytes: 0,
                    output_size_bytes: 0,
                    peak_cpu_cores: 0.0,
                })
                .await
                .is_err()
            {
                break;
            }
            done += 1;
        }
        // Free up the worker ID for the next iteration. Without
        // this, re-registering "bench-worker" on the next iter
        // either hangs (actor thinks worker is still connected)
        // or the old stream_rx gets the assignments meant for
        // the new DAG.
        let _ = actor
            .send_unchecked(ActorCommand::ExecutorDisconnected {
                executor_id: self.id.into(),
            })
            .await;
    }
}

/// One full drain: connect worker, merge DAG, race worker drain
/// against the BuildCompleted event. Returns elapsed wall-clock.
///
/// `iter_custom` wants us to do our own timing (criterion's auto-
/// timing is for simple closures; here the worker-connect setup
/// shouldn't be on the clock).
async fn drain_once(actor: &ActorHandle, dag: Dag) -> Duration {
    let n = dag.nodes.len();
    // Setup: connect the mock worker. NOT on the clock.
    let worker = InstantWorker::connect(actor, "bench-worker")
        .await
        .expect("connect mock worker");

    // --- clock starts ---
    let t0 = Instant::now();

    // Merge. The reply comes back once the DAG is in memory + ready
    // queue seeded. Dispatch starts firing immediately after.
    let (reply_tx, reply_rx) = oneshot::channel();
    actor
        .send_unchecked(ActorCommand::MergeDag {
            req: MergeDagRequest {
                build_id: uuid::Uuid::now_v7(),
                tenant_id: None,
                priority_class: PriorityClass::Scheduled,
                nodes: dag.nodes,
                edges: dag.edges,
                options: BuildOptions::default(),
                keep_going: false,
                traceparent: String::new(),
                jti: None,
                jwt_token: None,
            },
            reply: reply_tx,
        })
        .await
        .expect("send MergeDag");
    let mut events = reply_rx
        .await
        .expect("MergeDag reply")
        .expect("MergeDag succeeded");

    // Spawn the worker drain concurrently: assignments arrive on
    // its stream_rx as soon as the actor's dispatch loop picks up
    // ready nodes. The actor task, this task, and the worker task
    // are all on the same runtime; tokio's scheduler interleaves
    // them at await points.
    let actor_for_worker = actor.clone();
    let drain = tokio::spawn(async move {
        worker.run_until_drained(&actor_for_worker, n).await;
    });

    // Wait for terminal: BuildCompleted on the broadcast channel.
    // BuildFailed should never appear (all our completions are
    // status=Built); if it does, that's a bench bug — panic.
    loop {
        match events.recv().await {
            Ok(BuildEvent {
                event: Some(build_event::Event::Completed(_)),
                ..
            }) => break,
            Ok(BuildEvent {
                event: Some(build_event::Event::Failed(f)),
                ..
            }) => panic!("build failed unexpectedly: {f:?}"),
            Ok(_) => {} // progress/log/derivation events — ignore
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                // Broadcast ring overflow — we're slow at draining
                // events. Keep going; the terminal event will still
                // arrive (broadcast sender lives until cleanup delay).
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                panic!("event broadcast closed before terminal")
            }
        }
    }

    let elapsed = t0.elapsed();
    // --- clock stops ---

    let _ = drain.await;
    elapsed
}

fn bench_dispatch(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    let harness = rt.block_on(BenchHarness::spawn()).expect("spawn harness");
    let actor = harness.actor.clone();

    let mut group = c.benchmark_group("dispatch/binary_tree_drain");
    // Shared harness across iterations: DB rows accumulate (no
    // teardown between iters). Criterion's default sample count
    // (~100) at depth=10 writes ~100K derivation rows over the
    // run. Later iterations see a bigger DB, which skews the tail
    // of the distribution slightly high — but that's arguably
    // closer to a long-running scheduler's conditions than a
    // pristine DB every iter. For a true isolated measurement,
    // set `--sample-size 10` or drop depth.
    //
    // --test mode runs 1 iteration, so the CI smoke check stays fast.
    for depth in [4usize, 7, 10] {
        let n = (1usize << depth) - 1;
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(depth), &depth, |b, &depth| {
            // iter_custom: we time manually (worker-connect is
            // setup, not bench work). Criterion tells us how many
            // iterations to run (`iters`); we sum elapsed time.
            b.iter_custom(|iters| {
                rt.block_on(async {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let dag = binary_tree(depth);
                        total += drain_once(&actor, dag).await;
                    }
                    total
                })
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_dispatch);
criterion_main!(benches);
