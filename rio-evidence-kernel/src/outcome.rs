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

/// One (tenant, path) cell of the arm-3 re-probe (bug_299): the RAW
/// membership of one live-wanted path in one tenant's
/// FindMissingPaths answer. The caller's only job is this mechanical
/// mapping — no quantifier decision is expressible caller-side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathProbeCell {
    /// Not in `missing_paths`: present in the store under this
    /// tenant's visibility.
    Present,
    /// Missing locally but a HEAD probe hit an upstream.
    Substitutable,
    /// Missing locally and the probe could not classify (429/5xx/
    /// timeout/deadline). Conservatively obtainable for THIS path
    /// under THIS tenant.
    Indeterminate,
    /// Missing locally, no upstream hit, definitively classified.
    Missing,
}

/// One tenant's re-probe row: per-path cells (caller path order) +
/// whether this tenant's probe could confirm at all (a probe issued
/// without tenant identity cannot run upstream substitution checks,
/// so its "missing" is meaningless).
#[derive(Debug, Clone, Copy)]
pub struct TenantPathAnswers<'a> {
    /// Per-path cells, index-aligned with the caller's live-wanted
    /// path list.
    pub cells: &'a [PathProbeCell],
    /// Whether this tenant's answer can confirm absence.
    pub can_confirm: bool,
}

/// The per-(tenant, path) settlement re-probe fold (bughunt wave A4,
/// merged_bug_028 / owner Q2; re-granulated per-path by bug_299):
/// `ConfirmedMissing` — the verdict that can fail-fast a pruned root
/// or settle a leaf from-source — requires a NON-EMPTY tenant set in
/// which EVERY tenant can confirm, and SOME path that is `Missing`
/// under EVERY tenant. The quantifier order is the point: ∃ path ∀
/// tenant, never ∀ tenant ∃ path. The pre-fix fold consumed one
/// pre-projected boolean per tenant (each tenant's "∃ path missing
/// under me"), so complementary coverage — tenant A has X and lacks
/// Y, tenant B has Y and lacks X — folded to `ConfirmedMissing` and
/// fail-fasted a job every path of which was obtainable under SOME
/// tenant.
///
/// Conservative rows: an EMPTY tenant set folds to `Obtainable`
/// (nothing was confirmed; the caller's no-live-tenant shape has its
/// own arms); ANY tenant that cannot confirm poisons the conjunction
/// to `Obtainable`; a ragged matrix (caller bug — rows of different
/// lengths) folds to `Obtainable`, never to a fail-fast. RPC
/// failures never reach this fold — the caller maps them to its B3
/// ReArm before folding (same posture as `route_unobtainable`'s
/// `None` reprobe).
// r[impl sched.materialize.routing+5]
// r[impl sched.materialize.reprobe-per-path]
pub fn fold_path_reprobes(tenants: &[TenantPathAnswers<'_>]) -> crate::routing::ReprobeAnswer {
    use crate::routing::ReprobeAnswer;
    if tenants.is_empty() {
        return ReprobeAnswer::Obtainable;
    }
    let mut i = 0;
    while i < tenants.len() {
        if !tenants[i].can_confirm || tenants[i].cells.len() != tenants[0].cells.len() {
            return ReprobeAnswer::Obtainable;
        }
        i += 1;
    }
    let n_paths = tenants[0].cells.len();
    let mut p = 0;
    while p < n_paths {
        let mut missing_everywhere = true;
        let mut t = 0;
        while t < tenants.len() {
            if !matches!(tenants[t].cells[p], PathProbeCell::Missing) {
                missing_everywhere = false;
                break;
            }
            t += 1;
        }
        if missing_everywhere {
            return ReprobeAnswer::ConfirmedMissing;
        }
        p += 1;
    }
    ReprobeAnswer::Obtainable
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

/// One consulted tenant's recorded attempt evidence (merged_bug_133,
/// mirroring [`SubstituteLoopCells`] one level up: per-tenant instead
/// of per-upstream). The executor's tenant loops record ONLY through
/// [`TenantAttemptCells::record_failure`] (merged_bug_188) — a hit
/// breaks, everything else is recorded and folded after every
/// consulted tenant, and a `Raced` verdict aborts the loop (the
/// placeholder slot is path-keyed, so remaining tenants would race
/// the same held slot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TenantAttemptEvidence {
    /// The tenant's attempt failed with a charging class
    /// (stall/capacity/fetch/integrity/ingest) — the charge ladder
    /// must see it if nothing serves.
    Charge {
        /// The charging class, for the caller's detail message.
        class: SubstituteFailureClass,
    },
    /// Transient by the substituter's own contract (raced / 429) —
    /// an uncharged retry may serve.
    Transient {
        /// Parsed `Retry-After` advice, if any.
        retry_after: Option<core::time::Duration>,
    },
    /// Clean miss under this tenant's upstream view (every upstream
    /// answered hit-or-404).
    CleanMiss,
}

/// merged_bug_188: the tenant axis's recording chokepoint, mirroring
/// [`SubstituteLoopCells::record`] one level up. The tenant loop's
/// failure recording routes through [`Self::record_failure`], whose
/// [`LoopControl`] return is `#[must_use]` — dropping `AbortRaced` at
/// the tenant axis is as unwritable as it already is at the upstream
/// axis. `Raced` IS recorded as a `Transient` cell BEFORE aborting,
/// so the uncharged deferral survives the fold: the placeholder slot
/// is path-keyed (tenant-independent), consulting further tenants
/// burns doomed attempts against the held slot, and a sibling
/// tenant's pre-claim charging failure would otherwise convert the
/// uncharged race into a job-fatal charge via the fold's
/// Charge-dominates precedence.
#[derive(Debug, Clone, Default)]
pub struct TenantAttemptCells {
    cells: Vec<TenantAttemptEvidence>,
}

impl TenantAttemptCells {
    /// Empty cells: no tenant consulted yet.
    pub const fn new() -> Self {
        Self { cells: Vec::new() }
    }

    /// Record a clean miss under one tenant (every upstream answered
    /// hit-or-404).
    pub fn record_clean_miss(&mut self) {
        self.cells.push(TenantAttemptEvidence::CleanMiss);
    }

    /// THE failure-recording chokepoint — total over the class
    /// alphabet via [`classify_substitute_failure`] (no catch-all: a
    /// new class fails the kernel truth table, not this routing).
    pub fn record_failure(
        &mut self,
        class: SubstituteFailureClass,
        retry_after: Option<core::time::Duration>,
    ) -> LoopControl {
        match classify_substitute_failure(class) {
            FailureDisposition::RetryUncharged => {
                self.cells
                    .push(TenantAttemptEvidence::Transient { retry_after });
                if matches!(class, SubstituteFailureClass::Raced) {
                    return LoopControl::AbortRaced;
                }
            }
            FailureDisposition::ChargeInfra => {
                self.cells.push(TenantAttemptEvidence::Charge { class });
            }
        }
        LoopControl::Continue
    }

    /// The recorded cells, index-aligned with the caller's per-cell
    /// detail messages (one cell per consulted tenant).
    pub fn cells(&self) -> &[TenantAttemptEvidence] {
        &self.cells
    }

    /// Number of recorded cells.
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// No cells recorded.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Fold the recorded cells ([`fold_tenant_attempts`]).
    pub fn fold(&self) -> TenantAttemptsVerdict {
        fold_tenant_attempts(&self.cells)
    }
}

/// The post-loop verdict over every consulted tenant's evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TenantAttemptsVerdict {
    /// ≥1 tenant produced charging evidence and none served. `idx` is
    /// the FIRST charging cell (the caller's detail message names it).
    ChargeInfra {
        /// Index of the first `Charge` cell.
        idx: usize,
    },
    /// No charging evidence; ≥1 transient. The job closes UNCHARGED
    /// and defers — a 429/raced wave must never park a healthy job.
    RetryTransient {
        /// Index of the transient cell with the LARGEST advice (ties
        /// → first), so the caller's label/detail match `max`.
        idx: usize,
        /// Largest `Retry-After` advice across the transient cells.
        max: Option<core::time::Duration>,
    },
    /// Every consulted tenant cleanly missed (vacuously true for an
    /// empty set — the caller's no-tenant shape has its own arms).
    AllCleanMiss,
}

/// merged_bug_133's pure post-loop fold: ALL failure dispositions
/// exit at the fold, after every tenant has been consulted —
/// restoring owner-Q2 ("a job fails only when NO interested tenant
/// can obtain"); the deterministic resolve order can no longer
/// starve later tenants of their chance to serve. Precedence
/// `ChargeInfra > RetryTransient > AllCleanMiss`: charging evidence
/// (recorded strikes, capacity, integrity) outranks back-off advice
/// (matching [`fold_substitute_loop`]'s ordering one level down),
/// and the miss lane requires a clean miss under EVERY tenant.
// r[impl store.materialize.tenant-fold+2]
pub fn fold_tenant_attempts(cells: &[TenantAttemptEvidence]) -> TenantAttemptsVerdict {
    let mut first_charge: Option<usize> = None;
    let mut best_transient: Option<(usize, Option<core::time::Duration>)> = None;
    let mut i = 0;
    while i < cells.len() {
        match cells[i] {
            TenantAttemptEvidence::Charge { .. } => {
                if first_charge.is_none() {
                    first_charge = Some(i);
                }
            }
            TenantAttemptEvidence::Transient { retry_after } => {
                best_transient = Some(match best_transient {
                    Some((pi, prev)) => {
                        // Keep the cell with the larger advice; a
                        // bare transient never displaces an advised
                        // one, ties keep the earlier cell.
                        match (prev, retry_after) {
                            (None, Some(_)) => (i, retry_after),
                            (Some(p), Some(r)) if r > p => (i, retry_after),
                            _ => (pi, prev),
                        }
                    }
                    None => (i, retry_after),
                });
            }
            TenantAttemptEvidence::CleanMiss => {}
        }
        i += 1;
    }
    match (first_charge, best_transient) {
        (Some(idx), _) => TenantAttemptsVerdict::ChargeInfra { idx },
        (None, Some((idx, max))) => TenantAttemptsVerdict::RetryTransient { idx, max },
        (None, None) => TenantAttemptsVerdict::AllCleanMiss,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::ReprobeAnswer;

    /// merged_bug_188: `TenantAttemptCells` routing — `Raced` records
    /// the uncharged Transient cell FIRST and then aborts the tenant
    /// axis; `RateLimited` records and continues; charging classes
    /// record and continue; an already-recorded charge still
    /// dominates the fold (the abort prevents only FUTURE doomed
    /// consults, never erases evidence).
    #[test]
    fn tenant_cells_record_routing() {
        use core::time::Duration;
        let mut cells = TenantAttemptCells::new();
        assert_eq!(
            cells.record_failure(
                SubstituteFailureClass::RateLimited,
                Some(Duration::from_secs(7))
            ),
            LoopControl::Continue
        );
        assert_eq!(
            cells.record_failure(SubstituteFailureClass::Fetch, None),
            LoopControl::Continue
        );
        assert_eq!(cells.len(), 2);

        let mut raced = TenantAttemptCells::new();
        assert_eq!(
            raced.record_failure(SubstituteFailureClass::Raced, None),
            LoopControl::AbortRaced
        );
        assert_eq!(
            raced.cells(),
            &[TenantAttemptEvidence::Transient { retry_after: None }],
            "Raced records the uncharged cell BEFORE aborting"
        );
        assert!(
            matches!(raced.fold(), TenantAttemptsVerdict::RetryTransient { .. }),
            "the aborted sweep folds to the uncharged deferral"
        );

        let mut pre = TenantAttemptCells::new();
        let _ = pre.record_failure(SubstituteFailureClass::Fetch, None);
        let _ = pre.record_failure(SubstituteFailureClass::Raced, None);
        assert!(
            matches!(pre.fold(), TenantAttemptsVerdict::ChargeInfra { .. }),
            "an already-recorded charge still dominates the fold"
        );
    }

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

    // r[verify store.materialize.tenant-fold+2]
    /// merged_bug_133: the tenant fold's precedence (charge >
    /// transient > all-clean-miss), idx selection, and max-advice
    /// semantics.
    #[test]
    fn tenant_attempts_fold_precedence_and_advice() {
        use TenantAttemptEvidence as E;
        use TenantAttemptsVerdict as V;
        use core::time::Duration;

        assert_eq!(fold_tenant_attempts(&[]), V::AllCleanMiss);
        assert_eq!(
            fold_tenant_attempts(&[E::CleanMiss, E::CleanMiss]),
            V::AllCleanMiss
        );
        // Charge outranks transient regardless of order; idx = FIRST
        // charge.
        assert_eq!(
            fold_tenant_attempts(&[
                E::Transient { retry_after: None },
                E::Charge {
                    class: SubstituteFailureClass::Stalled
                },
                E::Charge {
                    class: SubstituteFailureClass::Fetch
                },
            ]),
            V::ChargeInfra { idx: 1 }
        );
        // Transient lane: idx rides the LARGEST advice; a bare
        // transient never displaces an advised one; max is the
        // largest advice.
        assert_eq!(
            fold_tenant_attempts(&[
                E::Transient {
                    retry_after: Some(Duration::from_secs(3))
                },
                E::CleanMiss,
                E::Transient {
                    retry_after: Some(Duration::from_secs(9))
                },
                E::Transient { retry_after: None },
            ]),
            V::RetryTransient {
                idx: 2,
                max: Some(Duration::from_secs(9))
            }
        );
        assert_eq!(
            fold_tenant_attempts(&[E::Transient { retry_after: None }, E::CleanMiss]),
            V::RetryTransient { idx: 0, max: None }
        );
    }

    // r[verify sched.materialize.reprobe-per-path]
    /// bug_299 red: complementary coverage — A has X (Y missing under
    /// A), B has Y (X missing under B). Every wanted path is
    /// obtainable under SOME tenant, so the job must stay armed.
    /// Recorded red (pre-fix, via the deleted pre-projected-boolean
    /// fold whose input domain had already lost the path axis):
    /// the per-tenant fold over `[ConfirmedMissing, ConfirmedMissing]`
    /// → `left: ConfirmedMissing` where Obtainable required.
    #[test]
    fn complementary_coverage_is_obtainable() {
        use PathProbeCell::*;
        use ReprobeAnswer::*;
        // Paths [X, Y]. A: X present, Y missing. B: X missing, Y present.
        let a = [Present, Missing];
        let b = [Missing, Present];
        assert_eq!(
            fold_path_reprobes(&[
                TenantPathAnswers {
                    cells: &a,
                    can_confirm: true
                },
                TenantPathAnswers {
                    cells: &b,
                    can_confirm: true
                },
            ]),
            Obtainable,
            "complementary coverage must keep the job armed"
        );
    }

    // r[verify sched.materialize.reprobe-per-path]
    /// merged_bug_028 + bug_299: ConfirmedMissing requires ∃ path
    /// missing under EVERY tenant, over a non-empty all-confirming
    /// set; every conservative row folds Obtainable.
    #[test]
    fn reprobe_fold_quantifier_order_and_conservative_rows() {
        use PathProbeCell::*;
        use ReprobeAnswer::*;
        let row = |cells: &'static [PathProbeCell], can_confirm: bool| TenantPathAnswers {
            cells,
            can_confirm,
        };
        // Empty tenant set never confirms.
        assert_eq!(fold_path_reprobes(&[]), Obtainable);
        // Same path missing under EVERY tenant → confirmed.
        assert_eq!(
            fold_path_reprobes(&[
                row(&[Missing, Present], true),
                row(&[Missing, Present], true)
            ]),
            ConfirmedMissing
        );
        // One tenant sees it substitutable → armed.
        assert_eq!(
            fold_path_reprobes(&[
                row(&[Missing, Present], true),
                row(&[Substitutable, Present], true)
            ]),
            Obtainable
        );
        // Indeterminate under one tenant blocks confirmation of that
        // path (conservative per cell).
        assert_eq!(
            fold_path_reprobes(&[row(&[Missing], true), row(&[Indeterminate], true)]),
            Obtainable
        );
        // A tenant that cannot confirm poisons the conjunction.
        assert_eq!(
            fold_path_reprobes(&[row(&[Missing], true), row(&[Missing], false)]),
            Obtainable
        );
        // Single confirming tenant, single missing path → confirmed.
        assert_eq!(
            fold_path_reprobes(&[row(&[Missing], true)]),
            ConfirmedMissing
        );
        // Ragged matrix (caller bug) folds conservative.
        assert_eq!(
            fold_path_reprobes(&[row(&[Missing, Missing], true), row(&[Missing], true)]),
            Obtainable
        );
        // Zero paths → nothing can be missing.
        assert_eq!(fold_path_reprobes(&[row(&[], true)]), Obtainable);
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

    /// K2 (merged_bug_133): `fold_tenant_attempts` charge-precedence
    /// and permutation invariance, swept over every 3-cell vector
    /// with symbolic transient advice. Permutation invariance is
    /// proven via an arbitrary transposition: transpositions generate
    /// the symmetric group, and the cell contents are universally
    /// quantified, so swap-invariance at every (i, j) implies
    /// invariance under every permutation. The verdict CLASS and the
    /// max advice are the invariants (`idx` deliberately is not — it
    /// names a cell in the given order for the caller's message).
    #[kani::proof]
    fn check_fold_tenant_attempts_permutation_and_precedence() {
        let mk = |sel: u8, has_advice: bool, secs: u8| match sel % 3 {
            0 => TenantAttemptEvidence::Charge {
                // The verdict is class-blind by construction; one
                // charging class stands for all (the class only rides
                // into the caller's detail message).
                class: SubstituteFailureClass::Fetch,
            },
            1 => TenantAttemptEvidence::Transient {
                retry_after: if has_advice {
                    Some(core::time::Duration::from_secs(secs as u64))
                } else {
                    None
                },
            },
            _ => TenantAttemptEvidence::CleanMiss,
        };
        let a = [
            mk(kani::any(), kani::any(), kani::any()),
            mk(kani::any(), kani::any(), kani::any()),
            mk(kani::any(), kani::any(), kani::any()),
        ];
        let i: usize = kani::any();
        let j: usize = kani::any();
        kani::assume(i < 3 && j < 3);
        let mut b = a;
        b.swap(i, j);

        let va = fold_tenant_attempts(&a);
        let vb = fold_tenant_attempts(&b);
        match (va, vb) {
            (
                TenantAttemptsVerdict::ChargeInfra { .. },
                TenantAttemptsVerdict::ChargeInfra { .. },
            ) => {}
            (
                TenantAttemptsVerdict::RetryTransient { max: ma, .. },
                TenantAttemptsVerdict::RetryTransient { max: mb, .. },
            ) => assert_eq!(ma, mb),
            (TenantAttemptsVerdict::AllCleanMiss, TenantAttemptsVerdict::AllCleanMiss) => {}
            _ => panic!("verdict class must be permutation-invariant"),
        }

        // Charge-precedence + lane totality against an independent
        // scan of the cells.
        let mut any_charge = false;
        let mut any_transient = false;
        let mut k = 0;
        while k < a.len() {
            match a[k] {
                TenantAttemptEvidence::Charge { .. } => any_charge = true,
                TenantAttemptEvidence::Transient { .. } => any_transient = true,
                TenantAttemptEvidence::CleanMiss => {}
            }
            k += 1;
        }
        match va {
            TenantAttemptsVerdict::ChargeInfra { idx } => {
                assert!(any_charge);
                assert!(matches!(a[idx], TenantAttemptEvidence::Charge { .. }));
            }
            TenantAttemptsVerdict::RetryTransient { idx, .. } => {
                assert!(!any_charge && any_transient);
                assert!(matches!(a[idx], TenantAttemptEvidence::Transient { .. }));
            }
            TenantAttemptsVerdict::AllCleanMiss => assert!(!any_charge && !any_transient),
        }
    }

    /// K3 (bug_299, superseding merged_bug_028's pre-projected
    /// harness): the per-(tenant, path) fold's quantifier order over
    /// the FULL 3×3 cell space (each cell one of 4 variants, 2 bits —
    /// 2¹⁸ matrices × confirm bits), checked against an independent
    /// recomputation of `∃ path ∀ tenant Missing` at every width
    /// (1..=3 tenants; concrete-length calls — symbolic slice lengths
    /// blow up CBMC's fat-pointer reasoning, observed 19+ CPU-min).
    /// `kani::cover` pins that the DISAGREEMENT matrix vs the old
    /// per-tenant projection (`∀ tenant ∃ path`) is reachable — the
    /// complementary-coverage witness class the old fold fail-fasted.
    #[kani::proof]
    fn check_reprobe_quantifier_per_path() {
        let cell = |bits: u8| match bits & 0b11 {
            0 => PathProbeCell::Present,
            1 => PathProbeCell::Substitutable,
            2 => PathProbeCell::Indeterminate,
            _ => PathProbeCell::Missing,
        };
        let m: [[PathProbeCell; 3]; 3] = [
            [cell(kani::any()), cell(kani::any()), cell(kani::any())],
            [cell(kani::any()), cell(kani::any()), cell(kani::any())],
            [cell(kani::any()), cell(kani::any()), cell(kani::any())],
        ];
        let confirm: [bool; 3] = [kani::any(), kani::any(), kani::any()];

        // Width-3 fold (all tenants).
        let rows = [
            TenantPathAnswers {
                cells: &m[0],
                can_confirm: confirm[0],
            },
            TenantPathAnswers {
                cells: &m[1],
                can_confirm: confirm[1],
            },
            TenantPathAnswers {
                cells: &m[2],
                can_confirm: confirm[2],
            },
        ];
        let missing = |t: usize, p: usize| matches!(m[t][p], PathProbeCell::Missing);

        // Independent recomputation per width.
        let mut w = 1;
        while w <= 3 {
            let verdict = fold_path_reprobes(&rows[..w]);
            let mut all_confirm = true;
            let mut t = 0;
            while t < w {
                if !confirm[t] {
                    all_confirm = false;
                }
                t += 1;
            }
            let mut exists_path_forall_tenant = false;
            let mut p = 0;
            while p < 3 {
                let mut everywhere = true;
                let mut t2 = 0;
                while t2 < w {
                    if !missing(t2, p) {
                        everywhere = false;
                    }
                    t2 += 1;
                }
                if everywhere {
                    exists_path_forall_tenant = true;
                }
                p += 1;
            }
            let want_confirmed = all_confirm && exists_path_forall_tenant;
            assert_eq!(
                verdict == ReprobeAnswer::ConfirmedMissing,
                want_confirmed,
                "∃ path ∀ tenant, over an all-confirming non-empty set"
            );
            w += 1;
        }
        // Empty set never confirms.
        assert!(fold_path_reprobes(&rows[..0]) == ReprobeAnswer::Obtainable);

        // The disagreement witness vs the OLD projection (∀ tenant ∃
        // path missing) is REACHABLE at width 2 with both confirming:
        // old says confirmed, new says obtainable — complementary
        // coverage.
        let mut forall_tenant_exists_path = true;
        let mut t3 = 0;
        while t3 < 2 {
            let mut any = false;
            let mut p3 = 0;
            while p3 < 3 {
                if missing(t3, p3) {
                    any = true;
                }
                p3 += 1;
            }
            if !any {
                forall_tenant_exists_path = false;
            }
            t3 += 1;
        }
        let mut exists_forall_2 = false;
        let mut p4 = 0;
        while p4 < 3 {
            if missing(0, p4) && missing(1, p4) {
                exists_forall_2 = true;
            }
            p4 += 1;
        }
        kani::cover!(
            confirm[0] && confirm[1] && forall_tenant_exists_path && !exists_forall_2,
            "complementary-coverage disagreement matrix reachable"
        );
    }
}
