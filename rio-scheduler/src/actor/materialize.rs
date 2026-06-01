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

use crate::db::materialization::FencedJobCreate;
use crate::state::{DerivationStatus, DrvHash, ExecutorId, JobOrigin};

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

/// The in-memory job view entry (droppable, never written back —
/// design handoff input 1's "derived droppable view"). Authority lives
/// in PG: creation dedup is the partial-unique index; consumption is
/// the fenced exec_id-keyed transaction. The view exists so pull
/// admission can answer from memory inside the actor turn; it is
/// populated only by the flag-gated creation paths (so flag-off it is
/// always empty) and rebuilt by query at recovery (Phase B).
#[derive(Debug, Clone)]
pub(crate) struct JobViewEntry {
    /// `materialization_jobs.job_id`.
    // Read by the consumption transaction and the establishment/
    // cancellation closers (the next commits of this campaign wave);
    // the allow is removed with the first reader.
    #[allow(dead_code)]
    pub job_id: Uuid,
    /// Backoff expiry while parked; `None` = not parked.
    pub parked_until: Option<std::time::Instant>,
    /// `Some(identity)` while an open materialization attempt exists.
    pub claimed_by: Option<ExecutorId>,
}

/// One job the merge transaction created (or found via the dedup) —
/// what `persist_merge_to_db` returns to the post-commit phase so the
/// in-memory view is fed OUTSIDE the transaction (a rolled-back merge
/// must leave no view entry; the view is a cache, not an authority).
#[derive(Debug, Clone)]
pub(crate) struct CreatedJob {
    pub drv_hash: DrvHash,
    pub job_id: Uuid,
    /// False when the dedup found a pre-existing unresolved job.
    pub created: bool,
    pub origin: JobOrigin,
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

    /// THE single job-creation helper for callers with NO enclosing
    /// transaction (every §2.1 probe-partition site calls this one fn —
    /// the "one helper" the design's B7 disposition requires; the merge
    /// sites use the in-tx core inside `persist_merge_to_db` instead).
    /// No-op flag-off and on standby. Creates the job row fenced +
    /// dedup'd, updates the in-memory view, and records the wanted
    /// relation for the creating build when one is named.
    ///
    /// Returns whether an unresolved job exists for the node after the
    /// call (created now or found by the dedup).
    // r[impl sched.materialize.job]
    pub(super) async fn create_materialization_job_if_enabled(
        &mut self,
        drv_hash: &DrvHash,
        origin: JobOrigin,
        creating_build: Option<Uuid>,
    ) -> bool {
        if !self.materialization_cfg.enabled || !self.leader.is_leader() {
            return false;
        }
        let Some(state) = self.dag.node(drv_hash) else {
            return false;
        };
        let Some(db_id) = state.db_id else {
            return false;
        };
        // Skip Substituting nodes (the AS-6 flip-boundary guard: a
        // flag-off-era walk in flight owns the node; creating a job
        // under it would race the walk's completion).
        if state.status() == DerivationStatus::Substituting {
            return false;
        }
        // Tenant: any live interested build's tenant (substitution is
        // content-addressed, so whose upstream config we use is
        // irrelevant to the result — the same derivation
        // probe_substitute_auth uses). NULL = no tenant context; the
        // executor re-resolves at execution time (design §2.2 item 3 /
        // PDQ-8).
        let tenant: Option<Uuid> = state
            .interested_builds
            .iter()
            .filter_map(|bid| self.builds.get(bid))
            .find_map(|b| b.tenant_id);
        let serving_generation = self.serving_generation();
        match self
            .db
            .create_materialization_job_fenced(
                db_id,
                drv_hash.as_str(),
                tenant,
                origin,
                serving_generation,
            )
            .await
        {
            Ok(FencedJobCreate::Applied { job_id, created }) => {
                self.materialization_jobs.insert(
                    drv_hash.clone(),
                    JobViewEntry {
                        job_id,
                        parked_until: None,
                        claimed_by: None,
                    },
                );
                if created {
                    metrics::counter!(
                        "rio_scheduler_materialization_jobs_created_total",
                        "origin" => origin.as_str()
                    )
                    .increment(1);
                }
                // Interest: the creating build's wanted-relation row
                // (merge-tx callers write the relation for ALL nodes in
                // the tx instead — this arm covers the probe-partition
                // path where no merge tx is open).
                if let Some(build_id) = creating_build {
                    self.record_wanted_for_build_node(build_id, drv_hash).await;
                }
                true
            }
            Ok(FencedJobCreate::Fenced) => {
                self.note_fenced_evidence_write("materialization job create");
                false
            }
            Err(e) => {
                warn!(drv_hash = %drv_hash, error = %e, "materialization job create failed");
                false
            }
        }
    }

    /// Standalone fenced wanted-relation write for one (build, node)
    /// pair — the probe-partition path's interest registration (the
    /// merge path writes the relation for all nodes inside its own tx).
    /// Best-effort: a failure leaves the durable relation behind the
    /// in-memory interest, which the next merge of the build repairs.
    pub(super) async fn record_wanted_for_build_node(
        &mut self,
        build_id: Uuid,
        drv_hash: &DrvHash,
    ) {
        let Some((db_id, wanted)) = self
            .dag
            .node(drv_hash)
            .and_then(|s| s.db_id.map(|id| (id, s.wanted_output_names.clone())))
        else {
            return;
        };
        let rows = [crate::db::wanted::WantedRow {
            build_id,
            derivation_id: db_id,
            wanted_output_names: &wanted,
        }];
        match self
            .db
            .record_wanted_fenced(self.serving_generation(), &rows)
            .await
        {
            Ok(crate::db::FencedWrite::Applied(_)) => {}
            Ok(crate::db::FencedWrite::Fenced) => {
                self.note_fenced_evidence_write("wanted relation record");
            }
            Err(e) => {
                warn!(drv_hash = %drv_hash, build_id = %build_id, error = %e,
                      "wanted-relation record failed (best-effort)");
            }
        }
    }

    /// Post-commit feed of the in-memory job view from the merge
    /// transaction's created-jobs list. Called only AFTER the merge tx
    /// committed (never inside it — a rolled-back merge must leave no
    /// view entry), in the same post-commit phase that seeds states and
    /// spawns walks.
    pub(super) fn note_created_materialization_jobs(&mut self, created: &[CreatedJob]) {
        for job in created {
            self.materialization_jobs.insert(
                job.drv_hash.clone(),
                JobViewEntry {
                    job_id: job.job_id,
                    parked_until: None,
                    claimed_by: None,
                },
            );
            if job.created {
                metrics::counter!(
                    "rio_scheduler_materialization_jobs_created_total",
                    "origin" => job.origin.as_str()
                )
                .increment(1);
            }
        }
    }

    /// Project the node's materialization-job state for pull admission
    /// (the kernel's `JobView` input). Flag-off: always `None` — the
    /// view map is never populated, and the projection never even looks
    /// (so the flag-off pull path does zero extra work).
    pub(super) fn materialization_job_view(
        &self,
        drv_hash: &DrvHash,
        pulling_identity: &ExecutorId,
    ) -> rio_evidence_kernel::pull::JobView {
        use rio_evidence_kernel::pull::JobView;
        if !self.materialization_cfg.enabled {
            return JobView::None;
        }
        match self.materialization_jobs.get(drv_hash) {
            None => JobView::None,
            Some(entry) => match &entry.claimed_by {
                Some(holder) => JobView::Claimed {
                    held_by_puller: holder == pulling_identity,
                },
                None => JobView::Pending {
                    parked: entry
                        .parked_until
                        .is_some_and(|until| until > std::time::Instant::now()),
                },
            },
        }
    }

    /// Note a materialization claim in the in-memory view (called by
    /// the pull mint after the fenced transaction committed for a
    /// materialization-kind delivery). Reachable only flag-on.
    pub(super) fn note_materialization_claimed(&mut self, drv_hash: &DrvHash, holder: &ExecutorId) {
        if let Some(entry) = self.materialization_jobs.get_mut(drv_hash) {
            entry.claimed_by = Some(holder.clone());
        }
    }
}
