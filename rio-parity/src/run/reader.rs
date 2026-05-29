//! `ResultReader` — the warm stage's read-back surface: per-(build, drv)
//! status and exec_id for the warm prefetch's roots-only builds.
//!
//! The build-path collect loop does NOT use this module: collection is
//! driven by the in-band per-root results each submission returns
//! ([`super::model::PathOutcome`] on the batch record). The warm stage
//! still shells out to `nix build` and reads its dispositions back from
//! the build graph here; the supply planner absorbs that prefetch (and
//! this read-back with it) in a later phase.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use async_trait::async_trait;

use super::grpc::{AdminApi, GraphSnapshot};
use super::model::STATUS_POISONED;

/// Per-(build, drv) observation, normalized from the build-graph read.
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
    /// Never reported by the build-graph reader (always None).
    pub is_fixed_output: Option<bool>,
}

/// The read surface the warm stage drives to learn what happened to each
/// root of a settled warm batch.
#[async_trait]
pub trait ResultReader: Send + Sync {
    /// Observations for `drv_paths` within `build_id`'s DAG. Paths absent
    /// from the build's graph come back with an empty `status`.
    async fn read_build(&self, build_id: &str, drv_paths: &[String])
    -> Result<Vec<DrvObservation>>;
}

/// Reader backed by `GetBuildGraph` + `ListPoisoned`: one graph snapshot per
/// warm build, with the poisoned sweep filling `failed_builders` for
/// poisoned nodes (same-day evidence — `ListPoisoned` rows decay with the
/// scheduler's poison TTL). Warm batches are roots-only merges, so they sit
/// far below the dashboard's graph-size cap.
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
            None if node.status == STATUS_POISONED => (None, None),
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
            // Warm batches are roots-only merges sized well under the
            // dashboard's graph cap; a truncated response means the batch
            // sizing was misconfigured — surface it loudly rather than
            // silently mis-classifying the missing nodes as "no derivation
            // rows".
            anyhow::bail!(
                "GetBuildGraph({build_id}) truncated at {} nodes — merged DAG exceeds the \
                 dashboard cap; lower batch_max_nodes",
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

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::Mutex;

    /// Scripted [`ResultReader`] for warm-stage tests: observations are
    /// keyed by (build_id, drv_path); paths with no scripted observation
    /// come back as the default (empty-status) observation, mirroring the
    /// real reader's missing-path behavior. Setting `error` makes every
    /// `read_build` call fail with that message instead.
    #[derive(Default)]
    pub struct FakeReader {
        /// build_id → drv_path → observation
        pub observations: Mutex<HashMap<String, HashMap<String, DrvObservation>>>,
        /// When set, `read_build` fails with this message.
        pub error: Mutex<Option<String>>,
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

        /// Make every subsequent `read_build` call fail with `message`.
        pub fn fail_with(&self, message: &str) {
            *self.error.lock().unwrap() = Some(message.to_string());
        }
    }

    #[async_trait]
    impl ResultReader for FakeReader {
        async fn read_build(
            &self,
            build_id: &str,
            drv_paths: &[String],
        ) -> Result<Vec<DrvObservation>> {
            if let Some(message) = self.error.lock().unwrap().clone() {
                anyhow::bail!("{message}");
            }
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

    /// Pin the [`test_support::FakeReader`] scripting contract the
    /// warm-stage tests rely on: scripted observations come back for
    /// their (build_id, drv_path) key, unscripted paths come back as
    /// the default empty-status observation — the same missing-path shape
    /// the real reader produces — and an injected error fails every read
    /// until it is cleared.
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
        // Injected errors fail every read until cleared.
        fake.fail_with("scripted reader outage");
        let err = fake
            .read_build("b1", std::slice::from_ref(&target))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("scripted reader outage"), "{err}");
        *fake.error.lock().unwrap() = None;
        assert!(
            fake.read_build("b1", std::slice::from_ref(&target))
                .await
                .is_ok()
        );
    }
}
