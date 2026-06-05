//! The pull-admission decision kernel: [`admit_pull`].
//!
//! A pull-mode pod is born knowing its derivation (the HMAC-attested
//! intent id) and its work class (build / materialization); its first
//! and only ask is `PullAssignment`, and this function is the pure
//! decision behind it: given the already-loaded state of one node and
//! its materialization-job view, either deliver (mint a fresh attempt /
//! re-deliver the open one), refuse (token / generation fence), park
//! (`NotYetReady`), or dismiss (`Gone`). The scheduler's
//! `rio_scheduler::actor::pull::admit_pull` is the projection shim over
//! this function (decision P10 — the function was kept pure from its
//! introduction precisely so it could be lifted into a kani harness
//! without refactoring); the durable mint that follows a `DeliverNew`
//! is the fenced SQL transaction and stays in the scheduler. The
//! coexistence-era flag selection (`MaterializationInputs.enabled`) and
//! the as-built `must_substitute` refusal arm died with the
//! substitution-replacement cutover: the kinded table IS the table.
//!
//! ## Check order is load-bearing
//!
//! Identity first (a mis-bound token never learns anything about the
//! drv — not even whether it exists), then the generation fence (a
//! deposed believer answers nothing), then wantedness/deliverability.
//! The proofs pin this dominance order
//! (`check_admit_pull_rejections_dominate`).
//!
//! ## The vocabulary is mirrored, not imported
//!
//! [`PullNodeStatus`] mirrors the scheduler's `DerivationStatus`
//! variant-for-variant; the scheduler shim's exhaustive `match` pins
//! the two alphabets in lockstep (adding a variant to either enum
//! breaks that compile). Identity types are generic — the scheduler
//! instantiates the intent with `str` and the executor identity with
//! its `Arc<str>`-backed `ExecutorId`; the proofs use small copy types
//! so the solver never models heap-allocated string comparison.

/// The scheduler's derivation-status alphabet, as the pull admission
/// sees it. Kernel-side mirror of `rio_scheduler`'s `DerivationStatus`
/// (which owns the SQL string round-trip); the shim's exhaustive
/// `match` fails to compile if either side gains a variant the other
/// lacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullNodeStatus {
    /// Inserted, deps not yet evaluated.
    Created,
    /// Waiting on unbuilt deps.
    Queued,
    /// All deps produced: dispatchable.
    Ready,
    /// Dispatched, binding ack pending.
    Assigned,
    /// Running on an executor.
    Running,
    /// Terminal: produced.
    Completed,
    /// Failed, awaiting retry verdict.
    Failed,
    /// Terminal: poisoned by the retry budget.
    Poisoned,
    /// Terminal: a dependency failed.
    DependencyFailed,
    /// Terminal: explicitly cancelled.
    Cancelled,
    /// Terminal: CA early-cutoff skip.
    Skipped,
}

/// The pure admission decision for one pull. Generic over the exec-id
/// type a re-delivery carries (the scheduler instantiates `ExecId =
/// Uuid`; the proofs use `u8`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullAdmission<ExecId> {
    /// No open attempt for this identity: run the fenced mint
    /// transaction and deliver a fresh payload.
    DeliverNew,
    /// The open attempt already belongs to the pulling identity:
    /// re-deliver the identical payload/exec_id, write nothing.
    DeliverExisting {
        /// The open attempt's execution id.
        exec_id: ExecId,
    },
    /// No longer wanted: cancelled, substituted/completed, skipped,
    /// permanently failed/poisoned, or absent from the DAG.
    Gone,
    /// Still wanted but not deliverable to this pod right now.
    NotYetReady,
    /// Token↔intent binding failed.
    RejectToken,
    /// Serving generation below the durable claims floor.
    RejectStaleGeneration,
}

/// Everything [`admit_pull`] needs, already projected by the caller —
/// the kernel-side mirror of the scheduler's `PullInputs`, with the
/// scheduler vocabulary (string hashes, `Arc<str>` executor identities,
/// `Uuid` exec ids) replaced by type parameters.
#[derive(Debug)]
pub struct PullRequest<'a, IntentId: ?Sized, ExecutorIdent: ?Sized, ExecId> {
    /// The request's intent id (== drv hash, the DAG key).
    pub intent_id: &'a IntentId,
    /// The HMAC-attested intent binding (`None` = dev mode, no key).
    pub auth_intent: Option<&'a IntentId>,
    /// The serving replica's lease generation.
    pub serving_generation: u64,
    /// The durable claims floor (`None` = fresh cluster, no rows).
    pub generation_floor: Option<i64>,
    /// The derivation's current status; `None` if the DAG has no node.
    pub status: Option<PullNodeStatus>,
    /// The open attempt bound to the derivation, if any:
    /// (executor identity, exec id).
    pub open_attempt: Option<(&'a ExecutorIdent, ExecId)>,
    /// The identity this pull would bind a fresh attempt to.
    pub pulling_identity: &'a ExecutorIdent,
    /// Whether the derivation's transient-retry backoff has expired
    /// (`true` when no backoff is set). Consumed ONLY by the
    /// `(Build, JobView::None)` arm: a fresh from-source mint inside
    /// the backoff window downgrades DeliverNew to NotYetReady —
    /// bug_282's enforcement point (the backoff was produced by
    /// `handle_transient_failure` but enforced nowhere in the pull
    /// architecture). Re-deliveries (`DeliverExisting`) are untouched:
    /// the attempt already exists, the backoff gates NEW work only.
    pub build_backoff_expired: bool,
}

// r[impl sched.lease.generation-fence+3]
/// The base (kind-independent) admission gates: token binding, the
/// generation fence, then the node-status table. Private — every
/// public decision routes through [`admit_pull`]'s kinded table, which
/// composes this with the job view. (The walk-era `must_substitute`
/// refusal arm died with the kinded collapse: an unresolved job is the
/// only never-from-source gate now, and it lives in the kinded table.)
///
/// Check order is load-bearing: identity first (a mis-bound token never
/// learns anything about the drv), then the generation fence (a deposed
/// believer answers nothing), then wantedness/deliverability.
fn base_admission<IntentId, ExecutorIdent, ExecId>(
    request: PullRequest<'_, IntentId, ExecutorIdent, ExecId>,
) -> PullAdmission<ExecId>
where
    IntentId: PartialEq + ?Sized,
    ExecutorIdent: PartialEq + ?Sized,
{
    // Token↔intent binding (mechanism #6, applied per-unary).
    if let Some(auth) = request.auth_intent
        && auth != request.intent_id
    {
        return PullAdmission::RejectToken;
    }

    // Transaction-side generation fence, advisory half: a serving
    // generation below the durable claims floor answers nothing. The
    // authoritative check re-runs inside the mint transaction.
    if let Some(floor) = request.generation_floor
        && floor >= 0
        && request.serving_generation < floor as u64
    {
        return PullAdmission::RejectStaleGeneration;
    }

    let Some(status) = request.status else {
        // Not in the DAG: nothing wants it (never submitted, already
        // reaped after completion, or cancelled and swept).
        return PullAdmission::Gone;
    };

    use PullNodeStatus as S;
    match status {
        // No longer wanted: terminal or permanently failed states.
        S::Completed | S::Cancelled | S::Skipped | S::Poisoned | S::DependencyFailed => {
            PullAdmission::Gone
        }
        // Wanted but not deliverable yet: deps unbuilt or a retry
        // waiting to requeue. Never `Gone` (the reap→respawn churn
        // loop), never a write.
        S::Created | S::Queued | S::Failed => PullAdmission::NotYetReady,
        // Ready: deliverable now — mint a fresh attempt.
        S::Ready => PullAdmission::DeliverNew,
        // Already open on some executor: idempotent re-delivery only
        // for the same identity; anyone else waits (another pod's
        // open attempt for the same drv).
        S::Assigned | S::Running => match request.open_attempt {
            Some((executor, exec_id)) if executor == request.pulling_identity => {
                PullAdmission::DeliverExisting { exec_id }
            }
            // Open elsewhere — or in-flight bookkeeping is missing its
            // exec_id (never deliverable without an attempt to share).
            _ => PullAdmission::NotYetReady,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request<'a>(
        status: Option<PullNodeStatus>,
        open: Option<(&'a u8, u8)>,
        pulling: &'a u8,
    ) -> PullRequest<'a, u8, u8, u8> {
        PullRequest {
            intent_id: &7,
            auth_intent: Some(&7),
            serving_generation: 3,
            generation_floor: Some(3),
            status,
            open_attempt: open,
            pulling_identity: pulling,
            build_backoff_expired: true,
        }
    }

    /// Route a request through the public table as a no-job build pull
    /// (the base-table path).
    fn admit_base(req: PullRequest<'_, u8, u8, u8>) -> PullAdmission<u8> {
        admit_pull(
            req,
            MaterializationInputs {
                kind: PullKind::Build,
                job: JobView::None,
                resume_exec_id: None,
                presented_nonce: None,
                attempt_nonce: None,
            },
        )
    }

    #[test]
    fn status_table() {
        use PullNodeStatus as S;
        let me = 1u8;
        for (status, want) in [
            (None, PullAdmission::Gone),
            (Some(S::Created), PullAdmission::NotYetReady),
            (Some(S::Queued), PullAdmission::NotYetReady),
            (Some(S::Failed), PullAdmission::NotYetReady),
            (Some(S::Ready), PullAdmission::DeliverNew),
            (Some(S::Completed), PullAdmission::Gone),
            (Some(S::Cancelled), PullAdmission::Gone),
            (Some(S::Skipped), PullAdmission::Gone),
            (Some(S::Poisoned), PullAdmission::Gone),
            (Some(S::DependencyFailed), PullAdmission::Gone),
        ] {
            assert_eq!(
                admit_base(request(status, None, &me)),
                want,
                "status {status:?}"
            );
        }
        for status in [S::Assigned, S::Running] {
            assert_eq!(
                admit_base(request(Some(status), None, &me)),
                PullAdmission::NotYetReady,
                "in-flight without exec bookkeeping must wait"
            );
        }
    }

    #[test]
    fn open_attempt_identity() {
        use PullNodeStatus as S;
        let me = 1u8;
        let other = 2u8;
        for status in [S::Assigned, S::Running] {
            assert_eq!(
                admit_base(request(Some(status), Some((&me, 9)), &me)),
                PullAdmission::DeliverExisting { exec_id: 9 }
            );
            assert_eq!(
                admit_base(request(Some(status), Some((&other, 9)), &me)),
                PullAdmission::NotYetReady,
                "an attempt open on another executor is never re-delivered"
            );
        }
    }

    #[test]
    fn rejections_dominate() {
        let me = 1u8;
        // Token mismatch wins even for a Ready drv.
        let mut req = request(Some(PullNodeStatus::Ready), None, &me);
        req.auth_intent = Some(&8);
        assert_eq!(admit_base(req), PullAdmission::RejectToken);
        // Below-floor serving generation answers nothing.
        let mut req = request(Some(PullNodeStatus::Ready), None, &me);
        req.serving_generation = 2;
        req.generation_floor = Some(3);
        assert_eq!(admit_base(req), PullAdmission::RejectStaleGeneration);
        // Dev mode (no token) and fresh cluster (no floor) both admit.
        let mut req = request(Some(PullNodeStatus::Ready), None, &me);
        req.auth_intent = None;
        req.generation_floor = None;
        req.serving_generation = 0;
        assert_eq!(admit_base(req), PullAdmission::DeliverNew);
    }
}

#[cfg(kani)]
mod proofs {
    //! CBMC proof harnesses for the pull-admission kernel.
    //!
    //! Domain: identity types are `u8` (the kernel only ever uses
    //! `PartialEq` of them, so the verdict logic proven for `u8`
    //! identities is the same code production runs with `str` /
    //! `Arc<str>` identities); statuses are drawn from the full
    //! 12-variant alphabet plus absent; generations are full symbolic
    //! `u64`/`i64`; the open attempt and the auth token are free
    //! `Option`s.

    use super::*;

    /// The owned form of one arbitrary input vector — harnesses keep it
    /// to re-state postconditions after the borrowed [`PullRequest`]
    /// view of it has been consumed by [`admit_pull`].
    struct Inputs {
        intent: u8,
        auth: Option<u8>,
        serving: u64,
        floor: Option<i64>,
        status: Option<PullNodeStatus>,
        open: Option<(u8, u8)>,
        pulling: u8,
        backoff_expired: bool,
    }

    fn any_status() -> Option<PullNodeStatus> {
        let sel: u8 = kani::any();
        kani::assume(sel < 12);
        match sel {
            0 => None,
            1 => Some(PullNodeStatus::Created),
            2 => Some(PullNodeStatus::Queued),
            3 => Some(PullNodeStatus::Ready),
            4 => Some(PullNodeStatus::Assigned),
            5 => Some(PullNodeStatus::Running),
            6 => Some(PullNodeStatus::Completed),
            7 => Some(PullNodeStatus::Failed),
            8 => Some(PullNodeStatus::Poisoned),
            9 => Some(PullNodeStatus::DependencyFailed),
            10 => Some(PullNodeStatus::Cancelled),
            _ => Some(PullNodeStatus::Skipped),
        }
    }

    fn any_inputs() -> Inputs {
        Inputs {
            intent: kani::any(),
            auth: if kani::any() { Some(kani::any()) } else { None },
            serving: kani::any(),
            floor: if kani::any() { Some(kani::any()) } else { None },
            status: any_status(),
            open: if kani::any() {
                Some((kani::any(), kani::any()))
            } else {
                None
            },
            pulling: kani::any(),
            backoff_expired: kani::any(),
        }
    }

    /// The base-table path of the public fn (kind=Build, no job).
    fn run(inputs: &Inputs) -> PullAdmission<u8> {
        admit_pull(
            PullRequest {
                intent_id: &inputs.intent,
                auth_intent: inputs.auth.as_ref(),
                serving_generation: inputs.serving,
                generation_floor: inputs.floor,
                status: inputs.status,
                open_attempt: inputs.open.as_ref().map(|(e, x)| (e, *x)),
                pulling_identity: &inputs.pulling,
                build_backoff_expired: inputs.backoff_expired,
            },
            MaterializationInputs {
                kind: PullKind::Build,
                job: JobView::None,
                resume_exec_id: None,
                presented_nonce: None,
                attempt_nonce: None,
            },
        )
    }

    /// The base table's exhaustive partition (via the public fn's
    /// kind=Build/no-job path): for every input vector the decision is
    /// exactly the one the documented case analysis names — token
    /// mismatch → RejectToken; else below-floor →
    /// RejectStaleGeneration; else absent → Gone; else terminal → Gone;
    /// else parked statuses → NotYetReady; else Ready → DeliverNew;
    /// else (Assigned/Running) identity-matched open attempt →
    /// DeliverExisting, otherwise NotYetReady. Total and panic-free
    /// over the domain. (The job-view arms' partition is covered by
    /// the kinded harnesses below.)
    #[kani::proof]
    fn check_admit_pull_partition() {
        let inputs = any_inputs();
        let decision = run(&inputs);

        let token_mismatch = inputs.auth.is_some_and(|a| a != inputs.intent);
        let below_floor = inputs
            .floor
            .is_some_and(|f| f >= 0 && inputs.serving < f as u64);

        use PullNodeStatus as S;
        let expected = if token_mismatch {
            PullAdmission::RejectToken
        } else if below_floor {
            PullAdmission::RejectStaleGeneration
        } else {
            match inputs.status {
                None => PullAdmission::Gone,
                Some(
                    S::Completed | S::Cancelled | S::Skipped | S::Poisoned | S::DependencyFailed,
                ) => PullAdmission::Gone,
                Some(S::Created | S::Queued | S::Failed) => PullAdmission::NotYetReady,
                // bug_282: the fresh mint waits out the transient-retry
                // backoff; only the Ready/DeliverNew cell is gated.
                Some(S::Ready) if inputs.backoff_expired => PullAdmission::DeliverNew,
                Some(S::Ready) => PullAdmission::NotYetReady,
                Some(S::Assigned | S::Running) => match inputs.open {
                    Some((executor, exec_id)) if executor == inputs.pulling => {
                        PullAdmission::DeliverExisting { exec_id }
                    }
                    _ => PullAdmission::NotYetReady,
                },
            }
        };

        assert_eq!(decision, expected);
    }

    /// The dominance order of the two rejections: a mismatched token is
    /// rejected as RejectToken whatever else holds (the pod learns
    /// nothing about the drv — not even whether it exists), and an
    /// authenticated below-floor pull is rejected as
    /// RejectStaleGeneration whatever the node state (a deposed
    /// believer answers nothing).
    #[kani::proof]
    fn check_admit_pull_rejections_dominate() {
        let inputs = any_inputs();
        let decision = run(&inputs);

        let token_mismatch = inputs.auth.is_some_and(|a| a != inputs.intent);
        let below_floor = inputs
            .floor
            .is_some_and(|f| f >= 0 && inputs.serving < f as u64);

        if token_mismatch {
            assert_eq!(decision, PullAdmission::RejectToken);
        } else if below_floor {
            assert_eq!(decision, PullAdmission::RejectStaleGeneration);
        } else {
            assert!(!matches!(
                decision,
                PullAdmission::RejectToken | PullAdmission::RejectStaleGeneration
            ));
        }
    }

    /// Re-delivery is identity-keyed: DeliverExisting is produced only
    /// for an Assigned/Running node whose open attempt is bound to the
    /// pulling identity, and the exec_id it carries is exactly that
    /// attempt's — never fabricated, never another identity's.
    #[kani::proof]
    fn check_admit_pull_identity_match() {
        let inputs = any_inputs();
        let decision = run(&inputs);

        if let PullAdmission::DeliverExisting { exec_id } = decision {
            assert!(matches!(
                inputs.status,
                Some(PullNodeStatus::Assigned | PullNodeStatus::Running)
            ));
            match inputs.open {
                Some((executor, open_exec)) => {
                    assert_eq!(executor, inputs.pulling);
                    assert_eq!(exec_id, open_exec);
                }
                None => unreachable!("DeliverExisting requires an open attempt"),
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// The kind-aware admission table (design §2.3) — THE table since the
// substitution-replacement cutover (the coexistence flag selection and
// the as-built must_substitute arm died with Phase D').
// ──────────────────────────────────────────────────────────────────────

/// Which work class a pull claims (mirror of the proto AttemptKind;
/// UNSPECIFIED maps to Build at the gRPC layer, never here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullKind {
    /// A from-source build pull (the as-built work class).
    Build,
    /// A store-replica materialization claim.
    Materialization,
}

/// Which display family a running attempt belongs to — the SINGLE
/// projection from work class to display surface, shared by the mint
/// display match and the WatchBuild snapshot's running-set mapper
/// (bug_144). A consumer that pattern-matches `PullKind` directly for
/// display purposes is a review error; route through this so the two
/// surfaces cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplaySurface {
    /// Builder treatment: per-drv build activity + log tail.
    Build,
    /// Substitution treatment: substitute activity pair, NO tail.
    Substitution,
}

/// Total projection `PullKind -> DisplaySurface`. Pure and exhaustive:
/// adding a `PullKind` variant fails compilation here, and the kani
/// sweep pins that a materialization claim NEVER receives the build
/// display surface.
// r[impl sched.pull.kinded-running-surface]
pub fn display_class(kind: PullKind) -> DisplaySurface {
    match kind {
        PullKind::Build => DisplaySurface::Build,
        PullKind::Materialization => DisplaySurface::Substitution,
    }
}

/// The node's materialization-job state, as pull admission needs it.
/// Projected by the scheduler from its in-memory job view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobView {
    /// No unresolved job for this derivation.
    None,
    /// Unresolved job, no open attempt; `parked` = backoff unexpired.
    Pending {
        /// Whether the job's park backoff is still running.
        parked: bool,
    },
    /// Unresolved job with an open attempt; `held_by_puller` = the
    /// open attempt's executor identity equals the pulling identity.
    Claimed {
        /// Whether the open attempt belongs to the pulling identity.
        held_by_puller: bool,
    },
}

/// The materialization-side inputs of one kinded admission.
#[derive(Debug, Clone, Copy)]
pub struct MaterializationInputs<ExecId> {
    /// The pull's claimed kind.
    pub kind: PullKind,
    /// The node's job state (the scheduler's in-memory job view).
    pub job: JobView,
    /// merged_bug_158: the materialization re-delivery resume token —
    /// `Some(exec_id)` is the puller's proof that it already holds the
    /// open attempt (the exec id the original `WorkAssignment` carried);
    /// `None` is a fresh claim and NEVER re-delivers. The
    /// `(Materialization, Claimed{held_by_puller})` DELIVERY arm
    /// requires identity AND token agreement (BC-1 deepened: a
    /// colliding executor identity — sanitize-fold, restarted pod —
    /// cannot resume an attempt it did not open, because it cannot
    /// know the exec id). Tokenless same-identity re-pulls answer
    /// `NotYetReady` and settle through the establishment window —
    /// the T-0e.6 rule-4 amendment (status at the
    /// executor-invariant-map.md rule-4 anchor, the single record).
    /// Build-kind re-delivery is
    /// untouched (lost-ack resume keeps working tokenlessly).
    pub resume_exec_id: Option<ExecId>,
    /// bug_251 (rule-4b, SIGNED 2026-06-04 — the wave signature packet;
    /// status at the executor-invariant-map.md rule-4 anchor): the
    /// client-chosen claim nonce PRESENTED on this pull
    /// (`PullAssignmentRequest.claim_nonce`, parsed at the gRPC
    /// boundary; u128 = the UUID's scalar form). The credential that
    /// SURVIVES response loss: minted store-side BEFORE the pull and
    /// persisted with the assignment at mint, so a worker whose
    /// original response (and the exec_id resume token it carried)
    /// never arrived can still prove it opened the attempt.
    pub presented_nonce: Option<u128>,
    /// The open attempt's PERSISTED claim nonce
    /// (`assignments.claim_nonce`, mirrored into the node state at
    /// mint and re-hydrated at recovery). `None` for attempts minted
    /// nonceless (old stores, build pulls): the nonce leg then never
    /// matches — `None == None` is NOT a credential (the
    /// Option-equality trap; see [`redelivery_credential_ok`]).
    pub attempt_nonce: Option<u128>,
}

// r[impl sched.materialize.claim-resume]
/// The rule-4b re-delivery credential (SIGNED 2026-06-04; the record
/// lives at the executor-invariant-map.md rule-4 anchor):
/// materialization re-delivery requires `held_by_puller` AND
/// (`resume_exec_id` match OR persisted `claim_nonce` match) —
/// credential-less same-identity re-pulls remain `NotYetReady`.
///
/// The nonce leg requires BOTH sides present: `None` never matches
/// `None`. A puller presenting nothing gains nothing from an attempt
/// that persisted nothing — the Option-equality trap is the exact
/// fumble this single fn exists to make unrepeatable; every future
/// arm goes through it or fails review. Pinned by
/// `check_materialization_redelivery_requires_credential`.
pub fn redelivery_credential_ok<ExecId: PartialEq>(
    presented_resume: Option<&ExecId>,
    attempt_exec_id: &ExecId,
    presented_nonce: Option<u128>,
    attempt_nonce: Option<u128>,
) -> bool {
    presented_resume == Some(attempt_exec_id)
        || (presented_nonce.is_some() && presented_nonce == attempt_nonce)
}

// r[impl sched.materialize.job+2]
// r[impl sched.executor.pull-gone]
// r[impl sched.executor.pull-not-ready+2]
/// The kinded admission (design §2.3's table). Pure, total.
///
///   kind=Build:
///     job Pending/Claimed   → NotYetReady (the never-from-source
///                             gate: never serve from source while
///                             materialization is undecided)
///     job None              → the base table (token/fence/status)
///   kind=Materialization:
///     job None              → Gone (nothing to materialize)
///     job Pending{parked:f} → the base gates (token/fence/status)
///                             then: node Ready → DeliverNew;
///                             node Queued → DeliverNew (PD-6, Phase B:
///                             materialization does not wait for deps —
///                             the dep-racing claim is legal; the mint's
///                             Queued→Assigned edge is the kinded
///                             transition pair of this arm);
///                             node terminal → Gone; else NotYetReady
///     job Pending{parked:t} → NotYetReady (backoff unexpired)
///     job Claimed{held:t}   → DeliverExisting (re-delivery to the same
///                             replica — requires the rule-4b credential:
///                             resume token ∨ persisted claim nonce; the
///                             arm consumes request.open_attempt)
///     job Claimed{held:f}   → NotYetReady (the one-winner arbiter, BC-1)
pub fn admit_pull<IntentId, ExecutorIdent, ExecId>(
    request: PullRequest<'_, IntentId, ExecutorIdent, ExecId>,
    mat: MaterializationInputs<ExecId>,
) -> PullAdmission<ExecId>
where
    IntentId: PartialEq + ?Sized,
    ExecutorIdent: PartialEq + ?Sized,
    ExecId: PartialEq,
{
    match (mat.kind, mat.job) {
        // Build pulls while a job is unresolved: refuse (A1/A11 successor).
        (PullKind::Build, JobView::Pending { .. } | JobView::Claimed { .. }) => {
            // Identity/fence rejections still dominate (check order is
            // load-bearing): run the base gates and override only
            // their *delivery* outcomes.
            match base_admission(request) {
                PullAdmission::RejectToken => PullAdmission::RejectToken,
                PullAdmission::RejectStaleGeneration => PullAdmission::RejectStaleGeneration,
                PullAdmission::Gone => PullAdmission::Gone,
                // The NAMED refusal cells (merged_bug_307
                // documented-rationale): a base-table delivery while
                // the job is unresolved is REFUSED deliberately —
                // DeliverNew (a fresh from-source mint would race the
                // job's store-side fetch) and DeliverExisting
                // (re-delivering a build attempt under an unresolved
                // job re-opens the dual-execution window) both map to
                // NotYetReady, pinned by
                // `check_kinded_no_build_delivery_while_job_unresolved`
                // and `check_kinded_one_winner_arbitration`. A
                // pass-through here would fail both proofs and
                // `noFromSourceWhileJobUnresolved`.
                PullAdmission::DeliverNew
                | PullAdmission::DeliverExisting { .. }
                | PullAdmission::NotYetReady => PullAdmission::NotYetReady,
            }
        }
        (PullKind::Build, JobView::None) => {
            // bug_282: the transient-retry backoff gates the FRESH
            // from-source mint. The base table's other outcomes —
            // token/fence rejections, Gone, NotYetReady, and
            // DeliverExisting (a re-delivery to the bound identity;
            // the attempt already exists) — pass through untouched.
            let backoff_expired = request.build_backoff_expired;
            match base_admission(request) {
                PullAdmission::DeliverNew if !backoff_expired => PullAdmission::NotYetReady,
                other => other,
            }
        }
        (PullKind::Materialization, JobView::None) => {
            // Token/fence still dominate; then Gone.
            match base_admission(request) {
                PullAdmission::RejectToken => PullAdmission::RejectToken,
                PullAdmission::RejectStaleGeneration => PullAdmission::RejectStaleGeneration,
                _ => PullAdmission::Gone,
            }
        }
        (PullKind::Materialization, JobView::Pending { parked }) => {
            // The node's status, captured before `request` moves into
            // the base gates: the PD-6 Queued arm below keys on it.
            let status = request.status;
            match base_admission(request) {
                PullAdmission::RejectToken => PullAdmission::RejectToken,
                PullAdmission::RejectStaleGeneration => PullAdmission::RejectStaleGeneration,
                PullAdmission::Gone => PullAdmission::Gone,
                // The base table said the node is deliverable from
                // Ready; materialization claims need an unparked job.
                PullAdmission::DeliverNew if !parked => PullAdmission::DeliverNew,
                // r[impl sched.state.machine+2]
                // The two NotYetReady cells the materialization claim
                // upgrades to DeliverNew (both Phase B, design §2.3):
                //
                //  - QUEUED + unparked pending job (PD-6, "one new
                //    transition edge"): materialization does not wait
                //    for deps — the store fetches from upstream, so dep
                //    state is irrelevant to the claim.
                //
                //  - (Historical: the walk-era READY+must_substitute
                //    refusal cell upgraded here too — a MATERIALIZATION
                //    claim is the substitution mechanism itself; the
                //    arm died with the as-built table.)
                //
                // The token/fence/Gone dominance above is untouched (a
                // mis-bound token or terminal node never reaches here).
                PullAdmission::NotYetReady
                    if !parked
                        && matches!(
                            status,
                            Some(PullNodeStatus::Queued | PullNodeStatus::Ready)
                        ) =>
                {
                    PullAdmission::DeliverNew
                }
                // Named cells: a parked job answers NotYetReady to
                // every delivery shape — including a base-table
                // DeliverExisting (an Assigned/Running attempt
                // surviving into a Pending job is the establishment
                // crash window: the re-delivery must wait for the
                // job's own arbitration, merged_bug_307's rationale).
                PullAdmission::DeliverNew
                | PullAdmission::DeliverExisting { .. }
                | PullAdmission::NotYetReady => PullAdmission::NotYetReady,
            }
        }
        (PullKind::Materialization, JobView::Claimed { held_by_puller }) => {
            // The base gates run FIRST (gate-ordering: a mis-bound
            // token or a below-floor generation must never receive a
            // re-delivery — the same dominance every other arm
            // enforces), and the re-delivery is exactly what the
            // base table itself re-delivers: an Assigned/Running
            // attempt whose open-attempt identity equals the pulling
            // identity. The job view's held_by_puller must AGREE
            // (defense in depth, BC-1: the request-level identity
            // comparison and the scheduler's view projection are both
            // required; a stale view never re-delivers another
            // identity's attempt).
            match base_admission(request) {
                PullAdmission::RejectToken => PullAdmission::RejectToken,
                PullAdmission::RejectStaleGeneration => PullAdmission::RejectStaleGeneration,
                PullAdmission::Gone => PullAdmission::Gone,
                // merged_bug_158 + bug_251 (rule-4b): the DELIVERY
                // cell additionally requires the re-delivery
                // CREDENTIAL — identity agreement alone is forgeable
                // (a sanitize-fold collision or a restarted pod
                // re-pulls under the same composite identity without
                // ever having held the attempt). Two credentials prove
                // holdership: the resume token (the open attempt's own
                // exec id, known only to the puller the original
                // WorkAssignment answered) OR the persisted claim
                // nonce (minted client-side BEFORE the pull, so it
                // survives the lost-response window the token cannot —
                // the one failure mode re-delivery exists for).
                // Credential-less/mismatched same-identity re-pulls
                // answer NotYetReady and settle through the
                // establishment window (the T-0e.6 rule-4 amendment —
                // status at the executor-invariant-map.md rule-4
                // anchor). Pinned by
                // `check_materialization_redelivery_requires_credential`.
                PullAdmission::DeliverExisting { exec_id }
                    if held_by_puller
                        && redelivery_credential_ok(
                            mat.resume_exec_id.as_ref(),
                            &exec_id,
                            mat.presented_nonce,
                            mat.attempt_nonce,
                        ) =>
                {
                    PullAdmission::DeliverExisting { exec_id }
                }
                // Named cells: DeliverExisting to a NON-holder is the
                // one-winner arbiter's refusal (BC-1); DeliverExisting
                // WITHOUT the resume token is the colliding-identity
                // window (158); DeliverNew under a claimed job is the
                // dual-claim window. All deliberately NotYetReady
                // (merged_bug_307 + merged_bug_158).
                PullAdmission::DeliverNew
                | PullAdmission::DeliverExisting { .. }
                | PullAdmission::NotYetReady => PullAdmission::NotYetReady,
            }
        }
    }
}

#[cfg(test)]
mod kinded_tests {
    use super::*;

    fn request<'a>(
        status: Option<PullNodeStatus>,
        open: Option<(&'a u8, u8)>,
        pulling: &'a u8,
    ) -> PullRequest<'a, u8, u8, u8> {
        PullRequest {
            intent_id: &7,
            auth_intent: Some(&7),
            serving_generation: 3,
            generation_floor: Some(3),
            status,
            open_attempt: open,
            pulling_identity: pulling,
            build_backoff_expired: true,
        }
    }

    /// Flag-on: a build pull is refused (NotYetReady) while the node has an
    /// unresolved job — Pending or Claimed, parked or not — for every
    /// deliverable status; the token/fence rejections still dominate; Gone
    /// stays Gone (a terminal node is not laundered into a park).
    #[test]
    fn kinded_flag_on_build_pull_refused_while_job_unresolved() {
        use PullNodeStatus as S;
        let me = 1u8;
        let jobs = [
            JobView::Pending { parked: false },
            JobView::Pending { parked: true },
            JobView::Claimed {
                held_by_puller: false,
            },
            JobView::Claimed {
                held_by_puller: true,
            },
        ];
        for job in jobs {
            let mat = MaterializationInputs {
                kind: PullKind::Build,
                job,
                resume_exec_id: None,
                presented_nonce: None,
                attempt_nonce: None,
            };
            // Ready (deliverable as-built) → refused.
            assert_eq!(
                admit_pull(request(Some(S::Ready), None, &me), mat),
                PullAdmission::NotYetReady,
                "build pull of a job-unresolved Ready node must park ({job:?})"
            );
            // Queued (not deliverable as-built) → still NotYetReady.
            assert_eq!(
                admit_pull(request(Some(S::Queued), None, &me), mat),
                PullAdmission::NotYetReady
            );
            // Terminal → Gone (never laundered into a park).
            assert_eq!(
                admit_pull(request(Some(S::Completed), None, &me), mat),
                PullAdmission::Gone
            );
            // Token mismatch dominates.
            let mut req = request(Some(S::Ready), None, &me);
            req.auth_intent = Some(&8);
            assert_eq!(admit_pull(req, mat), PullAdmission::RejectToken);
            // Below-floor dominates.
            let mut req = request(Some(S::Ready), None, &me);
            req.serving_generation = 2;
            assert_eq!(admit_pull(req, mat), PullAdmission::RejectStaleGeneration);
        }
    }

    // r[verify sched.materialize.job+2]
    /// merged_bug_158: materialization re-delivery requires the resume
    /// token. A puller reaching the (Materialization,
    /// Claimed{held_by_puller:true}) arm with the base table offering a
    /// re-delivery gets it ONLY when `resume_exec_id` matches the open
    /// attempt's exec id: a fresh claim (None — a colliding identity, a
    /// restarted pod, a lost-ack retry) answers NotYetReady and settles
    /// through the establishment window; a mismatched token likewise.
    /// RED (recorded, pre-fix): the None case DELIVERED — identity
    /// agreement alone re-opened the attempt to any same-named puller
    /// (`left: DeliverExisting { exec_id: 9 } / right: NotYetReady`).
    #[test]
    fn colliding_identity_fresh_claim_gets_not_yet_ready() {
        use PullNodeStatus as S;
        let me = 1u8;
        let mat = |resume| MaterializationInputs {
            kind: PullKind::Materialization,
            job: JobView::Claimed {
                held_by_puller: true,
            },
            resume_exec_id: resume,
            presented_nonce: None,
            attempt_nonce: None,
        };
        // Fresh claim (no token): refused.
        assert_eq!(
            admit_pull(request(Some(S::Assigned), Some((&me, 9)), &me), mat(None)),
            PullAdmission::NotYetReady,
            "tokenless re-pull must wait for the establishment window"
        );
        // Mismatched token: refused.
        assert_eq!(
            admit_pull(
                request(Some(S::Assigned), Some((&me, 9)), &me),
                mat(Some(7))
            ),
            PullAdmission::NotYetReady,
            "a wrong exec id never resumes"
        );
        // The legitimate resume: matching token → re-delivery.
        assert_eq!(
            admit_pull(
                request(Some(S::Assigned), Some((&me, 9)), &me),
                mat(Some(9))
            ),
            PullAdmission::DeliverExisting { exec_id: 9 },
            "the original holder resumes with its exec id"
        );
        // Token agreement never overrides identity disagreement: the
        // one-winner refusal (held_by_puller=false) stands even with a
        // "matching" token.
        assert_eq!(
            admit_pull(
                request(Some(S::Assigned), Some((&me, 9)), &me),
                MaterializationInputs {
                    kind: PullKind::Materialization,
                    job: JobView::Claimed {
                        held_by_puller: false,
                    },
                    resume_exec_id: Some(9),
                    presented_nonce: None,
                    attempt_nonce: None,
                }
            ),
            PullAdmission::NotYetReady,
            "BC-1: the view projection still arbitrates"
        );
        // rule-4b: NONCE agreement never overrides identity
        // disagreement either — the credential proves holdership of
        // the attempt, not ownership of the arbitration.
        assert_eq!(
            admit_pull(
                request(Some(S::Assigned), Some((&me, 9)), &me),
                MaterializationInputs {
                    kind: PullKind::Materialization,
                    job: JobView::Claimed {
                        held_by_puller: false,
                    },
                    resume_exec_id: None,
                    presented_nonce: Some(42),
                    attempt_nonce: Some(42),
                }
            ),
            PullAdmission::NotYetReady,
            "BC-1 dominates the nonce leg too"
        );
    }

    // r[verify sched.materialize.claim-resume]
    /// bug_251 (rule-4b, SIGNED): the claim nonce is the credential
    /// that SURVIVES response loss. A puller whose original response
    /// never arrived holds no exec_id token — but it minted and
    /// persisted a nonce with the claim, and presenting THAT nonce
    /// resumes the attempt. Every leg of the disjunction:
    ///   1. nonce match, no token            → DeliverExisting
    ///   2. nonce mismatch, no token         → NotYetReady
    ///   3. nonce presented, none persisted  → NotYetReady
    ///   4. none presented, one persisted    → NotYetReady (None≠None)
    ///   5. wrong token + matching nonce     → DeliverExisting
    /// RED (recorded, pre-fix): leg 1 refused — `left: NotYetReady /
    /// right: DeliverExisting { exec_id: 9 }` — the cell required the
    /// response-borne token alone, which is exactly what a lost
    /// response destroys.
    #[test]
    fn lost_response_nonce_resumes_claim() {
        use PullNodeStatus as S;
        let me = 1u8;
        let mat = |presented, attempt| MaterializationInputs {
            kind: PullKind::Materialization,
            job: JobView::Claimed {
                held_by_puller: true,
            },
            resume_exec_id: None,
            presented_nonce: presented,
            attempt_nonce: attempt,
        };
        // 1. The lost-response resume: persisted nonce match → re-delivery.
        assert_eq!(
            admit_pull(
                request(Some(S::Running), Some((&me, 9)), &me),
                mat(Some(42), Some(42))
            ),
            PullAdmission::DeliverExisting { exec_id: 9 },
            "the nonce survives response loss and resumes the attempt"
        );
        // 2. Mismatched nonce: refused.
        assert_eq!(
            admit_pull(
                request(Some(S::Running), Some((&me, 9)), &me),
                mat(Some(42), Some(43))
            ),
            PullAdmission::NotYetReady
        );
        // 3. Nonce presented but the attempt persisted none (old
        //    scheduler / build-minted row): refused.
        assert_eq!(
            admit_pull(
                request(Some(S::Running), Some((&me, 9)), &me),
                mat(Some(42), None)
            ),
            PullAdmission::NotYetReady
        );
        // 4. The Option-equality trap: nothing presented, nothing
        //    persisted — None==None must NOT be a credential.
        assert_eq!(
            admit_pull(
                request(Some(S::Running), Some((&me, 9)), &me),
                mat(None, None)
            ),
            PullAdmission::NotYetReady,
            "credential-less same-identity re-pulls remain NotYetReady"
        );
        // 5. The disjunction: a WRONG resume token alongside a
        //    matching nonce still resumes (either credential proves
        //    holdership; the worker may have recorded both).
        assert_eq!(
            admit_pull(
                request(Some(S::Running), Some((&me, 9)), &me),
                MaterializationInputs {
                    kind: PullKind::Materialization,
                    job: JobView::Claimed {
                        held_by_puller: true,
                    },
                    resume_exec_id: Some(7),
                    presented_nonce: Some(42),
                    attempt_nonce: Some(42),
                }
            ),
            PullAdmission::DeliverExisting { exec_id: 9 }
        );
    }

    /// Flag-on: the materialization claim table (design §2.3), including
    /// PD-6 (Phase B, the PDQ-6 amendment's prescribed flip):
    /// materialization claims deliver from Ready AND from Queued — a
    /// pending unparked job's node is claimable regardless of dep state
    /// (materialization does not wait for deps; the store fetches from
    /// upstream).
    #[test]
    fn kinded_flag_on_materialization_claim_table() {
        use PullNodeStatus as S;
        let me = 1u8;
        let other = 2u8;
        let mat = |job| MaterializationInputs {
            kind: PullKind::Materialization,
            job,
            resume_exec_id: None,
            presented_nonce: None,
            attempt_nonce: None,
        };
        // No job → Gone (nothing to materialize).
        assert_eq!(
            admit_pull(request(Some(S::Ready), None, &me), mat(JobView::None)),
            PullAdmission::Gone
        );
        // Pending unparked + node Ready → DeliverNew.
        assert_eq!(
            admit_pull(
                request(Some(S::Ready), None, &me),
                mat(JobView::Pending { parked: false })
            ),
            PullAdmission::DeliverNew
        );
        // PD-6 (Phase B): Pending unparked + node Queued → DeliverNew —
        // the dep-racing claim is legal (was the Phase A Ready-only
        // NotYetReady pin, flipped red-first per the PDQ-6 amendment).
        assert_eq!(
            admit_pull(
                request(Some(S::Queued), None, &me),
                mat(JobView::Pending { parked: false })
            ),
            PullAdmission::DeliverNew
        );
        // The Queued delivery still requires an unparked pending job:
        // Queued + parked → NotYetReady; Queued + Created (pre-dep
        // statuses other than Queued) stay NotYetReady.
        assert_eq!(
            admit_pull(
                request(Some(S::Queued), None, &me),
                mat(JobView::Pending { parked: true })
            ),
            PullAdmission::NotYetReady
        );
        assert_eq!(
            admit_pull(
                request(Some(S::Created), None, &me),
                mat(JobView::Pending { parked: false })
            ),
            PullAdmission::NotYetReady,
            "Created (deps not yet evaluated) is not claimable — only Ready/Queued are"
        );
        // A pruned-origin node (the walk era called this marked /
        // must_substitute=true) with an unparked pending job IS
        // claimable: the claim IS the substitution the pruned
        // classification demands — refusing here would park exactly
        // the pruned-root jobs forever. (The walk-era A11 refusal arm
        // for from-source BUILD dispatch died with Phase D'; the
        // JobView::None requirement subsumes it.)
        assert_eq!(
            admit_pull(
                request(Some(S::Ready), None, &me),
                mat(JobView::Pending { parked: false })
            ),
            PullAdmission::DeliverNew,
            "the materialization claim delivers (the claim IS the substitution; \
             the walk-era marked-cell upgrade is structural now — no mark input exists)"
        );
        // ...but the node's BUILD pull stays refused while the job is
        // unresolved (the never-from-source gate).
        assert_eq!(
            admit_pull(
                request(Some(S::Ready), None, &me),
                MaterializationInputs {
                    kind: PullKind::Build,
                    job: JobView::Pending { parked: false },
                    resume_exec_id: None,
                    presented_nonce: None,
                    attempt_nonce: None,
                }
            ),
            PullAdmission::NotYetReady
        );
        // Pending unparked + node terminal → Gone.
        assert_eq!(
            admit_pull(
                request(Some(S::Completed), None, &me),
                mat(JobView::Pending { parked: false })
            ),
            PullAdmission::Gone
        );
        // Pending parked → NotYetReady (backoff unexpired), even for Ready.
        assert_eq!(
            admit_pull(
                request(Some(S::Ready), None, &me),
                mat(JobView::Pending { parked: true })
            ),
            PullAdmission::NotYetReady
        );
        // Claimed held-by-puller → re-delivery REQUIRES the resume
        // token (merged_bug_158, the rule-4 amendment): the closure's
        // tokenless inputs park; presenting the open attempt's exec id
        // resumes. Behavioral re-pin — this assertion documented the
        // pre-158 tokenless contract (`DeliverExisting { exec_id: 9 }`).
        assert_eq!(
            admit_pull(
                request(Some(S::Running), Some((&me, 9)), &me),
                mat(JobView::Claimed {
                    held_by_puller: true
                })
            ),
            PullAdmission::NotYetReady,
            "tokenless re-pull settles via the establishment window"
        );
        assert_eq!(
            admit_pull(
                request(Some(S::Running), Some((&me, 9)), &me),
                MaterializationInputs {
                    kind: PullKind::Materialization,
                    job: JobView::Claimed {
                        held_by_puller: true
                    },
                    resume_exec_id: Some(9),
                    presented_nonce: None,
                    attempt_nonce: None,
                }
            ),
            PullAdmission::DeliverExisting { exec_id: 9 }
        );
        // Claimed held-by-puller but no open-attempt bookkeeping → park.
        assert_eq!(
            admit_pull(
                request(Some(S::Running), None, &me),
                mat(JobView::Claimed {
                    held_by_puller: true
                })
            ),
            PullAdmission::NotYetReady
        );
        // Claimed by another identity → NotYetReady (the one-winner
        // arbiter, BC-1).
        assert_eq!(
            admit_pull(
                request(Some(S::Running), Some((&other, 9)), &me),
                mat(JobView::Claimed {
                    held_by_puller: false
                })
            ),
            PullAdmission::NotYetReady
        );
        // Token/fence rejections dominate the whole table.
        let mut req = request(Some(S::Ready), None, &me);
        req.auth_intent = Some(&8);
        assert_eq!(
            admit_pull(req, mat(JobView::Pending { parked: false })),
            PullAdmission::RejectToken
        );
        let mut req = request(Some(S::Ready), None, &me);
        req.serving_generation = 2;
        assert_eq!(
            admit_pull(req, mat(JobView::None)),
            PullAdmission::RejectStaleGeneration
        );
        // ...INCLUDING the Claimed re-delivery arm (the gate-ordering
        // pin: held_by_puller must never bypass the identity/fence
        // gates — a mis-bound token or a deposed believer must never
        // receive a re-delivery, exactly as the build-kind arms behave).
        let mut req = request(Some(S::Running), Some((&me, 9)), &me);
        req.auth_intent = Some(&8);
        assert_eq!(
            admit_pull(
                req,
                mat(JobView::Claimed {
                    held_by_puller: true
                })
            ),
            PullAdmission::RejectToken,
            "a mis-bound token must never receive a re-delivery"
        );
        let mut req = request(Some(S::Running), Some((&me, 9)), &me);
        req.serving_generation = 2;
        assert_eq!(
            admit_pull(
                req,
                mat(JobView::Claimed {
                    held_by_puller: true
                })
            ),
            PullAdmission::RejectStaleGeneration,
            "a below-floor pull must never receive a re-delivery"
        );
        // Defense in depth: a view that says held_by_puller while the
        // REQUEST's open attempt is bound to a different identity never
        // re-delivers — the request-level identity comparison and the
        // view's projection must both agree (BC-1).
        assert_eq!(
            admit_pull(
                request(Some(S::Running), Some((&other, 9)), &me),
                mat(JobView::Claimed {
                    held_by_puller: true
                })
            ),
            PullAdmission::NotYetReady,
            "a stale/mismatched view projection must never re-deliver another identity's attempt"
        );
    }

    /// kindMatchesWorker (§3.6) at the kernel level: a materialization pull
    /// never receives the build delivery that the as-built table would have
    /// produced for a no-job node, and a build pull never receives a
    /// materialization-job delivery.
    #[test]
    fn kinded_kind_match_worker() {
        use PullNodeStatus as S;
        let me = 1u8;
        // A build pull of a Ready no-job node delivers...
        let build = admit_pull(
            request(Some(S::Ready), None, &me),
            MaterializationInputs {
                kind: PullKind::Build,
                job: JobView::None,
                resume_exec_id: None,
                presented_nonce: None,
                attempt_nonce: None,
            },
        );
        assert_eq!(build, PullAdmission::DeliverNew);
        // ...but a materialization pull of the same node never gets it
        // (no job → Gone).
        let got = admit_pull(
            request(Some(S::Ready), None, &me),
            MaterializationInputs {
                kind: PullKind::Materialization,
                job: JobView::None,
                resume_exec_id: None,
                presented_nonce: None,
                attempt_nonce: None,
            },
        );
        assert_ne!(
            got,
            PullAdmission::DeliverNew,
            "a materialization pull must never receive a build delivery"
        );
        // And a build pull while a materialization job is unresolved never
        // receives the job's delivery (it parks instead).
        let got = admit_pull(
            request(Some(S::Ready), None, &me),
            MaterializationInputs {
                kind: PullKind::Build,
                job: JobView::Pending { parked: false },
                resume_exec_id: None,
                presented_nonce: None,
                attempt_nonce: None,
            },
        );
        assert_eq!(got, PullAdmission::NotYetReady);
    }
}

#[cfg(kani)]
mod display_proofs {
    //! Exhaustive sweep of the display projection (bug_144): a
    //! materialization claim NEVER receives the build display surface,
    //! and the projection is total (no panic on any variant).

    use super::*;

    // r[verify sched.pull.kinded-running-surface]
    #[kani::proof]
    fn check_materialization_never_build_surface() {
        let kind = if kani::any() {
            PullKind::Build
        } else {
            PullKind::Materialization
        };
        let surface = display_class(kind);
        match kind {
            PullKind::Build => assert_eq!(surface, DisplaySurface::Build),
            PullKind::Materialization => {
                assert_eq!(surface, DisplaySurface::Substitution);
            }
        }
    }
}

#[cfg(kani)]
mod kinded_proofs {
    //! CBMC proof harnesses for the kind-aware coexistence wrapper
    //! ([`admit_pull`]). Same bounded domain as the as-built
    //! `mod proofs` above (u8 identities, full status alphabet, free
    //! token/floor/attempt options), extended with the materialization
    //! inputs (flag × kind × job view). The domain helpers are
    //! duplicated rather than shared so the as-built proofs module
    //! stays byte-identical (dormancy criterion 3).

    use super::*;

    struct Inputs {
        intent: u8,
        auth: Option<u8>,
        serving: u64,
        floor: Option<i64>,
        status: Option<PullNodeStatus>,
        open: Option<(u8, u8)>,
        pulling: u8,
        backoff_expired: bool,
    }

    fn any_status() -> Option<PullNodeStatus> {
        let sel: u8 = kani::any();
        kani::assume(sel < 12);
        match sel {
            0 => None,
            1 => Some(PullNodeStatus::Created),
            2 => Some(PullNodeStatus::Queued),
            3 => Some(PullNodeStatus::Ready),
            4 => Some(PullNodeStatus::Assigned),
            5 => Some(PullNodeStatus::Running),
            6 => Some(PullNodeStatus::Completed),
            7 => Some(PullNodeStatus::Failed),
            8 => Some(PullNodeStatus::Poisoned),
            9 => Some(PullNodeStatus::DependencyFailed),
            10 => Some(PullNodeStatus::Cancelled),
            _ => Some(PullNodeStatus::Skipped),
        }
    }

    fn any_inputs() -> Inputs {
        Inputs {
            intent: kani::any(),
            auth: if kani::any() { Some(kani::any()) } else { None },
            serving: kani::any(),
            floor: if kani::any() { Some(kani::any()) } else { None },
            status: any_status(),
            open: if kani::any() {
                Some((kani::any(), kani::any()))
            } else {
                None
            },
            pulling: kani::any(),
            backoff_expired: kani::any(),
        }
    }

    fn any_job_view() -> JobView {
        let sel: u8 = kani::any();
        kani::assume(sel < 5);
        match sel {
            0 => JobView::None,
            1 => JobView::Pending { parked: false },
            2 => JobView::Pending { parked: true },
            3 => JobView::Claimed {
                held_by_puller: false,
            },
            _ => JobView::Claimed {
                held_by_puller: true,
            },
        }
    }

    /// Symbolic resume token: absent, matching-shaped, or arbitrary.
    fn any_resume() -> Option<u8> {
        if kani::any() {
            let t: u8 = kani::any();
            kani::assume(t < 16);
            Some(t)
        } else {
            None
        }
    }

    /// Symbolic claim nonce (the u128 credential scalar): absent or
    /// arbitrary. EQ-only downstream, so no bound is needed.
    fn any_nonce() -> Option<u128> {
        if kani::any() { Some(kani::any()) } else { None }
    }

    fn run_kinded(inputs: &Inputs, mat: MaterializationInputs<u8>) -> PullAdmission<u8> {
        admit_pull(
            PullRequest {
                intent_id: &inputs.intent,
                auth_intent: inputs.auth.as_ref(),
                serving_generation: inputs.serving,
                generation_floor: inputs.floor,
                status: inputs.status,
                open_attempt: inputs.open.as_ref().map(|(e, x)| (e, *x)),
                pulling_identity: &inputs.pulling,
                build_backoff_expired: inputs.backoff_expired,
            },
            mat,
        )
    }

    /// Flag-on: a build pull is never delivered while a job is unresolved
    /// (noFromSourceWhileJobUnresolved, kernel half — F8/F13's anchor).
    /// Neither a fresh mint nor a re-delivery escapes the refusal, and
    /// the token/fence rejections still dominate.
    #[kani::proof]
    fn check_kinded_no_build_delivery_while_job_unresolved() {
        let inputs = any_inputs();
        let job = any_job_view();
        kani::assume(!matches!(job, JobView::None));

        let decision = run_kinded(
            &inputs,
            MaterializationInputs {
                kind: PullKind::Build,
                job,
                // Widened domain (158): the build arms must hold for
                // every token value — the field is materialization-only.
                resume_exec_id: any_resume(),
                presented_nonce: None,
                attempt_nonce: None,
            },
        );

        // Never any delivery.
        assert!(!matches!(
            decision,
            PullAdmission::DeliverNew | PullAdmission::DeliverExisting { .. }
        ));
        // And the rejections keep their dominance order.
        let token_mismatch = inputs.auth.is_some_and(|a| a != inputs.intent);
        let below_floor = inputs
            .floor
            .is_some_and(|f| f >= 0 && inputs.serving < f as u64);
        if token_mismatch {
            assert_eq!(decision, PullAdmission::RejectToken);
        } else if below_floor {
            assert_eq!(decision, PullAdmission::RejectStaleGeneration);
        } else {
            assert!(matches!(
                decision,
                PullAdmission::NotYetReady | PullAdmission::Gone
            ));
        }
    }

    /// One-winner: a materialization pull is delivered only when no open
    /// attempt is held by a different identity (atMostOneClaimWinner's
    /// admission half, AS-3/BC-1). A fresh claim (DeliverNew) requires an
    /// unparked Pending job on a Ready or Queued node (PD-6, Phase B:
    /// the dep-racing Queued claim is legal) with no open attempt held
    /// by anyone else; a re-delivery (DeliverExisting) requires the open
    /// attempt to be the puller's own and carries exactly its exec id.
    /// bug_282 (A3, the named cell): a BUILD pull with no
    /// materialization job and an UNEXPIRED transient backoff is never
    /// minted fresh — the (Build, None) arm downgrades DeliverNew to
    /// NotYetReady while the window runs. The wide partition proof
    /// (`check_kinded_no_build_delivery_while_job_unresolved` +
    /// `check_kinded_partition_total`) already covers the cell; this
    /// harness names the contract so the backoff conjunct cannot be
    /// dropped without a targeted failure.
    #[kani::proof]
    fn check_no_build_mint_inside_backoff_window() {
        let mut inputs = any_inputs();
        inputs.backoff_expired = false;

        let decision = run_kinded(
            &inputs,
            MaterializationInputs {
                kind: PullKind::Build,
                job: JobView::None,
                resume_exec_id: None,
                presented_nonce: None,
                attempt_nonce: None,
            },
        );

        // The fresh mint is refused while the backoff runs; everything
        // else (rejections, Gone, NotYetReady, the bound-identity
        // re-delivery whose attempt already exists) passes through.
        assert!(!matches!(decision, PullAdmission::DeliverNew));
    }

    #[kani::proof]
    fn check_kinded_one_winner_arbitration() {
        let inputs = any_inputs();
        let job = any_job_view();
        let mat_resume = any_resume();
        let presented = any_nonce();
        let attempt = any_nonce();

        let decision = run_kinded(
            &inputs,
            MaterializationInputs {
                kind: PullKind::Materialization,
                job,
                // Widened domain (158, then rule-4b): one-winner holds
                // for every credential value; the DeliverExisting
                // branch additionally proves a credential matched.
                resume_exec_id: mat_resume,
                presented_nonce: presented,
                attempt_nonce: attempt,
            },
        );

        match decision {
            PullAdmission::DeliverNew => {
                // Fresh claims happen only through the Pending-unparked
                // arm, on a Ready or Queued node (PD-6: the kinded
                // Queued→Assigned mint edge is the transition pair of
                // the Queued admission), via the as-built gates — which
                // require no open attempt by another identity: neither
                // Ready nor Queued nodes carry an open attempt in the
                // as-built table (open attempts exist only for
                // Assigned/Running).
                assert!(matches!(job, JobView::Pending { parked: false }));
                assert!(matches!(
                    inputs.status,
                    Some(PullNodeStatus::Ready | PullNodeStatus::Queued)
                ));
            }
            PullAdmission::DeliverExisting { exec_id } => {
                // Re-deliveries happen only to the identity already
                // holding the claim (the Claimed{held_by_puller} arm),
                // carry exactly the open attempt's exec id — which
                // must itself be bound to the pulling identity (the
                // request-level comparison and the view projection must
                // BOTH hold; a stale view never re-delivers another
                // identity's attempt) — and require the resume token
                // (merged_bug_158): the puller proved it holds the
                // attempt by presenting its exec id.
                assert!(matches!(
                    job,
                    JobView::Claimed {
                        held_by_puller: true
                    }
                ));
                match inputs.open {
                    Some((open_ident, open_exec)) => {
                        assert_eq!(exec_id, open_exec);
                        assert_eq!(open_ident, inputs.pulling);
                    }
                    None => unreachable!("DeliverExisting requires an open attempt"),
                }
                // 158 + rule-4b: a materialization re-delivery
                // REQUIRES a matching credential — resume token OR
                // persisted claim nonce (build-kind re-delivery is
                // credential-less by contract — this harness is the
                // materialization arm).
                assert!(
                    mat_resume == Some(exec_id) || (presented.is_some() && presented == attempt)
                );
                // And it never escapes the identity/fence gates.
                assert!(!inputs.auth.is_some_and(|a| a != inputs.intent));
                assert!(
                    !inputs
                        .floor
                        .is_some_and(|f| f >= 0 && inputs.serving < f as u64)
                );
            }
            _ => {}
        }

        // The contrapositive: an open attempt held by a DIFFERENT
        // identity (Claimed{held_by_puller: false}) never yields any
        // delivery to this puller.
        if matches!(
            job,
            JobView::Claimed {
                held_by_puller: false
            }
        ) {
            assert!(!matches!(
                decision,
                PullAdmission::DeliverNew | PullAdmission::DeliverExisting { .. }
            ));
        }
    }

    // r[verify sched.materialize.job+2]
    // r[verify sched.materialize.claim-resume]
    /// bug_251 (rule-4b, SIGNED 2026-06-04): over the full kinded
    /// domain, a materialization re-delivery is produced ONLY under
    /// the credential disjunction — the resume token matches the open
    /// attempt's exec id, OR the presented claim nonce matches the
    /// attempt's persisted nonce with BOTH sides present. Credential-
    /// less and mismatched same-identity re-pulls are refused
    /// (NotYetReady; the establishment window settles the attempt),
    /// and None==None never satisfies the nonce leg. REPLACES (widens)
    /// `check_materialization_redelivery_requires_resume_token` over
    /// the (resume ∨ nonce) domain — merged_bug_158's refusal
    /// direction is the mat_resume.is_none() ∧ nonce-less slice of
    /// this proof, preserved verbatim.
    #[kani::proof]
    fn check_materialization_redelivery_requires_credential() {
        let inputs = any_inputs();
        let job = any_job_view();
        let mat_resume = any_resume();
        let presented = any_nonce();
        let attempt = any_nonce();

        let decision = run_kinded(
            &inputs,
            MaterializationInputs {
                kind: PullKind::Materialization,
                job,
                resume_exec_id: mat_resume,
                presented_nonce: presented,
                attempt_nonce: attempt,
            },
        );

        let nonce_match = presented.is_some() && presented == attempt;
        if let PullAdmission::DeliverExisting { exec_id } = decision {
            assert!(mat_resume == Some(exec_id) || nonce_match);
            assert!(matches!(
                job,
                JobView::Claimed {
                    held_by_puller: true
                }
            ));
        }
        // The refusal direction: with NO credential of either kind,
        // no re-delivery exists anywhere in the materialization table.
        if mat_resume.is_none() && !nonce_match {
            assert!(!matches!(decision, PullAdmission::DeliverExisting { .. }));
        }
    }

    /// The dominance order of the two rejections holds over the FULL
    /// flag-on (kind × job-view) domain — every arm of the kinded
    /// table, including the Claimed re-delivery arm: a mismatched token
    /// always answers RejectToken, an authenticated below-floor pull
    /// always answers RejectStaleGeneration, and no other input is ever
    /// rejected. The kinded-table mirror of
    /// `check_admit_pull_rejections_dominate` (the flag-off half is
    /// covered by `check_kinded_flag_off_identity`: build delegates to
    /// the as-built kernel — whose own dominance proof applies — and
    /// materialization parks without delivering anything).
    #[kani::proof]
    fn check_kinded_rejections_dominate() {
        let inputs = any_inputs();
        let kind = if kani::any() {
            PullKind::Build
        } else {
            PullKind::Materialization
        };
        let job = any_job_view();

        let decision = run_kinded(
            &inputs,
            MaterializationInputs {
                kind,
                job,
                resume_exec_id: any_resume(),
                presented_nonce: None,
                attempt_nonce: None,
            },
        );

        let token_mismatch = inputs.auth.is_some_and(|a| a != inputs.intent);
        let below_floor = inputs
            .floor
            .is_some_and(|f| f >= 0 && inputs.serving < f as u64);

        if token_mismatch {
            assert_eq!(decision, PullAdmission::RejectToken);
        } else if below_floor {
            assert_eq!(decision, PullAdmission::RejectStaleGeneration);
        } else {
            assert!(!matches!(
                decision,
                PullAdmission::RejectToken | PullAdmission::RejectStaleGeneration
            ));
        }
    }
}
