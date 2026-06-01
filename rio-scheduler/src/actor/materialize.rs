//! Materialization-job actor logic (substitution-replacement Phase A).
//!
//! Everything here is reachable ONLY when materialization dispatch is
//! enabled (`materialization_cfg.enabled`); flag-off, the only
//! reachable code is the empty-list answer and the kind=BUILD
//! passthrough in pull admission. Design: substitution-replacement-
//! design.md §2; spec: sched.materialize.{job,routing,pinning}.
// r[impl sched.materialize.job]

use tokio::sync::oneshot;
use tracing::warn;
use uuid::Uuid;

use super::DagActor;

/// What `ListMaterializationJobs` returns per job (the proto
/// descriptor's actor-side source).
#[derive(Debug, Clone)]
pub struct JobDescriptor {
    /// `materialization_jobs.job_id`.
    pub job_id: Uuid,
    /// The derivation hash (the DAG key / claim intent).
    pub drv_hash: String,
    /// Creating build's tenant; `None` = no tenant context (the
    /// executor re-resolves at execution time — design §2.2 item 3).
    pub tenant_id: Option<Uuid>,
    /// Which classification demanded the job (observability).
    pub origin: crate::state::JobOrigin,
}

impl JobDescriptor {
    fn from_row(row: crate::db::materialization::MaterializationJobRow) -> Self {
        Self {
            job_id: row.job_id,
            drv_hash: row.drv_hash,
            tenant_id: row.tenant_id,
            origin: row.origin,
        }
    }
}

impl DagActor {
    /// Leader-served job listing (the store's poll). Flag-off, standby,
    /// or no jobs → empty vec (never an error — the AS-6 mixed-flag
    /// posture: a flag-on store polling a flag-off scheduler hangs
    /// harmlessly on empty lists).
    // r[impl sched.materialize.job]
    pub(super) async fn handle_list_materialization_jobs(
        &mut self,
        limit: u32,
        reply: oneshot::Sender<Vec<JobDescriptor>>,
    ) {
        let jobs = if !self.materialization_cfg.enabled || !self.leader.is_leader() {
            Vec::new()
        } else {
            match self
                .db
                .list_claimable_materialization_jobs(i64::from(limit.min(256)))
                .await
            {
                Ok(rows) => rows.into_iter().map(JobDescriptor::from_row).collect(),
                Err(e) => {
                    warn!(error = %e, "ListMaterializationJobs query failed; answering empty");
                    Vec::new()
                }
            }
        };
        let _ = reply.send(jobs);
    }
}
