//! Ready-set store short-circuit, plus the shared `WorkAssignment`
//! payload constructor the pull path uses.
//!
//! The stream-era placement/assign pass (`dispatch_ready` and the
//! 4-phase assign path) was deleted with the placement layer; work
//! delivery is pull-only (`actor/pull.rs`). The detached substitute
//! closure walk this file used to host was deleted with the
//! substitution-replacement cutover (store replicas execute
//! materialization jobs instead). What remains is
//! dispatch-mode-independent: completing Ready derivations whose
//! outputs already exist (or routing them to materialization jobs),
//! and `build_assignment_proto`/`emit_assignment_started` (shared
//! with the pull mint).

use std::collections::{HashMap, HashSet};
use std::future::Future;

use uuid::Uuid;

use tracing::{debug, error, info, warn};

use rio_proto::types::FindMissingPathsRequest;

use crate::state::{
    AttemptEventKind, AttemptKind, AttemptRecord, BuildStateExt, DerivationStatus, DrvHash,
    ExecutorId, SolvedIntent, effective_wanted, verifiable_wanted_paths,
};

use super::DagActor;

/// Derive the `StartedPredecessor` payload for the next mint of `drv`
/// from its in-memory attempt history (sh-042). `None` on a first
/// mint, on a Subst→Build flip, on a worker-reported close whose
/// reason is not yet established, or across a poison-clear cycle
/// boundary.
///
/// The rfind predicate is the STRUCTURAL key — `event_kind ==
/// Attempt && exec_id.is_some() && resubmit_cycle == cycle` — and the
/// CONTENT guards (`attempt_kind == Build`, `termination_reason`)
/// live in the post-`.and_then` so a newest structural match that
/// fails them yields `None` rather than letting rfind skip past it to
/// a stale older record. `max_infra_retries: 10` makes a
/// witnessed-OOM(A, `Some("oom_killed")`) → worker-timeout(B, `None`)
/// → re-mint(C) chain reachable within one cycle, and the I-047
/// dep-output-GC'd Ready→Queued demotion makes a Build(A) →
/// Materialization(B) → re-mint(C) chain reachable too; surfacing A
/// as C's predecessor would mint a misleading marker in both shapes.
///
/// `attempt_kind == Build` is load-bearing: materialization charge
/// rows DO carry an `exec_id` (`materialize.rs:2991`) and DO land in
/// `attempt_history` (`materialize.rs:3632`), so `exec_id.is_some()`
/// alone does not exclude them; a Subst→Build flip's predecessor is
/// `flip_to_family`'s `SubstCloseCause::FellThroughToBuild`, not a
/// `rio: retry` marker (whose reason alphabet is build-only).
///
/// `resubmit_cycle == cycle` keeps a poison-clear from carrying a
/// prior cycle's record forward: close sites stamp the row from
/// `state.retry.resubmit_cycles` (pull.rs:2175 / completion.rs:385),
/// which is NOT incremented by any infra/witnessed/timeout retry path
/// — only on poison-clear (completion.rs:4074).
///
/// `floor_bumped`/`new_axis_bytes`: only the two grace=0 promote arms
/// (`oom_killed` → `Mem`, `evicted_empty_dir_size_limit` → `Disk` —
/// the inverse of `witnessed_disposition`'s `PromoteMemFloor` /
/// `PromoteDiskFloor` arms in `rio-scheduler/src/actor/floor.rs`) name
/// an axis; every other reason (including `deadline_exceeded`, which
/// is `ClassifyOnly`) carries `PredecessorFloorAxis::None`. The bytes
/// are `last_intent.{mem,disk}_bytes` — the JUST-stamped solve at
/// `pull.rs:1227`, what the next pod will GET (so the marker → `rio:
/// builder` header transition reads coherently).
pub(super) fn predecessor_for_started(
    history: &[AttemptRecord],
    cycle: u32,
    last_intent: Option<&SolvedIntent>,
) -> Option<rio_proto::types::StartedPredecessor> {
    use rio_proto::types::{PredecessorFloorAxis, StartedPredecessor};
    let cycle = i32::try_from(cycle).unwrap_or(i32::MAX);
    history
        .iter()
        .rfind(|r| {
            r.event_kind == AttemptEventKind::Attempt
                && r.exec_id.is_some()
                && r.resubmit_cycle == cycle
        })
        .and_then(|r| {
            if r.attempt_kind != AttemptKind::Build {
                return None;
            }
            let reason = r.termination_reason.clone()?;
            let exec_id = r.exec_id?;
            let (axis, bytes) = match (reason.as_str(), last_intent) {
                ("oom_killed", Some(li)) => (PredecessorFloorAxis::Mem, li.mem_bytes),
                ("evicted_empty_dir_size_limit", Some(li)) => {
                    (PredecessorFloorAxis::Disk, li.disk_bytes)
                }
                _ => (PredecessorFloorAxis::None, 0),
            };
            Some(StartedPredecessor {
                exec_id: exec_id.to_string(),
                termination_reason: reason,
                floor_bumped: axis as i32,
                new_axis_bytes: bytes,
            })
        })
}

impl DagActor {
    // -----------------------------------------------------------------------
    // Ready-set store short-circuit
    // -----------------------------------------------------------------------

    /// Complete or substitute Ready derivations whose outputs already
    /// exist (locally or upstream-substitutable) — the I-067/I-070
    /// store short-circuit, the dispatch-mode-independent half of the
    /// former dispatch pass. There is no placement decision left (work
    /// delivery is pull-only), but a Ready derivation whose outputs
    /// appeared in the store since merge time must still be completed
    /// or demoted to substitution instead of having a pod spawned for
    /// it. Called where the old dispatch pass ran inline (after merge,
    /// after each completion cascade, on leader acquire);
    /// `probe_generation` (advanced once per Tick) bounds re-probing.
    pub(super) async fn sweep_ready_cached(&mut self) {
        // Same standby/recovery gates the old dispatch pass carried: a
        // standby or mid-recovery actor must not act on its DAG.
        if !self.leader.is_leader() {
            return;
        }
        if !self.leader.recovery_complete() {
            return;
        }
        // sh-002 advisory row 6: per-phase wall-clock guard
        // (defense-in-depth WARN now the lease is guard-isolated). The
        // lease loop is on its own runtime (sched.lease.guard-isolated),
        // so a 16.35s Tick no longer self-fences — but a sweep this
        // long still head-of-line blocks every queued RPC. The
        // `DISPATCH_PROBE_TICK_QUOTA` ledger already bounds the FMP
        // batch; this WARN names the residual (the
        // `complete_ready_from_store_batch` 3× serial PG awaits) when
        // it crosses `DISPATCH_PROBE_SWEEP_BUDGET` (= 5.5s), the
        // threshold past which the pre-guard shape would have starved
        // a renew.
        let t0 = std::time::Instant::now();
        let _ = self.batch_probe_cached_ready().await;
        let elapsed = t0.elapsed();
        if elapsed > super::DISPATCH_PROBE_SWEEP_BUDGET {
            tracing::warn!(
                ?elapsed,
                budget_secs = super::DISPATCH_PROBE_SWEEP_BUDGET.as_secs_f64(),
                "17-ready-cache-sweep exceeded DISPATCH_PROBE_SWEEP_BUDGET; the \
                 lease is guard-isolated so this no longer self-fences, but the \
                 dag-actor is head-of-line blocked — the quota-deferred tail is \
                 served by the next probe_generation"
            );
        }
    }
    // -----------------------------------------------------------------------
    // Dispatch
    // -----------------------------------------------------------------------

    /// I-067: best-effort store check for a Ready IA derivation's
    /// outputs (was FOD-only; generalised per the >4096 cap-gap).
    ///
    /// I-070: batched form — collect every unprobed Ready node's
    /// expected outputs, ONE `FindMissingPaths`, then
    /// `Self::complete_ready_from_store` each whose outputs are all
    /// present. Fail-open: store unreachable → no-op (per-drv
    /// fallback in the dispatch loop covers it next pass).
    ///
    /// Iterates the full DAG. Full-DAG scan is O(nodes) but the actor
    /// is single-threaded so there's no contention; for a 1085-node
    /// merge the scan is sub-ms vs. ~25s of sequential RPCs it
    /// replaces.
    ///
    /// Returns the set of hashes the drain loop must skip
    /// `ready_check_or_spawn` for (I-163). On success this is the
    /// batch-probed head (completed here or definitively found-missing
    /// one RPC ago) plus the quota-deferred tail (served by a LATER
    /// generation's batch, oldest-first — never re-granted within this
    /// one). On RPC error/timeout this is the tail only — the stamped
    /// head is protected via `probed_generation`, so neither hits the
    /// per-drv fallback. A quota-exhausted sweep returns the whole
    /// candidate set unprobed.
    ///
    /// sh-044: nodes with an unresolved materialization job are
    /// SKIPPED — the job row owns disposition (`ReportPullOutcome` →
    /// `JobViewState::remove_settled` re-admits the node to the next
    /// generation's candidate set). Without this conjunct, the sweep
    /// re-probes the same substitutable set every generation (1047
    /// paths × 8.77 s/tick under sh-044's store → cost-axis
    /// backpressure latched). The phase-15 age-out arm at
    /// `tick_reevaluate_materialization_jobs` bounds the skip
    /// unconditionally.
    // r[impl sched.dispatch.fod-substitute+3]
    // r[impl sched.admission.work-per-turn]
    // r[impl sched.dispatch.probe-skip-pending-mat]
    async fn batch_probe_cached_ready(&mut self) -> HashSet<DrvHash> {
        let Some(store) = &self.store_client else {
            return HashSet::new();
        };
        let started = std::time::Instant::now();
        let probe_gen = self.probe_generation;
        // Candidate set: (drv_hash, output_paths). Collected up-front
        // so the FindMissingPaths borrow doesn't hold &self.dag across
        // the .await (and so the completion loop can take &mut self).
        // Floating-CA (`expected_output_paths == [""]`) is excluded by
        // the `!is_empty()` + path-known check; the realisations lane
        // at merge-time handles those.
        //
        // Field-disjoint borrow: bind the job-view ref before the
        // `self.dag` chain so the closure captures `jobs`, not `self`.
        // [`JobViewState::get`] returns `None` under `Unavailable`
        // (fail-open: an unhydrated view falls through to today's
        // probe-everything behaviour) — `recovery_complete()` above
        // means the view is `Hydrated` whenever this filter runs.
        let jobs = &self.materialization_jobs;
        let mut candidates: Vec<(DrvHash, Vec<String>)> = self
            .dag
            .iter_nodes()
            .filter(|(h, s)| {
                s.status() == DerivationStatus::Ready
                    && s.probed_generation < probe_gen
                    && s.output_paths_probeable()
                    && jobs.get(*h).is_none()
            })
            .map(|(h, s)| (DrvHash::from(h), s.expected_output_paths.clone()))
            .collect();
        if candidates.is_empty() {
            return HashSet::new();
        }
        // Per-tick admission quota (round-9 B7). The ledger expires
        // structurally by generation key: the first sweep of a new
        // `probe_generation` observes the stale key and re-arms the
        // budget — there is no tick-site reset to bypass.
        if self.probe_quota.generation != probe_gen {
            self.probe_quota = super::ProbeQuotaLedger {
                generation: probe_gen,
                admitted: 0,
            };
        }
        let remaining = super::DISPATCH_PROBE_TICK_QUOTA.saturating_sub(self.probe_quota.admitted);
        // Quota exhausted for this generation: the ENTIRE candidate set
        // is the deferred tail — unstamped, so the next generation
        // serves it (oldest-first below); within this generation it is
        // never re-granted (the within-tick re-sweep defeat is dead).
        let mut checked = HashSet::with_capacity(candidates.len());
        if remaining == 0 {
            for (h, _) in &candidates {
                checked.insert(h.clone());
            }
            return checked;
        }
        // Over-quota sweep: serve the least-recently-probed candidates
        // first ((probed_generation, drv_hash) order — deterministic),
        // so the deferred tail (older stamps) advances ahead of any
        // same-tick-age re-probe and the window self-heals to full
        // coverage across ticks instead of starving behind arbitrary
        // iteration order. The unserved tail is inserted into `checked`
        // (the caller-side skip set) but NOT stamped with
        // `probed_generation` — a LATER generation's sweep batch-probes
        // that window. Each FMP batch is ≤ the remaining quota ≤ the
        // full quota — the same single-RPC bound the old per-sweep cap
        // enforced (the wire's max_batch_paths is far larger; the
        // budget here is what prices a sweep). Letting the tail fall
        // through to the per-drv path would be O(N) sequential 30s-
        // timeout RPCs in the actor (24h+ stall with a wide layer and
        // an unreachable store; I-139/I-140 invariant).
        if candidates.len() > remaining {
            // Decorate-sort-undecorate: one generation lookup per
            // candidate (not per comparison) — the over-quota branch
            // can see 100K+-wide layers.
            let mut keyed: Vec<(u64, DrvHash, Vec<String>)> = candidates
                .drain(..)
                .map(|(h, p)| {
                    let g = self.dag.node(&h).map_or(0, |s| s.probed_generation);
                    (g, h, p)
                })
                .collect();
            keyed.sort_unstable_by(|(ga, ha, _), (gb, hb, _)| {
                ga.cmp(gb).then_with(|| ha.as_ref().cmp(hb.as_ref()))
            });
            for (_, h, _) in &keyed[remaining..] {
                checked.insert(h.clone());
            }
            keyed.truncate(remaining);
            candidates.extend(keyed.into_iter().map(|(_, h, p)| (h, p)));
        }
        self.probe_quota.admitted += candidates.len();
        for (h, _) in &candidates {
            if let Some(s) = self.dag.node_mut(h) {
                s.probed_generation = probe_gen;
            }
        }

        // merged_bug_028: per-tenant probe plan. Presence and
        // substitutability are PER-TENANT facts (the store's
        // sig-visibility gate and `tenant_upstreams` both key on the
        // request tenant), so the batch asks once per LIVE tenant of
        // the candidate set and folds per candidate:
        // inline-completion requires present-and-visible under EVERY
        // interested tenant (the pre-fix find_map pick laundered one
        // tenant's visibility onto the rest); a materialization job is
        // created when EVERY wanted path is obtainable under SOME
        // tenant (owner Q2: any interested tenant's upstreams may
        // serve); leave-Ready (from-source) only when no tenant can
        // obtain. Candidates with NO tenant context probe once
        // unauthenticated (dev mode — visibility gating is moot).
        let tenant_sets: Vec<std::collections::BTreeSet<Uuid>> = candidates
            .iter()
            .map(|(h, _)| self.live_tenants_of(h))
            .collect();
        let mut probe_groups: std::collections::BTreeMap<Option<Uuid>, Vec<String>> =
            std::collections::BTreeMap::new();
        for ((_, paths), tenants) in candidates.iter().zip(&tenant_sets) {
            if tenants.is_empty() {
                probe_groups
                    .entry(None)
                    .or_default()
                    .extend(paths.iter().cloned());
            } else {
                for t in tenants {
                    probe_groups
                        .entry(Some(*t))
                        .or_default()
                        .extend(paths.iter().cloned());
                }
            }
        }

        struct ProbeAnswer {
            missing: HashSet<String>,
            substitutable: HashSet<String>,
            indeterminate: HashSet<String>,
        }
        let mut answers: std::collections::BTreeMap<Option<Uuid>, ProbeAnswer> =
            std::collections::BTreeMap::new();
        // Deliberately NOT gated on `cache_breaker`: dispatch-time
        // probe failure degrades to cache-miss (per-drv fallback /
        // next pass retries), not StoreUnavailable. The breaker is for
        // merge-time admission only — here the call IS the work.
        //
        // ONE AttemptBudget prices the whole sweep (bug_127): the old
        // shape awaited each tenant sequentially under a full
        // grpc_timeout, so T hung tenants stalled the actor T x 30 s —
        // unbounded in tenant count. Probes now fan out
        // buffer_unordered(min(T, MAX_PROBE_CONCURRENCY)) with each
        // attempt clamped to the budget's remainder, and an expired
        // budget short-circuits straight into the dropped-from-fold
        // arm. Worst-case actor stall: 1 x grpc_timeout regardless of
        // tenant count — inherited by any future partitioning of the
        // probe groups by construction.
        //
        // sh-044: capped at `DISPATCH_PROBE_SWEEP_BUDGET` (= 5.5 s).
        // Candidates are stamped at `probed_generation` BEFORE the FMP
        // fires; with a within-quota single-tenant batch the RPC is
        // all-or-nothing under `tokio::time::timeout(attempt_bound)` —
        // a >5.5 s store yields `ProbeOutcome::TimedOut` with NO
        // partial answer (`answers.is_empty()` → early return below),
        // so neither `locally_present` nor `to_create_job` makes
        // progress that tick. Fail-open (Ready dispatches from source
        // via the normal drain); under nominal store latency
        // (~100 ms/1k) headroom is ~50×. The oldest-first self-heal
        // is the OVER-QUOTA truncation mechanism (sort by
        // `probed_generation`, leave the tail unstamped) and only
        // applies when `candidates.len() > DISPATCH_PROBE_TICK_QUOTA`
        // — it does NOT shard a within-quota batch.
        // r[impl sched.dispatch.probe-budget]
        // r[impl sched.dispatch.probe-sweep-budget+2]
        let budget = rio_common::transport::AttemptBudget::new(
            self.grpc_timeout.min(super::DISPATCH_PROBE_SWEEP_BUDGET),
        );
        let probes: Vec<(Option<Uuid>, tonic::Request<FindMissingPathsRequest>)> = probe_groups
            .into_iter()
            .map(|(tenant, store_paths)| {
                let probe = self.probe_service_meta_for(tenant);
                let probe_meta: Vec<(&'static str, &str)> =
                    probe.iter().map(|(k, v)| (*k, v.as_str())).collect();
                let mut req = tonic::Request::new(FindMissingPathsRequest { store_paths });
                Self::inject_probe_meta(req.metadata_mut(), &probe_meta);
                (tenant, req)
            })
            .collect();
        let grpc_timeout = self.grpc_timeout;
        let fold = fan_out_probes(probes, &budget, grpc_timeout, |req| {
            let mut client = store.clone();
            async move { client.find_missing_paths(req).await }
        })
        .await;
        for (tenant, outcome) in fold {
            match outcome {
                ProbeOutcome::Answered(r) => {
                    answers.insert(
                        tenant,
                        ProbeAnswer {
                            missing: r.missing_paths.into_iter().collect(),
                            substitutable: r.substitutable_paths.into_iter().collect(),
                            indeterminate: r.indeterminate_paths.into_iter().collect(),
                        },
                    );
                }
                ref outcome => {
                    match outcome {
                        ProbeOutcome::Failed(e) => debug!(?tenant, error = %e,
                            "per-tenant Ready store-check FindMissingPaths failed; \
                             that tenant's answers drop from this pass's fold"),
                        ProbeOutcome::TimedOut => debug!(?tenant, timeout = ?grpc_timeout,
                            "per-tenant Ready store-check timed out; that tenant's \
                             answers drop from this pass's fold"),
                        ProbeOutcome::BudgetExpired => debug!(
                            ?tenant,
                            "per-tenant Ready store-check short-circuited by sweep \
                             budget expiry (RPC never issued); no store-health \
                             evidence (merged_bug_179)"
                        ),
                        ProbeOutcome::Answered(_) => unreachable!("matched above"),
                    }
                    // merged_bug_032 + merged_bug_179: the stamp
                    // decision goes through THE policy match — only
                    // ISSUED-RPC failures are store-health evidence.
                    if is_store_health_evidence(outcome) {
                        self.note_issued_store_rpc_failure("ready-check");
                    }
                }
            }
        }
        if answers.is_empty() {
            // Every probe failed: the pre-028 fail-open shape — tail
            // already in `checked`; head protected via the
            // probed_generation stamp at `ready_check_or_spawn`.
            debug!(
                candidates = candidates.len(),
                "all Ready store-check probes failed; \
                 dispatching fail-open (next pass batch-retries)"
            );
            return checked;
        }
        // I-139: collect-then-batch. The locally-present branch awaited
        // `complete_ready_from_store` per item (≥3 sequential PG RTTs
        // each); on warm-restart of a large closure ~all 2048 candidates
        // hit it → 12-30s actor stall → heartbeats missed → live workers
        // reaped.
        let mut locally_present = Vec::new();
        // Nodes routed to a materialization job (creation itself is
        // owned by `create_materialization_job` — leader-gated,
        // fenced, and dedup'd there).
        let mut to_create_job: Vec<DrvHash> = Vec::new();
        for ((drv_hash, paths), tenants) in candidates.into_iter().zip(tenant_sets) {
            checked.insert(drv_hash.clone());
            // The candidate's answer set: its own tenants' answers (or
            // the unauthenticated answer for a tenant-less candidate).
            // A tenant whose probe failed is ABSENT here — it can
            // satisfy neither the every-tenant visibility conjunction
            // nor the some-tenant obtainability existential
            // (conservative both ways); a candidate with NO surviving
            // answer takes no action this pass (stamped; the next
            // generation re-probes).
            let keys: Vec<Option<Uuid>> = if tenants.is_empty() {
                vec![None]
            } else {
                tenants.iter().map(|t| Some(*t)).collect()
            };
            let candidate_answers: Vec<&ProbeAnswer> =
                keys.iter().filter_map(|k| answers.get(k)).collect();
            let all_answered = candidate_answers.len() == keys.len();
            if candidate_answers.is_empty() {
                continue;
            }
            // r[impl sched.merge.wanted-outputs+3]
            // Demand-driven completeness: only the WANTED outputs must
            // be present (→ complete inline) or present-or-
            // substitutable (→ detached fetch). A missing output
            // nothing consumes must not force a from-source dispatch.
            // The wanted slice is the LIVE effective wanted set
            // (`effective_wanted` over live interested builds'
            // contributions; a terminal build's wants stop counting),
            // degrading to ALL DECLARED outputs when no live union
            // resolves (the conservative-absent branch, T-D2.3 — the
            // stored-union fallback is gone; divergence is
            // widening-only). `verifiable_wanted_paths` returns None
            // for a wanted set that resolves to no verifiable path;
            // degrade to all of `paths` then (and for a node that
            // vanished from the DAG mid-probe). The probe set stays
            // ALL expected paths (opportunistic completeness — fetch
            // the unwanted output too if the upstream has it).
            let wanted: Vec<String> = self
                .dag
                .node(&drv_hash)
                .and_then(|s| {
                    let eff = effective_wanted(s, &self.builds);
                    verifiable_wanted_paths(
                        &s.output_names,
                        &s.expected_output_paths,
                        eff.as_deref().unwrap_or(&[]),
                    )
                    .map(|w| w.into_iter().map(str::to_owned).collect::<Vec<String>>())
                })
                .unwrap_or_else(|| paths.clone());
            if all_answered
                && wanted
                    .iter()
                    .all(|p| candidate_answers.iter().all(|a| !a.missing.contains(p)))
            {
                // Present-and-visible under EVERY interested tenant
                // (merged_bug_028: the inline completion is the
                // visibility-laundering site — one tenant's view must
                // not complete another tenant's build).
                locally_present.push(drv_hash);
            } else if wanted.iter().all(|p| {
                candidate_answers.iter().any(|a| {
                    !a.missing.contains(p)
                        || a.substitutable.contains(p)
                        || a.indeterminate.contains(p)
                })
            }) {
                // r[impl sched.materialize.job+2]
                // r[impl sched.merge.substitute-probe-indeterminate+2]
                // Route to a materialization job. The job row is the
                // in-flight marker; the node stays Ready (claimable by
                // a store replica). Indeterminate (probe got
                // 429/5xx/deadline) is treated optimistically — same as
                // merge.rs; the job's own routing settles a genuine
                // miss.
                to_create_job.push(drv_hash);
            }
            // else: a wanted output is confirmed missing upstream and
            // not substitutable — leave Ready (dispatches from source).
        }
        // Signed Q2: every stamped tenant's OWN visibility-gated probe
        // answered present (batch_probe_cached_ready asks once per
        // live tenant and folds — merged_bug_028) — the all-tenant
        // stamp is lawful here.
        self.complete_ready_from_store_batch(
            &locally_present
                .into_iter()
                .map(|h| (h, crate::db::live_pins::StampProvenance::AllTenantProbe))
                .collect::<Vec<_>>(),
        )
        .await;
        // The probe-partition creation site — the standalone fenced
        // helper, no enclosing transaction (design §2.1 row 3).
        // sh-007c S5: one fenced batch over `to_create_job` (was N
        // serial `begin_fenced` round-trips). Defense-in-depth: skip
        // when the sweep has already crossed
        // `DISPATCH_PROBE_SWEEP_BUDGET` — the lease loop is
        // guard-isolated so this no longer self-fences, but the
        // batch's commit is best skipped past the budget the WARN
        // above names; the next probe_generation re-probes
        // (self-healing, no carrier at stake on this lane).
        if started.elapsed() <= super::DISPATCH_PROBE_SWEEP_BUDGET {
            self.create_materialization_jobs_batch(
                &to_create_job,
                crate::state::JobOrigin::CacheOpportunity,
            )
            .await;
        }
        checked
    }

    /// The deterministic LIVE tenant set of a node — the union of its
    /// interested builds' tenants, BTreeSet-ordered (merged_bug_028:
    /// every per-tenant probe fact is asked of THIS set; map order can
    /// never pick the answering tenant).
    pub(super) fn live_tenants_of(&self, drv_hash: &DrvHash) -> std::collections::BTreeSet<Uuid> {
        self.dag
            .node(drv_hash)
            .into_iter()
            .flat_map(|s| s.interested_builds.iter())
            .filter_map(|bid| self.builds.get(bid))
            .filter_map(|b| b.tenant_id)
            .collect()
    }

    /// Service-token metadata for ONE tenant's store probe
    /// (`FindMissingPaths`): `(service token, probe tenant id)` when
    /// `service_signer` is configured; empty (no-auth, dev mode)
    /// otherwise. Tenant context matters because the store's
    /// upstream-substitution probe resolves `tenant_upstreams` AND its
    /// sig-visibility gate from it. One-shot mint: each probe is a
    /// single bounded gRPC call (the re-mintable walk auth died with
    /// the walk). Since merged_bug_028 the dispatch and settlement
    /// probes ask once PER LIVE TENANT and fold — never one
    /// arbitrarily-picked tenant for a per-tenant fact.
    pub(super) fn probe_service_meta_for(
        &self,
        tenant_id: Option<Uuid>,
    ) -> Vec<(&'static str, String)> {
        match (&self.service_signer, tenant_id) {
            (Some(signer), Some(tenant_id)) => {
                let claims = rio_auth::hmac::ServiceClaims {
                    caller: "rio-scheduler".to_string(),
                    expiry_unix: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0)
                        + super::PROBE_TOKEN_EXPIRY.as_secs(),
                    // Probe tokens are not replica-bound (T-5.1: only the
                    // store's materialization client mints Some).
                    instance: None,
                };
                vec![
                    (rio_proto::SERVICE_TOKEN_HEADER, signer.sign(&claims)),
                    (rio_proto::PROBE_TENANT_ID_HEADER, tenant_id.to_string()),
                ]
            }
            _ => Vec::new(),
        }
    }

    fn inject_probe_meta(md: &mut tonic::metadata::MetadataMap, meta: &[(&'static str, &str)]) {
        for (k, v) in meta {
            if let Ok(mv) = tonic::metadata::MetadataValue::try_from(*v) {
                md.insert(*k, mv);
            }
        }
    }

    // r[impl sched.merge.substitute-topdown+13]
    /// Topdown-pruned fail-fast: the node's dep subgraph was dropped
    /// from its submission, so a from-source build dispatch cannot
    /// succeed (the worker ENOENTs on inputDrvs that were never
    /// merged). Fail every interested build with a resubmit-directing
    /// error and park the node instead of leaving it dispatchable.
    ///
    /// `cause` is the user-facing reason spliced into the build's
    /// error summary ("topdown-pruned root `<hash>`: `<cause>`; resubmit
    /// to re-probe or full-merge").
    ///
    /// Sole caller: the materialization consumption routing's arm 3
    /// (route_unobtainable in materialize.rs) — an unobtainable outcome
    /// on a pruned-origin job whose live-wanted outputs are confirmed
    /// missing (the four-conjunct settlement; r[sched.materialize.settlement]).
    /// The walk-era callers (the SubstituteComplete Broken arm, the
    /// reap-time survivor settlement, the dispatch-probe fail-fast
    /// cell) died with the walk.
    pub(super) async fn fail_fast_pruned_root(&mut self, drv_hash: &DrvHash, cause: &str) {
        // A prior iteration of the same dispatch pass may already have
        // settled this node: `batch_probe_cached_ready` collects the
        // whole `to_fail_fast` layer up front, and the first node's
        // `cancel_build_derivations` transitions every other
        // sole-interest not-yet-dispatched node of that build —
        // including later entries of the list — to DependencyFailed,
        // persists it, and strips the build's interest. Re-running the
        // park here would resurrect that terminal verdict
        // (DependencyFailed→Queued is a valid reprobe edge) and leave a
        // non-terminal, zero-interest, dep-less orphan behind in memory
        // and PG that no reap collects and every recovery reloads.
        // Equally nothing to do when the node vanished or no build is
        // interested any more — the verdicts are already settled. (The
        // walk-era SubstituteComplete{ok=false} caller never tripped
        // this either: it only reached the helper for a node it had
        // just observed in the walk's in-flight status with live
        // interest.)
        let actionable = self
            .dag
            .node(drv_hash)
            .is_some_and(|s| !s.status().is_terminal() && !s.interested_builds.is_empty());
        if !actionable {
            debug!(%drv_hash,
                   "topdown fail-fast: node already terminal/orphaned (settled by an \
                    earlier iteration of this pass); skipping");
            return;
        }
        warn!(%drv_hash, cause,
              "topdown-pruned root cannot complete via substitution; \
               deps were dropped from DAG — failing build (resubmit \
               will re-probe or full-merge)");
        metrics::counter!("rio_scheduler_topdown_substitute_fail_total").increment(1);
        let msg =
            format!("topdown-pruned root {drv_hash}: {cause}; resubmit to re-probe or full-merge");
        // Queued (not Ready): zero DAG deps → vacuous Ready would
        // re-dispatch on the next Tick. cancel_build_derivations
        // strips interest below; with zero remaining interest the
        // node is reaped on the next sweep.
        // The fail-fast's one-shot is the consumed JOB: the arm-3
        // settlement resolves the pruned-origin job row terminally
        // (resolved_unobtainable) before calling this helper, so a
        // failover cannot re-arm the fail-fast from stale state — a
        // resubmitted genuinely-pruned root re-prunes and gets a fresh
        // pruned-origin job; a full merge re-declares the closure and
        // creates none.
        // The park is a materialization RELEASE through the kinded
        // chokepoint (the A2.5 law: every claim-held materialization
        // exit routes through `validate_transition_for_release` — the
        // node arrives Assigned from the arm-3 settlement, which
        // consumes a CLAIMED attempt's report and never requeues
        // first), and the persist consumes the release's RETURNED
        // target: no edge, no write. `deps_completed: false` is the
        // truthful dep verdict — the pruned root's deps were DROPPED
        // from the DAG, not completed (a vacuous all-deps-completed
        // would release to Ready and re-dispatch, which the
        // Queued-not-Ready design above forbids). An already-Queued
        // arrival is parked already: a same-value persist is a
        // comparand non-event (merged_bug_006) and the witness
        // discipline forbids the write regardless.
        let released_to = match self.dag.node_mut(drv_hash) {
            None => None,
            Some(s) if s.status() == DerivationStatus::Queued => None,
            Some(s) => {
                match s.reset_after_attempt(crate::state::AttemptKind::Materialization, false) {
                    Ok(crate::state::ReleaseOutcome::Released(to)) => Some(to),
                    // bug_120: already at a released status (Ready —
                    // the Queued case pre-guards above). Nothing
                    // moved; the same no-persist disposition as the
                    // Queued arm, without the skew WARN.
                    Ok(crate::state::ReleaseOutcome::AlreadyReleased(_)) => None,
                    Err(e) => {
                        warn!(%drv_hash, %e,
                              "topdown fail-fast: release toward Queued rejected; \
                               skipping persist (no edge, no write)");
                        None
                    }
                }
            }
        };
        if let Some(to) = released_to {
            self.persist_status(drv_hash, to, None).await;
        }
        for build_id in self.get_interested_builds(drv_hash) {
            if let Some(build) = self.builds.get_mut(&build_id) {
                // The message itself says "resubmit to re-probe or
                // full-merge" — TransientFailure is the wire signal for
                // "might work if retried". One struct, first wins.
                build.note_first_failure(crate::state::FirstFailure {
                    summary: msg.clone(),
                    failed_drv: Some(drv_hash.to_string()),
                    status: Some(rio_proto::types::BuildResultStatus::TransientFailure),
                });
            }
            self.cancel_build_derivations(
                build_id,
                &format!("build {build_id}: topdown-pruned root: {cause}"),
            )
            .await;
            if let Err(e) = self.transition_build_to_failed(build_id).await {
                error!(%build_id, error = %e,
                       "failed to persist build-failed after topdown fail-fast");
            }
        }
    }

    // r[impl sched.poison.clear-survivor-reevaluation+2]
    // r[impl sched.merge.substitute-topdown+13]
    /// Per-survivor verdicts after children were removed from the DAG.
    /// Shared by every leader-side removal path that leaves surviving
    /// parents behind: the terminal-build reap
    /// (`handle_cleanup_terminal_build`), admin `ClearPoison`
    /// (`handle_clear_poison`) and the poison-TTL sweep
    /// (`tick_process_expired_poisons`).
    ///
    /// One stranded shape is closed (the walk-era settlement arm — the
    /// reap-time fail-fast of a marked spent survivor — died with the
    /// walk consumption machinery; survivors carrying an unresolved
    /// materialization job are armed by the job itself, and marked
    /// survivors without one are re-classified by the next dispatch
    /// sweep / settled at consumption):
    ///
    ///  - A `Queued` survivor whose last un-produced children were
    ///    removed: it is now vacuously all-deps-completed, but no
    ///    completion event will ever promote it. Promote it to Ready,
    ///    push it and persist, so the next dispatch pass picks it up.
    ///    This is what un-blocks a parent the recovery condemnation
    ///    spared on co-ownership grounds
    ///    (`sched.recovery.failed-dep-cascade+2`'s MUST NOT clause): it
    ///    recovered Queued above a non-co-owned within-TTL poisoned
    ///    child, and the poison-clear removal of that child is its only
    ///    wake-up edge — without this arm it would sit Queued forever
    ///    and its build would hang (the L3 strand).
    ///
    /// Skipped: vanished nodes, terminal nodes (already settled), and
    /// nodes with no interested builds (no build left to hang).
    ///
    /// Leader-only: every caller is leader-gated — the reap hook runs
    /// inside its `is_leader()` block, `handle_clear_poison`'s only
    /// production caller is the leader-guarded admin RPC, and
    /// `handle_tick` no-ops on standby
    /// (`r[sched.lease.standby-tick-noop]`).
    pub(super) async fn reevaluate_removal_survivors(&mut self, survivors: &[DrvHash]) {
        for parent in survivors {
            let Some(node) = self.dag.node(parent) else {
                continue;
            };
            if node.status().is_terminal() || node.interested_builds.is_empty() {
                continue;
            }
            let status = node.status();
            // r[impl sched.materialize.job+2]
            // Substitution-replacement Phase B (T-4.3): a survivor
            // carrying an unresolved materialization job needs NOTHING
            // from this loop — the job is already the armed action
            // (design §2.1: "survivors with an unresolved job need
            // nothing"), and both arms below would race it: the
            // settlement spends the verification one-shot and spawns a
            // walk (a flag-on walk spawn for fresh work — the
            // criterion-3 violation) or fail-fasts the surviving builds
            // outright; the promotion arm would push a Ready node whose
            // builder pulls the kinded admission refuses anyway. The
            // job's own §2.4 consumption settlement is the survivor's
            // settlement authority.
            if self.has_unresolved_job(parent.as_str()) {
                continue;
            }
            if status == DerivationStatus::Queued
                && self.dag.all_deps_completed(parent)
                && let Some(s) = self.dag.node_mut(parent)
                && s.transition(DerivationStatus::Ready).is_ok()
            {
                // Promotion arm. A promoted marked-Broken survivor is
                // settled by the next dispatch sweep's settlement-aware
                // partition; an unmarked one dispatches normally.
                self.persist_status(parent, DerivationStatus::Ready, None)
                    .await;
            }
        }
    }

    // r[impl gw.activity.subst-progress+4]
    /// Relay byte-progress from a store replica's materialization
    /// execution to every interested build via
    /// `Event::SubstituteProgress` (BC-4: the
    /// `ReportMaterializationProgress` RPC posts the
    /// `ActorCommand::SubstituteProgress` this handles — the walk
    /// producer this relay was built for is deleted). Display-only
    /// (routed through the log broadcast ring, not persisted, reuses
    /// last seq); the gateway translates to `actCopyPath` +
    /// `resProgress`. Non-leader emits are fine — this is read-only
    /// (no DAG/PG mutation).
    pub(super) fn handle_substitute_progress(
        &mut self,
        drv_hash: &DrvHash,
        bytes_done: u64,
        bytes_expected: u64,
        upstream_uri: String,
    ) {
        let Some(state) = self.dag.node(drv_hash) else {
            return;
        };
        let drv_path = state.drv_path().to_string();
        let interested = state.interested_builds.clone();
        let event = rio_proto::types::build_event::Event::SubstituteProgress(
            rio_proto::types::SubstituteProgress {
                derivation_path: drv_path,
                bytes_done,
                bytes_expected,
                upstream_uri,
            },
        );
        for build_id in interested {
            self.events.emit(build_id, event.clone());
        }
    }

    /// Batched `complete_ready_from_store`:
    /// transition + `output_paths` set in-mem first (no await), then a
    /// joined `persist_status_batch(Completed)` ∥
    /// `upsert_path_tenants_for_batch` (disjoint tables — sh-007b
    /// S3-lite), one batched newly-ready promote, then per-BUILD (not
    /// per-drv) summary/counts/completion-check. I-139: the per-item
    /// variant in `batch_probe_cached_ready`'s locally-present branch
    /// was 3 sequential PG awaits × ≤2048 candidates → 12-30s actor
    /// stall on warm-restart of a large closure.
    // pub(super): also called by the materialization consumption handler
    // (the Success/moot-covered arms complete through this same chokepoint).
    pub(super) async fn complete_ready_from_store_batch(
        &mut self,
        items: &[(DrvHash, crate::db::live_pins::StampProvenance)],
    ) {
        if items.is_empty() {
            return;
        }
        #[cfg(test)]
        self.test_counters
            .complete_ready_batch_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        struct Done {
            hash: DrvHash,
            drv_path: String,
            output_paths: Vec<String>,
            interested: HashSet<Uuid>,
        }
        let mut ok: Vec<Done> = Vec::with_capacity(items.len());
        let mut ok_items: Vec<(DrvHash, crate::db::live_pins::StampProvenance)> =
            Vec::with_capacity(items.len());
        for (drv_hash, provenance) in items {
            let Some(state) = self.dag.node_mut(drv_hash) else {
                continue;
            };
            if let Err(e) = state.transition(DerivationStatus::Completed) {
                warn!(drv_hash = %drv_hash, error = %e,
                      "store-hit Ready→Completed rejected; dispatching instead");
                continue;
            }
            // IA-only convenience: `expected_output_paths` IS the
            // realised path. Non-destructive when a path is already
            // known — a caller that resolved the REALIZED floating-CA
            // path (the materialization consumption path carries it on
            // the job report) must not have it clobbered with
            // `expected_output_paths == [""]`, which would drop GC
            // retention and emit `[""]` to clients.
            if state.output_paths.is_empty() {
                state.output_paths = state.expected_output_paths.clone();
            }
            ok.push(Done {
                hash: drv_hash.clone(),
                drv_path: state.drv_path().to_string(),
                output_paths: state.output_paths.clone(),
                interested: state.interested_builds.clone(),
            });
            ok_items.push((drv_hash.clone(), provenance.clone()));
        }
        if ok.is_empty() {
            return;
        }
        info!(
            count = ok.len(),
            "outputs already in store; skipping dispatch"
        );
        metrics::counter!("rio_scheduler_cache_hits_total", "source" => "dispatch")
            .increment(ok.len() as u64);

        let ok_hashes: Vec<DrvHash> = ok.iter().map(|d| d.hash.clone()).collect();
        let ok_refs: Vec<&str> = ok_hashes.iter().map(|h| h.as_str()).collect();

        // Batched promote: dedup find_newly_ready across all completed
        // hashes, transition in-mem, then one
        // persist_status_batch(Ready). Same shape as the
        // ca_cutoff_cascade batched-promote. sh-007c S5 hoist: this
        // walk depends only on in-mem `dag.node(h).status` (every
        // `ok` row already transitioned Completed at the top of this
        // fn), NOT the PG write — so it runs BEFORE the join and the
        // Ready persist becomes the join's third arm.
        let mut newly_ready: Vec<DrvHash> = Vec::new();
        let mut seen_ready: HashSet<DrvHash> = HashSet::new();
        for h in &ok_hashes {
            for ready_hash in self.dag.find_newly_ready(h) {
                if !seen_ready.insert(ready_hash.clone()) {
                    continue;
                }
                if let Some(s) = self.dag.node_mut(&ready_hash)
                    && s.transition(DerivationStatus::Ready).is_ok()
                {
                    newly_ready.push(ready_hash);
                }
            }
        }
        // Disjoint-rows witness: a node going Completed is never its
        // own dependent — `Completed→Ready` is invalid, so
        // `s.transition(Ready)` above structurally rejects any
        // `ok_hashes` member. The two `derivations`-table writes
        // therefore touch disjoint row sets and the join holds.
        debug_assert!(
            {
                let ok_set: HashSet<&str> = ok_refs.iter().copied().collect();
                newly_ready.iter().all(|h| !ok_set.contains(h.as_str()))
            },
            "newly_ready ∩ ok_hashes must be empty (Completed→Ready invalid)"
        );
        let ready_refs: Vec<&str> = newly_ready.iter().map(|h| h.as_str()).collect();

        // sh-007c S5 (3-way join, the §Nth-strike option-b on-actor
        // re-decision): Completed persist (`derivations` rows
        // `ok_refs`) ∥ path-tenant/deriver upsert
        // (`path_tenants`/`narinfo`) ∥ Ready persist (`derivations`
        // rows `ready_refs`). Disjoint rows (the debug_assert above)
        // and disjoint tables for the tenant arm — no FK or
        // row-ordering dependency. Both `_db` arms hold `&self.db`;
        // the two `&mut self` Err-arm latches run AFTER the join,
        // serially. Infallible error type: every arm is best-effort
        // and never propagates, so `try_join!` never short-circuits —
        // the macro is the run-concurrently primitive, not an error
        // funnel.
        //
        // **Commit-order weakening (subtraction-only for the in-mem
        // actor):** the Completed and Ready batches are now two
        // INDEPENDENT `begin_fenced` txns on separate pool
        // connections, so PG commit order between them is
        // nondeterministic. Benign on this store-hit-only path: this
        // fn is reached only when outputs ARE durably in the store
        // (cache-hit / materialization-success); a recovered Ready
        // dependent whose dep's status row is stale still has its
        // inputs durably in the store, and recovery's A2.5 rider
        // (`recovery.rs` `revert_target_for` →
        // `test_recovery_heals_corrupted_ready`) maps a
        // Ready-with-unbuilt-deps row back to Queued at failover, then
        // the dep re-probes as a store hit on the next tick —
        // one-tick efficiency cost only. Fresh-write-only,
        // `transition_build` DB-first invariant, fence-coverage
        // census, and ack-after-durable are UNTOUCHED.
        let generation = self.serving_generation();
        let Ok((completed_r, (), ready_r)) = tokio::try_join!(
            async {
                Ok::<_, std::convert::Infallible>(
                    Self::persist_status_batch_db(
                        &self.db,
                        &ok_refs,
                        DerivationStatus::Completed,
                        generation,
                    )
                    .await,
                )
            },
            async {
                self.upsert_path_tenants_for_batch(&ok_items).await;
                Ok(())
            },
            async {
                Ok::<_, std::convert::Infallible>(
                    Self::persist_status_batch_db(
                        &self.db,
                        &ready_refs,
                        DerivationStatus::Ready,
                        generation,
                    )
                    .await,
                )
            },
        );
        self.handle_persist_status_batch_result(&ok_refs, DerivationStatus::Completed, completed_r);
        self.handle_persist_status_batch_result(&ready_refs, DerivationStatus::Ready, ready_r);

        // Per-build (not per-drv): emit one cached event per (drv,
        // interested-build), then a single summary scan + counts +
        // completion-check per distinct build. I-103: dispatch-time
        // short-circuit counts as cached.
        let mut cached_per_build: HashMap<Uuid, u32> = HashMap::new();
        for d in &ok {
            let event = rio_proto::types::build_event::Event::Derivation(
                rio_proto::types::DerivationEvent::cached(
                    d.drv_path.clone(),
                    d.output_paths.clone(),
                ),
            );
            for &build_id in &d.interested {
                self.events.emit(build_id, event.clone());
                *cached_per_build.entry(build_id).or_default() += 1;
            }
        }
        // sh-007c S5: collect the per-build counts tuples and persist
        // ONCE (UNNEST UPDATE on `builds`) before the
        // `check_build_completion` loop — replaces N serial
        // `persist_build_counts` RTTs at the iter3 actor profile's
        // phase-17 hot path.
        let cached_builds: Vec<(Uuid, u32)> = cached_per_build.into_iter().collect();
        let mut counts: Vec<(Uuid, u32, u32, u32)> = Vec::with_capacity(cached_builds.len());
        for &(build_id, n) in &cached_builds {
            // r[impl sched.build.terminal-status-settled+3]
            // Dispatch-time store hits can fan out to resident terminal
            // builds that retained interest on the shared node (a
            // stale-Completed reset under a later build re-dispatched
            // it); their served accounting and progress are frozen at
            // the terminal transition. The per-drv DerivationCached
            // event above still flows.
            if let Some(b) = self.builds.get_mut(&build_id)
                && !b.state().is_terminal()
            {
                b.cached_count += n;
            }
            // I-140: one build_summary scan shared, not two.
            let summary = self.dag.build_summary(build_id);
            if let Some((t, c, h)) = self.update_build_counts_with(build_id, &summary) {
                counts.push((build_id, t, c, h));
            }
            self.emit_progress_with(build_id, &summary);
        }
        self.persist_build_counts_batch(&counts).await;
        for &(build_id, _) in &cached_builds {
            self.check_build_completion(build_id).await;
        }
    }

    /// The post-assignment emit phase (phase 4 of the retired
    /// stream-era `assign_to_worker` flow): emit
    /// `DerivationStarted` + progress to interested gateways.
    pub(super) fn emit_assignment_started(&mut self, drv_hash: &DrvHash, executor_id: &ExecutorId) {
        let drv_path = self.dag.path_or_hash_fallback(drv_hash);
        // The execution this dispatch minted (`assign_to_worker` set
        // `state.exec_id = Some(..)` before calling here). The gateway
        // keys its per-execution TailLog subscription on this; a
        // duplicate Started carrying a *different* exec_id tells it the
        // derivation was re-dispatched and the old subscription is
        // stale. Empty only on the unreachable node-vanished race
        // (the event is then display-only noise for an already-dead
        // derivation).
        //
        // sh-042: single `self.dag.node` bind for exec_id +
        // predecessor (the `state.*` reads). `mint_and_deliver`
        // stamped `state.sched.last_intent` (pull.rs:1227) and the
        // predecessor's close site pushed onto `attempt_history()` in
        // a PRIOR actor turn — both are readable here without DB.
        // The owned `DerivationEvent` is built BEFORE the
        // `get_interested_builds` loop (drop-the-borrow shape) and
        // cloned per build — N interested builds share one
        // construction (one `executor_id.to_string()` Display-format,
        // one predecessor clone) instead of N (sh-042-r1).
        let (exec_id, predecessor) = match self.dag.node(drv_hash) {
            Some(state) => (
                state.exec_id.map(|id| id.to_string()).unwrap_or_default(),
                predecessor_for_started(
                    state.attempt_history(),
                    state.retry.resubmit_cycles,
                    state.sched.last_intent.as_ref(),
                ),
            ),
            None => (String::new(), None),
        };
        let event = rio_proto::types::DerivationEvent {
            predecessor,
            ..rio_proto::types::DerivationEvent::started(drv_path, executor_id.to_string(), exec_id)
        };
        for build_id in self.get_interested_builds(drv_hash) {
            self.events.emit(
                build_id,
                rio_proto::types::build_event::Event::Derivation(event.clone()),
            );
            // Progress snapshot: running count +1, worker set changed.
            // Critpath unchanged on dispatch (no completion) — but the
            // dashboard also uses Progress for running/queued columns.
            self.emit_progress(build_id);
        }
    }

    /// Construct the [`WorkAssignment`] proto for `drv_hash` →
    /// `executor_id`: CA-input resolve, HMAC token sign, build-options
    /// lookup. Side-effect: stashes `pending_realisation_deps` on the
    /// node so `handle_success_completion` can write the realisation FK
    /// rows post-build.
    ///
    /// Returns `None` if the DAG node is gone (TOCTOU vs. concurrent
    /// cancel) — caller treats that as assignment failure.
    ///
    /// [`WorkAssignment`]: rio_proto::types::WorkAssignment
    pub(super) async fn build_assignment_proto(
        &mut self,
        drv_hash: &DrvHash,
        executor_id: &ExecutorId,
        attempt_kind: rio_evidence_kernel::pull::PullKind,
    ) -> Option<rio_proto::types::WorkAssignment> {
        // merged_bug_026: the PRODUCER asserts the attempt↔job binding.
        // A materialization delivery names the job it is minted/held
        // under (the current job-view entry for the drv — the same
        // source the kernel's admission consulted, so delivery and
        // admission cannot disagree on which job this is); a
        // build-kind payload has no materialization job and sends
        // empty (the proto's documented absent state).
        let materialization_job = match attempt_kind {
            rio_evidence_kernel::pull::PullKind::Materialization => self
                .materialization_jobs
                .hydrated()
                .and_then(|view| view.get(drv_hash))
                .map(|entry| entry.job_id),
            rio_evidence_kernel::pull::PullKind::Build => None,
        };
        // CA input resolution: rewrite placeholder paths in
        // env/args/builder to realized output paths before
        // dispatch. Fires when gateway set needs_resolve (ADR-018
        // Appendix B: floating-CA self OR ia.deferred — IA drv
        // with a floating-CA input).
        //
        // `maybe_resolve_ca` returns the (possibly rewritten)
        // drv_content PLUS the realisation lookups performed. On
        // resolve error (missing realisation, PG blip) it logs and
        // returns the original unresolved bytes + empty lookups —
        // the worker's build fails on the placeholder path, which
        // is the correct signal (retry after the realisation lands).
        //
        // The resolve runs in its OWN scoped borrow of `self.dag`
        // (node() + collect_ca_inputs both &-borrow) so the lookups
        // can be stashed via node_mut() below before the main
        // WorkAssignment construction takes its own & borrow.
        let (drv_content_to_send, resolve_lookups, resolved_output_paths) = {
            let state = self.dag.node(drv_hash)?;
            self.maybe_resolve_ca(drv_hash, state).await
        };

        // Stash lookups for handle_success_completion's
        // insert_realisation_deps (the FK needs the parent's own
        // realisation row to exist, which only happens post-build).
        // Empty vec → no-op; non-empty only for CA-on-CA chains
        // that actually resolved.
        //
        // Deferred-IA: also overwrite expected_output_paths with the
        // post-resolve computed paths (index-aligned with output_names)
        // so the HMAC `expected_outputs` claim below carries the real
        // path, not `""`. Floating-CA leaves resolved_output_paths
        // empty → no overwrite (its HMAC path is `is_ca` instead).
        if (!resolve_lookups.is_empty() || !resolved_output_paths.is_empty())
            && let Some(state) = self.dag.node_mut(drv_hash)
        {
            state.ca.pending_realisation_deps = resolve_lookups;
            for (name, path) in resolved_output_paths {
                if let Some(i) = state.output_names.iter().position(|n| n == &name)
                    && let Some(slot) = state.expected_output_paths.get_mut(i)
                {
                    *slot = path;
                }
            }
        }

        // ADR-022 castore-FUSE (P0588): resolve the transitive input
        // closure + castore root nodes. On PG failure or timeout we
        // send empty input_roots and the builder falls back to
        // QueryPathInfo BFS. Timeout-bounded like every other
        // actor-blocking PG await on this path (I-139): a slow
        // recursive CTE must not stall the mailbox.
        //
        // Seeds come from `attested_input_seeds` (the parsed drv's
        // exact direct inputs), NOT `approx_input_closure`: this
        // closure is signed into the assignment token (P0589) and is
        // the builder's refscan candidate set, so it must never be
        // narrower than the true input closure. `None` (recovered
        // node without drv_content, .drv not inlined, or an inputDrv
        // whose outputs aren't known) → no attestation; the builder
        // computes its own drv-parsed closure — the same degradation
        // as the PG-failure arms below.
        //
        // Build-only: a Materialization pull has no .drv to refscan
        // (the worker materialises an already-built closure), so the
        // attested closure is structurally empty there → the
        // empty-digest "no attestation" sentinel that validate_begin
        // (P0586) already treats as scheduler-couldn't-compute.
        // r[impl sched.dispatch.input-roots+2]
        let (input_root_rows, input_closure, input_closure_digest) = match attempt_kind {
            rio_evidence_kernel::pull::PullKind::Build => {
                let input_root_rows =
                    match crate::assignment::attested_input_seeds(&self.dag, drv_hash) {
                        None => {
                            debug!(drv_hash = %drv_hash,
                                   "input closure not attestable from scheduler state; \
                                    builder falls back to its own drv-parsed closure");
                            metrics::counter!("rio_scheduler_input_closure_unattested_total",
                                              "reason" => "seeds_unknown")
                            .increment(1);
                            Vec::new()
                        }
                        Some(seeds) if seeds.is_empty() => Vec::new(),
                        Some(seeds) => {
                            match tokio::time::timeout(
                                self.grpc_timeout,
                                self.db.compute_input_roots(&seeds),
                            )
                            .await
                            {
                                Ok(Ok(Some(rows))) => rows,
                                // A closure member with no narinfo row → the walk
                                // can't prove the set complete (already warned in
                                // compute_input_roots). Same degrade as the arms
                                // below: no attestation, builder computes its own
                                // drv-parsed closure.
                                Ok(Ok(None)) => {
                                    metrics::counter!(
                                        "rio_scheduler_input_closure_unattested_total",
                                        "reason" => "missing_narinfo"
                                    )
                                    .increment(1);
                                    Vec::new()
                                }
                                Ok(Err(e)) => {
                                    warn!(drv_hash = %drv_hash, error = %e,
                                          "input_roots closure compute failed; \
                                           builder falls back to QueryPathInfo BFS");
                                    metrics::counter!(
                                        "rio_scheduler_input_closure_unattested_total",
                                        "reason" => "db_error"
                                    )
                                    .increment(1);
                                    Vec::new()
                                }
                                Err(_) => {
                                    warn!(drv_hash = %drv_hash, timeout = ?self.grpc_timeout,
                                          "input_roots closure compute timed out; \
                                           builder falls back to QueryPathInfo BFS");
                                    metrics::counter!(
                                        "rio_scheduler_input_closure_unattested_total",
                                        "reason" => "timeout"
                                    )
                                    .increment(1);
                                    Vec::new()
                                }
                            }
                        }
                    };
                // Cloned once: reused for digest and WorkAssignment.input_closure.
                let input_closure: Vec<String> = input_root_rows
                    .iter()
                    .map(|r| r.store_path.clone())
                    .collect();
                // Wire-compat: a non-empty digest is serialized in the token;
                // a pre-P0589 store rejects it on deny_unknown_fields. The
                // store fleet must roll before the scheduler singleton (or
                // wipe deploy) — see r[common.hmac.claims].
                let input_closure_digest = if input_closure.is_empty() {
                    // Empty = no attestation. validate_begin (P0586) treats it
                    // as "scheduler couldn't compute", not "closure was empty".
                    String::new()
                } else {
                    rio_auth::hmac::AssignmentClaims::digest_input_closure(&input_closure)
                };
                (input_root_rows, input_closure, input_closure_digest)
            }
            rio_evidence_kernel::pull::PullKind::Materialization => {
                (Vec::new(), Vec::new(), String::new())
            }
        };

        let state = self.dag.node(drv_hash)?;
        let build_opts = self.build_options_for_derivation(drv_hash);

        // Assignment token: HMAC-signed if configured, else
        // legacy format-string. The store verifies signed
        // tokens on PutPath (prevents arbitrary-path upload
        // from a compromised worker). Unsigned tokens are
        // accepted by a store with hmac_verifier=None (dev).
        //
        // Expiry: 2× build_timeout (or 2× daemon_timeout
        // default if timeout=0), bounded by the 7-day LIFETIME
        // law below (expiry − now ≤ 7d for any requested
        // timeout). A worker legitimately uploading after
        // completion is well within that window. Prevents
        // replay from a leaked token later.
        let assignment_token = if let Some(signer) = &self.hmac_signer {
            // Typed consume (merged_bug_034): the folded wire
            // value re-enters the WireSecs domain — it is
            // ceiling-bounded by construction (the tenant seam
            // mints, the fold preserves the bound), and the
            // unset arm reads the SHARED daemon-default const
            // (rio-builder's DEFAULT_DAEMON_TIMEOUT derives
            // from the same symbol — no mirrored 7200s, R14).
            let timeout_secs = rio_common::clamped::WireSecs::from_wire(build_opts.build_timeout)
                .to_duration_nonzero()
                .map_or(rio_common::clamped::DAEMON_DEFAULT_TIMEOUT_SECS, |d| {
                    d.as_secs()
                });
            // r[impl common.hmac.claims+3]
            // E3, the token-LIFETIME law, INDEPENDENT of the wire
            // ceiling (1 yr): the 7-day bound is on the EXPIRY —
            // the security-relevant quantity is a leaked token's
            // replay window, which is expiry − now, not the
            // timeout input. The ×2 grace (build run time plus the
            // upload/report tail) lives INSIDE the bound: the
            // timeout clamp is derived as lifetime ÷ grace, so
            // expiry − now ≤ MAX_HMAC_LIFETIME_SECS by
            // construction. Pre-fix the clamp bounded the timeout
            // and then doubled it — a 14-day effective window
            // under a "7 days max" comment. merged_bug_045: the
            // const MOVED into the signer (`rio_auth::hmac`) — the
            // LAW is `HmacKey::sign`'s family clamp now; this
            // derivation keeps the ×2 grace inside the bound so the
            // signed expiry equals the requested one (a mint the
            // family clamp would cut is a derivation bug, not a
            // security event).
            use rio_auth::hmac::MAX_HMAC_LIFETIME_SECS;
            const HMAC_LIFETIME_GRACE_FACTOR: u64 = 2;
            let timeout_secs =
                timeout_secs.min(MAX_HMAC_LIFETIME_SECS / HMAC_LIFETIME_GRACE_FACTOR);
            let expiry_unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
                .saturating_add(timeout_secs.saturating_mul(HMAC_LIFETIME_GRACE_FACTOR));
            signer.sign(&rio_auth::hmac::AssignmentClaims {
                executor_id: executor_id.to_string(),
                drv_hash: drv_hash.to_string(),
                expected_outputs: state.expected_output_paths.clone(),
                // Floating-CA: output path is computed post-build
                // from the NAR hash, so expected_output_paths is
                // [""] here. Store skips the path-in-claims check
                // when is_ca is set (verify-on-put still hashes
                // the NAR independently; threat model holds).
                // Fixed-output CA (FOD) has a known path → treat
                // as IA for the membership check.
                is_ca: state.ca.is_ca && !state.is_fixed_output,
                expiry_unix,
                // Tenant attribution for hw_perf_samples.submitting_tenant (M_054).
                // Phase 2 of the bug_011 two-phase rollout (Phase 1 = fb096e50f);
                // safe to set unconditionally since fb096e50f's `skip_serializing_if`
                // + `#[serde(default)]` cover both rolling-upgrade skew directions.
                tenant: state.attributed_tenant(&self.builds).map(|u| u.to_string()),
                input_closure_digest,
            })
        } else {
            // Legacy unsigned: format-string. Store with
            // hmac_verifier=None accepts this (content is not
            // verified; the executor/drv pair keeps it unique enough
            // for log correlation).
            format!("{executor_id}-{drv_hash}")
        };

        Some(rio_proto::types::WorkAssignment {
            drv_path: state.drv_path().to_string(),
            // Forward what the gateway inlined (or empty → worker
            // fetches from store). Gateway only inlines for nodes
            // whose outputs are MISSING (will-dispatch), so cache
            // hits don't bloat this. Worker already handles both
            // paths (executor/mod.rs:241 branches on is_empty).
            // For CA-depends-on-CA derivations, this is the
            // RESOLVED ATerm (placeholders replaced by realized
            // paths) — see maybe_resolve_ca above.
            drv_content: drv_content_to_send,
            output_names: state.output_names.clone(),
            build_options: Some(build_opts),
            assignment_token,
            is_fixed_output: state.is_fixed_output,
            traceparent: state.traceparent.clone(),
            // Intent for this drv (matches the SpawnIntent that spawned the
            // pod). Builder clamps to cgroup cpu.max so a wildcard worker
            // (different intent) still gets ground-truth. Populating this
            // makes resolve_build_opts override client `--cores N`. The
            // telemetry side (build_samples.cpu_limit_cores) records this
            // same value at completion — see record_build_sample.
            assigned_cores: state.sched.last_intent.as_ref().map(|i| i.cores),
            // mem/disk are display-only on the worker side — the `rio:
            // builder` banner renders them; runtime limits come from the
            // pod's cgroup, which the spawn intent already shaped. Populate
            // from the same `SolvedIntent` so the banner shows what was
            // actually fitted instead of `?`. Absent (no intent — wildcard
            // worker, or pre-ADR-023 path) → banner renders `?`.
            assigned_mem_bytes: state.sched.last_intent.as_ref().map(|i| i.mem_bytes),
            assigned_disk_bytes: state.sched.last_intent.as_ref().map(|i| i.disk_bytes),
            // Per-execution identifier minted in `assign_to_worker` and
            // stored on `DerivationState`. The worker echoes this in the
            // log header (display-only). Empty only on the unreachable
            // path where the dag node lost its exec_id between assign and
            // proto-build — the actor is single-threaded so that can't
            // happen, but the field is non-Option in the proto.
            exec_id: state.exec_id.map(|u| u.to_string()).unwrap_or_default(),
            // merged_bug_026: the producer-asserted job binding (see
            // the resolution at the top of this fn). Empty for builds
            // and for a materialization whose view entry vanished
            // mid-delivery (the client then falls back to its own
            // identity — the pre-field behavior).
            job_id: materialization_job
                .map(|u| u.to_string())
                .unwrap_or_default(),
            input_closure,
            input_roots: input_root_rows
                .into_iter()
                .map(|r| {
                    // Corrupt RootNode blob → treat as unindexed and log.
                    // Indexer/scheduler encoding skew would otherwise
                    // surface only as the builder always GetNarIndex'ing.
                    let root_node = r.root_node.and_then(|bytes| {
                        prost::Message::decode(bytes.as_slice())
                            .map_err(|e| {
                                warn!(store_path = %r.store_path, error = %e,
                                      "corrupt nar_index.root_node; \
                                       sending without root");
                            })
                            .ok()
                    });
                    rio_proto::types::InputRoot {
                        store_path: r.store_path,
                        root_node,
                    }
                })
                .collect(),
        })
    }

    /// If this derivation is CA-floating with CA inputs, resolve
    /// placeholder paths to realized output paths before dispatch.
    /// Returns `(drv_content_bytes, realisation_lookups)`: the
    /// (possibly rewritten) ATerm plus every
    /// `(dep_modular_hash, dep_output_name) → realized_path` lookup
    /// the resolve performed. Caller stashes lookups on
    /// `DerivationState.ca.pending_realisation_deps` for the
    /// completion-time `insert_realisation_deps` call (the FK needs
    /// the parent's OWN realisation row to exist first).
    ///
    /// ADR-018 Appendix B: resolve fires when `needs_resolve` is set
    /// by the gateway — floating-CA self (`has_ca_floating_outputs`)
    /// OR any inputDrv is floating-CA (`ia.deferred`: an IA drv
    /// depending on a CA input has the CA placeholder embedded in
    /// its env/args). Fixed-output CA with no CA inputs doesn't need
    /// resolve — its output path AND its inputs' paths are all
    /// eval-time known.
    ///
    /// The resolve step queries the `realisations` table for each CA
    /// input's `(modular_hash, output_name)` → `output_path`, then
    /// string-replaces placeholders through the ATerm. Each lookup
    /// is staged for `realisation_deps` INSERT (rio's derived build
    /// trace, per ADR-018:45) — though the actual INSERT is deferred
    /// to completion time (the FK needs the parent's OWN realisation
    /// to exist, which only happens post-build).
    ///
    /// Error handling: resolve failure (missing realisation, PG blip)
    /// logs and returns the original unresolved bytes + empty lookups.
    /// The worker's build will then fail on the placeholder path not
    /// existing (`/1ril1qzj...` is not a real store path), triggering
    /// the normal retry-with-backoff. This is correct: a missing
    /// realisation means the input's `wopRegisterDrvOutput` hasn't
    /// landed yet (race), and retry-after-backoff gives it time to.
    async fn maybe_resolve_ca(
        &self,
        drv_hash: &DrvHash,
        state: &crate::state::DerivationState,
    ) -> (
        Vec<u8>,
        Vec<crate::ca::RealisationLookup>,
        Vec<(String, String)>,
    ) {
        // Gate: ADR-018 Appendix B `shouldResolve`. Gateway computes
        // `needs_resolve = has_ca_floating_outputs() || any inputDrv
        // is floating-CA` at translate time. Covers both floating-CA
        // self AND ia.deferred (IA with CA inputs — the CA input's
        // placeholder is embedded in this drv's env/args and needs
        // rewriting to the realized path).
        if !state.ca.needs_resolve {
            return (state.drv_content.clone(), Vec::new(), Vec::new());
        }

        // Build the input lists: walk DAG children, split into CA
        // and IA. For CA children we need the MODULAR hash (the
        // `realisations` table key, plumbed by the gateway via
        // `DerivationNode.ca.modular_hash`). For IA children we need
        // the `expected_output_paths` (deterministic, computed at
        // gateway submit time from the parsed `.drv`).
        //
        // Nix's `tryResolve` (derivations.cc:1206-1234) iterates ALL
        // inputDrvs regardless of addressing mode, adding each output
        // path to `inputSrcs`. CA outputs come from realisations; IA
        // outputs are concrete and already in the DAG.
        //
        // Floating-CA with NO inputs at all (rare: a leaf CA drv with
        // only fixed srcs) doesn't need resolve — nothing to collapse
        // into inputSrcs. Short-circuit before the ATerm parse.
        let ca_inputs = self.collect_ca_inputs(drv_hash);
        let ia_inputs = self.collect_ia_inputs(drv_hash);
        if ca_inputs.is_empty() && ia_inputs.is_empty() {
            return (state.drv_content.clone(), Vec::new(), Vec::new());
        }

        // No drv_content → recovered derivation (scheduler restart,
        // DAG reloaded from PG, drv_content not persisted). The store
        // has the ATerm — fetch it. Workers do the same when the
        // inline is empty (build_types.proto:231: "Empty = fallback;
        // worker fetches via GetPath"). ~10-50ms round-trip, once
        // per recovered floating-CA dispatch.
        //
        // Checked AFTER the both-empty short-circuit: a recovered
        // floating-CA with no DAG inputs doesn't need resolve and
        // doesn't need the fetch — worker fetches the unresolved
        // `.drv` from the store itself (same path it always does
        // when `drv_content` is empty). Any floating-CA WITH inputs
        // (CA or IA) needs the scheduler-side fetch so
        // `resolve_ca_inputs` can parse `inputDrvs` and serialize
        // the resolved `BasicDerivation` form.
        //
        // The same lossy-on-recovery pattern still applies to
        // `ca_modular_hash` (see `collect_ca_inputs`'s skip-on-None)
        // and `pending_realisation_deps` (best-effort cache,
        // reconstituted here on each resolve).
        //
        // r[impl sched.ca.resolve+3]
        let drv_content = if state.drv_content.is_empty() {
            match self.fetch_drv_content_from_store(drv_hash, state).await {
                Some(bytes) => bytes,
                None => {
                    // Store unreachable or .drv not found — dispatch
                    // unresolved (worker fails on placeholder,
                    // self-heals via retry after a fresh SubmitBuild
                    // re-merges with inline drv_content). Same
                    // degrade as before P0408.
                    warn!(
                        drv_hash = %drv_hash,
                        "recovered CA-on-CA dispatch: drv_content empty + store fetch failed; \
                         dispatching unresolved (worker will fail on placeholder)"
                    );
                    return (state.drv_content.clone(), Vec::new(), Vec::new());
                }
            }
        } else {
            state.drv_content.clone()
        };

        match crate::ca::resolve_ca_inputs(&drv_content, &ca_inputs, &ia_inputs, self.db.pool())
            .await
        {
            Ok(resolved) => {
                debug!(
                    drv_hash = %drv_hash,
                    n_ca_inputs = ca_inputs.len(),
                    n_ia_inputs = ia_inputs.len(),
                    n_lookups = resolved.lookups.len(),
                    "CA resolve: rewrote placeholders + collapsed inputSrcs for dispatch"
                );
                (
                    resolved.drv_content,
                    resolved.lookups,
                    resolved.output_paths,
                )
            }
            Err(e) => {
                // Swallow-to-warn for ALL ResolveError variants,
                // including `Db` (transient PG blip). Rationale:
                // the unresolved dispatch → worker fails on the
                // placeholder path → retry-with-backoff fires →
                // the next attempt's mint re-runs resolve (the
                // pull admission holds the fresh mint until
                // `backoff_until` lapses — bug_282). For
                // `RealisationMissing`, the backoff gives the
                // input's `wopRegisterDrvOutput` time to land
                // (race). For `Db`, the backoff IS the retry-PG
                // mechanism — the wasted worker cycle (~seconds
                // to fail on ENOENT) is acceptable vs adding a
                // defer-and-requeue path here (the pull-admission
                // gate already provides the timing on the FAILURE
                // path). Slot-wasteful
                // but correct; profiling can drive a `Db → defer`
                // split if the waste proves measurable.
                warn!(
                    drv_hash = %drv_hash,
                    error = %e,
                    "CA resolve failed; dispatching unresolved (worker will fail on placeholder)"
                );
                // Return the (possibly fetched-from-store) bytes
                // unresolved. If the fetch succeeded but resolve
                // failed, the worker at least skips its own GetPath.
                (drv_content, Vec::new(), Vec::new())
            }
        }
    }

    /// Fetch a derivation's ATerm bytes from the store via `GetPath`.
    ///
    /// The store returns NAR-framed bytes; a `.drv` is a single
    /// regular file, so [`rio_nix::nar::extract_single_file`] unwraps
    /// it to the raw ATerm. This is the same path the worker takes
    /// when `WorkAssignment.drv_content` is empty
    /// (`rio-builder/src/executor/inputs.rs::fetch_drv_from_store`).
    ///
    /// Returns `None` on any failure: store unconfigured
    /// (`store_client = None`, test mode), `GetPath` error, timeout,
    /// not-found, or NAR unwrap failure. Callers treat `None` as
    /// "degrade to the pre-P0408 behavior" — dispatch unresolved,
    /// worker fails on placeholder, retry-with-backoff self-heals.
    ///
    /// Hard 2s timeout + 1 MiB NAR cap: a `.drv` is ~1-50 KB ASCII.
    /// A larger-than-1-MiB blob means something is badly wrong (the
    /// path isn't a `.drv`, or the store returned a closure NAR).
    /// Either way, bail — resolve can't parse a non-ATerm.
    async fn fetch_drv_content_from_store(
        &self,
        drv_hash: &DrvHash,
        state: &crate::state::DerivationState,
    ) -> Option<Vec<u8>> {
        /// `.drv` NAR cap. ~1-50 KB typical; 1 MiB is ~20× any
        /// real-world `.drv`. Avoids pulling a multi-GB closure if
        /// the store path was mis-resolved.
        const MAX_DRV_NAR_SIZE: u64 = 1024 * 1024;
        /// Per-chunk idle bound for `GetPath` (initial RPC + each
        /// stream.message() — I-211, not whole-call). ~10-50 ms
        /// typical; 2 s covers a slow store without blocking
        /// dispatch for long. On timeout we degrade to unresolved
        /// dispatch (same as store-unconfigured).
        const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

        let mut client = self.store_client.as_ref()?.clone();
        let drv_path = state.drv_path().to_string();

        let result = rio_proto::client::get_path_nar(
            &mut client,
            &drv_path,
            FETCH_TIMEOUT,
            MAX_DRV_NAR_SIZE,
            &[],
        )
        .await;

        let nar = match result {
            Ok(Some((_info, nar))) => nar,
            Ok(None) => {
                debug!(
                    drv_hash = %drv_hash,
                    drv_path = %drv_path,
                    "recovered CA resolve: .drv not found in store"
                );
                return None;
            }
            Err(e) => {
                debug!(
                    drv_hash = %drv_hash,
                    drv_path = %drv_path,
                    error = %e,
                    "recovered CA resolve: GetPath failed"
                );
                return None;
            }
        };

        // NAR unwrap: .drv is a single regular file. Anything else
        // (directory, symlink, corrupt NAR) → None.
        match rio_nix::nar::extract_single_file(&nar) {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                debug!(
                    drv_hash = %drv_hash,
                    error = %e,
                    "recovered CA resolve: NAR unwrap failed (not a single regular file)"
                );
                None
            }
        }
    }

    /// Collect CA inputs for resolve. Walks the DAG children (deps)
    /// and returns a `CaResolveInput` for each child with
    /// `is_ca = true` AND a populated `ca_modular_hash`.
    ///
    /// Children with `is_ca && ca_modular_hash.is_none()` are
    /// skipped — the gateway couldn't compute the modular hash
    /// (BasicDerivation fallback, or recovered state where the
    /// hash wasn't persisted). The parent's resolve is incomplete
    /// for that input → worker fails on the placeholder path →
    /// retry-with-backoff. The next SubmitBuild referencing this
    /// child re-merges the proto with a fresh `ca_modular_hash`.
    fn collect_ca_inputs(&self, drv_hash: &DrvHash) -> Vec<crate::ca::CaResolveInput> {
        // DAG children = dependencies (must complete before this drv).
        let children = self.dag.get_children(drv_hash);
        let mut inputs = Vec::new();
        for child_hash in children {
            let Some(child) = self.dag.node(&child_hash) else {
                continue;
            };
            if !child.ca.is_ca {
                continue;
            }
            let Some(modular_hash) = child.ca.modular_hash else {
                // Gateway didn't populate (BasicDerivation fallback
                // OR recovered state). Skip — resolve is incomplete
                // for this input, worker fails on placeholder,
                // retry-with-backoff handles it. debug not warn:
                // recovered chains hit this legitimately; the
                // scheduler-restart-mid-CA-chain case is expected
                // to degrade to worker-retry, not spam logs.
                debug!(
                    drv_hash = %drv_hash,
                    child = %child_hash,
                    "collect_ca_inputs: child is CA but ca_modular_hash unset; \
                     resolve incomplete, worker will fail on placeholder"
                );
                continue;
            };
            inputs.push(crate::ca::CaResolveInput {
                drv_path: child.drv_path().to_string(),
                modular_hash,
            });
        }
        inputs
    }

    /// Collect IA (input-addressed) inputs for resolve. Walks the
    /// DAG children and returns an [`IaResolveInput`] for each child
    /// with `is_ca = false` AND non-empty `expected_output_paths`.
    ///
    /// IA output paths are deterministic — the gateway computed them
    /// at submit time from the parsed `.drv` and plumbed them via
    /// `DerivationNode.expected_output_paths`. No store RPC needed.
    /// This is the same field [`approx_input_closure`] reads for the
    /// prefetch hint, so the data is already live.
    ///
    /// Children with empty `expected_output_paths` (recovered state
    /// where the paths weren't persisted, or a proto without the
    /// field) are skipped — `resolve_ca_inputs` will log and skip
    /// the `inputSrcs` add for that input. The worker's FUSE layer
    /// on-demand-fetches regardless, so builds don't break; only
    /// resolved-drv-hash compat with Nix is affected.
    ///
    /// [`IaResolveInput`]: crate::ca::IaResolveInput
    /// [`approx_input_closure`]: crate::assignment::approx_input_closure
    fn collect_ia_inputs(&self, drv_hash: &DrvHash) -> Vec<crate::ca::IaResolveInput> {
        let children = self.dag.get_children(drv_hash);
        let mut inputs = Vec::new();
        for child_hash in children {
            let Some(child) = self.dag.node(&child_hash) else {
                continue;
            };
            if child.ca.is_ca {
                // CA child with a modular hash — handled by
                // collect_ca_inputs via realisation lookup. But a CA
                // child WITHOUT a modular hash (recovered state,
                // BasicDerivation fallback) that HAS completed can
                // still contribute its realized output_paths here:
                // the resolve doesn't need the realisation table when
                // we already have the concrete path in-memory.
                if child.ca.modular_hash.is_some() || child.output_paths.is_empty() {
                    continue;
                }
                // Fall through: CA child, no modular hash, but
                // output_paths is populated (completed). Treat as IA
                // for the purpose of inputSrcs collection — the
                // realized path is just as concrete as an IA
                // expected_output_path.
            }
            // Prefer realized output_paths (filled on completion) over
            // expected_output_paths (filled at merge). For IA children
            // the two are equivalent; for the CA-no-hash fallthrough
            // above, only output_paths is usable (expected is [""]
            // for floating-CA).
            let paths = if !child.output_paths.is_empty() {
                &child.output_paths
            } else {
                &child.expected_output_paths
            };
            if paths.is_empty() {
                // Recovered node or proto without the field. Skip;
                // resolve_ca_inputs logs and skips the inputSrcs add.
                continue;
            }
            inputs.push(crate::ca::IaResolveInput {
                drv_path: child.drv_path().to_string(),
                output_names: child.output_names.clone(),
                output_paths: paths.clone(),
            });
        }
        inputs
    }
}

/// One probe's outcome in the budgeted fan-out fold.
pub(super) enum ProbeOutcome<R> {
    /// The store answered (the RPC's own success).
    Answered(R),
    /// The store answered with an error.
    Failed(tonic::Status),
    /// The per-attempt bound elapsed on an ISSUED RPC.
    TimedOut,
    /// The sweep budget was already expired when this probe's turn
    /// came: short-circuited WITHOUT issuing the RPC
    /// (merged_bug_179). Never store-health evidence — under
    /// multi-tenant load (ceil(T/8)*L > budget on a healthy store)
    /// every pass would otherwise re-stamp the corroboration gate's
    /// OR-leg and re-open the single-node store_degraded forgery
    /// lane the merged_bug_032 gate exists to close.
    BudgetExpired,
}

/// THE store-health-evidence policy over probe outcomes
/// (merged_bug_179): exhaustive, so adding a variant forces the
/// decision here — the machine witness that only outcomes of ISSUED
/// store RPCs can stamp `last_store_rpc_failure`.
pub(super) fn is_store_health_evidence<R>(outcome: &ProbeOutcome<R>) -> bool {
    match outcome {
        ProbeOutcome::Answered(_) => false,
        ProbeOutcome::Failed(_) => true,
        ProbeOutcome::TimedOut => true,
        ProbeOutcome::BudgetExpired => false,
    }
}

/// Fan probes out `buffer_unordered(min(T, MAX_PROBE_CONCURRENCY))`,
/// every attempt clamped to the shared
/// [`AttemptBudget`](rio_common::transport::AttemptBudget) remainder
/// (floored at 1 ms by `attempt_bound`; `expired()` short-circuits the
/// not-yet-started tail straight to `TimedOut`). Total wall-clock is
/// bounded by the budget regardless of how many probes hang — the
/// bug_127 class (T sequential awaits x full timeout) is unwriteable
/// through this entry point.
///
/// Generic over the probe so the budget law is testable with hung
/// futures and a paused clock; production passes the
/// `find_missing_paths` call.
// r[impl sched.dispatch.probe-budget]
impl super::DagActor {
    /// THE single writer of `last_store_rpc_failure` (merged_bug_179):
    /// store-health evidence for the corroboration gate's OR-leg.
    /// Callers are the ISSUED-RPC failure arms only — the
    /// budget-expiry short-circuit (`ProbeOutcome::BudgetExpired`)
    /// never reaches here, and `is_store_health_evidence` is the
    /// exhaustive policy match.
    pub(super) fn note_issued_store_rpc_failure(&mut self, surface: &'static str) {
        tracing::debug!(
            surface,
            "issued store RPC failed: store-health evidence stamped"
        );
        self.last_store_rpc_failure = Some(std::time::Instant::now());
    }
}

pub(super) async fn fan_out_probes<K, Req, R, F, Fut>(
    probes: Vec<(K, Req)>,
    budget: &rio_common::transport::AttemptBudget,
    per_attempt_cap: std::time::Duration,
    mut probe: F,
) -> Vec<(K, ProbeOutcome<R>)>
where
    F: FnMut(Req) -> Fut,
    Fut: Future<Output = Result<tonic::Response<R>, tonic::Status>>,
{
    use futures_util::stream::StreamExt;
    let concurrency = probes.len().clamp(1, super::MAX_PROBE_CONCURRENCY);
    futures_util::stream::iter(probes.into_iter().map(|(key, req)| {
        let fut = probe(req);
        async move {
            if budget.expired() {
                // Short-circuit: the RPC is never issued.
                return (key, ProbeOutcome::BudgetExpired);
            }
            let bound = budget.attempt_bound(per_attempt_cap);
            match tokio::time::timeout(bound, fut).await {
                Ok(Ok(resp)) => (key, ProbeOutcome::Answered(resp.into_inner())),
                Ok(Err(status)) => (key, ProbeOutcome::Failed(status)),
                Err(_elapsed) => (key, ProbeOutcome::TimedOut),
            }
        }
    }))
    .buffer_unordered(concurrency)
    .collect()
    .await
}
