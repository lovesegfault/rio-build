//! Actor + Postgres stress benchmark.
//!
//! Drives the full DAG-actor pipeline (merge → dispatch → completion)
//! against an ephemeral Postgres so the load profile matches
//! production: `batch_upsert_derivations` UNNEST inserts, per-dispatch
//! `insert_assignment`, per-completion `update_derivation_status`
//! transactions, denormalized count updates, event-log writes.
//!
//! ```sh
//! cargo xtask bench capture --repo R --repo-root D --targets-file F \
//!   --nix-eval-jobs B --out-dag dag.json --out-traffic traffic.json
//! BENCH_DAG=dag.json BENCH_TRAFFIC=traffic.json \
//!   cargo run --release -p rio-scheduler --example actor_pg_bench
//! ```
//!
//! | env                | default | meaning                            |
//! |--------------------|---------|------------------------------------|
//! | `BENCH_DAG`        | (req'd) | real-DAG JSON fixture              |
//! | `BENCH_TRAFFIC`    | (unset) | PR-traffic JSON fixture            |
//! | `BENCH_BUILDS`     | 50      | builds (or traffic events) to run  |
//! | `BENCH_WORKERS`    | 8       | concurrent connected executors     |
//! | `BENCH_PARALLEL`   | 8       | concurrent submitters (no traffic) |
//! | `BENCH_RATE_SCALE` | 20.0    | traffic-replay speed-up factor     |
//!
//! Without `BENCH_TRAFFIC`, every build replays the full fixture graph
//! as fast as the actor accepts them. With it, the bench replays the
//! real PR inter-arrival sequence and submits only the subgraph
//! reachable from each PR's touched services.
//!
//! `BENCH_RATE_SCALE` controls how compressed the replay is. The
//! drain phase plateaus around ~180 drvs/s on a single fsync'd
//! Postgres (per-completion COMMIT bound) — that's the throughput
//! ceiling under sustained load, regardless of scale.
//!
//! Cached nodes (substitutable from a binary cache, marked at
//! extraction) are cut before merge — the gateway never submits
//! cache-hit derivations, so the dispatch profile matches production.
//!
//! By default the bench bootstraps an ephemeral local Postgres tuned
//! for test speed (`fsync=off`, unix socket, 5-conn pool). For a
//! production-shaped run, set `DATABASE_URL` to a real Postgres — the
//! same `TestDb` machinery skips bootstrap, creates an isolated
//! `rio_test_*` database there, and drops it on exit. Real fsync,
//! real network latency, real lock contention. Note the bench's pool
//! is test-tuned (5 connections) vs production's 10 — not a
//! bottleneck for the actor (single-threaded mailbox), but worth
//! knowing when reading `pg_stat_activity`.
//!
//! Load generator, not a Criterion harness — attach
//! `pg_stat_statements` / flamegraph externally.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::{Mutex, mpsc, oneshot};
use uuid::Uuid;

use rio_proto::types::{
    DerivationEdge, DerivationNode, ExecutorKind, SchedulerMessage, scheduler_message::Msg,
};
use rio_scheduler::actor::{
    ActorCommand, ActorHandle, DagActorConfig, DagActorPlumbing, HeartbeatPayload, MergeDagRequest,
};
use rio_scheduler::db::SchedulerDb;
use rio_scheduler::state::{BuildOptions, PriorityClass};
use rio_test_support::TestDb;

fn env<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ─── Fixtures ────────────────────────────────────────────────────────

/// DAG fixture from `xtask bench capture`. Shape-only: no .drv
/// content, no output paths.
#[derive(Deserialize)]
struct DagFixture {
    /// Service-name → root drv_hash. Required for `BENCH_TRAFFIC`.
    #[serde(default)]
    roots: BTreeMap<String, String>,
    nodes: Vec<FixtureNode>,
    /// `[parent, child]` hash pairs; parent depends on child.
    edges: Vec<(String, String)>,
}

#[derive(Deserialize)]
struct FixtureNode {
    drv_hash: String,
    drv_path: String,
    pname: String,
    system: String,
    output_names: Vec<String>,
    is_fixed_output: bool,
    #[serde(default)]
    required_features: Vec<String>,
    /// Substitutable from a binary cache → cut before merge.
    #[serde(default)]
    cached: bool,
}

/// Traffic fixture from `xtask bench capture`. Carries no
/// repo-identifying data.
#[derive(Deserialize)]
struct TrafficFixture {
    events: Vec<TrafficEvent>,
}

#[derive(Deserialize, Clone)]
struct TrafficEvent {
    gap_secs: f64,
    services: Vec<String>,
}

// ─── DAG template ────────────────────────────────────────────────────

struct Dag {
    nodes: Vec<DerivationNode>,
    edges: Vec<DerivationEdge>,
    /// Service name → index into `nodes`.
    roots: HashMap<String, usize>,
    /// Adjacency: `deps[i]` = indices of node `i`'s dependencies.
    deps: Vec<Vec<usize>>,
    systems: Vec<String>,
    features: Vec<String>,
}

impl Dag {
    fn load(path: &str) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let mut f: DagFixture = serde_json::from_str(&raw)?;
        let total = f.nodes.len();

        // Cut cached nodes; keep edges where both endpoints survive.
        // A node depending on a cached parent becomes a root (Ready
        // immediately) — exactly the gateway's missing-drvs probe.
        f.nodes.retain(|n| !n.cached);
        let kept: HashSet<&str> = f.nodes.iter().map(|n| n.drv_hash.as_str()).collect();
        f.edges
            .retain(|(p, c)| kept.contains(p.as_str()) && kept.contains(c.as_str()));
        eprintln!(
            "BENCH_DAG: {} nodes ({} cached cut), {} edges",
            f.nodes.len(),
            total - f.nodes.len(),
            f.edges.len()
        );

        let idx: HashMap<&str, usize> = f
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.drv_hash.as_str(), i))
            .collect();
        let path_of: HashMap<&str, &str> = f
            .nodes
            .iter()
            .map(|n| (n.drv_hash.as_str(), n.drv_path.as_str()))
            .collect();
        let roots = f
            .roots
            .iter()
            .filter_map(|(name, h)| Some((name.clone(), *idx.get(h.as_str())?)))
            .collect();
        let mut deps = vec![vec![]; f.nodes.len()];
        for (p, c) in &f.edges {
            if let (Some(&pi), Some(&ci)) = (idx.get(p.as_str()), idx.get(c.as_str())) {
                deps[pi].push(ci);
            }
        }
        let collect_set = |it: &mut dyn Iterator<Item = String>| -> Vec<String> {
            it.collect::<BTreeSet<_>>().into_iter().collect()
        };
        let systems = collect_set(&mut f.nodes.iter().map(|n| n.system.clone()));
        let features = collect_set(
            &mut f
                .nodes
                .iter()
                .flat_map(|n| n.required_features.iter().cloned()),
        );
        let nodes = f
            .nodes
            .iter()
            .map(|n| DerivationNode {
                drv_hash: n.drv_hash.clone(),
                drv_path: n.drv_path.clone(),
                pname: n.pname.clone(),
                system: n.system.clone(),
                required_features: n.required_features.clone(),
                output_names: n.output_names.clone(),
                is_fixed_output: n.is_fixed_output,
                ..Default::default()
            })
            .collect();
        let edges = f
            .edges
            .iter()
            .filter_map(|(p, c)| {
                Some(DerivationEdge {
                    parent_drv_path: (*path_of.get(p.as_str())?).into(),
                    child_drv_path: (*path_of.get(c.as_str())?).into(),
                })
            })
            .collect();
        Ok(Dag {
            nodes,
            edges,
            roots,
            deps,
            systems,
            features,
        })
    }

    /// Subgraph reachable from `services`' roots (BFS over `deps`).
    fn subgraph(&self, services: &[String]) -> (Vec<DerivationNode>, Vec<DerivationEdge>) {
        let mut seen = vec![false; self.nodes.len()];
        let mut q: Vec<usize> = services
            .iter()
            .filter_map(|s| self.roots.get(s).copied())
            .collect();
        for &i in &q {
            seen[i] = true;
        }
        while let Some(i) = q.pop() {
            for &c in &self.deps[i] {
                if !seen[c] {
                    seen[c] = true;
                    q.push(c);
                }
            }
        }
        let nodes: Vec<_> = self
            .nodes
            .iter()
            .zip(&seen)
            .filter(|&(_, &s)| s)
            .map(|(n, _)| n.clone())
            .collect();
        let kept: HashSet<&str> = nodes.iter().map(|n| n.drv_path.as_str()).collect();
        let edges = self
            .edges
            .iter()
            .filter(|e| {
                kept.contains(e.parent_drv_path.as_str())
                    && kept.contains(e.child_drv_path.as_str())
            })
            .cloned()
            .collect();
        (nodes, edges)
    }
}

// ─── Bench ───────────────────────────────────────────────────────────

#[derive(Default)]
struct Counters {
    merges: AtomicU64,
    merge_ns: AtomicU64,
    assignments: AtomicU64,
    completions: AtomicU64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let builds: usize = env("BENCH_BUILDS", 50);
    let workers: usize = env("BENCH_WORKERS", 8);
    let parallel: usize = env("BENCH_PARALLEL", 8);
    let rate_scale: f64 = env("BENCH_RATE_SCALE", 20.0);

    let dag_path = std::env::var("BENCH_DAG")
        .map_err(|_| anyhow::anyhow!("BENCH_DAG is required (run `xtask bench capture`)"))?;
    let dag = Arc::new(Dag::load(&dag_path)?);

    let traffic: Option<Vec<TrafficEvent>> = match std::env::var("BENCH_TRAFFIC") {
        Ok(p) => {
            anyhow::ensure!(
                !dag.roots.is_empty(),
                "BENCH_TRAFFIC needs a multi-root fixture (re-extract with NAME=DRV pairs)"
            );
            let f: TrafficFixture = serde_json::from_str(&std::fs::read_to_string(&p)?)?;
            let total = f.events.len();
            let events: Vec<_> = f
                .events
                .into_iter()
                .filter(|e| e.services.iter().any(|s| dag.roots.contains_key(s)))
                .take(builds)
                .collect();
            eprintln!(
                "BENCH_TRAFFIC: {total} events, {} match a root, {} replayed",
                events.len(),
                events.len()
            );
            Some(events)
        }
        Err(_) => None,
    };

    // Drain target = unique drvs that will actually be submitted.
    let total_drvs = match &traffic {
        Some(ev) => {
            let svcs: Vec<_> = ev
                .iter()
                .flat_map(|e| e.services.iter().cloned())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            dag.subgraph(&svcs).0.len()
        }
        None => dag.nodes.len(),
    };

    eprintln!(
        "actor_pg_bench: builds={builds} workers={workers} drvs={total_drvs} \
         systems={:?} features={:?} mode={}",
        dag.systems,
        dag.features,
        if traffic.is_some() {
            "traffic"
        } else {
            "saturate"
        },
    );

    let db = TestDb::new(&rio_scheduler::MIGRATOR).await;
    let handle = ActorHandle::spawn(
        SchedulerDb::new(db.pool.clone()),
        DagActorConfig::default(),
        DagActorPlumbing::default(),
    );
    let counters = Arc::new(Counters::default());

    // Workers: half builders, half fetchers (FODs route to fetchers).
    // Each is one-shot — completes one build, reconnects with a fresh
    // stream, mirroring the real pod lifecycle.
    let mut worker_tasks = Vec::with_capacity(workers);
    for w in 0..workers {
        let kind = if w % 2 == 0 {
            ExecutorKind::Builder
        } else {
            ExecutorKind::Fetcher
        };
        // FODs always have effective_features = ["fetcher"].
        let mut feats = dag.features.clone();
        if kind == ExecutorKind::Fetcher {
            feats.push(rio_common::k8s::FETCHER_FEATURE.to_string());
        }
        worker_tasks.push(tokio::spawn(worker_loop(
            handle.clone(),
            format!("bench-{kind:?}-{w}").to_lowercase(),
            kind,
            dag.systems.clone(),
            feats,
            counters.clone(),
        )));
    }

    let start = Instant::now();
    if let Some(events) = traffic {
        for (i, ev) in events.into_iter().enumerate() {
            if i > 0 && ev.gap_secs > 0.0 {
                tokio::time::sleep(Duration::from_secs_f64(ev.gap_secs / rate_scale)).await;
            }
            let (nodes, edges) = dag.subgraph(&ev.services);
            if !nodes.is_empty() {
                submit(&handle, &counters, nodes, edges).await;
            }
        }
    } else {
        let pending = Arc::new(Mutex::new(builds));
        let mut tasks = Vec::with_capacity(parallel);
        for _ in 0..parallel {
            let h = handle.clone();
            let c = counters.clone();
            let q = pending.clone();
            let d = dag.clone();
            tasks.push(tokio::spawn(async move {
                loop {
                    {
                        let mut g = q.lock().await;
                        if *g == 0 {
                            return;
                        }
                        *g -= 1;
                    }
                    submit(&h, &c, d.nodes.clone(), d.edges.clone()).await;
                }
            }));
        }
        for t in tasks {
            t.await?;
        }
    }
    let merge_elapsed = start.elapsed();

    // Drain: tick until everything completes or 30s without progress.
    let drain_start = Instant::now();
    let mut last = (Instant::now(), 0u64);
    loop {
        let done = counters.completions.load(Ordering::Relaxed);
        if done >= total_drvs as u64 {
            break;
        }
        if done != last.1 {
            last = (Instant::now(), done);
        } else if last.0.elapsed() > Duration::from_secs(30) {
            eprintln!("STALL: {done}/{total_drvs} after 30s idle");
            break;
        }
        let _ = handle.send(ActorCommand::Tick).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let drain_elapsed = drain_start.elapsed();

    drop(handle);
    for t in worker_tasks {
        t.abort();
    }

    let m = counters.merges.load(Ordering::Relaxed);
    let mn = counters.merge_ns.load(Ordering::Relaxed);
    let a = counters.assignments.load(Ordering::Relaxed);
    let c = counters.completions.load(Ordering::Relaxed);
    println!("== actor_pg_bench ==");
    println!("builds:      {m}");
    println!("dispatched:  {a}");
    println!("completed:   {c} / {total_drvs}");
    println!(
        "merge:       {merge_elapsed:.2?}  ({:.1} builds/s, avg {:.2?})",
        m as f64 / merge_elapsed.as_secs_f64(),
        Duration::from_nanos(mn / m.max(1))
    );
    println!(
        "drain:       {drain_elapsed:.2?}  ({:.1} drvs/s)",
        c as f64 / drain_elapsed.as_secs_f64().max(1e-3)
    );
    println!(
        "end-to-end:  {:.2?}  ({:.1} drvs/s)",
        start.elapsed(),
        c as f64 / start.elapsed().as_secs_f64()
    );

    if c < total_drvs as u64 {
        anyhow::bail!("incomplete: {c}/{total_drvs}");
    }
    Ok(())
}

async fn submit(
    handle: &ActorHandle,
    counters: &Counters,
    nodes: Vec<DerivationNode>,
    edges: Vec<DerivationEdge>,
) {
    let req = MergeDagRequest {
        build_id: Uuid::new_v4(),
        tenant_id: None,
        priority_class: PriorityClass::Scheduled,
        nodes,
        edges,
        options: BuildOptions::default(),
        keep_going: false,
        traceparent: String::new(),
        jti: None,
        jwt_token: None,
    };
    let (tx, rx) = oneshot::channel();
    let t0 = Instant::now();
    if handle
        .send(ActorCommand::MergeDag { req, reply: tx })
        .await
        .is_err()
    {
        return;
    }
    let _ = rx.await;
    counters.merges.fetch_add(1, Ordering::Relaxed);
    counters
        .merge_ns
        .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
}

/// One-shot worker: connect, accept one assignment, complete,
/// reconnect (the scheduler marks an executor draining after one
/// completion). Heartbeats echo the in-flight drv_path so
/// phantom-drain doesn't reset the build.
async fn worker_loop(
    handle: ActorHandle,
    executor_id: String,
    kind: ExecutorKind,
    systems: Vec<String>,
    features: Vec<String>,
    counters: Arc<Counters>,
) {
    let mut hb = tokio::time::interval(Duration::from_millis(100));
    'reconnect: loop {
        let Ok(mut rx) = connect(&handle, &executor_id, kind, &systems, &features).await else {
            return;
        };
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    let Some(msg) = msg else { return };
                    let Some(Msg::Assignment(a)) = msg.msg else { continue };
                    counters.assignments.fetch_add(1, Ordering::Relaxed);
                    let result = rio_proto::types::BuildResult {
                        status: rio_proto::types::BuildResultStatus::Built.into(),
                        built_outputs: vec![rio_proto::types::BuiltOutput {
                            output_name: "out".into(),
                            // Must parse as a valid store path or the
                            // trust-boundary filter drops it.
                            output_path: a.drv_path.strip_suffix(".drv").unwrap_or(&a.drv_path).into(),
                            output_hash: vec![0u8; 32],
                        }],
                        ..Default::default()
                    };
                    if handle
                        .send_unchecked(ActorCommand::ProcessCompletion {
                            executor_id: executor_id.clone().into(),
                            drv_key: a.drv_path.clone(),
                            result,
                            peak_memory_bytes: 256 << 20,
                            peak_cpu_cores: 1.0,
                            node_name: None,
                            hw_class: None,
                            final_resources: None,
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    counters.completions.fetch_add(1, Ordering::Relaxed);
                    continue 'reconnect;
                }
                _ = hb.tick() => {
                    // Heartbeat only — production builders never send
                    // Tick. The drain loop owns the tick cadence;
                    // ticking from 32 workers at 100ms hammered the
                    // ~5min housekeeping GC every 43ms (a bench
                    // artifact dominating pg_stat_statements).
                    if heartbeat(&handle, &executor_id, kind, &systems, &features, None).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

/// `send_unchecked`: worker lifecycle events bypass backpressure (the
/// production gRPC layer does the same). With `send`, a 5k-drv merge
/// burst flips the backpressure flag, the next reconnect fails, and
/// the worker pool collapses to zero — a permanent stall.
async fn connect(
    handle: &ActorHandle,
    executor_id: &str,
    kind: ExecutorKind,
    systems: &[String],
    features: &[String],
) -> anyhow::Result<mpsc::Receiver<SchedulerMessage>> {
    static EPOCH: AtomicU64 = AtomicU64::new(1);
    let (tx, rx) = mpsc::channel(1024);
    let (ack_tx, ack_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::ExecutorConnected {
            executor_id: executor_id.into(),
            stream_tx: tx,
            stream_epoch: EPOCH.fetch_add(1, Ordering::Relaxed),
            auth_intent: None,
            reply: ack_tx,
        })
        .await
        .map_err(|e| anyhow::anyhow!("connect: {e:?}"))?;
    ack_rx
        .await?
        .map_err(|e| anyhow::anyhow!("rejected: {e}"))?;
    heartbeat(handle, executor_id, kind, systems, features, None).await?;
    handle
        .send_unchecked(ActorCommand::PrefetchComplete {
            executor_id: executor_id.into(),
            paths_fetched: 0,
        })
        .await
        .map_err(|e| anyhow::anyhow!("ack: {e:?}"))?;
    Ok(rx)
}

async fn heartbeat(
    handle: &ActorHandle,
    executor_id: &str,
    kind: ExecutorKind,
    systems: &[String],
    features: &[String],
    running_build: Option<String>,
) -> anyhow::Result<()> {
    handle
        .send_unchecked(ActorCommand::Heartbeat(HeartbeatPayload {
            executor_id: executor_id.into(),
            systems: systems.to_vec(),
            supported_features: features.to_vec(),
            running_build,
            resources: None,
            store_degraded: false,
            draining: false,
            kind,
            intent_id: None,
        }))
        .await
        .map_err(|e| anyhow::anyhow!("heartbeat: {e:?}"))?;
    Ok(())
}
