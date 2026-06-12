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
//! Scope narrowing (a recorded campaign decision): `Stalled` and
//! `AdmissionSaturated` STAY `ChargeInfra` by
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
    /// merged_bug_005: narinfo PRESENT and identity-correct, but no
    /// signature verified against the tenant's `trusted_keys` for the
    /// serving upstream. Deterministic until keys or content change —
    /// neither a miss (the path IS present) nor infrastructure
    /// trouble (nothing is broken on our side): a typed trust
    /// refusal, settled uncharged.
    Untrusted,
    /// merged_bug_046: the upstream's narinfo names the path but
    /// claims DIFFERENT bytes than the locally-stored row (nar_hash /
    /// nar_size / reference-set disagreement at the AlreadyComplete
    /// dedup arm). Deterministic stored-row/upstream disagreement —
    /// this upstream does not have *this* path's bytes: never a miss
    /// (folding it to the cacheable CleanMiss re-opened the
    /// merged_bug_005 laundering loop one axis over — the sig/content-
    /// blind HEAD confirmation re-found the path and charged
    /// infrastructure every retry until park), never infrastructure
    /// (nothing is broken on our side). Input-addressed paths from
    /// non-reproducible rebuilds hit this legitimately.
    ContentMismatch,
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
    /// merged_bug_005: present-but-untrusted is a typed trust refusal
    /// — settle toward `Unobtainable` with a trust cause, UNCHARGED
    /// (the park budget must never see a deterministic key-rotation
    /// refusal), and never fold it into the cacheable miss lane.
    TrustRefusal,
    /// merged_bug_046: stored-row/upstream content disagreement is a
    /// typed CONTENT refusal — settle toward `Unobtainable` with a
    /// content cause, UNCHARGED, skipping the HEAD confirmation (the
    /// path IS present upstream; a content-blind HEAD 200 proves
    /// nothing about agreement and pre-fix converted this exact state
    /// into a per-retry infrastructure charge). Distinct from
    /// [`Self::TrustRefusal`] so the settle cause names the actual
    /// disagreement instead of a key problem.
    ContentRefusal,
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
        SubstituteFailureClass::Untrusted => FailureDisposition::TrustRefusal,
        SubstituteFailureClass::ContentMismatch => FailureDisposition::ContentRefusal,
    }
}

/// The precedence TIER of a disposition — the single executable
/// precedence source BOTH evidence folds consume (merged_bug_210,
/// bughunt-4): charge-grade evidence (0) outranks back-off advice
/// (1), which outranks the deterministic refusals (2, 3); absence
/// (the clean miss) ranks last by construction (no disposition). The
/// folds' verdict orderings are pinned to this table by the
/// tier-monotonicity tests and the kani sweep — a fold ranking a
/// back-off class above a charge class cannot survive either.
// r[impl store.materialize.executor+5]
pub const fn disposition_tier(d: FailureDisposition) -> u8 {
    match d {
        FailureDisposition::ChargeInfra => 0,
        FailureDisposition::RetryUncharged => 1,
        FailureDisposition::TrustRefusal => 2,
        FailureDisposition::ContentRefusal => 3,
    }
}

/// The class alphabet's cardinality, structurally tied to the enum by
/// [`class_index`] (no catch-all there): adding a variant breaks
/// `class_index` at compile time, which forces this count — and every
/// kani pick table built from it — to follow. The proofs' "swept over
/// the entire alphabet" claim is machine-witnessed by this pair, not
/// by reviewer memory (merged_bug_046 sweep duty; the round-3
/// pick-table lesson).
pub const SUBSTITUTE_FAILURE_CLASS_COUNT: u8 = 9;

/// Exhaustive class→index map — the compile-time witness behind
/// [`SUBSTITUTE_FAILURE_CLASS_COUNT`]. NO catch-all by design.
pub const fn class_index(class: SubstituteFailureClass) -> u8 {
    match class {
        SubstituteFailureClass::Raced => 0,
        SubstituteFailureClass::RateLimited => 1,
        SubstituteFailureClass::Stalled => 2,
        SubstituteFailureClass::AdmissionSaturated => 3,
        SubstituteFailureClass::Fetch => 4,
        SubstituteFailureClass::Integrity => 5,
        SubstituteFailureClass::Ingest => 6,
        SubstituteFailureClass::Untrusted => 7,
        SubstituteFailureClass::ContentMismatch => 8,
    }
}

/// Index→class, total over `0..SUBSTITUTE_FAILURE_CLASS_COUNT` (the
/// kani pick tables route through THIS instead of ad-hoc `_ =>`
/// catch-alls, so a new variant cannot be silently excluded from any
/// proof: `class_index` breaks the build first).
pub const fn class_of_index(sel: u8) -> SubstituteFailureClass {
    match sel % SUBSTITUTE_FAILURE_CLASS_COUNT {
        0 => SubstituteFailureClass::Raced,
        1 => SubstituteFailureClass::RateLimited,
        2 => SubstituteFailureClass::Stalled,
        3 => SubstituteFailureClass::AdmissionSaturated,
        4 => SubstituteFailureClass::Fetch,
        5 => SubstituteFailureClass::Integrity,
        6 => SubstituteFailureClass::Ingest,
        7 => SubstituteFailureClass::Untrusted,
        _ => SubstituteFailureClass::ContentMismatch,
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
// r[impl sched.materialize.routing+7]
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
    any_untrusted: bool,
    any_content_mismatch: bool,
}

impl SubstituteLoopCells {
    /// Empty cells: no failure observed yet.
    pub const fn new() -> Self {
        Self {
            any_stall: None,
            any_429: None,
            any_errored: false,
            any_untrusted: false,
            any_content_mismatch: false,
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
            SubstituteFailureClass::Untrusted => self.any_untrusted = true,
            SubstituteFailureClass::ContentMismatch => self.any_content_mismatch = true,
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
    /// merged_bug_005: ≥1 upstream answered present-but-untrusted
    /// (narinfo there, identity OK, no sig verified against
    /// `trusted_keys`) and none served, stalled, 429'd, or errored.
    /// NEVER folds to `CleanMiss` — "as good as 404" laundered a
    /// deterministic trust refusal into the cacheable miss lane,
    /// where the sig-blind HEAD confirmation re-found the path and
    /// charged infrastructure forever. The caller surfaces a typed,
    /// UNCACHED trust refusal instead.
    UntrustedPresent,
    /// merged_bug_046: ≥1 upstream's narinfo disagreed with the
    /// stored row's content at the AlreadyComplete dedup arm (and
    /// none served, stalled, 429'd, errored, or trust-refused). The
    /// same never-a-miss law as [`Self::UntrustedPresent`], one axis
    /// over: the caller surfaces a typed, UNCACHED content refusal so
    /// the content-blind HEAD confirmation can never contradict a
    /// cached CleanMiss into a per-retry infrastructure charge.
    ContentMismatch,
}

/// bug_081's pure post-loop fold over merged_bug_044's evidence
/// cells: a failure on ONE upstream is upstream-local — the loop
/// records it and fails over; only after every upstream has been
/// tried does the recorded evidence pick the attempt outcome. Total
/// over all five observation axes; precedence
/// `Stalled, Errored, RateLimited, Untrusted, ContentMismatch, CleanMiss`
/// in strictly decreasing rank, ordered by [`disposition_tier`] over
/// [`classify_substitute_failure`] (merged_bug_210): BOTH charge-grade
/// cells (`Stalled` — which additionally carries the window evidence —
/// then `Errored`) outrank the 429 back-off advice, which outranks
/// the deterministic refusals, which outrank a cacheable miss; the
/// trust refusal outranks the content refusal because a key rotation
/// is the likelier-repairable cause when both were observed. Pre-fix
/// the fold ranked `RateLimited` above `Errored` — one persistent 429
/// sibling hid ChargeInfra evidence behind uncharged deferrals
/// forever, the inversion the tier table now pins out.
// r[impl store.substitute.stall-abort+2]
// r[impl store.substitute.loop-evidence-total]
pub fn fold_substitute_loop(cells: SubstituteLoopCells) -> SubstituteLoopVerdict {
    match (
        cells.any_stall,
        cells.any_429,
        cells.any_errored,
        cells.any_untrusted,
        cells.any_content_mismatch,
    ) {
        (Some(window), _, _, _, _) => SubstituteLoopVerdict::Stalled { window },
        // merged_bug_210: charge tier before back-off tier — an
        // errored upstream is ChargeInfra-grade evidence; ranking the
        // 429 advice above it deferred uncharged forever. Within the
        // charge tier Stalled wins (it carries the window evidence).
        // An errored upstream still outranks the deterministic
        // refusals: the error is possibly transient (a retry may
        // serve through that upstream), while the refusals are
        // deterministic.
        (None, _, true, _, _) => SubstituteLoopVerdict::Errored,
        (None, Some(retry_after), false, _, _) => {
            SubstituteLoopVerdict::RateLimited { retry_after }
        }
        (None, None, false, true, _) => SubstituteLoopVerdict::UntrustedPresent,
        (None, None, false, false, true) => SubstituteLoopVerdict::ContentMismatch,
        (None, None, false, false, false) => SubstituteLoopVerdict::CleanMiss,
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
    /// merged_bug_005: this tenant's view found the path PRESENT on
    /// ≥1 upstream but no signature verified against that upstream's
    /// `trusted_keys` (and no upstream served, stalled, 429'd, or
    /// errored). A deterministic trust refusal — never a miss, never
    /// infrastructure.
    UntrustedPresent,
    /// merged_bug_046: this tenant's view hit the AlreadyComplete
    /// content disagreement on ≥1 upstream (and nothing outranked
    /// it). A deterministic content refusal — never a miss, never
    /// infrastructure.
    ContentMismatch,
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
            FailureDisposition::TrustRefusal => {
                // merged_bug_005: present-but-untrusted under this
                // tenant's view — its own cell; the fold settles it
                // toward Unobtainable-with-cause only if no tenant
                // serves, charges, or defers. The next tenant may
                // still trust the sigs, so the sweep continues.
                self.cells.push(TenantAttemptEvidence::UntrustedPresent);
            }
            FailureDisposition::ContentRefusal => {
                // merged_bug_046: stored-row/upstream disagreement
                // under this tenant's view — its own cell, same
                // sweep-continues posture (another tenant's upstream
                // may carry agreeing bytes).
                self.cells.push(TenantAttemptEvidence::ContentMismatch);
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
    /// merged_bug_005: no charge, no transient, but ≥1 tenant
    /// answered present-but-untrusted (rest clean-missed). The job
    /// settles **Unobtainable-with-cause, UNCHARGED** — the from-
    /// source settlement names the trust refusal instead of parking
    /// the job through the charge ladder, and the sig-blind HEAD
    /// confirmation is skipped (the path IS present; a HEAD 200
    /// proves nothing about trust).
    UntrustedPresent {
        /// Index of the first `UntrustedPresent` cell (the caller's
        /// cause message names that tenant's refusal).
        idx: usize,
    },
    /// merged_bug_046: no charge, no transient, no trust refusal,
    /// but ≥1 tenant hit the AlreadyComplete content disagreement
    /// (rest clean-missed). Settles **Unobtainable-with-cause,
    /// UNCHARGED**, skipping the HEAD confirmation (the path IS
    /// present upstream with disagreeing bytes; a content-blind HEAD
    /// 200 proves nothing).
    ContentMismatch {
        /// Index of the first `ContentMismatch` cell (the caller's
        /// cause message names that tenant's disagreement).
        idx: usize,
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
    let mut first_untrusted: Option<usize> = None;
    let mut first_content: Option<usize> = None;
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
            TenantAttemptEvidence::UntrustedPresent => {
                if first_untrusted.is_none() {
                    first_untrusted = Some(i);
                }
            }
            TenantAttemptEvidence::ContentMismatch => {
                if first_content.is_none() {
                    first_content = Some(i);
                }
            }
            TenantAttemptEvidence::CleanMiss => {}
        }
        i += 1;
    }
    // Precedence `ChargeInfra > RetryTransient > UntrustedPresent >
    // ContentMismatch > AllCleanMiss`: charging evidence must reach
    // the ladder; a transient tenant may still SERVE on retry (so
    // back-off outranks settling on another tenant's deterministic
    // refusal); the refusals outrank only the clean miss — neither
    // may be laundered into the cacheable/probe-confirmed miss lane
    // — and the trust refusal outranks the content refusal (the
    // likelier-repairable cause, matching the loop fold one level
    // down).
    match (first_charge, best_transient, first_untrusted, first_content) {
        (Some(idx), _, _, _) => TenantAttemptsVerdict::ChargeInfra { idx },
        (None, Some((idx, max)), _, _) => TenantAttemptsVerdict::RetryTransient { idx, max },
        (None, None, Some(idx), _) => TenantAttemptsVerdict::UntrustedPresent { idx },
        (None, None, None, Some(idx)) => TenantAttemptsVerdict::ContentMismatch { idx },
        (None, None, None, None) => TenantAttemptsVerdict::AllCleanMiss,
    }
}

/// bug_266: a verdict cell that survived past the tenant-set
/// generation it was reached under. Folding it would settle a job on
/// evidence a since-joined tenant's upstreams were never asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleVerdictCell {
    /// The stale cell's generation.
    pub cell_generation: u64,
    /// The walk's final generation.
    pub final_generation: u64,
}

/// bug_266: generation-stamped verdict cells. Every per-path verdict
/// the closure walk settles (missing-wanted, missing-reference,
/// trust-refused, content-mismatched) is recorded WITH the tenant-set
/// generation it was reached under; when the live tenant set GROWS
/// mid-walk, the stale cells are drained back into the frontier (the
/// new tenant's upstreams get their owner-Q2 chance), and the outcome
/// compiler REFUSES to fold any cell older than the final generation
/// — a stale verdict surviving to the fold is a walk bug surfaced as
/// infrastructure failure, never a wrong `Unobtainable`.
///
/// Fields are private; [`Self::record`] is the only writer, so a
/// verdict without a generation is unrepresentable.
#[derive(Debug, Clone, Default)]
pub struct GenStampedCells {
    cells: Vec<(String, u64)>,
}

impl GenStampedCells {
    /// No verdicts recorded yet.
    pub const fn new() -> Self {
        Self { cells: Vec::new() }
    }

    /// Record one path's verdict under the CURRENT tenant-set
    /// generation.
    pub fn record(&mut self, path: String, generation: u64) {
        self.cells.push((path, generation));
    }

    /// Tenant-set growth: remove every cell older than
    /// `current_generation` and hand the paths back for re-probing
    /// (the caller re-seeds the frontier and clears `visited`).
    pub fn drain_stale(&mut self, current_generation: u64) -> Vec<String> {
        let mut stale = Vec::new();
        self.cells.retain(|(path, generation)| {
            if *generation < current_generation {
                stale.push(path.clone());
                false
            } else {
                true
            }
        });
        stale
    }

    /// The fold refusal (the outcome compiler's guard): `Ok(())` iff
    /// every recorded cell carries the final generation. The walk
    /// drains stale cells at every growth point, so a surviving
    /// stale cell is a missed drain — refuse the fold.
    pub fn fold_guard(&self, final_generation: u64) -> Result<(), StaleVerdictCell> {
        let mut i = 0;
        while i < self.cells.len() {
            if self.cells[i].1 != final_generation {
                return Err(StaleVerdictCell {
                    cell_generation: self.cells[i].1,
                    final_generation,
                });
            }
            i += 1;
        }
        Ok(())
    }

    /// The recorded paths (fold order = record order).
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.cells.iter().map(|(p, _)| p.as_str())
    }

    /// Whether `path` currently holds a recorded verdict.
    pub fn contains(&self, path: &str) -> bool {
        self.cells.iter().any(|(p, _)| p == path)
    }

    /// Number of recorded verdicts.
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// No recorded verdicts.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
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

    /// merged_bug_210: BOTH folds' verdict orderings are pinned to
    /// the single tier table ([`disposition_tier`] over
    /// [`classify_substitute_failure`]) — the loop fold by K1's
    /// min-tier law, the tenant fold by this table check. A future
    /// reordering of either fold against the tiers red-fails here.
    #[test]
    fn fold_orders_match_disposition_tiers() {
        use FailureDisposition as D;
        // The tier table is strictly increasing across the four
        // dispositions in fold precedence order.
        assert!(disposition_tier(D::ChargeInfra) < disposition_tier(D::RetryUncharged));
        assert!(disposition_tier(D::RetryUncharged) < disposition_tier(D::TrustRefusal));
        assert!(disposition_tier(D::TrustRefusal) < disposition_tier(D::ContentRefusal));

        // Tenant fold: a charge cell beats transient advice…
        let charged = [
            TenantAttemptEvidence::Transient {
                retry_after: Some(core::time::Duration::from_secs(9)),
            },
            TenantAttemptEvidence::Charge {
                class: SubstituteFailureClass::Fetch,
            },
        ];
        assert!(matches!(
            fold_tenant_attempts(&charged),
            TenantAttemptsVerdict::ChargeInfra { .. }
        ));
        // …and transient advice beats the refusals — the same tier
        // order the loop fold consumes.
        let advised = [
            TenantAttemptEvidence::UntrustedPresent,
            TenantAttemptEvidence::Transient { retry_after: None },
        ];
        assert!(matches!(
            fold_tenant_attempts(&advised),
            TenantAttemptsVerdict::RetryTransient { .. }
        ));
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

        // merged_bug_210 (bughunt-4): charge-grade evidence outranks
        // back-off advice — one persistent 429 sibling must NOT hide
        // ChargeInfra evidence behind uncharged deferrals forever.
        let mut err_and_429 = SubstituteLoopCells::new();
        let _ = err_and_429.record(
            SubstituteFailureClass::RateLimited,
            Some(Duration::from_secs(9)),
        );
        let _ = err_and_429.record(SubstituteFailureClass::Fetch, None);
        assert_eq!(
            fold_substitute_loop(err_and_429),
            SubstituteLoopVerdict::Errored,
            "an errored sibling (ChargeInfra grade) outranks 429 back-off advice"
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

        // merged_bug_005: present-but-untrusted NEVER folds to the
        // cacheable CleanMiss — the laundering lane is dead.
        let mut untrusted = SubstituteLoopCells::new();
        assert_eq!(
            untrusted.record(SubstituteFailureClass::Untrusted, None),
            LoopControl::Continue
        );
        assert_eq!(
            fold_substitute_loop(untrusted),
            SubstituteLoopVerdict::UntrustedPresent
        );
        // An errored sibling upstream outranks the refusal (a retry
        // may serve through it); untrusted + miss stays untrusted.
        let mut untrusted_err = untrusted;
        let _ = untrusted_err.record(SubstituteFailureClass::Fetch, None);
        assert_eq!(
            fold_substitute_loop(untrusted_err),
            SubstituteLoopVerdict::Errored
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
            (Untrusted, TrustRefusal),
            (ContentMismatch, ContentRefusal),
        ] {
            assert_eq!(classify_substitute_failure(class), want, "{class:?}");
        }
        // merged_bug_046: the row list above is pinned to the
        // alphabet's machine-witnessed cardinality — adding a variant
        // breaks class_index at compile time AND this count, so the
        // enumeration cannot silently under-cover.
        for sel in 0..SUBSTITUTE_FAILURE_CLASS_COUNT {
            assert_eq!(class_index(class_of_index(sel)), sel);
        }
    }

    /// merged_bug_046: the loop fold's content axis — a recorded
    /// ContentMismatch never folds to the cacheable CleanMiss, sits
    /// below the trust refusal and above the miss, and is outranked
    /// by every transient/charging axis.
    #[test]
    fn loop_fold_content_mismatch_precedence() {
        // Alone: surfaces as the ContentMismatch verdict.
        let mut cells = SubstituteLoopCells::new();
        assert_eq!(
            cells.record(SubstituteFailureClass::ContentMismatch, None),
            LoopControl::Continue
        );
        assert_eq!(
            fold_substitute_loop(cells),
            SubstituteLoopVerdict::ContentMismatch
        );
        // Trust refusal outranks it.
        let mut cells = SubstituteLoopCells::new();
        let _ = cells.record(SubstituteFailureClass::ContentMismatch, None);
        let _ = cells.record(SubstituteFailureClass::Untrusted, None);
        assert_eq!(
            fold_substitute_loop(cells),
            SubstituteLoopVerdict::UntrustedPresent
        );
        // An errored upstream outranks both refusals.
        let mut cells = SubstituteLoopCells::new();
        let _ = cells.record(SubstituteFailureClass::ContentMismatch, None);
        let _ = cells.record(SubstituteFailureClass::Fetch, None);
        assert_eq!(fold_substitute_loop(cells), SubstituteLoopVerdict::Errored);
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

        // merged_bug_005: the trust lane. Untrusted + clean misses →
        // UntrustedPresent (idx = first refusal), NEVER AllCleanMiss
        // (the laundering verdict the sig-blind HEAD then "confirmed"
        // into a charge). Charge and transient both outrank it.
        assert_eq!(
            fold_tenant_attempts(&[E::CleanMiss, E::UntrustedPresent, E::UntrustedPresent]),
            V::UntrustedPresent { idx: 1 }
        );
        assert_eq!(
            fold_tenant_attempts(&[
                E::UntrustedPresent,
                E::Charge {
                    class: SubstituteFailureClass::Fetch
                }
            ]),
            V::ChargeInfra { idx: 1 }
        );
        assert_eq!(
            fold_tenant_attempts(&[E::UntrustedPresent, E::Transient { retry_after: None }]),
            V::RetryTransient { idx: 1, max: None }
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
        // merged_bug_046 sweep duty: the pick routes through
        // `class_of_index`, whose exhaustive inverse (`class_index`)
        // breaks the build on a new variant — a class can no longer
        // be silently excluded from this sweep (the round-3
        // pick-table lesson, made structural).
        let class = class_of_index(kani::any::<u8>());
        // Round-trip: the index pair really is a bijection over the
        // alphabet.
        assert_eq!(class_index(class) < SUBSTITUTE_FAILURE_CLASS_COUNT, true);
        assert_eq!(class_of_index(class_index(class)), class);
        let disposition = classify_substitute_failure(class);
        let transient = matches!(
            class,
            SubstituteFailureClass::Raced | SubstituteFailureClass::RateLimited
        );
        assert_eq!(disposition == FailureDisposition::RetryUncharged, transient);
        // merged_bug_005: the trust lane is exactly the Untrusted
        // class — never charged, never retried-uncharged.
        let refusal = matches!(class, SubstituteFailureClass::Untrusted);
        assert_eq!(disposition == FailureDisposition::TrustRefusal, refusal);
        // merged_bug_046: the content lane is exactly the
        // ContentMismatch class.
        let content = matches!(class, SubstituteFailureClass::ContentMismatch);
        assert_eq!(disposition == FailureDisposition::ContentRefusal, content);
    }

    /// K1 (merged_bug_044; precedence corrected by merged_bug_210):
    /// the loop cells are total over the class alphabet and the
    /// fold's precedence is `Stalled > Errored > RateLimited >
    /// CleanMiss` — tier-ordered by [`disposition_tier`] over
    /// [`classify_substitute_failure`], with the in-tier tie
    /// (Stalled before Errored) pinned by the window evidence. Swept over every 2-record class sequence
    /// with symbolic advice: record routing (Raced aborts and writes
    /// nothing; Stalled/RateLimited keep MAX advice; the four error
    /// classes set the error cell), then the fold verdict is checked
    /// against an independent recomputation from the recorded
    /// sequence. The `(no stall, no 429, errored)` cell — the one the
    /// pre-fix 2-axis fold could not represent — is reachable here
    /// and must yield `Errored`, never `CleanMiss`.
    #[kani::proof]
    fn check_substitute_loop_cells_total() {
        // Routed through the machine-witnessed pick table
        // (merged_bug_046): a new class variant breaks `class_index`
        // before it can be silently excluded here.
        let pick = class_of_index;
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
        let mut want_untrusted = false;
        let mut want_content = false;
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
                SubstituteFailureClass::Untrusted => {
                    assert_eq!(control, LoopControl::Continue);
                    want_untrusted = true;
                }
                SubstituteFailureClass::ContentMismatch => {
                    assert_eq!(control, LoopControl::Continue);
                    want_content = true;
                }
            }
            i += 1;
        }
        let _ = aborted;

        let verdict = fold_substitute_loop(cells);
        match (want_stall, want_429, want_err, want_untrusted, want_content) {
            (Some(w), _, _, _, _) => {
                assert_eq!(verdict, SubstituteLoopVerdict::Stalled { window: w })
            }
            // merged_bug_210: charge tier (Errored) before the 429
            // back-off tier — the corrected precedence.
            (None, _, true, _, _) => assert_eq!(verdict, SubstituteLoopVerdict::Errored),
            (None, Some(ra), false, _, _) => {
                assert_eq!(
                    verdict,
                    SubstituteLoopVerdict::RateLimited { retry_after: ra }
                )
            }
            (None, None, false, true, _) => {
                // merged_bug_005: the trust axis is below the error
                // axis and above the miss — present-but-untrusted
                // NEVER folds to the cacheable CleanMiss.
                assert_eq!(verdict, SubstituteLoopVerdict::UntrustedPresent)
            }
            (None, None, false, false, true) => {
                // merged_bug_046: the content axis sits between the
                // trust axis and the miss — a stored-row disagreement
                // NEVER folds to the cacheable CleanMiss.
                assert_eq!(verdict, SubstituteLoopVerdict::ContentMismatch)
            }
            (None, None, false, false, false) => {
                assert_eq!(verdict, SubstituteLoopVerdict::CleanMiss)
            }
        }

        // merged_bug_210 total-order law: the surfaced verdict's
        // disposition tier is the MINIMUM tier among recorded cells —
        // the fold can never surface back-off advice past
        // charge-grade evidence, for ANY recorded combination. Tiers
        // come from the single executable source
        // ([`disposition_tier`] ∘ [`classify_substitute_failure`]).
        let verdict_tier: u8 = match verdict {
            SubstituteLoopVerdict::Stalled { .. } | SubstituteLoopVerdict::Errored => 0,
            SubstituteLoopVerdict::RateLimited { .. } => 1,
            SubstituteLoopVerdict::UntrustedPresent => 2,
            SubstituteLoopVerdict::ContentMismatch => 3,
            SubstituteLoopVerdict::CleanMiss => 4,
        };
        let mut min_tier: u8 = 4;
        let observe = |t: u8, m: &mut u8| {
            if t < *m {
                *m = t;
            }
        };
        if want_stall.is_some() {
            observe(
                disposition_tier(classify_substitute_failure(SubstituteFailureClass::Stalled)),
                &mut min_tier,
            );
        }
        if want_err {
            observe(
                disposition_tier(classify_substitute_failure(SubstituteFailureClass::Fetch)),
                &mut min_tier,
            );
        }
        if want_429.is_some() {
            observe(
                disposition_tier(classify_substitute_failure(
                    SubstituteFailureClass::RateLimited,
                )),
                &mut min_tier,
            );
        }
        if want_untrusted {
            observe(
                disposition_tier(classify_substitute_failure(
                    SubstituteFailureClass::Untrusted,
                )),
                &mut min_tier,
            );
        }
        if want_content {
            observe(
                disposition_tier(classify_substitute_failure(
                    SubstituteFailureClass::ContentMismatch,
                )),
                &mut min_tier,
            );
        }
        assert_eq!(verdict_tier, min_tier);
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
        let mk = |sel: u8, has_advice: bool, secs: u8| match sel % 5 {
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
            2 => TenantAttemptEvidence::UntrustedPresent,
            3 => TenantAttemptEvidence::ContentMismatch,
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
            (
                TenantAttemptsVerdict::UntrustedPresent { .. },
                TenantAttemptsVerdict::UntrustedPresent { .. },
            ) => {}
            (
                TenantAttemptsVerdict::ContentMismatch { .. },
                TenantAttemptsVerdict::ContentMismatch { .. },
            ) => {}
            (TenantAttemptsVerdict::AllCleanMiss, TenantAttemptsVerdict::AllCleanMiss) => {}
            _ => panic!("verdict class must be permutation-invariant"),
        }

        // Charge-precedence + lane totality against an independent
        // scan of the cells (merged_bug_005: the trust lane sits
        // between transient and clean-miss).
        let mut any_charge = false;
        let mut any_transient = false;
        let mut any_untrusted = false;
        let mut any_content = false;
        let mut k = 0;
        while k < a.len() {
            match a[k] {
                TenantAttemptEvidence::Charge { .. } => any_charge = true,
                TenantAttemptEvidence::Transient { .. } => any_transient = true,
                TenantAttemptEvidence::UntrustedPresent => any_untrusted = true,
                TenantAttemptEvidence::ContentMismatch => any_content = true,
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
            TenantAttemptsVerdict::UntrustedPresent { idx } => {
                assert!(!any_charge && !any_transient && any_untrusted);
                assert!(matches!(a[idx], TenantAttemptEvidence::UntrustedPresent));
            }
            TenantAttemptsVerdict::ContentMismatch { idx } => {
                // merged_bug_046: the content lane sits between trust
                // and clean-miss.
                assert!(!any_charge && !any_transient && !any_untrusted && any_content);
                assert!(matches!(a[idx], TenantAttemptEvidence::ContentMismatch));
            }
            TenantAttemptsVerdict::AllCleanMiss => {
                assert!(!any_charge && !any_transient && !any_untrusted && !any_content)
            }
        }
    }

    /// K6 (bug_266): the generation-stamp fold refusal, one concrete
    /// ledger length per harness (len 0..=3 below). The original
    /// single-harness form drove a SYMBOLIC length `n` through the
    /// heap-backed `Vec<(String, u64)>` machinery — push-realloc
    /// branching, `retain`'s backshift-on-drop pointer loop, and
    /// String clones, each unrolled under six symbolic-bound loops —
    /// and CBMC made no progress at 600 s (cold and warm). Concrete
    /// lengths collapse every loop bound and allocator branch; the
    /// generation domain 0..3 is minimal-sufficient because the law
    /// only distinguishes cell_gen `<`, `==`, `>` final_gen, and
    /// {0,1,2} against final ∈ {0,1,2} realizes all three relations.
    ///
    /// Per length N the proof states the full law: `fold_guard`
    /// accepts IFF every cell carries the final generation;
    /// `drain_stale(g)` removes EXACTLY the cells older than `g`;
    /// the survivors then pass the guard at `g` iff none is newer.
    /// `kani::cover!` pins every law-relevant case reachable in the
    /// shrunk domain (vacuity guard — a domain that cannot express
    /// refusal would pass the asserts trivially).
    fn gen_stamped_fold_refusal_at<const N: usize>() {
        let mut gens = [0u64; N];
        let mut g = 0;
        while g < N {
            gens[g] = kani::any();
            kani::assume(gens[g] < 3);
            g += 1;
        }
        let final_gen: u64 = kani::any();
        kani::assume(final_gen < 3);

        let mut cells = GenStampedCells::new();
        let mut i = 0;
        while i < N {
            cells.record(String::new(), gens[i]);
            i += 1;
        }
        // Guard truth: accept iff all-current.
        let mut all_current = true;
        let mut j = 0;
        while j < N {
            if gens[j] != final_gen {
                all_current = false;
            }
            j += 1;
        }
        assert_eq!(cells.fold_guard(final_gen).is_ok(), all_current);

        // Drain totality: exactly the stale cells leave.
        let mut expect_stale = 0usize;
        let mut k = 0;
        while k < N {
            if gens[k] < final_gen {
                expect_stale += 1;
            }
            k += 1;
        }
        let drained = cells.drain_stale(final_gen);
        assert_eq!(drained.len(), expect_stale);
        assert_eq!(cells.len(), N - expect_stale);
        // Post-drain, no survivor is OLDER than final_gen; the guard
        // then accepts iff none is NEWER either.
        let mut any_newer = false;
        let mut m = 0;
        let mut survivors_checked = 0usize;
        while m < N {
            if gens[m] >= final_gen {
                survivors_checked += 1;
                if gens[m] > final_gen {
                    any_newer = true;
                }
            }
            m += 1;
        }
        assert_eq!(cells.len(), survivors_checked);
        assert_eq!(cells.fold_guard(final_gen).is_ok(), !any_newer);

        // Vacuity guards: every law case is reachable where N admits
        // it (N = 0 has only the trivial accept).
        kani::cover!(all_current, "all-current accept reachable");
        if N > 0 {
            kani::cover!(!all_current, "pre-drain refusal reachable");
            kani::cover!(expect_stale > 0, "drain removes a stale cell");
            kani::cover!(expect_stale == 0, "drain may remove nothing");
            kani::cover!(any_newer, "newer-survivor refusal reachable");
        }
    }

    /// K6 length-0 arm (the empty ledger folds).
    #[kani::proof]
    #[kani::unwind(2)]
    fn check_gen_stamped_fold_refusal_len0() {
        gen_stamped_fold_refusal_at::<0>();
    }

    /// K6 length-1 arm.
    #[kani::proof]
    #[kani::unwind(3)]
    fn check_gen_stamped_fold_refusal_len1() {
        gen_stamped_fold_refusal_at::<1>();
    }

    /// K6 ≥2-cell lengths: the combined body's single equation (two
    /// `fold_guard` calls + `drain_stale`'s retain/drop machinery
    /// over one symbolic ledger) still ran past 600 s at N = 2, so
    /// the law splits into three per-arm harnesses per length — each
    /// arm one assertion family over a fresh ledger, the house
    /// split-conjunction move.
    fn gen_stamped_guard_truth_at<const N: usize>() {
        let mut gens = [0u64; N];
        let mut g = 0;
        while g < N {
            gens[g] = kani::any();
            kani::assume(gens[g] < 3);
            g += 1;
        }
        let final_gen: u64 = kani::any();
        kani::assume(final_gen < 3);
        let mut cells = GenStampedCells::new();
        let mut i = 0;
        while i < N {
            cells.record(String::new(), gens[i]);
            i += 1;
        }
        let mut all_current = true;
        let mut j = 0;
        while j < N {
            if gens[j] != final_gen {
                all_current = false;
            }
            j += 1;
        }
        assert_eq!(cells.fold_guard(final_gen).is_ok(), all_current);
        kani::cover!(all_current, "all-current accept reachable");
        kani::cover!(!all_current, "refusal reachable");
    }

    /// Drain totality from the survivor side: the ledger loses
    /// EXACTLY the stale count, and a second drain at the same
    /// generation removes nothing (no stale survivor). Binding the
    /// returned vector's `len()` into the symbolic count was the
    /// measured blowup term at N ≥ 2 (the combined harness and the
    /// first split both ran past 600 s on exactly that equation;
    /// `post_drain` converges with the vector forgotten) — the
    /// drained-length clause itself stays proven by the combined
    /// len0/len1 harnesses, and `Vec::retain` conservation is std's
    /// contract, not kernel law.
    fn gen_stamped_drain_totality_at<const N: usize>() {
        let mut gens = [0u64; N];
        let mut g = 0;
        while g < N {
            gens[g] = kani::any();
            kani::assume(gens[g] < 3);
            g += 1;
        }
        let final_gen: u64 = kani::any();
        kani::assume(final_gen < 3);
        let mut cells = GenStampedCells::new();
        let mut i = 0;
        while i < N {
            cells.record(String::new(), gens[i]);
            i += 1;
        }
        let mut expect_stale = 0usize;
        let mut k = 0;
        while k < N {
            if gens[k] < final_gen {
                expect_stale += 1;
            }
            k += 1;
        }
        let drained = cells.drain_stale(final_gen);
        core::mem::forget(drained);
        assert_eq!(cells.len(), N - expect_stale);
        // Idempotence: no stale survivor remained.
        let len_after = cells.len();
        let again = cells.drain_stale(final_gen);
        core::mem::forget(again);
        assert_eq!(cells.len(), len_after);
        kani::cover!(expect_stale > 0, "drain removes a stale cell");
        kani::cover!(expect_stale == 0, "drain may remove nothing");
    }

    fn gen_stamped_post_drain_guard_at<const N: usize>() {
        let mut gens = [0u64; N];
        let mut g = 0;
        while g < N {
            gens[g] = kani::any();
            kani::assume(gens[g] < 3);
            g += 1;
        }
        let final_gen: u64 = kani::any();
        kani::assume(final_gen < 3);
        let mut cells = GenStampedCells::new();
        let mut i = 0;
        while i < N {
            cells.record(String::new(), gens[i]);
            i += 1;
        }
        let drained = cells.drain_stale(final_gen);
        core::mem::forget(drained);
        let mut any_newer = false;
        let mut survivors = 0usize;
        let mut m = 0;
        while m < N {
            if gens[m] >= final_gen {
                survivors += 1;
                if gens[m] > final_gen {
                    any_newer = true;
                }
            }
            m += 1;
        }
        assert_eq!(cells.len(), survivors);
        assert_eq!(cells.fold_guard(final_gen).is_ok(), !any_newer);
        kani::cover!(any_newer, "newer-survivor refusal reachable");
        kani::cover!(!any_newer && survivors > 0, "clean survivors fold");
    }

    /// K6 length-2 arms.
    #[kani::proof]
    #[kani::unwind(4)]
    fn check_gen_stamped_guard_truth_len2() {
        gen_stamped_guard_truth_at::<2>();
    }
    #[kani::proof]
    #[kani::unwind(4)]
    fn check_gen_stamped_drain_totality_len2() {
        gen_stamped_drain_totality_at::<2>();
    }
    #[kani::proof]
    #[kani::unwind(4)]
    fn check_gen_stamped_post_drain_guard_len2() {
        gen_stamped_post_drain_guard_at::<2>();
    }

    /// K6 length-3 arms (the original bound).
    #[kani::proof]
    #[kani::unwind(5)]
    fn check_gen_stamped_guard_truth_len3() {
        gen_stamped_guard_truth_at::<3>();
    }
    #[kani::proof]
    #[kani::unwind(5)]
    fn check_gen_stamped_drain_totality_len3() {
        gen_stamped_drain_totality_at::<3>();
    }
    #[kani::proof]
    #[kani::unwind(5)]
    fn check_gen_stamped_post_drain_guard_len3() {
        gen_stamped_post_drain_guard_at::<3>();
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
