//! Materialization-job actor logic — the substitution mechanism
//! (unconditional since the substitution-replacement cutover).
//! Design: substitution-replacement-design.md §2; spec:
//! sched.materialize.{job,routing,pinning}.
// r[impl sched.materialize.job+2]

use tokio::sync::oneshot;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::db::materialization::FencedJobCreate;
use crate::state::{DrvHash, ExecutorId, JobOrigin};

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
    pub job_id: Uuid,
    /// Backoff expiry while parked; `None` = not parked.
    pub parked_until: Option<std::time::Instant>,
    /// `Some(identity)` while an open materialization attempt exists.
    pub claimed_by: Option<ExecutorId>,
    /// Realized-path carrier (migration 082, the floating-CA
    /// stale-reset lane) — display copy for the claim-intake
    /// SUBSTITUTING event; the executor and the consumption coverage
    /// read the durable column directly.
    pub carried_realized_paths: Option<Vec<String>>,
    /// When the job's MOST RECENT park began (re-park overwrites — the
    /// dwell clock restarts). Mirror of the durable `park_began_at`
    /// (migration 083): set at park time, restored failover-exact by
    /// the recovery rebuild as a [`crate::state::RecoveredInstant`] —
    /// the recovered dwell keeps its true age on a freshly booted
    /// leader instead of silently restarting (merged_bug_300). `None`
    /// = never parked (or parked pre-083 — the dwell gate treats it as
    /// unmet, conservatively).
    pub parked_at: Option<crate::state::RecoveredInstant>,
}

/// `#[must_use]` actor-side disposition of a fenced durable write —
/// the gate every job-view removal and companion action derives from
/// (`sched.materialize.view-settlement`). `Applied`/`AlreadyResolved`
/// (= settled) authorize the view mutation and the companions;
/// `Fenced` keeps the entry inert until the LeaderLost wipe (a deposed
/// believer mutates nothing it no longer owns); `Failed` keeps it for
/// the next tick's level-triggered retry (tick cadence bounds the
/// retry; the durable row is the authority either way).
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriteDisposition {
    /// The durable write applied (rows > 0): the at-most-once edge.
    Applied,
    /// Already settled durably by an earlier write (idempotent
    /// re-entry; not the at-most-once edge).
    AlreadyResolved,
    /// The claims-floor fence refused the write (deposed believer).
    Fenced,
    /// The write errored (PG unavailable, …) — retried next tick.
    Failed,
}

impl WriteDisposition {
    /// Whether the durable state is SETTLED (applied now or earlier)
    /// — the only dispositions that authorize removing a view entry
    /// or running a companion action.
    pub(super) fn settled(self) -> bool {
        matches!(self, Self::Applied | Self::AlreadyResolved)
    }
}

/// Fail-closed availability wrapper around the job view
/// (merged_bug_246). The view is either **Hydrated** — rebuilt from the
/// durable rows by recovery, the only constructor of trust — or
/// **Unavailable** — boot, post-wipe, or a term whose recovery failed.
///
/// `Unavailable` is NOT "no jobs": a populated DAG over an absent view
/// is exactly the 246 hole (mat claims answered `Gone`, the store
/// skips, the armed action strands; build pulls fall into the
/// `None→DeliverNew` kernel cell and race the job). Every consumer
/// reads through a per-question accessor whose Unavailable answer is
/// the conservative one:
/// pulls → `Pending{parked:true}` (every kind maps to NotYetReady,
/// token/fence checks still dominate); spawn exclusion → exclude;
/// KEDA backlog → 0; ticks → skip; creation feeds → dropped (the
/// durable row is the authority; the backstop sweep re-feeds once
/// Hydrated).
///
/// Transitions: [`Self::rebuild`] (recovery Ok) is the ONLY path to
/// `Hydrated` — a clear path that produced `Hydrated(empty)` over a
/// repopulated DAG would recreate the hole, so [`Self::wipe`] and the
/// default both land on `Unavailable`.
#[derive(Debug, Default)]
pub(crate) enum JobViewState {
    /// No trustworthy view exists this term. Fail closed.
    #[default]
    Unavailable,
    /// Rebuilt from PG by recovery; live-maintained by the creation
    /// and consumption paths.
    Hydrated(JobView),
}

impl JobViewState {
    /// The hydrated view, if any. Consumers that read entry state for
    /// settled-write companions use this directly (their writes are
    /// fence-gated; a `None` here simply skips the in-memory mirror).
    pub(super) fn hydrated(&self) -> Option<&JobView> {
        match self {
            Self::Unavailable => None,
            Self::Hydrated(v) => Some(v),
        }
    }

    pub(super) fn hydrated_mut(&mut self) -> Option<&mut JobView> {
        match self {
            Self::Unavailable => None,
            Self::Hydrated(v) => Some(v),
        }
    }

    /// Per-entry read through the availability gate (`None` both for
    /// "no entry" and "no view" — callers needing to distinguish use
    /// [`Self::hydrated`]).
    pub(super) fn get<Q>(&self, k: &Q) -> Option<&JobViewEntry>
    where
        DrvHash: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        self.hydrated().and_then(|v| v.get(k))
    }

    pub(super) fn get_mut<Q>(&mut self, k: &Q) -> Option<&mut JobViewEntry>
    where
        DrvHash: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        self.hydrated_mut().and_then(|v| v.get_mut(k))
    }

    /// Iterate the hydrated entries; empty when Unavailable (the
    /// "ticks → skip" posture: periodic arms do nothing rather than
    /// acting on an absent cache).
    pub(super) fn iter(&self) -> impl Iterator<Item = (&DrvHash, &JobViewEntry)> {
        self.hydrated().into_iter().flat_map(|v| v.iter())
    }

    /// Hydrated keys; empty when Unavailable (ticks → skip).
    pub(super) fn keys(&self) -> impl Iterator<Item = &DrvHash> {
        self.hydrated().into_iter().flat_map(|v| v.keys())
    }

    /// Settled-gated removal; `false` when Unavailable (nothing to
    /// remove — the durable row is the authority).
    pub(super) fn remove_settled<Q>(&mut self, k: &Q, d: WriteDisposition) -> bool
    where
        DrvHash: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        match self.hydrated_mut() {
            Some(v) => v.remove_settled(k, d),
            None => false,
        }
    }

    /// LeaderLost / pre-recovery clear: the cache is gone AND so is the
    /// trust — never `Hydrated(empty)` (merged_bug_246).
    pub(super) fn wipe(&mut self) {
        *self = Self::Unavailable;
    }

    /// Recovery: the only `Hydrated` constructor.
    pub(super) fn rebuild(&mut self, entries: impl IntoIterator<Item = (DrvHash, JobViewEntry)>) {
        let mut v = JobView::default();
        v.rebuild(entries);
        *self = Self::Hydrated(v);
    }

    /// Test seeding: hydrate-if-needed then insert (tests model a
    /// healthy post-recovery leader).
    #[cfg(test)]
    pub(super) fn insert(&mut self, k: DrvHash, v: JobViewEntry) -> Option<JobViewEntry> {
        if matches!(self, Self::Unavailable) {
            *self = Self::Hydrated(JobView::default());
        }
        match self {
            Self::Hydrated(view) => view.insert(k, v),
            Self::Unavailable => unreachable!(),
        }
    }
}

// r[impl sched.materialize.view-settlement]
/// The in-memory materialization job view (a droppable cache of the
/// durable job table). The wrapper makes the removal discipline
/// STRUCTURAL: [`JobView::remove_settled`] is the only per-entry
/// removal and demands the durable write's [`WriteDisposition`] — an
/// unconditional `.remove()` no longer typechecks. Whole-view
/// transitions are [`JobView::wipe`] (LeaderLost — the cache drops
/// with the tenure) and [`JobView::rebuild`] (recovery — re-read from
/// the durable authority). Availability (hydrated vs absent) is the
/// enclosing [`JobViewState`]'s concern.
#[derive(Debug, Default)]
pub(crate) struct JobView(std::collections::HashMap<DrvHash, JobViewEntry>);

impl JobView {
    pub(crate) fn get<Q>(&self, k: &Q) -> Option<&JobViewEntry>
    where
        DrvHash: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        self.0.get(k)
    }

    pub(super) fn get_mut<Q>(&mut self, k: &Q) -> Option<&mut JobViewEntry>
    where
        DrvHash: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        self.0.get_mut(k)
    }

    pub(crate) fn contains_key<Q>(&self, k: &Q) -> bool
    where
        DrvHash: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        self.0.contains_key(k)
    }

    pub(super) fn keys(&self) -> impl Iterator<Item = &DrvHash> {
        self.0.keys()
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (&DrvHash, &JobViewEntry)> {
        self.0.iter()
    }

    /// Recovery: rebuild the cache from the durable rows.
    pub(super) fn rebuild(&mut self, entries: impl IntoIterator<Item = (DrvHash, JobViewEntry)>) {
        self.0.clear();
        self.0.extend(entries);
    }

    /// Direct insertion (test seeding via [`JobViewState::insert`]).
    /// Additive only — the removal discipline is untouched.
    #[cfg(test)]
    fn insert(&mut self, k: DrvHash, v: JobViewEntry) -> Option<JobViewEntry> {
        self.0.insert(k, v)
    }

    /// Insert-or-keep for the creation paths: a pre-existing entry
    /// (the dedup found an unresolved row, or recovery rebuilt it)
    /// keeps its armament state (park backoff, claim holder); a new
    /// job gets the fresh entry. Additive only — never a removal.
    pub(super) fn entry_or_insert(
        &mut self,
        k: DrvHash,
        default: JobViewEntry,
    ) -> &mut JobViewEntry {
        self.0.entry(k).or_insert(default)
    }

    // r[impl sched.materialize.view-settlement]
    /// THE per-entry removal: only a settled durable disposition may
    /// remove. Returns whether THIS call removed the entry — the gate
    /// every companion action (requeue, completion batch, fail-fast,
    /// conversion counter) hangs on. `Fenced`/`Failed` keep the entry:
    /// the armed action stays level-triggered instead of stranding the
    /// durable row behind an empty view.
    pub(super) fn remove_settled<Q>(&mut self, k: &Q, d: WriteDisposition) -> bool
    where
        DrvHash: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        if d.settled() {
            self.0.remove(k).is_some()
        } else {
            false
        }
    }
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
    /// The dedup upgraded the existing pending row's origin to
    /// `'pruned'` (pruned-wins, PD-D1 — counted separately from
    /// creations).
    pub upgraded: bool,
    pub origin: JobOrigin,
}

impl DagActor {
    /// Leader-served job listing (the store's poll). Standby or no
    /// jobs → empty vec (never an error).
    // r[impl sched.materialize.job+2]
    pub(super) async fn handle_list_materialization_jobs(
        &mut self,
        limit: u32,
        reply: oneshot::Sender<Vec<JobDescriptor>>,
    ) {
        let jobs = if !self.leader.is_leader() {
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
    /// No-op on standby. Creates the job row fenced + dedup'd, updates
    /// the in-memory view, and records the wanted relation for the
    /// creating build when one is named.
    ///
    /// Returns whether an unresolved job exists for the node after the
    /// call (created now or found by the dedup).
    // r[impl sched.materialize.job+2]
    #[must_use = "a false return means the job row did NOT apply — a caller holding a \
                  realized-path carrier must stash it for the housekeeping retry"]
    pub(super) async fn create_materialization_job(
        &mut self,
        drv_hash: &DrvHash,
        origin: JobOrigin,
        creating_build: Option<Uuid>,
        carried_realized_paths: Option<Vec<String>>,
    ) -> bool {
        if !self.leader.is_leader() {
            return false;
        }
        let Some(state) = self.dag.node(drv_hash) else {
            return false;
        };
        let Some(db_id) = state.db_id else {
            return false;
        };
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
                carried_realized_paths.as_deref(),
                serving_generation,
            )
            .await
        {
            Ok(FencedJobCreate::Applied {
                job_id,
                created,
                upgraded,
            }) => {
                // The view feed (merged_bug_246's re-feed half): a
                // genuinely new job (created == true) cannot have a
                // view entry (the partial-unique index guarantees no
                // unresolved job existed) — insert the fresh entry. The
                // dedup arm (created == false) re-encounters jobs the
                // view may already track — the dispatch probe
                // re-probing a PARKED node every tick, the
                // post-recovery probe pass — and must NOT reset their
                // armament state. When the dedup finds a row the view
                // does NOT track (the unhydrated-entry case), the entry
                // is REHYDRATED FROM PG — never fabricated as
                // unparked/unclaimed: a fabricated entry would let a
                // second claim race the durable holder or deliver a
                // parked job early. On a load failure the entry stays
                // absent and the BACKSTOP SWEEP is the level-triggered
                // repair (tick_backstop_materialization_jobs).
                self.feed_job_view_entry(drv_hash, job_id, created, carried_realized_paths.clone())
                    .await;
                if created {
                    metrics::counter!(
                        "rio_scheduler_materialization_jobs_created_total",
                        "origin" => origin.as_str()
                    )
                    .increment(1);
                    self.mirror_job_creation_reset(drv_hash);
                }
                if upgraded {
                    metrics::counter!("rio_scheduler_materialization_jobs_origin_upgraded_total")
                        .increment(1);
                }
                // Interest: ensure the durable wanted relation reflects
                // EVERY live interested build of this node — not just a
                // named creating build. Builds that merged flag-on
                // already have their rows (their merge wrote them; the
                // record is an idempotent fenced upsert), but builds
                // that merged FLAG-OFF have none, and the §6 joins the
                // store executor runs (tenant resolution + wanted-set
                // resolution, both through build_wanted_outputs) come up
                // empty for them: every execution of the job then fails
                // instantly as InfraFailure("no tenant context") and the
                // build never completes — the FP-4(b) absorption gap
                // observed by the flag-transition scenario (a flag-off
                // era build's nodes get jobs after the flip that can
                // never execute).
                let live_interested: Vec<Uuid> = {
                    use crate::state::BuildStateExt;
                    self.dag
                        .node(drv_hash)
                        .map(|s| {
                            s.interested_builds
                                .iter()
                                .filter(|bid| {
                                    self.builds
                                        .get(bid)
                                        .is_some_and(|b| !b.state().is_terminal())
                                })
                                .copied()
                                .collect()
                        })
                        .unwrap_or_default()
                };
                for build_id in live_interested {
                    self.record_wanted_for_build_node(build_id, drv_hash).await;
                }
                // The named creating build (the reprobe lane's re-merging
                // build) is covered by the loop above — it is among the
                // node's live interested builds by the time this runs.
                let _ = creating_build;
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
        let Some(db_id) = self.dag.node(drv_hash).and_then(|s| s.db_id) else {
            return;
        };
        // The backfill row is the SATURATING '{}' (all-declared) row —
        // matching the model's backfill encoding (materializationJob.qnt
        // puts OUTPUTS): a legacy build's true narrow wants are unknown
        // here, and the relation must never under-state interest width
        // (T-D2.3 step 5; widening-only divergence). GAP-FILLING ONLY
        // (ON CONFLICT DO NOTHING): a build that merged flag-on already
        // has its EXACT row and the backfill must never widen it.
        match self
            .db
            .backfill_wanted_fenced(self.serving_generation(), build_id, db_id)
            .await
        {
            Ok(
                crate::db::FencedOutcome::Applied(_) | crate::db::FencedOutcome::AlreadyResolved,
            ) => {}
            Ok(crate::db::FencedOutcome::Fenced) => {
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
    /// Convert a durable pending-job row into its view entry (the
    /// recovery rebuild's per-row shape, shared by the dedup re-feed
    /// and the backstop sweep).
    fn entry_from_recovered_row(row: crate::db::open_attempts::RecoveredJobRow) -> JobViewEntry {
        JobViewEntry {
            job_id: row.job_id,
            parked_until: row
                .park_remaining_secs
                .filter(|secs| *secs > 0.0)
                .map(|secs| std::time::Instant::now() + std::time::Duration::from_secs_f64(secs)),
            claimed_by: row.claimed_by.map(crate::state::ExecutorId::from),
            carried_realized_paths: row.carried_realized_paths,
            parked_at: row
                .park_began_secs_ago
                .filter(|secs| *secs >= 0.0)
                .map(crate::state::RecoveredInstant::from_age_secs),
        }
    }

    /// The single view-feed discipline for every creation path
    /// (merged_bug_246): fresh rows insert fresh entries; dedup'd rows
    /// the view already tracks keep their armament state; dedup'd rows
    /// the view does NOT track are rehydrated from PG — never
    /// fabricated as unparked/unclaimed (a fabricated entry would race
    /// the durable holder or early-deliver a parked job). Under an
    /// Unavailable view the feed is dropped: the durable row is the
    /// authority and the next recovery hydrates it.
    async fn feed_job_view_entry(
        &mut self,
        drv_hash: &DrvHash,
        job_id: Uuid,
        created: bool,
        carried_realized_paths: Option<Vec<String>>,
    ) {
        if self.materialization_jobs.hydrated().is_none() {
            debug!(
                drv_hash = %drv_hash, %job_id,
                "job view unavailable: creation feed dropped (durable row is authoritative;                  the next recovery hydrates)"
            );
            return;
        }
        let have_entry = self.materialization_jobs.get(drv_hash).is_some();
        if created {
            // A genuinely new job cannot have a view entry (the
            // partial-unique index); insert the fresh shape.
            if let Some(view) = self.materialization_jobs.hydrated_mut() {
                view.entry_or_insert(
                    drv_hash.clone(),
                    JobViewEntry {
                        job_id,
                        parked_until: None,
                        claimed_by: None,
                        carried_realized_paths: None,
                        parked_at: None,
                    },
                );
            }
        } else if !have_entry {
            // Dedup'd row, untracked entry: rehydrate from PG.
            let db_id = self.dag.node(drv_hash.as_str()).and_then(|s| s.db_id);
            match db_id {
                Some(db_id) => match self.db.load_unresolved_job_row(db_id).await {
                    Ok(Some(row)) => {
                        let entry = Self::entry_from_recovered_row(row);
                        if let Some(view) = self.materialization_jobs.hydrated_mut() {
                            view.entry_or_insert(drv_hash.clone(), entry);
                        }
                    }
                    Ok(None) => {
                        // Resolved between the dedup and this read —
                        // nothing unresolved to track.
                    }
                    Err(e) => {
                        warn!(
                            drv_hash = %drv_hash, %job_id, error = %e,
                            "dedup re-feed load failed; entry stays absent                              (backstop sweep re-feeds)"
                        );
                    }
                },
                None => {
                    debug!(drv_hash = %drv_hash, %job_id,
                           "dedup re-feed skipped: node has no db_id yet");
                }
            }
        }
        // Mirror the durable set-if-null carrier semantics on the view
        // copy (display only) — applies to fresh, kept, and rehydrated
        // entries alike.
        if let Some(entry) = self.materialization_jobs.get_mut(drv_hash)
            && entry.carried_realized_paths.is_none()
            && carried_realized_paths
                .as_ref()
                .is_some_and(|c| !c.is_empty())
        {
            entry.carried_realized_paths = carried_realized_paths;
        }
    }

    /// view entry), in the same post-commit phase that seeds states.
    ///
    /// `entry().or_insert()` (DQ-1 armament preservation, T-D2.1): the
    /// dedup arm (`created == false`) re-encounters jobs the view may
    /// already track — a pruned merge dedup-upgrading a PARKED
    /// `cache_opportunity` job, a re-merge over a claimed one — and
    /// must NOT reset their armament state (park backoff, claim
    /// holder) to a fresh unparked/unclaimed entry. A genuinely new
    /// job (`created == true`) cannot have a view entry (the
    /// partial-unique index guarantees no unresolved job existed), so
    /// or_insert inserts it.
    pub(super) async fn note_created_materialization_jobs(&mut self, created: &[CreatedJob]) {
        for job in created {
            // Same feed discipline as the probe-partition helper
            // (merged_bug_246): fresh insert for created rows, PG
            // rehydration for dedup'd rows the view does not track —
            // never a fabricated unparked/unclaimed default. (The
            // merge in-tx creation lanes never carry realized paths —
            // only the stale_reset post-tx site does; recovery and the
            // rehydration read the column.)
            self.feed_job_view_entry(&job.drv_hash, job.job_id, job.created, None)
                .await;
            if job.created {
                metrics::counter!(
                    "rio_scheduler_materialization_jobs_created_total",
                    "origin" => job.origin.as_str()
                )
                .increment(1);
                self.mirror_job_creation_reset(&job.drv_hash);
            }
            if job.upgraded {
                metrics::counter!("rio_scheduler_materialization_jobs_origin_upgraded_total")
                    .increment(1);
            }
        }
    }

    /// Mirror the migration-085 job-creation reset row into the node's
    /// in-memory history (the durable row was written inside the
    /// creating transaction): the in-memory [`Self::mat_counters`]
    /// re-window immediately, identically to a post-failover suffix
    /// reload. The mirrored record's attempt_id differs from the
    /// committed row's (both are independent mints); no fold reads it.
    fn mirror_job_creation_reset(&mut self, drv_hash: &DrvHash) {
        let Some(state) = self.dag.node_mut(drv_hash) else {
            return;
        };
        let Some(db_id) = state.db_id else {
            return;
        };
        let record = crate::db::attempts::AttemptRow::new_reset(
            db_id,
            crate::state::OutcomeClass::MaterializationReset,
            crate::state::ReportingParty::Scheduler,
            0,
            crate::state::AttemptKind::Materialization,
        )
        .to_record();
        state.push_attempt_record(record);
        self.refresh_retry_view(drv_hash);
    }

    /// Project the node's materialization-job state for pull admission
    /// (the kernel's `JobView` input).
    pub(super) fn materialization_job_view(
        &self,
        drv_hash: &DrvHash,
        pulling_identity: &ExecutorId,
    ) -> rio_evidence_kernel::pull::JobView {
        use rio_evidence_kernel::pull::JobView;
        let Some(view) = self.materialization_jobs.hydrated() else {
            // Fail-closed projection (merged_bug_246): an Unavailable
            // view must never answer `None` — a build pull would fall
            // into the kernel's None→DeliverNew cell and race a job we
            // cannot see, and a materialization claim would get `Gone`
            // (the store treats Gone as resolved and never claims
            // again — the stranded-armed-action class).
            // `Pending { parked: true }` maps EVERY kind to
            // NotYetReady while keeping the kernel's token/fence
            // rejections dominant (check order is load-bearing).
            return JobView::Pending { parked: true };
        };
        match view.get(drv_hash) {
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
    // r[impl obs.metric.scheduler]
    pub(super) fn note_materialization_claimed(&mut self, drv_hash: &DrvHash, holder: &ExecutorId) {
        if let Some(entry) = self.materialization_jobs.get_mut(drv_hash) {
            entry.claimed_by = Some(holder.clone());
        }
        // T-6.2 lifecycle counter: one increment per delivered claim
        // (the open attempt's mint committed — this is called only on
        // that path). Pairs with jobs_created_total (supply) and
        // jobs_resolved_total (drain) for the dashboard rates.
        metrics::counter!("rio_scheduler_materialization_claims_total").increment(1);
    }

    // r[impl sched.materialize.job+2]
    /// BC-4 (Phase B): emit the SUBSTITUTING DerivationEvent at
    /// materialization-claim intake. The event KIND is wire-retained;
    /// its emission site moved here from the walk spawn (deleted with
    /// Phase D'). The gateway's
    /// actSubstitute/actCopyPath pair creation keys on this kind
    /// (rio-gateway handler/build.rs `relay_derivation_status`,
    /// the SUBSTITUTING arm) and is untouched (BC-4's contract). Keeps
    /// the walk-era emission shape on the wire: one event
    /// per interested build + a progress snapshot so the queued/running
    /// flip is visible.
    ///
    /// Called by the pull mint INSTEAD of `emit_assignment_started` for
    /// materialization-kind mints — STARTED is one of the gateway's
    /// pair-STOP triggers, so emitting it here would close the pair the
    /// instant it opened (and misrepresent substitution work as a
    /// builder dispatch).
    ///
    /// Reachable only flag-on (no materialization mint exists flag-off),
    /// so flag-off event streams are byte-identical to as-built.
    pub(super) fn emit_materialization_claimed(&mut self, drv_hash: &DrvHash) {
        let Some(state) = self.dag.node(drv_hash) else {
            return;
        };
        let drv_path = state.drv_path().to_string();
        // The same payload the walk-spawn site sends: the paths the
        // store will fetch. `output_paths` is set by completion only;
        // pre-completion the expected paths are the fetch targets —
        // except the floating-CA stale-reset lane, where the expected
        // slots are [""] placeholders and the job's carried realized
        // paths (migration 082) ARE the fetch targets (the walk-era
        // emission carried them too; display copy of the carrier).
        let carried = self
            .materialization_jobs
            .get(drv_hash)
            .and_then(|e| e.carried_realized_paths.clone())
            .filter(|c| !c.is_empty());
        let output_paths = match carried {
            Some(c) if state.expected_output_paths.iter().all(|p| p.is_empty()) => c,
            _ => state.expected_output_paths.clone(),
        };
        let event = rio_proto::types::build_event::Event::Derivation(
            rio_proto::types::DerivationEvent::substituting(drv_path, output_paths),
        );
        for build_id in self.get_interested_builds(drv_hash) {
            self.events.emit(build_id, event.clone());
            // build_summary counts the now-Assigned/Running node as
            // running — emit a progress snapshot so the queued/running
            // flip is visible (matches the walk site and
            // `emit_assignment_started`).
            self.emit_progress(build_id);
        }
    }

    /// Whether the node carries an unresolved, UNCLAIMED materialization
    /// job — the §2.6 "substitution backlog" predicate read by the
    /// snapshot bucket re-sourcing and the spawn-intent filter.
    ///
    /// Pending AND parked jobs both count: the consumers' question is
    /// "does store-side substitution work exist for this node", and a
    /// parked job is exactly that (work the store will resume once its
    /// backoff expires). Claimed jobs do NOT count — their nodes are
    /// Assigned/Running and surface through `running_derivations`.
    ///
    /// Reads only the in-memory view (a droppable cache of the durable
    /// job table, rebuilt at recovery).
    pub(super) fn has_pending_unclaimed_job(&self, drv_hash: &str) -> bool {
        match self.materialization_jobs.hydrated() {
            // Fail closed (merged_bug_246): with no trustworthy view,
            // assume store-side work MAY exist — the exclusion
            // consumers (spawn-intent filter, queued-bucket
            // disjointness) must not spawn builders for nodes whose
            // jobs we cannot see. Heals at the next successful
            // recovery.
            None => true,
            Some(view) => view
                .get(drv_hash)
                .is_some_and(|entry| entry.claimed_by.is_none()),
        }
    }

    /// Whether the node carries a job the store could claim RIGHT NOW
    /// (unclaimed AND not parked) — the KEDA "substituting backlog"
    /// question (bug_252): parked jobs are pacing, not claimable
    /// demand, so they must not hold store replicas up; they stay
    /// visible via `rio_scheduler_materialization_stalled` and the
    /// parked-inclusive exclusion predicate above.
    ///
    /// Unavailable view → `false` (an honest zero: the gauge advertises
    /// claimable work to autoscalers; advertising unverifiable work
    /// would scale the store against a view we don't have).
    pub(super) fn has_claimable_job(&self, drv_hash: &str, now: std::time::Instant) -> bool {
        self.materialization_jobs
            .get(drv_hash)
            .is_some_and(|entry| {
                entry.claimed_by.is_none() && entry.parked_until.is_none_or(|until| until <= now)
            })
    }

    /// Whether ANY unresolved job exists for the node (pending,
    /// claimed, or parked) — the reap-survivor armament predicate
    /// (T-4.3): a survivor carrying an unresolved job needs nothing
    /// from the removal-survivor settlement, whatever the job's
    /// sub-state, because every job state has its own armed action
    /// (the §3.3 settlement totality).
    pub(super) fn has_unresolved_job(&self, drv_hash: &str) -> bool {
        match self.materialization_jobs.hydrated() {
            // Fail closed: treat the node as armed-elsewhere so the
            // reap-survivor settlement does not run destructive
            // promotion/fail-fast against jobs we cannot see
            // (merged_bug_246; the backstop sweep + next recovery are
            // the level-triggered repair).
            None => true,
            Some(view) => view.contains_key(drv_hash),
        }
    }

    // r[impl sched.materialize.job+2]
    /// T-4.3 (Phase B): rebuild the in-memory job view from PG at
    /// recovery. Without this, a failed-over leader's empty view
    /// answers `JobView::None` to every materialization claim → the
    /// kernel's kinded table answers `Gone` → the store executor
    /// (which treats Gone as "job resolved, skip") never claims again
    /// and the armed action is stranded until a dispatch-probe tick
    /// happens to lazily re-feed the view (the F10/L1 class).
    ///
    /// The rebuild mirrors ALL unresolved state: claim holders (so the
    /// open attempt re-delivers to its holder and refuses everyone
    /// else) and park expiries (so parked jobs keep answering
    /// NotYetReady until their durable backoff lapses).
    pub(super) fn rebuild_materialization_job_view(
        &mut self,
        rows: Vec<crate::db::open_attempts::RecoveredJobRow>,
    ) {
        // Per-row conversion shared with the dedup re-feed and the
        // backstop sweep; the dwell anchor rides as RecoveredInstant
        // age-data — exact even when the park pre-dates this leader's
        // boot (merged_bug_300: no silent dwell restart).
        let entries = rows.into_iter().map(|row| {
            (
                DrvHash::from(row.drv_hash.as_str()),
                Self::entry_from_recovered_row(row),
            )
        });
        self.materialization_jobs.rebuild(entries);
    }
}

// ──────────────────────────────────────────────────────────────────────
// The consumption transaction (design §2.4): Success coverage + the
// four-arm Unobtainable routing. The routing core is PURE (no IO, no
// clocks — kani-liftable per design §9.4); the consumption handler
// wires it to the fenced db operations.
// ──────────────────────────────────────────────────────────────────────

/// What the Unobtainable routing decided (design §2.4's four arms).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnobtainableRouting {
    /// Arm 0, covered: consume as success-for-live-interest.
    CompleteForLiveInterest,
    /// Arms 0 (uncovered) / 3a: job returns to pending.
    ReArm,
    /// Arms 1/2: node becomes from-source dispatchable.
    ResolveFromSource,
    /// Arm 3b: fail-fast every live DAG-interested build.
    FailFast,
}

/// The durable declared-relation classification (computed by the caller
/// from the dependency relation + statuses; the routing core never
/// touches the DAG).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DurableEvidence {
    /// Children all produced, no closure hole: from-source is viable.
    Vouched,
    /// Children exist but not all produced yet: normal dep gating.
    Pending,
    /// Absent/childless/holed: from-source is doomed.
    Broken,
}

/// The same-transaction FMP re-probe answer over the live wanted paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReprobeAnswer {
    /// Every live-wanted path present, substitutable, or indeterminate.
    Obtainable,
    /// Some live-wanted path confirmed missing-and-unsubstitutable.
    ConfirmedMissing,
}

/// The inputs of one Unobtainable routing decision.
pub(crate) struct RoutingInputs<'a> {
    /// Paths the executor confirmed absent upstream.
    pub missing_paths: &'a [String],
    /// Paths the executor verified present (and pinned).
    pub verified_paths: &'a [String],
    /// The live effective wanted PATHS (the §6 join, resolved to store
    /// paths by the caller inside the consumption transaction).
    pub live_wanted_paths: &'a [String],
    pub durable_evidence: DurableEvidence,
    /// Prior materialization_unobtainable rows for THIS job (the
    /// re-probe one-shot; design §2.4 arm 3).
    pub prior_unobtainable_count: u32,
    /// The same-transaction FMP re-probe answer over live_wanted_paths.
    /// `None` = not fetched (arms 0–2 decided without it); the caller
    /// fetches it only when arms 0–2 do not apply (purity by
    /// parameterization — design §9.4).
    pub reprobe: Option<ReprobeAnswer>,
    /// Whether the consumed job's `origin == 'pruned'` — the durable
    /// successor of the walk-era pruned mark (design §4/A2/A13,
    /// T-D2.1) and the arm-3 settlement discriminator (finding 11):
    /// only a pruned-origin job may fail-fast (the prune deliberately
    /// dropped the node's closure, so from-source is doomed); a
    /// non-pruned-origin job whose evidence is Broken by structure
    /// (childless leaf / probe blip) releases to from-source dispatch
    /// instead — non-pruned nodes are never affected, whatever their
    /// evidence.
    pub pruned_origin: bool,
}

// r[impl sched.materialize.routing+3]
/// The four-arm routing core. PURE (no IO, no clocks) — kani-liftable
/// per design §9.4; the FMP re-probe answer is an input.
///
/// The probe-failure case: the consumption HANDLER maps "the re-probe
/// RPC itself failed/timed out" to ReArm before calling this core (B3:
/// an indeterminate answer never fail-fasts) — the core's `None`
/// reprobe arm is therefore only reachable as one-shot-spent.
pub(crate) fn route_unobtainable(inputs: &RoutingInputs<'_>) -> UnobtainableRouting {
    let missing_live: Vec<&String> = inputs
        .missing_paths
        .iter()
        .filter(|p| inputs.live_wanted_paths.contains(p))
        .collect();
    // Arm 0 — moot-failure (the C3 arm).
    if missing_live.is_empty() {
        let covered = inputs
            .live_wanted_paths
            .iter()
            .all(|w| inputs.verified_paths.contains(w));
        return if covered {
            UnobtainableRouting::CompleteForLiveInterest
        } else {
            UnobtainableRouting::ReArm
        };
    }
    // Arms 1/2 — durable Vouched / Pending: from-source.
    match inputs.durable_evidence {
        DurableEvidence::Vouched | DurableEvidence::Pending => {
            return UnobtainableRouting::ResolveFromSource;
        }
        DurableEvidence::Broken => {}
    }
    // Arm 3 — Broken + live-wanted missing: the re-probe gate.
    match inputs.reprobe {
        Some(ReprobeAnswer::Obtainable) if inputs.prior_unobtainable_count == 0 => {
            UnobtainableRouting::ReArm
        }
        // Re-probe confirms missing, or the one-shot is spent. (A
        // missing probe is mapped to ReArm by the caller before this
        // core runs — see the doc above.)
        //
        // The settlement discriminates on the consumed job's ORIGIN
        // (finding 11, durably re-sourced by T-D2.1): only a
        // pruned-origin job fail-fasts — the prune deliberately
        // dropped the node's closure ("this was not built because
        // outputs were expected available"), so from-source is doomed
        // and the resubmit-directing error is the correct verdict. A
        // non-pruned-origin job — a genuine leaf whose evidence is
        // Broken by structure (childless) or by a probe blip —
        // releases to from-source dispatch instead; non-pruned nodes
        // are never affected, whatever their evidence.
        _ if inputs.pruned_origin => UnobtainableRouting::FailFast,
        _ => UnobtainableRouting::ResolveFromSource,
    }
}

/// Success-consumption coverage check (the CE-17 closer): the live
/// wanted set is covered by what the execution ingested or verified.
// r[impl sched.materialize.routing+3]
pub(crate) fn success_covers_live_wanted(
    ingested: &[String],
    verified: &[String],
    live_wanted: &[String],
) -> bool {
    live_wanted
        .iter()
        .all(|w| ingested.contains(w) || verified.contains(w))
}

impl DagActor {
    // r[impl sched.materialize.routing+3]
    /// Consume one materialization outcome (the §2.4 consumption
    /// transaction). Reachable only flag-on in practice (no
    /// materialization attempt can exist otherwise) — but ALWAYS wired
    /// (design §4 "always-on regardless of flags": reports for existing
    /// attempts must drain after an ON→OFF flip).
    /// Takes the MATERIALIZATION witness: a build attempt cannot reach
    /// the consumption transaction — the cross-kind call no longer
    /// typechecks (the witness twin of `close_pull_attempt_uncharged`'s
    /// `&BuildAttempt`).
    pub(super) async fn consume_materialization_outcome(
        &mut self,
        exec_id: Uuid,
        attempt: &crate::db::open_attempts::MatAttempt,
        outcome: rio_proto::types::MaterializationOutcome,
    ) -> Result<(), super::pull::PullRejection> {
        use rio_proto::types::materialization_outcome::Outcome;
        let drv_hash = DrvHash::from(attempt.core.drv_hash.as_str());
        let serving_generation = self.serving_generation();
        let executor = ExecutorId::from(attempt.core.executor_id.as_str());

        // The unresolved job this attempt executes (PG is the
        // authority; the in-memory view is a cache). The row carries
        // the ORIGIN — `'pruned'` is the durable arm-3 settlement
        // discriminator (T-D2.1; the walk-era pruned column's
        // successor).
        let job = self
            .db
            .unresolved_job_for_derivation(attempt.core.derivation_id)
            .await
            .map_err(|e| {
                super::pull::PullRejection::Internal(format!("materialization job lookup: {e}"))
            })?;
        let job_id = job.as_ref().map(|(id, _, _)| *id);
        let pruned_origin = job
            .as_ref()
            .is_some_and(|(_, origin, _)| matches!(origin, JobOrigin::Pruned));
        // Realized-path carrier (migration 082): the floating-CA
        // stale-reset lane's fetch targets. Unioned into the wanted
        // set below, so coverage is non-vacuous exactly when a carrier
        // is present (the conservative-absent saturation arm and every
        // non-carried shape are untouched — the scope the records'
        // collision analysis settled).
        let carried_paths: Vec<String> = job
            .as_ref()
            .and_then(|(_, _, carried)| carried.clone())
            .unwrap_or_default();

        // 1. The live effective wanted set (the §6 join), resolved to
        //    store paths — the presence-re-check half of D7's closure.
        let mut live_wanted_paths = self
            .live_wanted_paths_for(attempt.core.derivation_id, &drv_hash)
            .await
            .map_err(|e| super::pull::PullRejection::Internal(format!("wanted-union read: {e}")))?;
        // r[impl sched.merge.stale-substitutable+3]
        for p in &carried_paths {
            if !p.is_empty() && !live_wanted_paths.contains(p) {
                live_wanted_paths.push(p.clone());
            }
        }

        match outcome.outcome {
            Some(Outcome::Success(s)) => {
                // Success appends NOTHING to the ledger (design §2.4 —
                // success is not a fold event). Coverage decides
                // Complete vs ReArm (the CE-17 class).
                //
                // Every companion below gates on the close disposition
                // (sched.materialize.view-settlement): a Fenced close
                // means a deposed believer — it mutates nothing it no
                // longer owns; a Failed close retries via the report's
                // idempotent re-delivery / the establishment backstop.
                let close_d = self
                    .close_materialization_attempt(exec_id, &drv_hash, None, serving_generation)
                    .await;
                if !close_d.settled() {
                    return Ok(());
                }
                if success_covers_live_wanted(
                    &s.ingested_paths,
                    &s.verified_paths,
                    &live_wanted_paths,
                ) {
                    // The build-success path: outputs are present and
                    // verified in the store; one chokepoint resolves,
                    // settles the view, stamps the carrier, and
                    // completes for live interest.
                    self.complete_materialization_for_live_interest(
                        &drv_hash,
                        job_id,
                        exec_id,
                        &carried_paths,
                        serving_generation,
                    )
                    .await;
                } else {
                    // Coverage failed — interest grew between execution
                    // and consumption, or the report did not cover the
                    // carried realized paths (the floating-CA stale-
                    // reset shape): the job stays pending; the next
                    // claim covers it. The node must leave the mint's
                    // Running state too (the InfraFailure arm's
                    // posture) — without the reassign the admission
                    // table answers NotYetReady to EVERY identity (the
                    // job is pending-unclaimed but the node is held
                    // Running by closed-attempt bookkeeping) and the
                    // re-arm is a wedge, not an armed action.
                    self.release_claim(&drv_hash, Some(&executor)).await;
                }
                Ok(())
            }
            Some(Outcome::Unobtainable(u)) => {
                // The charge row: kind=materialization — visible only to
                // the materialization budget, never to build budgets.
                let mut row = crate::db::attempts::AttemptRow::new(
                    attempt.core.derivation_id,
                    crate::state::OutcomeClass::MaterializationUnobtainable,
                    crate::state::ReportingParty::Worker,
                    crate::state::AttemptKind::Materialization,
                );
                row.exec_id = Some(exec_id);
                row.executor_id = Some(executor.clone());
                row.error_msg = (!u.cause.is_empty()).then(|| u.cause.clone());
                let prior_unobtainable = self.mat_counters(&drv_hash).unobtainable_since_reset;
                let close_d = self
                    .close_materialization_attempt(
                        exec_id,
                        &drv_hash,
                        Some(row),
                        serving_generation,
                    )
                    .await;
                if !close_d.settled() {
                    // view-settlement gate: a deposed/failed close runs
                    // no routing — the durable attempt row is still the
                    // authority and the establishment sweep / re-report
                    // is the armed action.
                    return Ok(());
                }

                // 2. The four-arm routing. Arms 0–2 decide without the
                //    re-probe; the probe is fetched only for arm 3. The
                //    evidence is classified over the DURABLE relation
                //    (T-D2.2/PD-D4: pg.edges + pg.status + live
                //    co-owning build links — the three-part strict
                //    criterion), never the in-memory child set, so a
                //    reap-truncated or post-failover view cannot
                //    launder a verdict (the F9 hazard).
                let durable_evidence = match self
                    .db
                    .classify_durable_evidence(attempt.core.derivation_id)
                    .await
                    .map_err(|e| {
                        super::pull::PullRejection::Internal(format!(
                            "durable evidence classification: {e}"
                        ))
                    })? {
                    rio_evidence_kernel::ClosureEvidence::Vouched => DurableEvidence::Vouched,
                    rio_evidence_kernel::ClosureEvidence::Pending => DurableEvidence::Pending,
                    rio_evidence_kernel::ClosureEvidence::Broken => DurableEvidence::Broken,
                };
                // The arm-3 discriminator (finding 11) is the consumed
                // job's origin — `pruned_origin` was read from the job
                // row above (T-D2.1: the durable fact, not the
                // in-memory column).
                let needs_probe = u
                    .missing_paths
                    .iter()
                    .any(|p| live_wanted_paths.contains(p))
                    && durable_evidence == DurableEvidence::Broken;
                let reprobe = if needs_probe {
                    match self
                        .reprobe_live_wanted_paths(&drv_hash, &live_wanted_paths)
                        .await
                    {
                        Some(answer) => Some(answer),
                        None => {
                            // B3: the re-probe RPC itself failed — an
                            // indeterminate answer never fail-fasts.
                            // Atomic release (merged_bug_015): the
                            // bare re-arm here was the wedge.
                            self.release_claim(&drv_hash, Some(&executor)).await;
                            return Ok(());
                        }
                    }
                } else {
                    None
                };
                let routing = route_unobtainable(&RoutingInputs {
                    missing_paths: &u.missing_paths,
                    verified_paths: &u.verified_paths,
                    live_wanted_paths: &live_wanted_paths,
                    durable_evidence,
                    prior_unobtainable_count: prior_unobtainable,
                    reprobe,
                    pruned_origin,
                });
                // 3. Execute the routing.
                match routing {
                    UnobtainableRouting::CompleteForLiveInterest => {
                        // The moot arm completes through the SAME
                        // chokepoint as Success — the carrier stamp
                        // cannot be skipped by arm choice
                        // (merged_bug_055: this arm completed with the
                        // [""] placeholder pre-fix).
                        self.complete_materialization_for_live_interest(
                            &drv_hash,
                            job_id,
                            exec_id,
                            &carried_paths,
                            serving_generation,
                        )
                        .await;
                    }
                    UnobtainableRouting::ReArm => {
                        // Atomic release (merged_bug_015): re-arm +
                        // requeue in ONE step — the bare re-arm here
                        // held the node Running with no armed action.
                        self.release_claim(&drv_hash, Some(&executor)).await;
                    }
                    UnobtainableRouting::ResolveFromSource => {
                        let d = match job_id {
                            Some(job_id) => {
                                self.resolve_materialization_job(
                                    job_id,
                                    Some(exec_id),
                                    crate::state::JobState::ResolvedFromSource,
                                    serving_generation,
                                )
                                .await
                            }
                            None => WriteDisposition::AlreadyResolved,
                        };
                        if self.materialization_jobs.remove_settled(&drv_hash, d) {
                            // The node returns to its dep-derived status
                            // (the normal Ready path) — requeue it.
                            self.requeue_after_attempt(
                                std::slice::from_ref(&drv_hash),
                                crate::state::AttemptKind::Materialization,
                                Some(&executor),
                            )
                            .await;
                        }
                    }
                    UnobtainableRouting::FailFast => {
                        let d = match job_id {
                            Some(job_id) => {
                                self.resolve_materialization_job(
                                    job_id,
                                    Some(exec_id),
                                    crate::state::JobState::ResolvedUnobtainable,
                                    serving_generation,
                                )
                                .await
                            }
                            None => WriteDisposition::AlreadyResolved,
                        };
                        if self.materialization_jobs.remove_settled(&drv_hash, d) {
                            self.fail_fast_pruned_root(
                                &drv_hash,
                                "materialization confirmed a live-wanted output missing upstream \
                                 and not substitutable",
                            )
                            .await;
                        }
                    }
                }
                Ok(())
            }
            Some(Outcome::InfraFailure(f)) => {
                // The infra charge: counts toward the materialization
                // budget and toward NOTHING else. Never fail-fasts,
                // never routes from source (B3). Charge + park verdict
                // are ONE chokepoint (view-settlement gate, verdict,
                // and requeue all inside) — no arm hangs on the
                // disposition here.
                let _ = self
                    .charge_materialization_infra(
                        exec_id,
                        attempt.core.derivation_id,
                        &drv_hash,
                        &executor,
                        job_id,
                        crate::state::ReportingParty::Worker,
                        (!f.detail.is_empty()).then(|| f.detail.clone()),
                        None,
                        None,
                        serving_generation,
                    )
                    .await;
                Ok(())
            }
            Some(Outcome::Aborted(a)) => {
                // Charge-free close (owner default Q3, 2026-06-03 — AD5
                // parity for the materialization kind): the worker was
                // told to stop (SIGTERM during a store rollout/drain),
                // which is evidence about the WORKER's lifecycle, not
                // about the upstream or the job. NO ledger row of any
                // class — routine store rollouts must not burn the
                // 3-attempt park budget (merged_bug_189) — and the
                // budget keeps its flapping-replica blindness by
                // decision. The job returns to pending claimable and
                // the node leaves the mint's Running state (re-arm
                // without the reassign is the documented wedge).
                tracing::info!(
                    %exec_id,
                    drv_hash = %drv_hash,
                    detail = %a.detail,
                    "materialization walk aborted by the worker; closing charge-free"
                );
                // view-settlement gate (sched.materialize.view-settlement):
                // the rearm + requeue companions run only when the
                // charge-free close SETTLED — a deposed believer's fenced
                // Aborted close mutates nothing it no longer owns, and a
                // failed close leaves the establishment sweep as the
                // armed action (the same composition as every other
                // consumption arm).
                let close_d = self
                    .close_materialization_attempt(exec_id, &drv_hash, None, serving_generation)
                    .await;
                if close_d.settled() {
                    self.release_claim(&drv_hash, Some(&executor)).await;
                }
                Ok(())
            }
            None => {
                warn!(%exec_id, "materialization outcome with no payload; acknowledged-and-ignored");
                Ok(())
            }
        }
    }

    /// The live effective wanted PATHS for a node: the §6 wanted-union
    /// (joined over live builds' contributions), resolved to store
    /// paths against the node's declared outputs. Zero live relation
    /// rows (the legacy shape) saturate to ALL DECLARED outputs — the
    /// conservative-absent arm (T-D2.3/PD-D5, DQ-2): arm-0 coverage
    /// becomes HARDER to satisfy, never vacuously complete.
    async fn live_wanted_paths_for(
        &self,
        derivation_id: Uuid,
        drv_hash: &DrvHash,
    ) -> Result<Vec<String>, sqlx::Error> {
        let union = self.db.effective_wanted_union(derivation_id).await?;
        let Some(state) = self.dag.node(drv_hash) else {
            return Ok(Vec::new());
        };
        let wanted_names: Vec<String> = match union {
            // Zero live relation rows: the conservative-absent arm —
            // saturate to all-declared width (observable).
            None => {
                crate::state::note_wanted_width_saturated(&Uuid::nil());
                Vec::new()
            }
            // '{}' saturation = all declared outputs.
            Some(v) if v.is_empty() => Vec::new(),
            Some(v) => v,
        };
        let paths: Vec<String> = if wanted_names.is_empty() {
            state.expected_output_paths.clone()
        } else {
            state
                .output_names
                .iter()
                .zip(state.expected_output_paths.iter())
                .filter(|(name, _)| wanted_names.iter().any(|w| w == *name))
                .map(|(_, path)| path.clone())
                .collect()
        };
        Ok(paths.into_iter().filter(|p| !p.is_empty()).collect())
    }

    /// The node's [`rio_retry_kernel::MatCounters`] over the in-memory
    /// history (the loaded per-lane view) — THE single
    /// budget/one-shot/strictness counter (merged_bug_020). All three
    /// counts share the kernel's mat-lane reset window; party survives
    /// recovery (the suffix load parses `reporting_party` into every
    /// rebuilt record), so the worker-only Item-T recount is identical
    /// live and post-failover.
    fn mat_counters(&self, drv_hash: &DrvHash) -> rio_retry_kernel::MatCounters {
        self.dag
            .node(drv_hash)
            .map(|s| crate::retry_policy::materialization_counters(s.attempt_history()))
            .unwrap_or_default()
    }

    /// THE charge→verdict chokepoint (bug_067 + merged_bug_020): every
    /// `materialization_infra` charge — worker-reported AND
    /// establishment-written — closes through here, and the park
    /// decision runs UNCONDITIONALLY on the post-append kernel
    /// counters. Party-blind by the config contract
    /// (`max_attempts`: "worker-reported AND establishment-written —
    /// both channels charge the same budget"); the owner-signed Q5
    /// reversal (2026-06-03) of counter-signed residual (a)
    /// "establishment never parks" is exactly this fusion — a charging
    /// channel without the decision no longer exists. Park backs off
    /// the job durably (and the node is requeued either way: claimable
    /// again / from-source dispatchable per the admission table).
    // r[impl sched.materialize.routing+3]
    #[allow(clippy::too_many_arguments)]
    async fn charge_materialization_infra(
        &mut self,
        exec_id: Uuid,
        derivation_id: Uuid,
        drv_hash: &DrvHash,
        executor: &ExecutorId,
        job_id: Option<Uuid>,
        party: crate::state::ReportingParty,
        error_detail: Option<String>,
        source_node: Option<String>,
        termination_reason: Option<&'static str>,
        serving_generation: i64,
    ) -> WriteDisposition {
        let mut row = crate::db::attempts::AttemptRow::new(
            derivation_id,
            crate::state::OutcomeClass::MaterializationInfra,
            party,
            crate::state::AttemptKind::Materialization,
        );
        row.exec_id = Some(exec_id);
        row.executor_id = Some(executor.clone());
        row.error_msg = error_detail;
        row.source_node = source_node;
        row.termination_reason = termination_reason.map(Into::into);
        let close_d = self
            .close_materialization_attempt(exec_id, drv_hash, Some(row), serving_generation)
            .await;
        // The companions gate on the close disposition
        // (sched.materialize.view-settlement): a deposed believer's
        // fenced charge runs no verdict and no requeue; a failed close
        // leaves the establishment sweep as the armed action.
        if !close_d.settled() {
            return close_d;
        }
        // The verdict, post-append: park at budget exhaustion, rearm
        // claimable under it. The job_id falls back to the view entry
        // (the establishment path has no report-context job id).
        let counters = self.mat_counters(drv_hash);
        let job_id = job_id.or_else(|| self.materialization_jobs.get(drv_hash).map(|e| e.job_id));
        if counters.infra_since_reset >= self.materialization_cfg.max_attempts {
            // Park ends with its own requeue companion — the node
            // returns from-source dispatchable per the admission table.
            self.park_materialization_job(
                drv_hash,
                job_id,
                counters.infra_since_reset,
                Some(executor),
                serving_generation,
            )
            .await;
        } else {
            // Under budget: the atomic claim release (re-arm + requeue
            // in ONE step — the node is claimable again immediately).
            self.release_claim(drv_hash, Some(executor)).await;
        }
        close_d
    }

    /// THE success-for-live-interest completion chokepoint
    /// (merged_bug_055): resolve the job, settle the view, stamp the
    /// carried realized paths, and complete the node — in ONE helper,
    /// so no consuming arm (Success coverage, the Unobtainable moot
    /// arm, or any future arm) can skip the carrier stamp. The stamp
    /// runs BEFORE `complete_ready_from_store_batch`: its
    /// non-destructive guard keeps a known path, so the floating-CA
    /// node re-completes with the realized path instead of the `[""]`
    /// placeholder (GC retention + the client-visible path restored).
    async fn complete_materialization_for_live_interest(
        &mut self,
        drv_hash: &DrvHash,
        job_id: Option<Uuid>,
        exec_id: Uuid,
        carried_paths: &[String],
        serving_generation: i64,
    ) {
        let d = match job_id {
            Some(job_id) => {
                self.resolve_materialization_job(
                    job_id,
                    Some(exec_id),
                    crate::state::JobState::ResolvedSuccess,
                    serving_generation,
                )
                .await
            }
            // No durable row: the job settled earlier (PG is the
            // authority) — removing the stale view entry is
            // reconciliation, not a decision.
            None => WriteDisposition::AlreadyResolved,
        };
        if self.materialization_jobs.remove_settled(drv_hash, d) {
            if !carried_paths.is_empty()
                && let Some(state) = self.dag.node_mut(drv_hash)
                && state.output_paths.is_empty()
            {
                state.output_paths = carried_paths.to_vec();
            }
            self.complete_ready_from_store_batch(std::slice::from_ref(drv_hash))
                .await;
        }
    }

    /// Close the open materialization attempt (assignment row) and
    /// append the charge row when one is given, in ONE transaction
    /// carrying the same claims-floor fence as every other attempt
    /// closer. Mirrors `close_pull_attempt_uncharged`'s shape WITH an
    /// optional charge. Idempotent: a row already present for the exec
    /// makes the append a no-op (terminal-row-wins).
    async fn close_materialization_attempt(
        &mut self,
        exec_id: Uuid,
        drv_hash: &DrvHash,
        charge_row: Option<crate::db::attempts::AttemptRow>,
        serving_generation: i64,
    ) -> WriteDisposition {
        let result: Result<Option<(u64, bool)>, sqlx::Error> = async {
            let mut tx = match self.db.begin_fenced(serving_generation).await? {
                crate::db::FencedBegin::Fenced { .. } => return Ok(None),
                crate::db::FencedBegin::Open(ftx) => ftx,
            };
            let mut inserted = false;
            if let Some(row) = &charge_row {
                inserted = crate::db::SchedulerDb::append_attempt(tx.conn(), row).await?;
            }
            let closed = tx
                .close_assignment(exec_id, crate::db::AssignmentCloseStatus::Completed)
                .await?;
            tx.commit().await?;
            Ok(Some((closed, inserted)))
        }
        .await;
        match result {
            Ok(Some((closed, inserted))) => {
                if inserted && let Some(row) = charge_row {
                    if let Some(state) = self.dag.node_mut(drv_hash) {
                        state.push_attempt_record(row.to_record());
                    }
                    self.refresh_retry_view(drv_hash);
                }
                if closed > 0 {
                    WriteDisposition::Applied
                } else {
                    // Already closed (idempotent re-report or a prior
                    // settlement): the durable state is settled either
                    // way — terminal-row-wins.
                    WriteDisposition::AlreadyResolved
                }
            }
            Ok(None) => {
                self.note_fenced_evidence_write("materialization attempt close");
                WriteDisposition::Fenced
            }
            Err(e) => {
                warn!(drv_hash = %drv_hash, %exec_id, error = %e,
                      "materialization attempt close failed; the establishment sweep remains \
                       the backstop");
                WriteDisposition::Failed
            }
        }
    }

    /// Resolve the job terminally (fenced, exec_id-keyed, at-most-once)
    /// and note a fence refusal. Returns whether the resolution was
    /// APPLIED (rows > 0) — the at-most-once edge callers may hang
    /// further accounting on (the PD-20 conversion counter).
    // r[impl obs.metric.scheduler]
    async fn resolve_materialization_job(
        &mut self,
        job_id: Uuid,
        exec_id: Option<Uuid>,
        to_state: crate::state::JobState,
        serving_generation: i64,
    ) -> WriteDisposition {
        match self
            .db
            .resolve_materialization_job_fenced(job_id, exec_id, to_state, serving_generation)
            .await
        {
            Ok(crate::db::FencedOutcome::Applied(rows)) => {
                // T-6.2 lifecycle counter: one increment per APPLIED
                // terminal resolution, labeled by outcome. rows == 0 is
                // the at-most-once no-op (already resolved) and never
                // double-counts.
                if rows > 0 {
                    metrics::counter!(
                        "rio_scheduler_materialization_jobs_resolved_total",
                        "outcome" => Self::resolution_outcome_label(to_state)
                    )
                    .increment(1);
                }
                // r[impl sched.materialize.pinning]
                // §5.3 release site (i): the resolution may have made
                // this job's pins releasable (resolved AND no live
                // interest — the single-build case where the interest
                // already went terminal before the report landed). The
                // release query self-scopes; calling it after every
                // resolution is a no-op when interest is still live.
                self.release_materialization_pins_best_effort("job resolution")
                    .await;
                if rows > 0 {
                    WriteDisposition::Applied
                } else {
                    WriteDisposition::AlreadyResolved
                }
            }
            Ok(crate::db::FencedOutcome::AlreadyResolved) => {
                // Settled by an earlier resolution: not the at-most-once
                // edge (no counter), but the pin-release re-check is the
                // same self-scoping no-op-if-live call.
                self.release_materialization_pins_best_effort("job resolution")
                    .await;
                WriteDisposition::AlreadyResolved
            }
            Ok(crate::db::FencedOutcome::Fenced) => {
                self.note_fenced_evidence_write("materialization job resolve");
                WriteDisposition::Fenced
            }
            Err(e) => {
                warn!(%job_id, error = %e, "materialization job resolve failed");
                WriteDisposition::Failed
            }
        }
    }

    /// The `outcome` label vocabulary for
    /// `rio_scheduler_materialization_jobs_resolved_total` — the
    /// JobState terminal alphabet minus its `resolved_`/state prefixes
    /// (Pending is unreachable here; resolve targets are terminal by
    /// the caller's debug_assert).
    fn resolution_outcome_label(state: crate::state::JobState) -> &'static str {
        use crate::state::JobState;
        match state {
            JobState::ResolvedSuccess => "success",
            JobState::ResolvedFromSource => "from_source",
            JobState::ResolvedUnobtainable => "unobtainable",
            JobState::Cancelled => "cancelled",
            JobState::Obsolete => "obsolete",
            JobState::Pending => "pending",
        }
    }

    // r[impl sched.materialize.pinning]
    /// The §5.3 pin-release call shared by the three wiring sites
    /// (consumption resolution, build-terminal transition, recovery
    /// sweep arm): delete materialization pins whose job is resolved
    /// AND whose derivation has no live interested build left. The
    /// query self-scopes (pins of unresolved jobs and pins with live
    /// interest always survive — the B2-strong holds-window).
    ///
    /// ALWAYS-ON, never flag-gated (PD-B17): flag-gating the release
    /// would make flag-on-era pins permanently GC-immune after an
    /// ON→OFF rollback — a store-state divergence no equivalence
    /// criterion would catch. Flag-off, no materialization pins exist
    /// for fresh work and the call is a cheap no-op over flag-on-era
    /// leftovers (which is exactly the rollback drain §5.3 wants).
    /// Event-driven (not per-tick), so this does not reproduce the
    /// dormancy-7 flag-off-PG-query concern.
    ///
    /// Best-effort: a failed release is retried by the next event-driven
    /// site or the recovery sweep arm (the orphan backstop).
    pub(super) async fn release_materialization_pins_best_effort(&mut self, trigger: &str) {
        match self
            .db
            .release_materialization_pins_for_resolved_jobs()
            .await
        {
            Ok(0) => {}
            Ok(released) => {
                tracing::info!(
                    released,
                    trigger,
                    "materialization pins released (job resolved + all interest terminal)"
                );
            }
            Err(e) => {
                warn!(error = %e, trigger,
                      "materialization pin release failed (best-effort; the recovery \
                       sweep arm is the backstop)");
            }
        }
    }

    /// THE claim-drop chokepoint (merged_bug_015 / merged_bug_307 legs
    /// a+b): re-arm the job — the in-memory view drops the claim so
    /// the next pull's one-winner arbitration sees Pending — AND
    /// requeue the node off the mint's Assigned/Running bookkeeping in
    /// the SAME call, the code twin of the model's single-step
    /// `consumeUnobtainable`. A claim drop without the requeue is the
    /// documented wedge: pending-unclaimed job + node held Running ⇒
    /// the admission table answers NotYetReady to EVERY identity and
    /// no armed action remains (the skew the housekeeping tripwire
    /// counts). B1's Aborted intake routes through here too.
    pub(super) async fn release_claim(
        &mut self,
        drv_hash: &DrvHash,
        executor: Option<&ExecutorId>,
    ) {
        if let Some(entry) = self.materialization_jobs.get_mut(drv_hash) {
            entry.claimed_by = None;
        }
        self.requeue_after_attempt(
            std::slice::from_ref(drv_hash),
            crate::state::AttemptKind::Materialization,
            executor,
        )
        .await;
    }

    /// Park the job (infra-budget exhaustion, design §2.5): durable
    /// `park_until` + the in-memory view. Never a fail-fast.
    async fn park_materialization_job(
        &mut self,
        drv_hash: &DrvHash,
        job_id: Option<Uuid>,
        infra_count: u32,
        executor: Option<&ExecutorId>,
        serving_generation: i64,
    ) {
        let base = self.materialization_cfg.park_backoff_base_secs;
        let cap = self.materialization_cfg.park_backoff_cap_secs;
        let exp = infra_count.saturating_sub(self.materialization_cfg.max_attempts);
        let backoff_secs = base.saturating_mul(2u64.saturating_pow(exp)).min(cap);
        // The view mutation gates on the durable park's disposition
        // (sched.materialize.view-settlement): a deposed believer's
        // refused park must not project a parked view over a durable
        // row the successor owns. A view-only entry (no durable job
        // row) has nothing to settle against — it parks in memory and
        // the next consumption/cancel pass reconciles it.
        let durable = if let Some(job_id) = job_id {
            let park_until_epoch = crate::db::attempts::epoch_now() + backoff_secs as f64;
            match self
                .db
                .park_materialization_job_fenced(job_id, park_until_epoch, serving_generation)
                .await
            {
                Ok(
                    crate::db::FencedOutcome::Applied(_)
                    | crate::db::FencedOutcome::AlreadyResolved,
                ) => WriteDisposition::Applied,
                Ok(crate::db::FencedOutcome::Fenced) => {
                    self.note_fenced_evidence_write("materialization job park");
                    WriteDisposition::Fenced
                }
                Err(e) => {
                    warn!(%job_id, error = %e, "materialization job park failed");
                    WriteDisposition::Failed
                }
            }
        } else {
            WriteDisposition::Applied
        };
        if durable.settled()
            && let Some(entry) = self.materialization_jobs.get_mut(drv_hash)
        {
            entry.claimed_by = None;
            entry.parked_until =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(backoff_secs));
            // The dwell clock (migration 083 mirror): this park is the
            // most recent — re-park restarts the clock by design.
            entry.parked_at = Some(crate::state::RecoveredInstant::fresh_now());
        }
        // The park's requeue companion (merged_bug_015's park half):
        // the node leaves the mint's Assigned/Running bookkeeping
        // either way — parked means "not claimable until the backoff
        // lapses", never "wedged Running with no armed action".
        self.requeue_after_attempt(
            std::slice::from_ref(drv_hash),
            crate::state::AttemptKind::Materialization,
            executor,
        )
        .await;
    }

    /// The arm-3 FMP re-probe over the live wanted paths. `None` = the
    /// probe could not answer (no store client / RPC failure / timeout)
    /// — the caller maps that to ReArm (B3).
    ///
    /// Without a service signer the store cannot run its upstream
    /// substitution check (no `x-rio-probe-tenant-id`), so a missing
    /// path is indeterminate, never confirmed-missing — the probe then
    /// cannot produce the fail-fast conjunct (B3's conservative
    /// direction).
    async fn reprobe_live_wanted_paths(
        &mut self,
        drv_hash: &DrvHash,
        live_wanted: &[String],
    ) -> Option<ReprobeAnswer> {
        if live_wanted.is_empty() {
            return Some(ReprobeAnswer::Obtainable);
        }
        let store = self.store_client.clone()?;
        // One-shot service-token probe metadata (the same mint the
        // dispatch probe uses); non-empty ⟺ a signer + tenant were
        // resolvable, which is exactly the can-confirm criterion.
        let probe = self.probe_service_meta(std::iter::once(drv_hash));
        let can_confirm = !probe.is_empty();
        let mut req = tonic::Request::new(rio_proto::types::FindMissingPathsRequest {
            store_paths: live_wanted.to_vec(),
        });
        for (k, v) in probe {
            if let Ok(mv) = tonic::metadata::MetadataValue::try_from(v.as_str()) {
                req.metadata_mut().insert(k, mv);
            }
        }
        let resp = tokio::time::timeout(self.grpc_timeout, store.clone().find_missing_paths(req))
            .await
            .ok()?
            .ok()?
            .into_inner();
        let missing: std::collections::HashSet<String> = resp.missing_paths.into_iter().collect();
        let substitutable: std::collections::HashSet<String> =
            resp.substitutable_paths.into_iter().collect();
        let indeterminate: std::collections::HashSet<String> =
            resp.indeterminate_paths.into_iter().collect();
        let obtainable = live_wanted.iter().all(|p| {
            !missing.contains(p) || substitutable.contains(p) || indeterminate.contains(p)
        });
        Some(if obtainable || !can_confirm {
            ReprobeAnswer::Obtainable
        } else {
            ReprobeAnswer::ConfirmedMissing
        })
    }
}

impl DagActor {
    /// Establish one expired open materialization attempt (the
    /// dead-store-replica case): the `materialization_infra` charge
    /// (kind=materialization, party=Scheduler, "unreported") routes
    /// through [`Self::charge_materialization_infra`] — close, charge,
    /// and the PARK DECISION in one chokepoint. Establishment-only
    /// crash-loops therefore park at `max_attempts` like every other
    /// charge channel (the owner-signed Q5 reversal of counter-signed
    /// residual (a), 2026-06-03: party-blind parking; the parked
    /// population is MD-D1's stalled gauge). NO adopt arm (BC-3: a
    /// mid-walk crash leaves outputs present but the closure
    /// incomplete) and never `executor_crash` (BC-2: the charge feeds
    /// the materialization budget and nothing else).
    // r[impl sched.materialize.routing+3]
    pub(super) async fn establish_materialization_attempt(
        &mut self,
        attempt: &crate::db::open_attempts::OpenAttemptRow,
    ) {
        let drv_hash = DrvHash::from(attempt.drv_hash.as_str());
        let executor = ExecutorId::from(attempt.executor_id.as_str());
        let serving_generation = self.serving_generation();
        // A deposed/failed close runs no verdict and no requeue — the
        // successor's own sweep owns this attempt now; a failed close
        // re-runs next tick (the sweep is idempotent).
        let close_d = self
            .charge_materialization_infra(
                attempt.exec_id,
                attempt.derivation_id,
                &drv_hash,
                &executor,
                None,
                crate::state::ReportingParty::Scheduler,
                None,
                attempt.source_node.clone(),
                Some("unreported"),
                serving_generation,
            )
            .await;
        if !close_d.settled() {
            return;
        }
        tracing::info!(
            drv_hash = %drv_hash,
            exec_id = %attempt.exec_id,
            executor_id = %executor,
            age_secs = attempt.age_secs,
            "establishment sweep: open materialization attempt established as \
             materialization_infra (no adopt arm; park decision applied — the \
             job re-armed claimable or parked at budget exhaustion)"
        );
    }

    // r[impl obs.metric.materialization-stalled+2]
    // r[impl sched.materialize.routing+3]
    /// PD-20 (design §2.5, Phase B T-6.1): the parked-job housekeeping
    /// arm. Every tick, flag-on, leader-only:
    ///
    ///   1. **Re-evaluation**: every parked job is re-read against its
    ///      node's durable closure evidence. Vouched/Pending — a
    ///      buildable dependency closure exists (produced by other
    ///      builds, or normally dep-gated) — resolves the job
    ///      `resolved_from_source` NOW (the same arm-1/arm-2 disposition
    ///      the consumption routing takes) and requeues the node for
    ///      normal dispatch: the park can never outlive from-source
    ///      viability. Broken evidence (childless/holed — from-source is
    ///      structurally impossible) stays parked, with the
    ///      backoff-expiry re-claim as its armed action.
    ///   2. **Visibility**: `rio_scheduler_materialization_stalled`
    ///      (gauge) is set to the ground-truth count of jobs still
    ///      parked after the pass — the §2.5 operator signal ("a
    ///      genuinely dead upstream makes builds wait visibly"). Set
    ///      from truth every tick (the `tick_publish_gauges`
    ///      self-healing discipline), so resolutions, re-arms, and
    ///      cancellations are never missed decrements.
    ///
    /// Leader-only by construction (`handle_tick` returns early on
    /// standby).
    // r[impl sched.materialize.settlement]
    pub(super) async fn tick_reevaluate_parked_materialization_jobs(&mut self) {
        let now = std::time::Instant::now();
        let parked: Vec<(DrvHash, Uuid)> = self
            .materialization_jobs
            .iter()
            .filter(|(_, e)| {
                e.claimed_by.is_none() && e.parked_until.is_some_and(|until| until > now)
            })
            .map(|(h, e)| (h.clone(), e.job_id))
            .collect();
        let mut still_parked = parked.len();
        for (drv_hash, job_id) in parked {
            // Classify over the DURABLE relation (T-D2.2/PD-D4 — the
            // same three-part criterion the consumption routing uses;
            // a stale in-memory view must neither strand a buildable
            // closure nor auto-resolve on dead previous-generation
            // evidence). One query per parked job: bounded by the
            // stalled population, which is small by construction
            // (alerted at >0 for 15m).
            let Some(db_id) = self.dag.node(drv_hash.as_str()).and_then(|s| s.db_id) else {
                continue;
            };
            let evidence = match self.db.classify_durable_evidence(db_id).await {
                Ok(ev) => ev,
                Err(e) => {
                    // Conservative: an unanswerable classification
                    // keeps the job parked (the armed action remains
                    // park-expiry re-claim).
                    warn!(drv_hash = %drv_hash, error = %e,
                          "park re-evaluation evidence query failed; job stays parked");
                    continue;
                }
            };
            let from_source_viable = matches!(
                evidence,
                rio_evidence_kernel::ClosureEvidence::Vouched
                    | rio_evidence_kernel::ClosureEvidence::Pending
            );
            if !from_source_viable {
                continue;
            }
            // r[impl sched.materialize.conversion-strictness]
            // Item T strictness gate (default-off; whole-arm scope —
            // every origin's Vouched/Pending conversion). Both halves
            // gate the CONVERSION ACT only: the park predicate, the
            // party-blind budget fold, and the stalled-gauge
            // definition are untouched (OQ1 amendment 1 forecloses
            // re-keying parking), so default-off is byte-identical
            // and knob-ON extends the gauge population by exactly the
            // deferred-conversion class. A refused job stays parked:
            // counted by the gauge below, armed via park-expiry
            // re-claim, accruing further worker charges across cycles.
            let strict_worker = self.materialization_cfg.conversion_requires_worker_charge;
            let dwell_secs = self.materialization_cfg.conversion_min_park_dwell_secs;
            let max_attempts = self.materialization_cfg.max_attempts;
            if strict_worker {
                let worker_only = self.mat_counters(&drv_hash).worker_infra_since_reset;
                if worker_only < max_attempts {
                    debug!(
                        drv_hash = %drv_hash, %job_id, worker_only, max_attempts,
                        "conversion deferred: worker-reported charges alone do not \
                         exhaust the budget (conversion_requires_worker_charge)"
                    );
                    continue;
                }
            }
            if dwell_secs > 0 {
                // Boundary: dwell_secs ≤ max_satisfiable_dwell_secs()
                // (= cap - 1) by config validation — a visited job's
                // clock truncates below the cap, so the gate is
                // reachable for every accepted dwell (bug_088).
                let dwell_met = self
                    .materialization_jobs
                    .get(&drv_hash)
                    .and_then(|e| e.parked_at)
                    .is_some_and(|began| began.elapsed().as_secs() >= dwell_secs);
                if !dwell_met {
                    debug!(
                        drv_hash = %drv_hash, %job_id, dwell_secs,
                        "conversion deferred: minimum park dwell not yet elapsed \
                         (conversion_min_park_dwell_secs)"
                    );
                    continue;
                }
            }
            // Item T conversion visibility: read the (still-pending)
            // job's origin BEFORE resolving — PG is the origin
            // authority (the dedup may have upgraded it after the view
            // entry was created). `unknown` only on a query/decode
            // failure, never silently dropped.
            let origin_label = match self.db.unresolved_job_for_derivation(db_id).await {
                Ok(Some((_, origin, _))) => origin.as_str(),
                Ok(None) => "unknown",
                Err(e) => {
                    warn!(drv_hash = %drv_hash, error = %e,
                          "conversion-origin read failed; counting origin=unknown");
                    "unknown"
                }
            };
            // From-source is viable: resolve the job (no exec_id — the
            // re-evaluation, not an execution, resolved it) and requeue
            // the node. The spawn-intent filter and the admission table
            // stop excluding the node the moment the job row is
            // terminal.
            let serving_generation = self.serving_generation();
            let d = self
                .resolve_materialization_job(
                    job_id,
                    None,
                    crate::state::JobState::ResolvedFromSource,
                    serving_generation,
                )
                .await;
            // Item T (harden-store reconciliation memo §6.2): every
            // PD-20 conversion — a TIME-driven from-source disposition
            // of a job whose park budget exhausted while from-source
            // stayed viable — is counted, discriminated by origin.
            // `origin="cache_opportunity"` conversions are upstream-
            // available content converting to builds (the incident's
            // outcome class re-entering through exhaustion): the
            // RioSchedulerMaterializationConversions alert watches
            // their sustained rate. Applied-only (the at-most-once
            // edge), so a deposed leader's fenced resolve never counts.
            // The view removal + every companion gate on the resolve
            // disposition (sched.materialize.view-settlement): a Fenced
            // or Failed resolve keeps the job parked — counted by the
            // gauge below, re-evaluated next tick.
            if self.materialization_jobs.remove_settled(&drv_hash, d) {
                if d == WriteDisposition::Applied {
                    metrics::counter!(
                        "rio_scheduler_materialization_converted_total",
                        "origin" => origin_label
                    )
                    .increment(1);
                }
                self.requeue_after_attempt(
                    std::slice::from_ref(&drv_hash),
                    crate::state::AttemptKind::Materialization,
                    None,
                )
                .await;
                still_parked -= 1;
                tracing::info!(
                    drv_hash = %drv_hash,
                    %job_id,
                    ?evidence,
                    origin = origin_label,
                    "parked materialization job re-evaluated: from-source is viable; \
                     resolved from_source and requeued (PD-20)"
                );
            }
        }
        // The stalled gauge: ground truth after the re-evaluation pass.
        metrics::gauge!("rio_scheduler_materialization_stalled").set(still_parked as f64);
    }

    /// The PG-backed view backstop (merged_bug_246's sweep half +
    /// bug_385's scheduler leg): low-frequency reconciliation of the
    /// in-memory view against the durable pending rows. Rows the view
    /// does not track are RE-FED from PG (a live node's job becomes
    /// claimable again instead of answering Gone-by-absence; a moot
    /// row — node terminal/absent — gains the entry the zero-interest
    /// canceler needs and is closed charge-free on the same tick,
    /// converging the refusal-producing rows). Skips when Unavailable
    /// (ticks → skip; the next recovery hydrates wholesale).
    pub(super) async fn tick_backstop_materialization_jobs(&mut self) {
        const BACKSTOP_EVERY: u64 = 30;
        if !self.tick_count.is_multiple_of(BACKSTOP_EVERY) {
            return;
        }
        if self.materialization_jobs.hydrated().is_none() {
            return;
        }
        let rows = match self.db.load_unresolved_materialization_jobs().await {
            Ok(rows) => rows,
            Err(e) => {
                warn!(error = %e, "materialization view backstop load failed; retried next pass");
                return;
            }
        };
        let mut refed = 0usize;
        for row in rows {
            let drv_hash = DrvHash::from(row.drv_hash.as_str());
            if self.materialization_jobs.get(&drv_hash).is_none() {
                let entry = Self::entry_from_recovered_row(row);
                if let Some(view) = self.materialization_jobs.hydrated_mut() {
                    view.entry_or_insert(drv_hash, entry);
                    refed += 1;
                }
            }
        }
        if refed > 0 {
            tracing::info!(
                refed,
                "materialization view backstop re-fed untracked pending rows from PG                  (moot rows are closed by the zero-interest pass this tick)"
            );
        }
    }

    /// The flag-gated housekeeping backstop: cancel jobs for
    /// derivations whose live interest dropped to zero (node gone,
    /// node terminal, or every interested build terminal), closing any
    /// open materialization attempt charge-free. Phase B's
    /// build-terminal hooks will call the closer directly; in Phase A
    /// this tick backstop and the tests are the only callers.
    // r[impl sched.materialize.settlement]
    pub(super) async fn tick_cancel_zero_interest_materialization(&mut self) {
        use crate::state::BuildStateExt;
        let zero_interest: Vec<DrvHash> = self
            .materialization_jobs
            .keys()
            .filter(|h| match self.dag.node(h.as_str()) {
                None => true,
                Some(state) => {
                    state.status().is_terminal()
                        || !state.interested_builds.iter().any(|bid| {
                            self.builds
                                .get(bid)
                                .is_some_and(|b| !b.state().is_terminal())
                        })
                }
            })
            .cloned()
            .collect();
        for drv_hash in zero_interest {
            self.cancel_materialization_for_zero_interest(&drv_hash)
                .await;
        }
    }

    // r[impl sched.materialize.job+2]
    // r[impl sched.materialize.view-settlement]
    /// Cancel the job for a derivation whose live interest dropped to
    /// zero, closing any open materialization attempt CHARGE-FREE (no
    /// drv_attempts row at all) — BC-2's no-controller closer. ONE
    /// fenced transaction resolves the job AND closes the kind-guarded
    /// assignments row, keyed entirely on durable state: the close is
    /// TOTAL over the DAG-absent arm (its own trigger — the
    /// `None => true` zero-interest filter — guarantees the node may
    /// be gone, so no in-memory exec_id is ever read). The view
    /// removal gates on the disposition; a Fenced/Failed cancel keeps
    /// the entry and re-attempts next tick (level-triggered).
    pub(super) async fn cancel_materialization_for_zero_interest(&mut self, drv_hash: &DrvHash) {
        let Some(entry) = self.materialization_jobs.get(drv_hash) else {
            return;
        };
        let job_id = entry.job_id;
        let serving_generation = self.serving_generation();
        let d = match self
            .db
            .cancel_job_and_close_attempt_fenced(job_id, serving_generation)
            .await
        {
            Ok(crate::db::FencedOutcome::Applied(_)) => {
                // The at-most-once edge: the same lifecycle counter the
                // exec-keyed resolver increments (T-6.2).
                metrics::counter!(
                    "rio_scheduler_materialization_jobs_resolved_total",
                    "outcome" => Self::resolution_outcome_label(crate::state::JobState::Cancelled)
                )
                .increment(1);
                WriteDisposition::Applied
            }
            Ok(crate::db::FencedOutcome::AlreadyResolved) => WriteDisposition::AlreadyResolved,
            Ok(crate::db::FencedOutcome::Fenced) => {
                self.note_fenced_evidence_write("materialization job cancel");
                WriteDisposition::Fenced
            }
            Err(e) => {
                warn!(drv_hash = %drv_hash, %job_id, error = %e,
                      "zero-interest materialization cancel failed; retried next tick");
                WriteDisposition::Failed
            }
        };
        if self.materialization_jobs.remove_settled(drv_hash, d) {
            // r[impl sched.materialize.pinning]
            // §5.3 release site: cancellation resolves the job, so its
            // pins may be releasable (self-scoping no-op when live
            // interest remains elsewhere).
            self.release_materialization_pins_best_effort("job cancellation")
                .await;
            tracing::info!(
                drv_hash = %drv_hash,
                %job_id,
                "materialization job cancelled: no live interested build remains \
                 (attempt closed in the same fenced transaction)"
            );
        }
    }
}
