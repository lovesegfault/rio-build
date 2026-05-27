//! `ResultReader` — the trait collect uses to learn per-(build, drv) status
//! and exec_id. Two implementations: [`GetBuildGraphReader`] (one
//! `GetBuildGraph` call per build plus a `ListPoisoned` sweep, valid only
//! while merged batch DAGs fit the 5000-node dashboard cap) and
//! [`QueryDerivationStatusesReader`] (a batched per-drv status RPC, chunked
//! at [`DRV_STATUS_CHUNK`] paths per call, for campaigns whose merged DAGs
//! outgrow that cap; its tonic adapter lands together with the RPC itself).

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use async_trait::async_trait;

use super::grpc::{AdminApi, GraphSnapshot};

/// Per-(build, drv) observation, normalized across both readers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DrvObservation {
    pub drv_path: String,
    /// Scheduler `derivations.status` string; empty = the drv was not found
    /// in the read (no derivation rows recorded for this build).
    pub status: String,
    /// Some(non-empty) ⇔ an execution was observed by THIS build.
    pub exec_id: Option<String>,
    pub assigned_executor: Option<String>,
    /// Failed-builder evidence for poisoned drvs (empty vec = none recorded;
    /// None = evidence unavailable, e.g. poison-TTL decay or an RPC gap).
    pub failed_builders: Option<Vec<String>>,
    pub poisoned_secs_ago: Option<u64>,
    /// Known only from the per-drv status reader (None from the
    /// build-graph reader).
    pub is_fixed_output: Option<bool>,
}

/// The read surface collect drives to learn what happened to each drv of a
/// settled batch.
#[async_trait]
pub trait ResultReader: Send + Sync {
    /// Observations for `drv_paths` within `build_id`'s DAG. Paths absent
    /// from the build's graph come back with an empty `status`.
    async fn read_build(&self, build_id: &str, drv_paths: &[String])
    -> Result<Vec<DrvObservation>>;
}

/// Reader backed by `GetBuildGraph` + `ListPoisoned`: one graph snapshot per
/// build, with the poisoned sweep filling `failed_builders` for poisoned
/// nodes (same-day evidence — `ListPoisoned` rows decay with the
/// scheduler's poison TTL).
pub struct GetBuildGraphReader<A: AdminApi> {
    pub admin: A,
}

impl<A: AdminApi> GetBuildGraphReader<A> {
    pub fn new(admin: A) -> Self {
        Self { admin }
    }
}

/// Normalize one graph snapshot + the poisoned sweep into per-path
/// observations for exactly `drv_paths` (in input order). Paths missing
/// from the graph yield a default observation with an empty `status`.
pub fn observations_from_graph(
    graph: &GraphSnapshot,
    poisoned: &HashMap<String, (Vec<String>, u64)>,
    drv_paths: &[String],
) -> Vec<DrvObservation> {
    let want: HashSet<&str> = drv_paths.iter().map(String::as_str).collect();
    let mut by_path: HashMap<&str, DrvObservation> = HashMap::new();
    for node in &graph.nodes {
        if !want.contains(node.drv_path.as_str()) {
            continue;
        }
        let (failed_builders, poisoned_secs_ago) = match poisoned.get(&node.drv_path) {
            Some((builders, age)) => (Some(builders.clone()), Some(*age)),
            // Poisoned but absent from ListPoisoned (TTL decay / cleared):
            // evidence unavailable, NOT "no failed builders".
            None if node.status == "poisoned" => (None, None),
            None => (Some(Vec::new()), None),
        };
        by_path.insert(
            node.drv_path.as_str(),
            DrvObservation {
                drv_path: node.drv_path.clone(),
                status: node.status.clone(),
                exec_id: (!node.exec_id.is_empty()).then(|| node.exec_id.clone()),
                assigned_executor: (!node.assigned_executor_id.is_empty())
                    .then(|| node.assigned_executor_id.clone()),
                failed_builders,
                poisoned_secs_ago,
                is_fixed_output: None,
            },
        );
    }
    drv_paths
        .iter()
        .map(|p| {
            by_path
                .remove(p.as_str())
                .unwrap_or_else(|| DrvObservation {
                    drv_path: p.clone(),
                    ..DrvObservation::default()
                })
        })
        .collect()
}

#[async_trait]
impl<A: AdminApi> ResultReader for GetBuildGraphReader<A> {
    async fn read_build(
        &self,
        build_id: &str,
        drv_paths: &[String],
    ) -> Result<Vec<DrvObservation>> {
        let graph = self.admin.get_build_graph(build_id).await?;
        if graph.truncated {
            // The batch node cap keeps merged DAGs under the dashboard's
            // 5000-node limit; a truncated response means the cap was
            // misconfigured — surface it loudly rather than silently
            // mis-classifying the missing nodes as "no derivation rows".
            anyhow::bail!(
                "GetBuildGraph({build_id}) truncated at {} nodes — merged DAG exceeds the \
                 dashboard cap; lower batch_max_nodes or switch to the batched \
                 QueryDerivationStatuses reader",
                graph.total_nodes
            );
        }
        let poisoned: HashMap<String, (Vec<String>, u64)> = self
            .admin
            .list_poisoned()
            .await?
            .into_iter()
            .map(|p| (p.drv_path, (p.failed_executors, p.poisoned_secs_ago)))
            .collect();
        Ok(observations_from_graph(&graph, &poisoned, drv_paths))
    }
}

// ───────────────────── Batched per-drv status reader ───────────────────────

/// Row shape of the batched per-drv status RPC (`QueryDerivationStatuses`,
/// PG-backed fields only). The tonic adapter is added once the scheduler
/// grows the RPC; until then the reader is exercised through this trait.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DrvStatusRow {
    pub drv_path: String,
    pub status: String,
    pub retry_count: u32,
    /// Scheduler-side resubmission cycles for this drv — distinct from the
    /// engine's own campaign resubmissions counted in each job record's
    /// `attempts`.
    pub resubmit_cycles: u32,
    pub failed_builders: Vec<String>,
    pub poisoned_secs_ago: Option<u64>,
    pub assigned_executor: Option<String>,
    pub is_fixed_output: bool,
    /// exec_id from the build's derivation rows for the supplied build_id
    /// (None ⇒ no execution observed by that build).
    pub exec_id: Option<String>,
}

/// The batched per-drv status query surface.
#[async_trait]
pub trait DerivationStatusApi: Send + Sync {
    /// Query statuses for `drv_paths` within `build_id`. Callers keep each
    /// call at or under [`DRV_STATUS_CHUNK`] paths — chunking is the
    /// reader's job, one RPC per chunk.
    async fn query(&self, build_id: &str, drv_paths: &[String]) -> Result<Vec<DrvStatusRow>>;
}

/// Max drv_paths per `QueryDerivationStatuses` call, keeping each request
/// comfortably inside message-size limits (same sizing rationale as
/// [`super::grpc::BATCH_QUERY_CHUNK`]).
pub const DRV_STATUS_CHUNK: usize = 500;

/// Reader backed by the batched per-drv status RPC; scales past the
/// build-graph reader's 5000-node cap because it never materializes the
/// whole DAG.
pub struct QueryDerivationStatusesReader<D: DerivationStatusApi> {
    pub api: D,
}

impl<D: DerivationStatusApi> QueryDerivationStatusesReader<D> {
    pub fn new(api: D) -> Self {
        Self { api }
    }
}

#[async_trait]
impl<D: DerivationStatusApi> ResultReader for QueryDerivationStatusesReader<D> {
    async fn read_build(
        &self,
        build_id: &str,
        drv_paths: &[String],
    ) -> Result<Vec<DrvObservation>> {
        let mut by_path: HashMap<String, DrvObservation> = HashMap::new();
        for chunk in drv_paths.chunks(DRV_STATUS_CHUNK) {
            for row in self.api.query(build_id, chunk).await? {
                by_path.insert(
                    row.drv_path.clone(),
                    DrvObservation {
                        drv_path: row.drv_path,
                        status: row.status,
                        exec_id: row.exec_id.filter(|e| !e.is_empty()),
                        assigned_executor: row.assigned_executor,
                        failed_builders: Some(row.failed_builders),
                        poisoned_secs_ago: row.poisoned_secs_ago,
                        is_fixed_output: Some(row.is_fixed_output),
                    },
                );
            }
        }
        Ok(drv_paths
            .iter()
            .map(|p| {
                by_path.remove(p).unwrap_or_else(|| DrvObservation {
                    drv_path: p.clone(),
                    ..DrvObservation::default()
                })
            })
            .collect())
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::Mutex;

    /// Scripted [`ResultReader`] for collect-stage tests: observations are
    /// keyed by (build_id, drv_path); paths with no scripted observation
    /// come back as the default (empty-status) observation, mirroring the
    /// real readers' missing-path behavior.
    #[derive(Default)]
    pub struct FakeReader {
        /// build_id → drv_path → observation
        pub observations: Mutex<HashMap<String, HashMap<String, DrvObservation>>>,
    }

    impl FakeReader {
        pub fn set(&self, build_id: &str, obs: DrvObservation) {
            self.observations
                .lock()
                .unwrap()
                .entry(build_id.to_string())
                .or_default()
                .insert(obs.drv_path.clone(), obs);
        }
    }

    #[async_trait]
    impl ResultReader for FakeReader {
        async fn read_build(
            &self,
            build_id: &str,
            drv_paths: &[String],
        ) -> Result<Vec<DrvObservation>> {
            let map = self.observations.lock().unwrap();
            let by_build = map.get(build_id).cloned().unwrap_or_default();
            Ok(drv_paths
                .iter()
                .map(|p| {
                    by_build.get(p).cloned().unwrap_or_else(|| DrvObservation {
                        drv_path: p.clone(),
                        ..DrvObservation::default()
                    })
                })
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::grpc::{GraphNodeView, GraphSnapshot, PoisonedView};
    use std::sync::Mutex;

    struct FakeAdmin {
        graph: GraphSnapshot,
        poisoned: Vec<PoisonedView>,
    }

    #[async_trait]
    impl AdminApi for FakeAdmin {
        async fn get_build_graph(&self, _build_id: &str) -> Result<GraphSnapshot> {
            Ok(self.graph.clone())
        }
        async fn list_poisoned(&self) -> Result<Vec<PoisonedView>> {
            Ok(self.poisoned.clone())
        }
        async fn log_tail(&self, _d: &str, _e: Option<&str>, _m: usize) -> Result<Vec<u8>> {
            Ok(b"log".to_vec())
        }
        async fn list_builds(&self, _t: &str, _l: u32) -> Result<Vec<(String, Option<String>)>> {
            Ok(vec![])
        }
    }

    fn node(drv: &str, status: &str, exec: &str) -> GraphNodeView {
        GraphNodeView {
            drv_path: drv.to_string(),
            status: status.to_string(),
            exec_id: exec.to_string(),
            assigned_executor_id: String::new(),
        }
    }

    #[tokio::test]
    async fn build_graph_reader_normalizes_observations() {
        let target = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-app.drv".to_string();
        let dep = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-dep.drv".to_string();
        let cached = "/nix/store/cccccccccccccccccccccccccccccccc-cached.drv".to_string();
        let admin = FakeAdmin {
            graph: GraphSnapshot {
                nodes: vec![
                    node(&target, "completed", "exec-1"),
                    node(&dep, "poisoned", ""),
                    node(&cached, "completed", ""),
                ],
                truncated: false,
                total_nodes: 3,
            },
            poisoned: vec![PoisonedView {
                drv_path: dep.clone(),
                failed_executors: vec!["builder-1".into()],
                poisoned_secs_ago: 60,
            }],
        };
        let reader = GetBuildGraphReader::new(admin);
        let obs = reader
            .read_build(
                "b1",
                &[
                    target.clone(),
                    dep.clone(),
                    cached.clone(),
                    "/nix/store/dddddddddddddddddddddddddddddddd-missing.drv".to_string(),
                ],
            )
            .await
            .unwrap();
        assert_eq!(obs.len(), 4);
        assert_eq!(obs[0].exec_id.as_deref(), Some("exec-1"));
        assert_eq!(obs[1].status, "poisoned");
        assert_eq!(
            obs[1].failed_builders.as_deref(),
            Some(&["builder-1".to_string()][..])
        );
        assert_eq!(obs[1].poisoned_secs_ago, Some(60));
        assert_eq!(obs[2].exec_id, None, "cache hit has empty exec_id");
        assert_eq!(
            obs[3].status, "",
            "missing drv → empty status (no-rows backstop)"
        );

        // Poisoned node absent from ListPoisoned → evidence unavailable
        // (None), not "no failed builders" (Some(empty)).
        let reader2 = GetBuildGraphReader::new(FakeAdmin {
            graph: GraphSnapshot {
                nodes: vec![node(&dep, "poisoned", "")],
                truncated: false,
                total_nodes: 1,
            },
            poisoned: vec![],
        });
        let obs = reader2
            .read_build("b1", std::slice::from_ref(&dep))
            .await
            .unwrap();
        assert_eq!(obs[0].failed_builders, None);
    }

    #[tokio::test]
    async fn build_graph_reader_rejects_truncated_graphs() {
        let admin = FakeAdmin {
            graph: GraphSnapshot {
                nodes: vec![],
                truncated: true,
                total_nodes: 7000,
            },
            poisoned: vec![],
        };
        let reader = GetBuildGraphReader::new(admin);
        let err = reader.read_build("b1", &[]).await.unwrap_err();
        assert!(err.to_string().contains("truncated"), "{err}");
    }

    struct FakeStatusApi {
        rows: Vec<DrvStatusRow>,
        calls: Mutex<Vec<usize>>,
    }

    #[async_trait]
    impl DerivationStatusApi for FakeStatusApi {
        async fn query(&self, _build_id: &str, drv_paths: &[String]) -> Result<Vec<DrvStatusRow>> {
            self.calls.lock().unwrap().push(drv_paths.len());
            Ok(self
                .rows
                .iter()
                .filter(|r| drv_paths.contains(&r.drv_path))
                .cloned()
                .collect())
        }
    }

    #[tokio::test]
    async fn query_derivation_statuses_reader_chunks_and_maps() {
        let mk = |i: usize| format!("/nix/store/{i:0>32}-p{i}.drv");
        let rows: Vec<DrvStatusRow> = (0..(DRV_STATUS_CHUNK + 10))
            .map(|i| DrvStatusRow {
                drv_path: mk(i),
                status: "completed".into(),
                exec_id: (i % 2 == 0).then(|| format!("e{i}")),
                is_fixed_output: i % 3 == 0,
                ..DrvStatusRow::default()
            })
            .collect();
        let api = FakeStatusApi {
            rows,
            calls: Mutex::new(vec![]),
        };
        let reader = QueryDerivationStatusesReader::new(api);
        let paths: Vec<String> = (0..(DRV_STATUS_CHUNK + 10)).map(mk).collect();
        let obs = reader.read_build("b1", &paths).await.unwrap();
        assert_eq!(obs.len(), DRV_STATUS_CHUNK + 10);
        assert_eq!(obs[0].exec_id.as_deref(), Some("e0"));
        assert_eq!(obs[1].exec_id, None);
        assert_eq!(obs[0].is_fixed_output, Some(true));
        let calls = reader.api.calls.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            &[DRV_STATUS_CHUNK, 10],
            "chunked at {DRV_STATUS_CHUNK}"
        );
    }

    /// Pin the [`test_support::FakeReader`] scripting contract the
    /// collect-stage tests rely on: scripted observations come back for
    /// their (build_id, drv_path) key, and unscripted paths come back as
    /// the default empty-status observation — the same missing-path shape
    /// the real readers produce.
    #[tokio::test]
    async fn fake_reader_returns_scripted_and_default_observations() {
        let target = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-app.drv".to_string();
        let missing = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-missing.drv".to_string();
        let fake = test_support::FakeReader::default();
        fake.set(
            "b1",
            DrvObservation {
                drv_path: target.clone(),
                status: "completed".into(),
                exec_id: Some("e1".into()),
                ..DrvObservation::default()
            },
        );
        let obs = fake
            .read_build("b1", &[target.clone(), missing.clone()])
            .await
            .unwrap();
        assert_eq!(obs[0].status, "completed");
        assert_eq!(obs[0].exec_id.as_deref(), Some("e1"));
        assert_eq!(
            obs[1],
            DrvObservation {
                drv_path: missing,
                ..DrvObservation::default()
            }
        );
        // A build with no scripted observations behaves the same way.
        let other = fake
            .read_build("b2", std::slice::from_ref(&target))
            .await
            .unwrap();
        assert_eq!(other[0].status, "");
    }
}
