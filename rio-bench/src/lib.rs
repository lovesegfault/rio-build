//! Benchmark harness for rio-scheduler.
//!
//! DAG shape generators ([`linear`], [`binary_tree`], [`shared_diamond`])
//! produce synthetic-but-valid `SubmitBuildRequest` payloads. The
//! [`BenchHarness`] bundles an ephemeral PostgreSQL, a fully-wired
//! scheduler actor + gRPC server, and a connected client — same setup
//! path the integration tests take, exposed through the public API so
//! criterion benches can drive it.
//!
//! Generated drv_paths are structurally valid (`/nix/store/{32-char-
//! nixbase32}-{name}.drv`) so they pass `SubmitBuild`'s StorePath::parse
//! gate, but they do NOT correspond to real derivations. The scheduler's
//! MergeDag path validates DAG shape and persists nodes — it never opens
//! the .drv on disk.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use rio_proto::types::SubmitBuildRequest;
use rio_proto::types::{DerivationEdge, DerivationNode};
use rio_scheduler::actor::ActorHandle;
use rio_scheduler::db::SchedulerDb;
use rio_scheduler::grpc::SchedulerGrpc;
use rio_scheduler::logs::LogBuffers;
use rio_test_support::TestDb;

// ---------------------------------------------------------------------------
// DAG shape generators
// ---------------------------------------------------------------------------

/// A generated DAG: nodes plus the dependency edges between them.
///
/// `SubmitBuildRequest` carries both (nodes define derivations; edges
/// define parent→child dependency ordering). A node with no incoming
/// edges is a root; a node with no outgoing edges is a leaf and
/// enters the ready queue immediately at merge time.
#[derive(Clone)]
pub struct Dag {
    pub nodes: Vec<DerivationNode>,
    pub edges: Vec<DerivationEdge>,
}

impl Dag {
    /// Wrap into a `SubmitBuildRequest` with bench-standard defaults
    /// (scheduled priority class, single-tenant mode, no timeouts).
    pub fn into_request(self) -> SubmitBuildRequest {
        SubmitBuildRequest {
            tenant_name: String::new(),
            priority_class: "scheduled".into(),
            nodes: self.nodes,
            edges: self.edges,
            max_silent_time: 0,
            build_timeout: 0,
            build_cores: 0,
            keep_going: false,
        }
    }
}

/// Linear chain: node `i` depends on node `i-1`. Stresses critical-path
/// dispatch latency — no parallelism is possible; the ready queue never
/// holds more than one entry at a time.
///
/// `n` nodes, `n-1` edges. `n=0` yields an empty DAG.
pub fn linear(n: usize) -> Dag {
    let nodes: Vec<_> = (0..n).map(|i| synth_node(&format!("lin-{i}"))).collect();
    let edges = (1..n)
        .map(|i| DerivationEdge {
            parent_drv_path: nodes[i].drv_path.clone(),
            child_drv_path: nodes[i - 1].drv_path.clone(),
        })
        .collect();
    Dag { nodes, edges }
}

/// Complete binary tree of given depth. `2^depth - 1` nodes. Stresses
/// fan-out dispatch: all `2^(depth-1)` leaves are ready at merge time
/// and flood the ready queue at once, then completion cascades up level
/// by level.
///
/// Node `i` (1-indexed, heap layout) depends on children `2i` and `2i+1`.
/// `depth=0` yields an empty DAG; `depth=1` yields a single leaf.
pub fn binary_tree(depth: usize) -> Dag {
    if depth == 0 {
        return Dag {
            nodes: vec![],
            edges: vec![],
        };
    }
    let n = (1usize << depth) - 1;
    let nodes: Vec<_> = (1..=n).map(|i| synth_node(&format!("bt-{i}"))).collect();
    let mut edges = Vec::with_capacity(n.saturating_sub(1));
    for i in 1..=n {
        // nodes is 0-indexed; DAG is 1-indexed (heap layout).
        let left = 2 * i;
        let right = 2 * i + 1;
        if left <= n {
            edges.push(DerivationEdge {
                parent_drv_path: nodes[i - 1].drv_path.clone(),
                child_drv_path: nodes[left - 1].drv_path.clone(),
            });
        }
        if right <= n {
            edges.push(DerivationEdge {
                parent_drv_path: nodes[i - 1].drv_path.clone(),
                child_drv_path: nodes[right - 1].drv_path.clone(),
            });
        }
    }
    Dag { nodes, edges }
}

/// `n` nodes where `frac` share a common dependency (the diamond root).
/// Stresses de-duplication in the scheduler's ready-set scan: when the
/// root completes, `frac * n` nodes all become ready in a single
/// completion handler.
///
/// Returns `n + 1` nodes (the root is extra) and `round(frac * n)`
/// edges. The `(1.0 - frac) * n` nodes with no dependency are ready
/// immediately at merge, alongside the root.
///
/// # Panics
/// If `frac` is not in `0.0..=1.0`.
pub fn shared_diamond(n: usize, frac: f64) -> Dag {
    assert!(
        (0.0..=1.0).contains(&frac),
        "frac must be in [0.0, 1.0], got {frac}"
    );
    let root = synth_node("diamond-root");
    let leaves: Vec<_> = (0..n)
        .map(|i| synth_node(&format!("diamond-{i}")))
        .collect();
    // Round-to-nearest: 0.5 * 3 → 2 (not 1 via truncation).
    let shared = ((n as f64) * frac).round() as usize;
    let edges = leaves
        .iter()
        .take(shared)
        .map(|leaf| DerivationEdge {
            parent_drv_path: leaf.drv_path.clone(),
            child_drv_path: root.drv_path.clone(),
        })
        .collect();
    let mut nodes = Vec::with_capacity(n + 1);
    nodes.push(root);
    nodes.extend(leaves);
    Dag { nodes, edges }
}

// ---------------------------------------------------------------------------
// Node synthesis
// ---------------------------------------------------------------------------

fn synth_node(name: &str) -> DerivationNode {
    use rio_test_support::fixtures::{make_derivation_node, rand_store_hash};
    let hash = rand_store_hash();
    DerivationNode {
        // drv_hash must be globally unique within a bench run (across
        // criterion iterations) or MergeDag's dedup path fires and we
        // measure a cache hit, not a cold insert. make_derivation_node
        // uses the tag AS the hash — that's deterministic → collision.
        drv_hash: hash.clone(),
        drv_path: format!("/nix/store/{hash}-{name}.drv"),
        pname: name.into(),
        // expected_output_paths stays empty: the gateway would populate
        // this from the parsed .drv; we have none. The scheduler's
        // TOCTOU re-check skips empty, which is what we want.
        ..make_derivation_node(name, "x86_64-linux")
    }
}

// ---------------------------------------------------------------------------
// Bench harness
// ---------------------------------------------------------------------------

/// Fully-wired scheduler stack for benchmarks: ephemeral PG + actor +
/// gRPC server + connected client. Drop when done; the ephemeral DB
/// tears down via `TestDb::Drop`.
///
/// Built via [`BenchHarness::spawn`]. Not `Clone` — the whole point
/// is single ownership of the stack lifetime.
pub struct BenchHarness {
    /// Connected `SchedulerServiceClient`. Clone per-iteration if
    /// criterion's closure captures by move.
    pub client: rio_proto::SchedulerServiceClient<tonic::transport::Channel>,
    /// Bound address of the in-process gRPC server (ephemeral port).
    pub addr: SocketAddr,
    /// Handle to the DAG actor. Benches that bypass gRPC and talk to
    /// the actor directly (dispatch throughput) use this.
    pub actor: ActorHandle,
    /// PG pool. Benches that assert post-merge DB state use this.
    pub pool: sqlx::PgPool,
    // Held for lifetime — drop order is LIFO so _server_task aborts
    // before _db drops the database.
    _server_task: tokio::task::JoinHandle<()>,
    _db: TestDb,
}

impl BenchHarness {
    /// Spawn the full stack. Mirrors what `SchedulerGrpc::new_for_tests`
    /// and `setup_actor` do inside rio-scheduler's `#[cfg(test)]` modules,
    /// but built from the public API so an external crate can drive it.
    pub async fn spawn() -> anyhow::Result<Self> {
        let db = TestDb::new(&rio_scheduler::MIGRATOR).await;
        let pool = db.pool.clone();

        // No store client: SubmitBuild's TOCTOU re-check against the
        // store is skipped when store_client is None (merge.rs gates
        // the FindMissingPaths call on Option::Some). Exactly what we
        // want — we're benching the DAG merge + DB insert path, not
        // store RPC latency.
        //
        // No log flusher, no size classes: both optional; absence
        // means no size-class routing (dispatch bench doesn't care)
        // and no S3 flush on completion (completion path is faster
        // without it, which is fine for the ready-scan isolation the
        // dispatch bench wants).
        let sched_db = SchedulerDb::new(pool.clone());
        let actor = ActorHandle::spawn(sched_db, Default::default(), Default::default());

        // Production constructor: shared LogBuffers + pool + leader
        // flag. is_leader hardwired true — there's no lease loop in
        // this harness, and the gRPC handlers reject with UNAVAILABLE
        // if false.
        let grpc = SchedulerGrpc::with_log_buffers(
            actor.clone(),
            Arc::new(LogBuffers::new()),
            pool.clone(),
            Arc::new(AtomicBool::new(true)),
        );
        let router = tonic::transport::Server::builder()
            .add_service(rio_proto::SchedulerServiceServer::new(grpc));
        let (addr, server_task) = rio_test_support::grpc::spawn_grpc_server(router).await;

        let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
            .connect()
            .await?;
        let client = rio_proto::SchedulerServiceClient::new(channel);

        Ok(Self {
            client,
            addr,
            actor,
            pool,
            _server_task: server_task,
            _db: db,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_shape() {
        let dag = linear(5);
        assert_eq!(dag.nodes.len(), 5);
        assert_eq!(dag.edges.len(), 4);
        // Chain: each edge's parent is the next node up.
        for (i, e) in dag.edges.iter().enumerate() {
            assert_eq!(e.parent_drv_path, dag.nodes[i + 1].drv_path);
            assert_eq!(e.child_drv_path, dag.nodes[i].drv_path);
        }
    }

    #[test]
    fn linear_empty() {
        let dag = linear(0);
        assert!(dag.nodes.is_empty());
        assert!(dag.edges.is_empty());
    }

    #[test]
    fn binary_tree_shape() {
        // depth=3 → 7 nodes, 6 edges (complete binary tree).
        let dag = binary_tree(3);
        assert_eq!(dag.nodes.len(), 7);
        assert_eq!(dag.edges.len(), 6);

        // depth=10 → 1023 nodes (the dispatch bench seed size).
        let big = binary_tree(10);
        assert_eq!(big.nodes.len(), 1023);
        assert_eq!(big.edges.len(), 1022);
    }

    #[test]
    fn binary_tree_empty() {
        let dag = binary_tree(0);
        assert!(dag.nodes.is_empty());
        assert!(dag.edges.is_empty());
    }

    #[test]
    fn shared_diamond_shape() {
        let dag = shared_diamond(100, 0.5);
        assert_eq!(dag.nodes.len(), 101); // 100 leaves + 1 root
        assert_eq!(dag.edges.len(), 50); // 50% of 100

        // All edges point to the root (first node).
        let root_path = &dag.nodes[0].drv_path;
        for e in &dag.edges {
            assert_eq!(&e.child_drv_path, root_path);
        }
    }

    #[test]
    #[should_panic(expected = "frac must be in [0.0, 1.0]")]
    fn shared_diamond_rejects_bad_frac() {
        shared_diamond(10, 1.5);
    }

    #[test]
    fn synth_paths_pass_store_path_parse() {
        // SubmitBuild rejects malformed drv_path via StorePath::parse
        // — our synthetic paths MUST pass or the bench measures the
        // error path. Guard that invariant.
        let dag = linear(10);
        for node in &dag.nodes {
            let sp = rio_nix::store_path::StorePath::parse(&node.drv_path)
                .expect("synthetic drv_path must be a valid store path");
            assert!(sp.is_derivation(), "must end in .drv");
        }
    }

    #[test]
    fn synth_hashes_are_unique() {
        // Criterion iterates the same generator many times; if hashes
        // repeat, MergeDag dedupes and we'd be benching a cache hit.
        // This is a proves-nothing guard: the test asserts the
        // PRECONDITION that makes the bench meaningful.
        let a = linear(100);
        let b = linear(100);
        let mut seen = std::collections::HashSet::new();
        for node in a.nodes.iter().chain(b.nodes.iter()) {
            assert!(
                seen.insert(node.drv_hash.clone()),
                "drv_hash collision: {}",
                node.drv_hash
            );
        }
    }
}
