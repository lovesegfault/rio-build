//! Build coordinator - watches Raft commits and starts assigned builds
//!
//! Phase 3.2: When Raft commits a BuildQueued entry and assigns it to this agent,
//! the coordinator notices and starts the build execution.

use openraft::Raft;
use rio_common::{AgentId, DerivationPath};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::agent::BuildJob;
use crate::builder;
use crate::storage::{StateMachineStore, TypeConfig};

/// Start build coordinator task
///
/// Watches Raft state for builds assigned to this agent and starts them.
/// Runs indefinitely until agent shuts down.
#[tracing::instrument(skip(current_build, raft, sm_store), fields(agent_id = %agent_id))]
pub fn start_build_coordinator(
    agent_id: AgentId,
    current_build: Arc<Mutex<Option<BuildJob>>>,
    raft: Arc<Raft<TypeConfig>>,
    sm_store: StateMachineStore,
) -> JoinHandle<()> {
    tracing::info!("Starting build coordinator");

    tokio::spawn(async move {
        // Track builds we've already started to avoid duplicates
        let started_builds: Arc<Mutex<HashSet<DerivationPath>>> =
            Arc::new(Mutex::new(HashSet::new()));

        let mut check_interval = tokio::time::interval(Duration::from_millis(100));

        loop {
            check_interval.tick().await;

            // Check if there are any builds assigned to us that haven't started
            let builds_to_start = {
                let started = started_builds.lock().await;
                let cluster_state = sm_store.data.read();

                cluster_state
                    .cluster
                    .builds_in_progress
                    .iter()
                    .filter(|(drv_path, tracker)| {
                        // Find builds assigned to this agent that we haven't started yet
                        tracker.agent_id == agent_id && !started.contains(*drv_path)
                    })
                    .map(|(drv_path, _)| drv_path.clone())
                    .collect::<Vec<_>>()
            }; // Both locks released here

            // Start each assigned build
            for drv_path in builds_to_start {
                tracing::info!(
                    "Build {} assigned to this agent, starting execution",
                    drv_path
                );

                // Get derivation NAR from Raft storage
                let drv_nar = {
                    let cluster_state = &sm_store.data.read().cluster;
                    cluster_state.pending_derivations.get(&drv_path).cloned()
                };

                match drv_nar {
                    Some(nar_bytes) => {
                        // Mark as started before actually starting (avoid duplicate starts)
                        started_builds.lock().await.insert(drv_path.clone());

                        // Start the build (this spawns background task)
                        if let Err(e) = builder::start_build(
                            &current_build,
                            raft.clone(),
                            sm_store.clone(),
                            drv_path.to_string(),
                            nar_bytes,
                        )
                        .await
                        {
                            tracing::error!("Failed to start build {}: {}", drv_path, e);
                            // TODO Phase 3.4: Propose BuildFailed to Raft
                            started_builds.lock().await.remove(&drv_path);
                        }
                    }
                    None => {
                        tracing::warn!(
                            "Build {} assigned but no derivation NAR in Raft storage",
                            drv_path
                        );
                    }
                }
            }
        }
    })
}
