//! Substitute-failure classification — the dependency-free truth table
//! behind the store executor's walk error arm (bughunt wave A4,
//! merged_bug_178).
//!
//! The walk's pre-fix arm was a blanket `Err(e) => infra_failure(...)`:
//! every failure class — including the two the substituter itself
//! documents as transient-and-retryable (`Raced`: another uploader
//! holds the placeholder slot; `RateLimited`: upstream 429 with a
//! parsed `Retry-After`) — became a job-fatal `InfraFailure` that
//! charged the materialization budget. Three 429 waves parked a
//! healthy job.
//!
//! This module is the single decision surface: rio-store maps its
//! `SubstituteError` variants onto [`SubstituteFailureClass`]
//! EXHAUSTIVELY (no catch-all — a future error variant fails the build
//! at BOTH hops), and [`classify_substitute_failure`] is total over the
//! class alphabet. The kani harness sweeps the entire table; the
//! enumeration unit test pins each row.
//!
//! Scope narrowing (recorded in the substitution-replacement invariant
//! map): `Stalled` and `AdmissionSaturated` STAY `ChargeInfra` by
//! design — the stall-abort contract (`store.substitute.stall-abort`)
//! deliberately reports infrastructure failure so the strike ladder
//! and the park budget see stalls, and a saturated per-replica
//! admission gate is capacity evidence, not a politeness signal.

/// The walk-relevant failure classes of one substitution attempt.
/// rio-store maps `SubstituteError` → this enum exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubstituteFailureClass {
    /// Another uploader holds the placeholder slot (cross-replica
    /// race). The upload in flight will land; retrying later reaches
    /// `AlreadyComplete`.
    Raced,
    /// Upstream 429 (rate-limited), with the parsed `Retry-After`
    /// available to the caller.
    RateLimited,
    /// Owner-side download-stall abort (no body bytes for the stall
    /// window). Charges by design — see the module doc.
    Stalled,
    /// The per-replica admission gate timed out or closed. Charges by
    /// design — capacity evidence.
    AdmissionSaturated,
    /// Upstream fetch trouble (connect/TLS/5xx) or upstream-served
    /// garbage that failed parse.
    Fetch,
    /// Integrity violation: NAR hash/size mismatch, oversize caps.
    Integrity,
    /// Local ingest failure (metadata write-ahead, chunk upload).
    Ingest,
}

/// What the scheduler-facing outcome of a classified failure is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureDisposition {
    /// Close the attempt charge-free and retry the job after a short
    /// deferral — the failure is transient by the substituter's own
    /// contract and says nothing about the upstream's content or the
    /// store's health.
    RetryUncharged,
    /// Report `InfraFailure`: the charge ladder (and therefore the
    /// park budget) must see this.
    ChargeInfra,
}

/// The total classification table (no catch-all: adding a
/// [`SubstituteFailureClass`] variant breaks this build).
// r[impl store.materialize.executor+5]
pub fn classify_substitute_failure(class: SubstituteFailureClass) -> FailureDisposition {
    match class {
        SubstituteFailureClass::Raced | SubstituteFailureClass::RateLimited => {
            FailureDisposition::RetryUncharged
        }
        SubstituteFailureClass::Stalled
        | SubstituteFailureClass::AdmissionSaturated
        | SubstituteFailureClass::Fetch
        | SubstituteFailureClass::Integrity
        | SubstituteFailureClass::Ingest => FailureDisposition::ChargeInfra,
    }
}

/// The per-tenant settlement re-probe fold (bughunt wave A4,
/// merged_bug_028 / owner decision Q2 2026-06-03): the consumption
/// arm-3 re-probe asks every LIVE tenant's upstream view, and
/// `ConfirmedMissing` — the verdict that can fail-fast a pruned root
/// or settle a leaf from-source — requires EVERY tenant to confirm.
/// One obtainable answer (the path is present, substitutable, or even
/// indeterminate under that tenant) keeps the job armed: the job
/// fails only when NO interested tenant can obtain.
///
/// An EMPTY answer set folds to `Obtainable` (nothing was confirmed;
/// the caller's no-live-tenant shape has its own conservative arms).
/// RPC failures never reach this fold — the caller maps them to its
/// B3 ReArm before folding (same posture as `route_unobtainable`'s
/// `None` reprobe).
// r[impl sched.materialize.routing+5]
pub fn fold_tenant_reprobes(
    answers: &[crate::routing::ReprobeAnswer],
) -> crate::routing::ReprobeAnswer {
    use crate::routing::ReprobeAnswer;
    if !answers.is_empty()
        && answers
            .iter()
            .all(|a| matches!(a, ReprobeAnswer::ConfirmedMissing))
    {
        ReprobeAnswer::ConfirmedMissing
    } else {
        ReprobeAnswer::Obtainable
    }
}

/// What the loop body does after recording one classified failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "AbortRaced must stop the iteration — ignoring it races the placeholder slot"]
pub enum LoopControl {
    /// Keep iterating: the failure was upstream-local and is now
    /// recorded evidence; a later upstream may still serve.
    Continue,
    /// Stop: another uploader holds the placeholder slot; remaining
    /// upstreams would race the same slot.
    AbortRaced,
}

/// The per-iteration evidence cells of one `do_substitute` upstream
/// loop (merged_bug_044, completing bug_081's fold). Fields are
/// PRIVATE and the ONE writer is [`SubstituteLoopCells::record`],
/// keyed on the already-exhaustive [`SubstituteFailureClass`]: a
/// future `SubstituteError` variant breaks the caller's error→class
/// hop and this routing together at compile time, and a loop arm
/// cannot reach the fold with unrecorded failure evidence — the
/// pre-fix catch-all `Err(e) => continue` (which made an all-errored
/// iteration indistinguishable from an all-404 clean miss) is no
/// longer expressible through this API.
#[derive(Debug, Clone, Copy, Default)]
pub struct SubstituteLoopCells {
    any_stall: Option<core::time::Duration>,
    any_429: Option<Option<core::time::Duration>>,
    any_errored: bool,
}

impl SubstituteLoopCells {
    /// Empty cells: no failure observed yet.
    pub const fn new() -> Self {
        Self {
            any_stall: None,
            any_429: None,
            any_errored: false,
        }
    }

    /// THE recording chokepoint — total over the class alphabet (no
    /// catch-all). `advice` is the class's duration evidence: the
    /// stall window for `Stalled`, the parsed `Retry-After` for
    /// `RateLimited` (`None` = 429 without advice); the remaining
    /// classes carry none and ignore it. Stall windows and
    /// retry-afters keep the MAX across upstreams (matching the
    /// probe semantics).
    pub fn record(
        &mut self,
        class: SubstituteFailureClass,
        advice: Option<core::time::Duration>,
    ) -> LoopControl {
        match class {
            SubstituteFailureClass::Raced => return LoopControl::AbortRaced,
            SubstituteFailureClass::Stalled => {
                let window = advice.unwrap_or_default();
                self.any_stall = Some(match self.any_stall {
                    Some(prev) => prev.max(window),
                    None => window,
                });
            }
            SubstituteFailureClass::RateLimited => {
                self.any_429 = Some(match (self.any_429.flatten(), advice) {
                    (Some(prev), Some(ra)) => Some(prev.max(ra)),
                    (Some(prev), None) => Some(prev),
                    (None, ra) => ra,
                });
            }
            SubstituteFailureClass::AdmissionSaturated
            | SubstituteFailureClass::Fetch
            | SubstituteFailureClass::Integrity
            | SubstituteFailureClass::Ingest => self.any_errored = true,
        }
        LoopControl::Continue
    }
}

/// The post-loop verdict of one `do_substitute` upstream iteration that
/// produced no hit (bughunt wave A4, bug_081 + merged_bug_044).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubstituteLoopVerdict {
    /// Every upstream answered hit-or-404 — the cacheable definitive
    /// miss. With the error axis recorded (merged_bug_044) this
    /// contract is established for the first time: an iteration where
    /// any upstream ERRORED folds to [`Self::Errored`], never here.
    CleanMiss,
    /// ≥1 upstream stalled (and none served). The stall dominates a
    /// concurrent 429: its strike is already durably recorded and the
    /// executor's classification charges it as infrastructure
    /// evidence — surfacing `RateLimited` instead would hide the
    /// recorded strike behind back-off advice.
    Stalled {
        /// The widest stall window observed across upstreams.
        window: core::time::Duration,
    },
    /// ≥1 upstream 429'd (and none served, none stalled). Uncached so
    /// a retry can re-ask the rate-limited upstream.
    RateLimited {
        /// Max parsed `Retry-After` across the 429ing upstreams.
        retry_after: Option<core::time::Duration>,
    },
    /// ≥1 upstream errored (fetch/TLS/parse/integrity/ingest/
    /// admission) and none served, stalled, or 429'd. NOT a
    /// definitive miss: the caller must surface an UNCACHED error so
    /// the next attempt re-asks — caching this as a miss poisons the
    /// `(tenant, path)` slot for the TTL window (merged_bug_044).
    Errored,
}

/// bug_081's pure post-loop fold over merged_bug_044's evidence
/// cells: a failure on ONE upstream is upstream-local — the loop
/// records it and fails over; only after every upstream has been
/// tried does the recorded evidence pick the attempt outcome. Total
/// over all three observation axes; precedence `Stalled >
/// RateLimited > Errored > CleanMiss` (charging evidence outranks
/// back-off advice outranks "something broke, don't cache" outranks
/// a cacheable miss).
// r[impl store.substitute.stall-abort+2]
// r[impl store.substitute.loop-evidence-total]
pub fn fold_substitute_loop(cells: SubstituteLoopCells) -> SubstituteLoopVerdict {
    match (cells.any_stall, cells.any_429, cells.any_errored) {
        (Some(window), _, _) => SubstituteLoopVerdict::Stalled { window },
        (None, Some(retry_after), _) => SubstituteLoopVerdict::RateLimited { retry_after },
        (None, None, true) => SubstituteLoopVerdict::Errored,
        (None, None, false) => SubstituteLoopVerdict::CleanMiss,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::ReprobeAnswer;

    /// bug_081 + merged_bug_044: the post-loop fold's precedence over
    /// the recorded cells, including the error axis the pre-fix
    /// 2-axis fold could not represent.
    #[test]
    fn substitute_loop_fold_precedence() {
        use core::time::Duration;
        let empty = SubstituteLoopCells::new();
        assert_eq!(
            fold_substitute_loop(empty),
            SubstituteLoopVerdict::CleanMiss
        );

        let mut just_429 = SubstituteLoopCells::new();
        assert_eq!(
            just_429.record(
                SubstituteFailureClass::RateLimited,
                Some(Duration::from_secs(7))
            ),
            LoopControl::Continue
        );
        assert_eq!(
            fold_substitute_loop(just_429),
            SubstituteLoopVerdict::RateLimited {
                retry_after: Some(Duration::from_secs(7))
            }
        );

        let mut just_stall = SubstituteLoopCells::new();
        let _ = just_stall.record(
            SubstituteFailureClass::Stalled,
            Some(Duration::from_secs(180)),
        );
        assert_eq!(
            fold_substitute_loop(just_stall),
            SubstituteLoopVerdict::Stalled {
                window: Duration::from_secs(180)
            }
        );

        // merged_bug_044: the all-errored iteration is NOT a clean
        // miss — one recorded error flips the verdict off the
        // cacheable lane.
        let mut just_err = SubstituteLoopCells::new();
        let _ = just_err.record(SubstituteFailureClass::Fetch, None);
        assert_eq!(
            fold_substitute_loop(just_err),
            SubstituteLoopVerdict::Errored
        );

        // Full precedence: stall > 429 > errored.
        let mut all = SubstituteLoopCells::new();
        let _ = all.record(SubstituteFailureClass::Ingest, None);
        let _ = all.record(SubstituteFailureClass::RateLimited, None);
        let _ = all.record(
            SubstituteFailureClass::Stalled,
            Some(Duration::from_secs(180)),
        );
        assert_eq!(
            fold_substitute_loop(all),
            SubstituteLoopVerdict::Stalled {
                window: Duration::from_secs(180)
            }
        );

        // Raced aborts and records nothing.
        let mut raced = SubstituteLoopCells::new();
        assert_eq!(
            raced.record(SubstituteFailureClass::Raced, None),
            LoopControl::AbortRaced
        );
        assert_eq!(
            fold_substitute_loop(raced),
            SubstituteLoopVerdict::CleanMiss
        );

        // Max semantics across upstreams on both advice channels.
        let mut maxed = SubstituteLoopCells::new();
        let _ = maxed.record(
            SubstituteFailureClass::RateLimited,
            Some(Duration::from_secs(3)),
        );
        let _ = maxed.record(
            SubstituteFailureClass::RateLimited,
            Some(Duration::from_secs(9)),
        );
        // A bare 429 after an advised one must not erase the advice.
        let _ = maxed.record(SubstituteFailureClass::RateLimited, None);
        assert_eq!(
            fold_substitute_loop(maxed),
            SubstituteLoopVerdict::RateLimited {
                retry_after: Some(Duration::from_secs(9))
            }
        );
    }

    /// The full table, row by row (merged_bug_178's enumeration pin).
    #[test]
    fn classification_table() {
        use FailureDisposition::*;
        use SubstituteFailureClass::*;
        for (class, want) in [
            (Raced, RetryUncharged),
            (RateLimited, RetryUncharged),
            (Stalled, ChargeInfra),
            (AdmissionSaturated, ChargeInfra),
            (Fetch, ChargeInfra),
            (Integrity, ChargeInfra),
            (Ingest, ChargeInfra),
        ] {
            assert_eq!(classify_substitute_failure(class), want, "{class:?}");
        }
    }

    /// merged_bug_028: ConfirmedMissing is the ALL-tenant conjunction;
    /// empty never confirms.
    #[test]
    fn reprobe_fold_is_all_tenant_conjunction() {
        use ReprobeAnswer::*;
        assert_eq!(fold_tenant_reprobes(&[]), Obtainable);
        assert_eq!(fold_tenant_reprobes(&[Obtainable]), Obtainable);
        assert_eq!(fold_tenant_reprobes(&[ConfirmedMissing]), ConfirmedMissing);
        assert_eq!(
            fold_tenant_reprobes(&[ConfirmedMissing, Obtainable]),
            Obtainable
        );
        assert_eq!(
            fold_tenant_reprobes(&[ConfirmedMissing, ConfirmedMissing]),
            ConfirmedMissing
        );
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;
    use crate::routing::ReprobeAnswer;

    /// merged_bug_178: the classification table, swept — transient by
    /// the substituter's own contract (Raced/RateLimited) closes
    /// uncharged; everything else (including the two deliberate
    /// scope-narrowings, Stalled and AdmissionSaturated) charges. Total
    /// over the class alphabet by construction (no catch-all).
    #[kani::proof]
    fn check_substitute_failure_truth_table() {
        let class = match kani::any::<u8>() {
            0 => SubstituteFailureClass::Raced,
            1 => SubstituteFailureClass::RateLimited,
            2 => SubstituteFailureClass::Stalled,
            3 => SubstituteFailureClass::AdmissionSaturated,
            4 => SubstituteFailureClass::Fetch,
            5 => SubstituteFailureClass::Integrity,
            _ => SubstituteFailureClass::Ingest,
        };
        let disposition = classify_substitute_failure(class);
        let transient = matches!(
            class,
            SubstituteFailureClass::Raced | SubstituteFailureClass::RateLimited
        );
        assert_eq!(disposition == FailureDisposition::RetryUncharged, transient);
    }

    /// K1 (merged_bug_044): the loop cells are total over the class
    /// alphabet and the fold's precedence is `Stalled > RateLimited >
    /// Errored > CleanMiss`. Swept over every 2-record class sequence
    /// with symbolic advice: record routing (Raced aborts and writes
    /// nothing; Stalled/RateLimited keep MAX advice; the four error
    /// classes set the error cell), then the fold verdict is checked
    /// against an independent recomputation from the recorded
    /// sequence. The `(no stall, no 429, errored)` cell — the one the
    /// pre-fix 2-axis fold could not represent — is reachable here
    /// and must yield `Errored`, never `CleanMiss`.
    #[kani::proof]
    fn check_substitute_loop_cells_total() {
        let pick = |sel: u8| match sel {
            0 => SubstituteFailureClass::Raced,
            1 => SubstituteFailureClass::RateLimited,
            2 => SubstituteFailureClass::Stalled,
            3 => SubstituteFailureClass::AdmissionSaturated,
            4 => SubstituteFailureClass::Fetch,
            5 => SubstituteFailureClass::Integrity,
            _ => SubstituteFailureClass::Ingest,
        };
        let advice_of = |has: bool, secs: u32| {
            if has {
                Some(core::time::Duration::from_secs(secs as u64))
            } else {
                None
            }
        };
        let classes = [pick(kani::any()), pick(kani::any())];
        let advices = [
            advice_of(kani::any(), kani::any()),
            advice_of(kani::any(), kani::any()),
        ];

        let mut cells = SubstituteLoopCells::new();
        // Independent shadow of what record() must accumulate.
        let mut want_stall: Option<core::time::Duration> = None;
        let mut want_429: Option<Option<core::time::Duration>> = None;
        let mut want_err = false;
        let mut aborted = false;
        let mut i = 0;
        while i < classes.len() {
            let control = cells.record(classes[i], advices[i]);
            match classes[i] {
                SubstituteFailureClass::Raced => {
                    assert_eq!(control, LoopControl::AbortRaced);
                    aborted = true;
                    // The real loop returns here; later records don't
                    // happen. Stop the shadow too.
                    break;
                }
                SubstituteFailureClass::Stalled => {
                    assert_eq!(control, LoopControl::Continue);
                    let w = advices[i].unwrap_or_default();
                    want_stall = Some(match want_stall {
                        Some(prev) => {
                            if prev > w {
                                prev
                            } else {
                                w
                            }
                        }
                        None => w,
                    });
                }
                SubstituteFailureClass::RateLimited => {
                    assert_eq!(control, LoopControl::Continue);
                    want_429 = Some(match (want_429.and_then(|x| x), advices[i]) {
                        (Some(prev), Some(ra)) => Some(if prev > ra { prev } else { ra }),
                        (Some(prev), None) => Some(prev),
                        (None, ra) => ra,
                    });
                }
                SubstituteFailureClass::AdmissionSaturated
                | SubstituteFailureClass::Fetch
                | SubstituteFailureClass::Integrity
                | SubstituteFailureClass::Ingest => {
                    assert_eq!(control, LoopControl::Continue);
                    want_err = true;
                }
            }
            i += 1;
        }
        let _ = aborted;

        let verdict = fold_substitute_loop(cells);
        match (want_stall, want_429, want_err) {
            (Some(w), _, _) => assert_eq!(verdict, SubstituteLoopVerdict::Stalled { window: w }),
            (None, Some(ra), _) => {
                assert_eq!(
                    verdict,
                    SubstituteLoopVerdict::RateLimited { retry_after: ra }
                )
            }
            (None, None, true) => assert_eq!(verdict, SubstituteLoopVerdict::Errored),
            (None, None, false) => assert_eq!(verdict, SubstituteLoopVerdict::CleanMiss),
        }
    }

    /// K1 falsify twin (merged_bug_044): the pre-fix 2-axis
    /// projection — drop the error cell, fold only `(any_stall,
    /// any_429)` — CANNOT agree with the 3-axis fold. The witness
    /// cell is exactly `(None, None, errored)`: the projection says
    /// `CleanMiss` (cacheable), the fold says `Errored` (uncached).
    /// `should_panic` pins that the disagreement is REAL — if a
    /// refactor ever collapses `Errored` back into `CleanMiss`, this
    /// twin stops panicking and the harness count check flags it.
    #[kani::proof]
    #[kani::should_panic]
    fn check_substitute_cells_errored_axis_required() {
        let mut cells = SubstituteLoopCells::new();
        let _ = cells.record(SubstituteFailureClass::Fetch, None);
        // The old projection had no error axis: (None, None) → CleanMiss.
        let projected = SubstituteLoopVerdict::CleanMiss;
        assert_eq!(fold_substitute_loop(cells), projected);
    }

    /// merged_bug_028 / owner Q2: `ConfirmedMissing` is the ALL-tenant
    /// conjunction over a NON-EMPTY answer set — one obtainable tenant
    /// view (or an empty set) keeps the job armed. Swept over every
    /// answer vector up to four tenants (bitmask representation; stack
    /// array — no heap under CBMC).
    #[kani::proof]
    fn check_confirmed_missing_is_all_tenant_conjunction() {
        let mask: u8 = kani::any();
        let mk = |i: usize| {
            if mask & (1 << i) != 0 {
                ReprobeAnswer::ConfirmedMissing
            } else {
                ReprobeAnswer::Obtainable
            }
        };
        let arr = [mk(0), mk(1), mk(2), mk(3)];
        // One concrete-length call per width (0..=4): symbolic slice
        // lengths make CBMC's fat-pointer + iterator reasoning blow up
        // (observed: the symbolic-len form ran past 19 CPU-minutes;
        // this form completes in seconds). The mask still sweeps every
        // answer VECTOR at each width — 2^4 × 5 widths = the full
        // bounded space.
        let confirmed = |i: usize| mask & (1 << i) != 0;
        assert_eq!(
            fold_tenant_reprobes(&arr[..0]) == ReprobeAnswer::ConfirmedMissing,
            false
        );
        assert_eq!(
            fold_tenant_reprobes(&arr[..1]) == ReprobeAnswer::ConfirmedMissing,
            confirmed(0)
        );
        assert_eq!(
            fold_tenant_reprobes(&arr[..2]) == ReprobeAnswer::ConfirmedMissing,
            confirmed(0) && confirmed(1)
        );
        assert_eq!(
            fold_tenant_reprobes(&arr[..3]) == ReprobeAnswer::ConfirmedMissing,
            confirmed(0) && confirmed(1) && confirmed(2)
        );
        assert_eq!(
            fold_tenant_reprobes(&arr[..4]) == ReprobeAnswer::ConfirmedMissing,
            confirmed(0) && confirmed(1) && confirmed(2) && confirmed(3)
        );
    }
}
