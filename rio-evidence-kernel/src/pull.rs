//! The pull-admission decision kernel: [`admit_pull`].
//!
//! A pull-mode pod is born knowing its derivation (the HMAC-attested
//! intent id); its first and only ask is `PullAssignment`, and this
//! function is the pure decision behind it: given the already-loaded
//! state of one node, either deliver (mint a fresh attempt / re-deliver
//! the open one), refuse (token / generation fence), park
//! (`NotYetReady`), or dismiss (`Gone`). The scheduler's
//! `rio_scheduler::actor::pull::admit_pull` is the projection shim over
//! this function (decision P10 — the function was kept pure from its
//! introduction precisely so it could be lifted into a kani harness
//! without refactoring); the durable mint that follows a `DeliverNew`
//! is the fenced SQL transaction and stays in the scheduler.
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

use crate::{ClosureEvidence, must_substitute};

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
    /// Upstream substitution in flight.
    Substituting,
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
    /// Whether the node may only complete via substitution (the
    /// [`must_substitute`] judgment, computed
    /// by the caller over the node's mark and closure evidence).
    pub must_substitute: bool,
    /// The open attempt bound to the derivation, if any:
    /// (executor identity, exec id).
    pub open_attempt: Option<(&'a ExecutorIdent, ExecId)>,
    /// The identity this pull would bind a fresh attempt to.
    pub pulling_identity: &'a ExecutorIdent,
}

// r[impl sched.executor.pull-gone]
// r[impl sched.executor.pull-not-ready+2]
// r[impl sched.merge.substitute-topdown+12]
// r[impl sched.lease.generation-fence+3]
/// Decide one pull from already-projected state. Pure — no clocks, no
/// IO — and total over its input domain (the proofs establish the full
/// partition).
///
/// Check order is load-bearing: identity first (a mis-bound token never
/// learns anything about the drv), then the generation fence (a deposed
/// believer answers nothing), then wantedness/deliverability.
///
/// The `Ready ∧ must_substitute` arm refuses the mint with
/// `NotYetReady` — never `DeliverNew` (a from-source dispatch of a
/// marked node with Broken closure evidence is doomed), and
/// deliberately NOT `Gone` (the node is still wanted; the Tick sweep's
/// probe/walk/reap arms own the definitive outcomes). The
/// `Assigned/Running` re-delivery does NOT re-check `must_substitute`
/// (the AW5 documented behavior: an attempt that was already minted is
/// re-delivered to its own identity; the evidence re-judgment happens
/// at the next decision point, not mid-attempt).
pub fn admit_pull<IntentId, ExecutorIdent, ExecId>(
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
        // Wanted but not deliverable yet: deps unbuilt, substitution in
        // flight, or a retry waiting to requeue. Never `Gone` (the
        // reap→respawn churn loop), never a write.
        S::Created | S::Queued | S::Substituting | S::Failed => PullAdmission::NotYetReady,
        // Ready but marked must-substitute (topdown-pruned with Broken
        // closure evidence): never serve it from source. Refuse the
        // mint — NotYetReady, no write, and deliberately NOT a
        // fail-fast: the pull carries no store verdict, so the node is
        // left for the Tick sweep's probe/walk/reap arms, which own the
        // definitive outcomes (inline-complete, route to substitution,
        // or the resubmit-directing fail-fast).
        S::Ready if request.must_substitute => PullAdmission::NotYetReady,
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

/// The pull-refusal chain in one function: classify the node's closure
/// evidence judgment ([`must_substitute`]) and
/// admit a Ready-node pull against it. The code-level form of the
/// closure-evidence campaign's A11 (`pullRefusalNoMint`):
/// `check_pull_refusal_chain` proves a marked node with Broken evidence
/// is always parked (`NotYetReady`), never minted and never dismissed.
///
/// `request.status` and `request.must_substitute` are overridden (the
/// node is taken as Ready; the judgment is computed from `topdown_pruned`
/// and `evidence`); every other request field is admitted as given.
pub fn pull_refused_for_evidence<IntentId, ExecutorIdent, ExecId>(
    topdown_pruned: bool,
    evidence: ClosureEvidence,
    request: PullRequest<'_, IntentId, ExecutorIdent, ExecId>,
) -> PullAdmission<ExecId>
where
    IntentId: PartialEq + ?Sized,
    ExecutorIdent: PartialEq + ?Sized,
{
    admit_pull(PullRequest {
        status: Some(PullNodeStatus::Ready),
        must_substitute: must_substitute(topdown_pruned, evidence),
        ..request
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request<'a>(
        status: Option<PullNodeStatus>,
        must_sub: bool,
        open: Option<(&'a u8, u8)>,
        pulling: &'a u8,
    ) -> PullRequest<'a, u8, u8, u8> {
        PullRequest {
            intent_id: &7,
            auth_intent: Some(&7),
            serving_generation: 3,
            generation_floor: Some(3),
            status,
            must_substitute: must_sub,
            open_attempt: open,
            pulling_identity: pulling,
        }
    }

    #[test]
    fn status_table() {
        use PullNodeStatus as S;
        let me = 1u8;
        for (status, want) in [
            (None, PullAdmission::Gone),
            (Some(S::Created), PullAdmission::NotYetReady),
            (Some(S::Queued), PullAdmission::NotYetReady),
            (Some(S::Substituting), PullAdmission::NotYetReady),
            (Some(S::Failed), PullAdmission::NotYetReady),
            (Some(S::Ready), PullAdmission::DeliverNew),
            (Some(S::Completed), PullAdmission::Gone),
            (Some(S::Cancelled), PullAdmission::Gone),
            (Some(S::Skipped), PullAdmission::Gone),
            (Some(S::Poisoned), PullAdmission::Gone),
            (Some(S::DependencyFailed), PullAdmission::Gone),
        ] {
            assert_eq!(
                admit_pull(request(status, false, None, &me)),
                want,
                "status {status:?}"
            );
        }
        for status in [S::Assigned, S::Running] {
            assert_eq!(
                admit_pull(request(Some(status), false, None, &me)),
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
                admit_pull(request(Some(status), false, Some((&me, 9)), &me)),
                PullAdmission::DeliverExisting { exec_id: 9 }
            );
            assert_eq!(
                admit_pull(request(Some(status), false, Some((&other, 9)), &me)),
                PullAdmission::NotYetReady,
                "an attempt open on another executor is never re-delivered"
            );
        }
    }

    #[test]
    fn must_substitute_refuses_mint() {
        let me = 1u8;
        assert_eq!(
            admit_pull(request(Some(PullNodeStatus::Ready), true, None, &me)),
            PullAdmission::NotYetReady
        );
        // The flag only parks deliverable-from-source work: a terminal
        // node still answers Gone.
        assert_eq!(
            admit_pull(request(Some(PullNodeStatus::Completed), true, None, &me)),
            PullAdmission::Gone
        );
    }

    #[test]
    fn rejections_dominate() {
        let me = 1u8;
        // Token mismatch wins even for a Ready drv.
        let mut req = request(Some(PullNodeStatus::Ready), false, None, &me);
        req.auth_intent = Some(&8);
        assert_eq!(admit_pull(req), PullAdmission::RejectToken);
        // Below-floor serving generation answers nothing.
        let mut req = request(Some(PullNodeStatus::Ready), false, None, &me);
        req.serving_generation = 2;
        req.generation_floor = Some(3);
        assert_eq!(admit_pull(req), PullAdmission::RejectStaleGeneration);
        // Dev mode (no token) and fresh cluster (no floor) both admit.
        let mut req = request(Some(PullNodeStatus::Ready), false, None, &me);
        req.auth_intent = None;
        req.generation_floor = None;
        req.serving_generation = 0;
        assert_eq!(admit_pull(req), PullAdmission::DeliverNew);
    }

    #[test]
    fn refusal_chain_composes_classifier_and_admission() {
        let me = 1u8;
        // Marked + holed (Broken evidence) ⇒ refused.
        let ev = crate::closure_evidence(true, true, Some([true, true].into_iter()));
        assert_eq!(
            pull_refused_for_evidence(true, ev, request(None, false, None, &me)),
            PullAdmission::NotYetReady
        );
        // Vouched evidence ⇒ delivered.
        let ev = crate::closure_evidence(true, false, Some([true, true].into_iter()));
        assert_eq!(
            pull_refused_for_evidence(true, ev, request(None, false, None, &me)),
            PullAdmission::DeliverNew
        );
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
        must_sub: bool,
        open: Option<(u8, u8)>,
        pulling: u8,
    }

    fn any_status() -> Option<PullNodeStatus> {
        let sel: u8 = kani::any();
        kani::assume(sel < 13);
        match sel {
            0 => None,
            1 => Some(PullNodeStatus::Created),
            2 => Some(PullNodeStatus::Queued),
            3 => Some(PullNodeStatus::Ready),
            4 => Some(PullNodeStatus::Assigned),
            5 => Some(PullNodeStatus::Running),
            6 => Some(PullNodeStatus::Substituting),
            7 => Some(PullNodeStatus::Completed),
            8 => Some(PullNodeStatus::Failed),
            9 => Some(PullNodeStatus::Poisoned),
            10 => Some(PullNodeStatus::DependencyFailed),
            11 => Some(PullNodeStatus::Cancelled),
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
            must_sub: kani::any(),
            open: if kani::any() {
                Some((kani::any(), kani::any()))
            } else {
                None
            },
            pulling: kani::any(),
        }
    }

    fn run(inputs: &Inputs) -> PullAdmission<u8> {
        admit_pull(PullRequest {
            intent_id: &inputs.intent,
            auth_intent: inputs.auth.as_ref(),
            serving_generation: inputs.serving,
            generation_floor: inputs.floor,
            status: inputs.status,
            must_substitute: inputs.must_sub,
            open_attempt: inputs.open.as_ref().map(|(e, x)| (e, *x)),
            pulling_identity: &inputs.pulling,
        })
    }

    /// The admission's exhaustive partition: for every input vector the
    /// decision is exactly the one the documented case analysis names —
    /// token mismatch → RejectToken; else below-floor →
    /// RejectStaleGeneration; else absent → Gone; else terminal → Gone;
    /// else parked statuses → NotYetReady; else Ready+must_substitute →
    /// NotYetReady; else Ready → DeliverNew; else (Assigned/Running)
    /// identity-matched open attempt → DeliverExisting, otherwise
    /// NotYetReady. Total and panic-free over the domain.
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
                Some(S::Created | S::Queued | S::Substituting | S::Failed) => {
                    PullAdmission::NotYetReady
                }
                Some(S::Ready) if inputs.must_sub => PullAdmission::NotYetReady,
                Some(S::Ready) => PullAdmission::DeliverNew,
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

    /// A11 (`pullRefusalNoMint`), code half: a Ready node with
    /// `must_substitute` is never delivered — the admission is
    /// NotYetReady (parked for the sweep), never DeliverNew and never
    /// DeliverExisting, and (being a refusal) implies no write. The
    /// re-delivery arm for Assigned/Running deliberately does NOT
    /// re-check the flag (AW5): that is the one delivery a
    /// must-substitute node can still receive, and only to the identity
    /// already holding the open attempt.
    #[kani::proof]
    fn check_admit_pull_refuses_must_substitute() {
        let inputs = any_inputs();
        let decision = run(&inputs);

        if inputs.must_sub && inputs.status == Some(PullNodeStatus::Ready) {
            // Possibly RejectToken/RejectStaleGeneration (they dominate),
            // but never a delivery and never Gone.
            assert!(matches!(
                decision,
                PullAdmission::NotYetReady
                    | PullAdmission::RejectToken
                    | PullAdmission::RejectStaleGeneration
            ));
        }
        // DeliverNew is only ever produced for a Ready node WITHOUT the
        // flag…
        if decision == PullAdmission::DeliverNew {
            assert_eq!(inputs.status, Some(PullNodeStatus::Ready));
            assert!(!inputs.must_sub);
        }
        // …and a delivery of any kind to a must-substitute node can
        // only be the AW5 re-delivery: an Assigned/Running attempt
        // already bound to the pulling identity.
        if inputs.must_sub
            && let PullAdmission::DeliverExisting { .. } = decision
        {
            assert!(matches!(
                inputs.status,
                Some(PullNodeStatus::Assigned | PullNodeStatus::Running)
            ));
            assert!(inputs.open.is_some_and(|(e, _)| e == inputs.pulling));
        }
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

    /// The end-to-end refusal chain (A11 through both kernels): for ANY
    /// classifier input whose judgment is must_substitute — a marked
    /// node that is absent, holed, or childless — an authenticated,
    /// at-or-above-floor pull of that Ready node is parked NotYetReady;
    /// it is never minted, never re-delivered, and never dismissed as
    /// Gone.
    #[kani::proof]
    #[kani::unwind(6)]
    fn check_pull_refusal_chain() {
        // Classifier inputs (the bounded child domain of the sibling
        // proofs in crate::proofs).
        let present: bool = kani::any();
        let hole: bool = kani::any();
        let has_entry: bool = kani::any();
        let bits: [bool; 4] = kani::any();
        let n: usize = kani::any();
        kani::assume(n <= 4);
        let marked: bool = kani::any();

        let children = if has_entry {
            Some(bits[..n].iter().copied())
        } else {
            None
        };
        let evidence = crate::closure_evidence(present, hole, children);

        // Pull inputs: authenticated, at-or-above floor, free attempt
        // state.
        let open: Option<(u8, u8)> = if kani::any() {
            Some((kani::any(), kani::any()))
        } else {
            None
        };
        let pulling: u8 = kani::any();
        let intent: u8 = kani::any();

        let decision = pull_refused_for_evidence(
            marked,
            evidence,
            PullRequest {
                intent_id: &intent,
                auth_intent: Some(&intent),
                serving_generation: 5,
                generation_floor: Some(3),
                status: None,
                must_substitute: false,
                open_attempt: open.as_ref().map(|(e, x)| (e, *x)),
                pulling_identity: &pulling,
            },
        );

        if crate::must_substitute(marked, evidence) {
            assert_eq!(
                decision,
                PullAdmission::NotYetReady,
                "a Ready must-substitute node is parked, never delivered or dismissed"
            );
        } else {
            assert_eq!(
                decision,
                PullAdmission::DeliverNew,
                "a Ready node without the must-substitute judgment mints"
            );
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Substitution-replacement Phase A: the kind-aware coexistence wrapper.
//
// During Phases A–C′ the kernel carries BOTH refusal predicates
// (design §2.3, finding FP-3): the as-built must_substitute arm
// (admit_pull above, byte-identical, its battery and proofs untouched)
// and the materialization kind/job-state table below, selected by the
// scheduler.materialization.enabled flag. Phase D′ collapses the
// wrapper into admit_pull when the flag and the must_substitute arm
// are deleted.
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
pub struct MaterializationInputs {
    /// scheduler.materialization.enabled.
    pub enabled: bool,
    /// The pull's claimed kind.
    pub kind: PullKind,
    /// The node's job state (always JobView::None when !enabled — the
    /// scheduler never loads the view flag-off).
    pub job: JobView,
}

// r[impl sched.materialize.job]
/// The coexistence-form admission (design §2.3's table). Pure, total.
///
/// Flag-off: delegates to [`admit_pull`] for kind=Build (bit-identical
/// as-built behavior) and parks kind=Materialization pulls NotYetReady
/// (nothing is claimable while the scheduler-side flag is off — the
/// AS-6 mixed-flag posture: a store whose flag is on but whose
/// scheduler's is off must hang harmlessly, never error).
///
/// Flag-on:
///   kind=Build:
///     job Pending/Claimed   → NotYetReady (the must_substitute
///                             refusal's successor: never serve from
///                             source while materialization is undecided)
///     job None              → as-built admit_pull (which still carries
///                             the must_substitute arm — the dual-write
///                             window keeps marks meaningful: both
///                             predicates are active flag-on, and the
///                             job-state predicate only takes precedence
///                             when a job exists)
///   kind=Materialization:
///     job None              → Gone (nothing to materialize)
///     job Pending{parked:f} → as-built admission gates (token/fence/
///                             status) then: node Ready → DeliverNew;
///                             node terminal → Gone; else NotYetReady
///                             (Phase A claims from Ready only — the
///                             Queued extension is Phase B, plan Delta D6)
///     job Pending{parked:t} → NotYetReady (backoff unexpired)
///     job Claimed{held:t}   → DeliverExisting (re-delivery to the same
///                             replica — needs the open attempt's exec id,
///                             so this arm consumes request.open_attempt)
///     job Claimed{held:f}   → NotYetReady (the one-winner arbiter, BC-1)
pub fn admit_pull_kinded<IntentId, ExecutorIdent, ExecId>(
    request: PullRequest<'_, IntentId, ExecutorIdent, ExecId>,
    mat: MaterializationInputs,
) -> PullAdmission<ExecId>
where
    IntentId: PartialEq + ?Sized,
    ExecutorIdent: PartialEq + ?Sized,
{
    if !mat.enabled {
        return match mat.kind {
            PullKind::Build => admit_pull(request),
            // Store executor running against a flag-off scheduler:
            // park, never error, never write (AS-6).
            PullKind::Materialization => PullAdmission::NotYetReady,
        };
    }
    match (mat.kind, mat.job) {
        // Build pulls while a job is unresolved: refuse (A1/A11 successor).
        (PullKind::Build, JobView::Pending { .. } | JobView::Claimed { .. }) => {
            // Identity/fence rejections still dominate (check order is
            // load-bearing): run the as-built kernel and override only
            // its *delivery* outcomes.
            match admit_pull(request) {
                PullAdmission::RejectToken => PullAdmission::RejectToken,
                PullAdmission::RejectStaleGeneration => PullAdmission::RejectStaleGeneration,
                PullAdmission::Gone => PullAdmission::Gone,
                _ => PullAdmission::NotYetReady,
            }
        }
        (PullKind::Build, JobView::None) => admit_pull(request),
        (PullKind::Materialization, JobView::None) => {
            // Token/fence still dominate; then Gone.
            match admit_pull(request) {
                PullAdmission::RejectToken => PullAdmission::RejectToken,
                PullAdmission::RejectStaleGeneration => PullAdmission::RejectStaleGeneration,
                _ => PullAdmission::Gone,
            }
        }
        (PullKind::Materialization, JobView::Pending { parked }) => {
            // PHASE B: a dedicated Queued-status check lands here when
            // the Queued→Assigned transition edge + mint-ordering rework
            // make dep-racing claims legal (plan Delta D6 / PDQ-6).
            match admit_pull(request) {
                PullAdmission::RejectToken => PullAdmission::RejectToken,
                PullAdmission::RejectStaleGeneration => PullAdmission::RejectStaleGeneration,
                PullAdmission::Gone => PullAdmission::Gone,
                // The as-built kernel said the node is deliverable from
                // Ready; materialization claims need Ready (Phase A)
                // and an unparked job.
                PullAdmission::DeliverNew if !parked => PullAdmission::DeliverNew,
                _ => PullAdmission::NotYetReady,
            }
        }
        (PullKind::Materialization, JobView::Claimed { held_by_puller }) => {
            // The as-built gates run FIRST (gate-ordering: a mis-bound
            // token or a below-floor generation must never receive a
            // re-delivery — the same dominance every other arm
            // enforces), and the re-delivery is exactly what the
            // as-built kernel itself re-delivers: an Assigned/Running
            // attempt whose open-attempt identity equals the pulling
            // identity. The job view's held_by_puller must AGREE
            // (defense in depth, BC-1: the request-level identity
            // comparison and the scheduler's view projection are both
            // required; a stale view never re-delivers another
            // identity's attempt).
            match admit_pull(request) {
                PullAdmission::RejectToken => PullAdmission::RejectToken,
                PullAdmission::RejectStaleGeneration => PullAdmission::RejectStaleGeneration,
                PullAdmission::Gone => PullAdmission::Gone,
                PullAdmission::DeliverExisting { exec_id } if held_by_puller => {
                    PullAdmission::DeliverExisting { exec_id }
                }
                _ => PullAdmission::NotYetReady,
            }
        }
    }
}

#[cfg(test)]
mod kinded_tests {
    use super::*;

    fn request<'a>(
        status: Option<PullNodeStatus>,
        must_sub: bool,
        open: Option<(&'a u8, u8)>,
        pulling: &'a u8,
    ) -> PullRequest<'a, u8, u8, u8> {
        PullRequest {
            intent_id: &7,
            auth_intent: Some(&7),
            serving_generation: 3,
            generation_floor: Some(3),
            status,
            must_substitute: must_sub,
            open_attempt: open,
            pulling_identity: pulling,
        }
    }

    /// Every (status, must_substitute, open-attempt, token, floor) corner the
    /// as-built battery covers, swept for both flag-off kinds.
    fn domain<'a>(me: &'a u8, other: &'a u8) -> Vec<PullRequest<'a, u8, u8, u8>> {
        use PullNodeStatus as S;
        let statuses = [
            None,
            Some(S::Created),
            Some(S::Queued),
            Some(S::Ready),
            Some(S::Assigned),
            Some(S::Running),
            Some(S::Substituting),
            Some(S::Completed),
            Some(S::Failed),
            Some(S::Poisoned),
            Some(S::DependencyFailed),
            Some(S::Cancelled),
            Some(S::Skipped),
        ];
        let mut out = Vec::new();
        for status in statuses {
            for must_sub in [false, true] {
                for open in [None, Some((me, 9u8)), Some((other, 9u8))] {
                    // Plain authenticated at-floor request.
                    out.push(request(status, must_sub, open, me));
                    // Token mismatch.
                    let mut req = request(status, must_sub, open, me);
                    req.auth_intent = Some(&8);
                    out.push(req);
                    // Below-floor serving generation.
                    let mut req = request(status, must_sub, open, me);
                    req.serving_generation = 2;
                    out.push(req);
                    // Dev mode / fresh cluster.
                    let mut req = request(status, must_sub, open, me);
                    req.auth_intent = None;
                    req.generation_floor = None;
                    req.serving_generation = 0;
                    out.push(req);
                }
            }
        }
        out
    }

    // r[verify sched.materialize.job]
    /// THE dormancy theorem for pull admission (design §2.3 / FP-3): with
    /// the materialization flag OFF, admit_pull_kinded is extensionally
    /// EQUAL to the as-built admit_pull for every input — same decision,
    /// every status, every attempt state, every fence/token state.
    #[test]
    fn kinded_wrapper_flag_off_equals_as_built() {
        let me = 1u8;
        let other = 2u8;
        for req in domain(&me, &other) {
            let mirror = PullRequest { ..req };
            let expected = admit_pull(mirror);
            let got = admit_pull_kinded(
                req,
                MaterializationInputs {
                    enabled: false,
                    kind: PullKind::Build,
                    job: JobView::None,
                },
            );
            assert_eq!(got, expected, "flag-off build pull must equal as-built");
        }
        // Flag-off materialization pulls (a flag-on store polling a
        // flag-off scheduler — the AS-6 mixed-flag posture): always
        // parked, never an error, never a delivery, never Gone.
        for req in domain(&me, &other) {
            let got = admit_pull_kinded(
                req,
                MaterializationInputs {
                    enabled: false,
                    kind: PullKind::Materialization,
                    job: JobView::None,
                },
            );
            assert_eq!(
                got,
                PullAdmission::NotYetReady,
                "flag-off materialization pulls park harmlessly"
            );
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
                enabled: true,
                kind: PullKind::Build,
                job,
            };
            // Ready (deliverable as-built) → refused.
            assert_eq!(
                admit_pull_kinded(request(Some(S::Ready), false, None, &me), mat),
                PullAdmission::NotYetReady,
                "build pull of a job-unresolved Ready node must park ({job:?})"
            );
            // Queued (not deliverable as-built) → still NotYetReady.
            assert_eq!(
                admit_pull_kinded(request(Some(S::Queued), false, None, &me), mat),
                PullAdmission::NotYetReady
            );
            // Terminal → Gone (never laundered into a park).
            assert_eq!(
                admit_pull_kinded(request(Some(S::Completed), false, None, &me), mat),
                PullAdmission::Gone
            );
            // Token mismatch dominates.
            let mut req = request(Some(S::Ready), false, None, &me);
            req.auth_intent = Some(&8);
            assert_eq!(admit_pull_kinded(req, mat), PullAdmission::RejectToken);
            // Below-floor dominates.
            let mut req = request(Some(S::Ready), false, None, &me);
            req.serving_generation = 2;
            assert_eq!(
                admit_pull_kinded(req, mat),
                PullAdmission::RejectStaleGeneration
            );
        }
    }

    /// Flag-on: the materialization claim table (design §2.3), including the
    /// PD-6/Delta-D6 Phase A boundary pin per adjudication PDQ-6:
    /// materialization claims are Ready-only — a Queued node refuses the
    /// claim NotYetReady (Phase B flips this case to DeliverNew when the
    /// Queued→Assigned edge + mint-ordering rework land).
    #[test]
    fn kinded_flag_on_materialization_claim_table() {
        use PullNodeStatus as S;
        let me = 1u8;
        let other = 2u8;
        let mat = |job| MaterializationInputs {
            enabled: true,
            kind: PullKind::Materialization,
            job,
        };
        // No job → Gone (nothing to materialize).
        assert_eq!(
            admit_pull_kinded(
                request(Some(S::Ready), false, None, &me),
                mat(JobView::None)
            ),
            PullAdmission::Gone
        );
        // Pending unparked + node Ready → DeliverNew.
        assert_eq!(
            admit_pull_kinded(
                request(Some(S::Ready), false, None, &me),
                mat(JobView::Pending { parked: false })
            ),
            PullAdmission::DeliverNew
        );
        // PDQ-6 boundary pin: Pending unparked + node Queued → NotYetReady
        // (Ready-only claims in Phase A; Phase B flips this to DeliverNew).
        assert_eq!(
            admit_pull_kinded(
                request(Some(S::Queued), false, None, &me),
                mat(JobView::Pending { parked: false })
            ),
            PullAdmission::NotYetReady
        );
        // Pending unparked + node terminal → Gone.
        assert_eq!(
            admit_pull_kinded(
                request(Some(S::Completed), false, None, &me),
                mat(JobView::Pending { parked: false })
            ),
            PullAdmission::Gone
        );
        // Pending parked → NotYetReady (backoff unexpired), even for Ready.
        assert_eq!(
            admit_pull_kinded(
                request(Some(S::Ready), false, None, &me),
                mat(JobView::Pending { parked: true })
            ),
            PullAdmission::NotYetReady
        );
        // Claimed held-by-puller → DeliverExisting (re-delivery; consumes
        // request.open_attempt's exec id).
        assert_eq!(
            admit_pull_kinded(
                request(Some(S::Running), false, Some((&me, 9)), &me),
                mat(JobView::Claimed {
                    held_by_puller: true
                })
            ),
            PullAdmission::DeliverExisting { exec_id: 9 }
        );
        // Claimed held-by-puller but no open-attempt bookkeeping → park.
        assert_eq!(
            admit_pull_kinded(
                request(Some(S::Running), false, None, &me),
                mat(JobView::Claimed {
                    held_by_puller: true
                })
            ),
            PullAdmission::NotYetReady
        );
        // Claimed by another identity → NotYetReady (the one-winner
        // arbiter, BC-1).
        assert_eq!(
            admit_pull_kinded(
                request(Some(S::Running), false, Some((&other, 9)), &me),
                mat(JobView::Claimed {
                    held_by_puller: false
                })
            ),
            PullAdmission::NotYetReady
        );
        // Token/fence rejections dominate the whole table.
        let mut req = request(Some(S::Ready), false, None, &me);
        req.auth_intent = Some(&8);
        assert_eq!(
            admit_pull_kinded(req, mat(JobView::Pending { parked: false })),
            PullAdmission::RejectToken
        );
        let mut req = request(Some(S::Ready), false, None, &me);
        req.serving_generation = 2;
        assert_eq!(
            admit_pull_kinded(req, mat(JobView::None)),
            PullAdmission::RejectStaleGeneration
        );
        // ...INCLUDING the Claimed re-delivery arm (the gate-ordering
        // pin: held_by_puller must never bypass the identity/fence
        // gates — a mis-bound token or a deposed believer must never
        // receive a re-delivery, exactly as the build-kind arms behave).
        let mut req = request(Some(S::Running), false, Some((&me, 9)), &me);
        req.auth_intent = Some(&8);
        assert_eq!(
            admit_pull_kinded(
                req,
                mat(JobView::Claimed {
                    held_by_puller: true
                })
            ),
            PullAdmission::RejectToken,
            "a mis-bound token must never receive a re-delivery"
        );
        let mut req = request(Some(S::Running), false, Some((&me, 9)), &me);
        req.serving_generation = 2;
        assert_eq!(
            admit_pull_kinded(
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
            admit_pull_kinded(
                request(Some(S::Running), false, Some((&other, 9)), &me),
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
        // As-built would deliver this Ready no-job node...
        let as_built = admit_pull(request(Some(S::Ready), false, None, &me));
        assert_eq!(as_built, PullAdmission::DeliverNew);
        // ...but a materialization pull of the same node never gets it
        // (no job → Gone), flag-on or flag-off.
        for enabled in [false, true] {
            let got = admit_pull_kinded(
                request(Some(S::Ready), false, None, &me),
                MaterializationInputs {
                    enabled,
                    kind: PullKind::Materialization,
                    job: JobView::None,
                },
            );
            assert_ne!(
                got,
                PullAdmission::DeliverNew,
                "a materialization pull must never receive a build delivery (enabled={enabled})"
            );
        }
        // And a build pull while a materialization job is unresolved never
        // receives the job's delivery (it parks instead).
        let got = admit_pull_kinded(
            request(Some(S::Ready), false, None, &me),
            MaterializationInputs {
                enabled: true,
                kind: PullKind::Build,
                job: JobView::Pending { parked: false },
            },
        );
        assert_eq!(got, PullAdmission::NotYetReady);
    }
}

#[cfg(kani)]
mod kinded_proofs {
    //! CBMC proof harnesses for the kind-aware coexistence wrapper
    //! ([`admit_pull_kinded`]). Same bounded domain as the as-built
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
        must_sub: bool,
        open: Option<(u8, u8)>,
        pulling: u8,
    }

    fn any_status() -> Option<PullNodeStatus> {
        let sel: u8 = kani::any();
        kani::assume(sel < 13);
        match sel {
            0 => None,
            1 => Some(PullNodeStatus::Created),
            2 => Some(PullNodeStatus::Queued),
            3 => Some(PullNodeStatus::Ready),
            4 => Some(PullNodeStatus::Assigned),
            5 => Some(PullNodeStatus::Running),
            6 => Some(PullNodeStatus::Substituting),
            7 => Some(PullNodeStatus::Completed),
            8 => Some(PullNodeStatus::Failed),
            9 => Some(PullNodeStatus::Poisoned),
            10 => Some(PullNodeStatus::DependencyFailed),
            11 => Some(PullNodeStatus::Cancelled),
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
            must_sub: kani::any(),
            open: if kani::any() {
                Some((kani::any(), kani::any()))
            } else {
                None
            },
            pulling: kani::any(),
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

    fn run_as_built(inputs: &Inputs) -> PullAdmission<u8> {
        admit_pull(PullRequest {
            intent_id: &inputs.intent,
            auth_intent: inputs.auth.as_ref(),
            serving_generation: inputs.serving,
            generation_floor: inputs.floor,
            status: inputs.status,
            must_substitute: inputs.must_sub,
            open_attempt: inputs.open.as_ref().map(|(e, x)| (e, *x)),
            pulling_identity: &inputs.pulling,
        })
    }

    fn run_kinded(inputs: &Inputs, mat: MaterializationInputs) -> PullAdmission<u8> {
        admit_pull_kinded(
            PullRequest {
                intent_id: &inputs.intent,
                auth_intent: inputs.auth.as_ref(),
                serving_generation: inputs.serving,
                generation_floor: inputs.floor,
                status: inputs.status,
                must_substitute: inputs.must_sub,
                open_attempt: inputs.open.as_ref().map(|(e, x)| (e, *x)),
                pulling_identity: &inputs.pulling,
            },
            mat,
        )
    }

    /// The flag-off identity, proven over the full bounded input domain
    /// (the dormancy theorem): !enabled ⇒ kinded(Build) ≡ as-built, and
    /// kinded(Materialization) ≡ NotYetReady (the AS-6 harmless park) —
    /// for EVERY job view (the scheduler never loads the view flag-off,
    /// but the kernel must not depend on that).
    #[kani::proof]
    fn check_kinded_flag_off_identity() {
        let inputs = any_inputs();
        let job = any_job_view();

        let kinded_build = run_kinded(
            &inputs,
            MaterializationInputs {
                enabled: false,
                kind: PullKind::Build,
                job,
            },
        );
        assert_eq!(kinded_build, run_as_built(&inputs));

        let kinded_mat = run_kinded(
            &inputs,
            MaterializationInputs {
                enabled: false,
                kind: PullKind::Materialization,
                job,
            },
        );
        assert_eq!(kinded_mat, PullAdmission::NotYetReady);
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
                enabled: true,
                kind: PullKind::Build,
                job,
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
    /// unparked Pending job on a Ready node with no open attempt held by
    /// anyone else; a re-delivery (DeliverExisting) requires the open
    /// attempt to be the puller's own and carries exactly its exec id.
    #[kani::proof]
    fn check_kinded_one_winner_arbitration() {
        let inputs = any_inputs();
        let job = any_job_view();

        let decision = run_kinded(
            &inputs,
            MaterializationInputs {
                enabled: true,
                kind: PullKind::Materialization,
                job,
            },
        );

        match decision {
            PullAdmission::DeliverNew => {
                // Fresh claims happen only through the Pending-unparked
                // arm, on a Ready node, via the as-built gates (which
                // require no open attempt by another identity:
                // DeliverNew is only produced for Ready, and Ready
                // nodes carry no open attempt in the as-built table).
                assert!(matches!(job, JobView::Pending { parked: false }));
                assert_eq!(inputs.status, Some(PullNodeStatus::Ready));
            }
            PullAdmission::DeliverExisting { exec_id } => {
                // Re-deliveries happen only to the identity already
                // holding the claim (the Claimed{held_by_puller} arm),
                // and carry exactly the open attempt's exec id — which
                // must itself be bound to the pulling identity (the
                // request-level comparison and the view projection must
                // BOTH hold; a stale view never re-delivers another
                // identity's attempt).
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
                enabled: true,
                kind,
                job,
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
