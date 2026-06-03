//! Shared campaign data model: per-job records (results.jsonl), per-path
//! supply outcomes (supply.jsonl), timed dispatch records (dispatch.jsonl),
//! batch records, and the engine's pause state. Wire field names are
//! camelCase, matching the rest of the campaign artifacts.
//!
//! Convention: the stringly-typed `verdict`/`disposition`/`outcome` fields
//! in the JSONL record structs stay `String` on the wire, but they MUST be
//! written via the corresponding enums ([`Verdict::as_str`] /
//! [`Disposition::as_str`], [`ExpectedOutcome`] / [`RioOutcome`] serde forms)
//! — never hand-typed literals — so the record values can never drift from
//! the enum vocabulary.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

use rio_nix::protocol::build::BuildStatus;
use rio_nix::protocol::client::KeyedBuildResult;
use serde::{Deserialize, Serialize};

/// Wall-clock now as RFC3339 (UTC). Single helper so records stay uniform.
pub fn now_rfc3339() -> String {
    jiff::Timestamp::now().to_string()
}

/// Parse an RFC3339 timestamp into unix seconds; None on parse failure.
pub fn rfc3339_to_unix(ts: &str) -> Option<i64> {
    ts.parse::<jiff::Timestamp>().ok().map(|t| t.as_second())
}

/// Expected outcome of one workload unit, as recorded in the replay
/// archive's neutral vocabulary (re-exported from the archive schema so the
/// classifier, the truth loader, and the record writers all share one type).
/// [`ExpectedSide::outcome`] carries its `as_str`/serde wire form.
pub use crate::archive::schema::ExpectedOutcome;

/// Rio-side outcome for one job after collect (input to the bucket
/// classifier).
///
/// Internally tagged as `outcome` (not `kind`) so the tag cannot
/// collide with the `TargetFailed::kind` field; the tag values are the
/// same kebab-case strings [`RioSide::outcome`] mirrors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "outcome")]
pub enum RioOutcome {
    /// Attemptable but no terminal observation (deadline/abort/limit/requeue).
    NotAttempted,
    /// Target derivation completed. `executed` distinguishes built from
    /// substituted: a non-empty exec_id was observed on one of THIS
    /// campaign's builds.
    Built { executed: bool },
    /// The target derivation itself failed.
    TargetFailed { kind: FailureKind },
    /// Blocked by a failed dependency whose root cause is `root` (after
    /// closure-membership re-attribution).
    DependencyFailed {
        root: RootCauseKind,
        failing_drv: String,
    },
}

impl RioOutcome {
    /// The [`RioSide::outcome`] wire string for this outcome — the same
    /// kebab-case name the serde `outcome` tag uses, so record writers
    /// never hand-type it.
    pub fn outcome_str(&self) -> &'static str {
        match self {
            RioOutcome::NotAttempted => "not-attempted",
            RioOutcome::Built { .. } => "built",
            RioOutcome::TargetFailed { .. } => "target-failed",
            RioOutcome::DependencyFailed { .. } => "dependency-failed",
        }
    }
}

/// Scheduler `derivations.status` value for a derivation that completed
/// successfully (built or substituted).
///
/// The `STATUS_*` constants mirror rio-scheduler's derivation state
/// machine — `DerivationStatus` in `rio-scheduler/src/state/derivation.rs`
/// is the source of truth for the wire strings. Only the statuses the
/// engine explicitly branches on are mirrored here; every other status
/// (created, queued, ready, assigned, running, substituting, and the
/// retried `failed`) is treated as still in flight.
pub const STATUS_COMPLETED: &str = "completed";

/// Scheduler `derivations.status` value for a derivation skipped via CA
/// early cutoff (a content-addressed dependency already produced
/// byte-identical output). Terminal, completed-without-execution. See
/// [`STATUS_COMPLETED`] for the source of truth.
pub const STATUS_SKIPPED: &str = "skipped";

/// Scheduler `derivations.status` value for a derivation that failed
/// terminally after the scheduler's retries. See [`STATUS_COMPLETED`]
/// for the source of truth.
pub const STATUS_POISONED: &str = "poisoned";

/// Scheduler `derivations.status` value for a derivation blocked by a
/// failed/poisoned dependency. See [`STATUS_COMPLETED`] for the source
/// of truth.
pub const STATUS_DEPENDENCY_FAILED: &str = "dependency_failed";

/// Scheduler `derivations.status` value for a derivation cancelled by an
/// operator or scheduler decision (CancelBuild, forced drain). See
/// [`STATUS_COMPLETED`] for the source of truth.
pub const STATUS_CANCELLED: &str = "cancelled";

/// How a failed target derivation is attributed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureKind {
    /// Genuine target failure (default for ambiguous/unmatched evidence).
    Genuine,
    /// Positively-identified infrastructure failure (two-signal rule).
    Infra,
    /// Build-side timeout (max_timeout_retries exhausted) — counted against rio.
    Timeout,
    /// Resource-ceiling exhaustion — counted against rio.
    ResourceCeiling,
    /// Fixed-output fetch failure (upstream source rot).
    SourceRot,
}

/// Root cause attributed to the failing dependency of a blocked job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RootCauseKind {
    Genuine,
    Infra,
    SourceRot,
}

/// Per-unit comparison verdict: how the replayed outcome of one workload
/// unit compares to the expected outcome its archive recorded. The
/// kebab-case string forms are the wire `verdict` value in results.jsonl
/// and the `buckets/<verdict>.jsonl` file names.
///
/// A unit ends a campaign with exactly one verdict or one
/// [`Disposition`], never both: dispositions cover units that were never
/// compared, verdicts cover units whose replayed outcome was actually
/// held against the expectation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// Expected built, replay built it; recorded output hashes (when the
    /// archive carries them) all agree.
    MatchBuilt,
    /// Both built, but at least one recorded output NAR hash differs from
    /// the replayed hash.
    OutputDivergence,
    /// Expected a failure and the replay also failed.
    MatchFailed,
    /// Expected built but the unit itself failed in the replay.
    UnexpectedFailure,
    /// Expected built but the unit was blocked by a dependency that
    /// failed in the replay.
    UnexpectedDependencyFailure,
    /// Expected a failure but the replay built the unit.
    UnexpectedSuccess,
    /// The unit (or a dependency) failed only because a fixed-output
    /// input could not be fetched from its upstream origin.
    SourceUnavailable,
    /// The replayed outcome cannot be trusted: rio-side infrastructure
    /// failure, transport failure after retries, or evidence loss.
    InfraIndeterminate,
    /// The recorded outcome was interrupted or infrastructure-dependent,
    /// so there is no deterministic expectation to compare against.
    TruthIndeterminate,
    /// The archive carries no expected outcome for this unit.
    NoTruth,
    /// Timed mode only: the recorded interruption (cancellation or client
    /// disconnect) was reproduced at its recorded offset and the unit did
    /// not complete, exactly as recorded.
    InterruptionReplayed,
    /// Timed mode only: the replayed build completed before the recorded
    /// interruption offset — informational timing divergence, not a
    /// correctness defect.
    InterruptionNotReproduced,
}

impl Verdict {
    /// The wire string for this verdict — the same kebab-case name the
    /// serde form uses, so record writers never hand-type it.
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::MatchBuilt => "match-built",
            Verdict::OutputDivergence => "output-divergence",
            Verdict::MatchFailed => "match-failed",
            Verdict::UnexpectedFailure => "unexpected-failure",
            Verdict::UnexpectedDependencyFailure => "unexpected-dependency-failure",
            Verdict::UnexpectedSuccess => "unexpected-success",
            Verdict::SourceUnavailable => "source-unavailable",
            Verdict::InfraIndeterminate => "infra-indeterminate",
            Verdict::TruthIndeterminate => "truth-indeterminate",
            Verdict::NoTruth => "no-truth",
            Verdict::InterruptionReplayed => "interruption-replayed",
            Verdict::InterruptionNotReproduced => "interruption-not-reproduced",
        }
    }

    /// Every verdict, in report/table order.
    pub const ALL: [Verdict; 12] = [
        Verdict::MatchBuilt,
        Verdict::OutputDivergence,
        Verdict::MatchFailed,
        Verdict::UnexpectedFailure,
        Verdict::UnexpectedDependencyFailure,
        Verdict::UnexpectedSuccess,
        Verdict::SourceUnavailable,
        Verdict::InfraIndeterminate,
        Verdict::TruthIndeterminate,
        Verdict::NoTruth,
        Verdict::InterruptionReplayed,
        Verdict::InterruptionNotReproduced,
    ];

    /// Inverse of [`Verdict::as_str`] for reading recorded verdicts back;
    /// `None` for a string outside the vocabulary (a record written by a
    /// different engine version).
    pub fn from_wire(verdict: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == verdict)
    }

    /// Whether this verdict is evidence the cluster EXECUTED (or
    /// completed) the unit's build — the work-evidence grade
    /// [`is_work_evidencing_terminal`] projects.
    ///
    /// Every comparison verdict is minted by classifying an attempt the
    /// cluster actually resolved (a build, a substitution, a genuine
    /// failure, an upstream fetch refusal observed ON the cluster) —
    /// except `InfraIndeterminate`, which states the OPPOSITE: the
    /// replayed outcome cannot be trusted because rio-side infrastructure
    /// failed. Outage-minted exhaustion terminals (engine-cancel cycles,
    /// engine-side submission failures) land exactly there, so treating
    /// it as work evidence would let an outage satisfy its own probe's
    /// success witness. Exhaustive on purpose: a new verdict refuses to
    /// compile until its evidence grade is decided here.
    pub fn evidences_cluster_work(self) -> bool {
        match self {
            Verdict::InfraIndeterminate => false,
            Verdict::MatchBuilt
            | Verdict::OutputDivergence
            | Verdict::MatchFailed
            | Verdict::UnexpectedFailure
            | Verdict::UnexpectedDependencyFailure
            | Verdict::UnexpectedSuccess
            | Verdict::SourceUnavailable
            | Verdict::TruthIndeterminate
            | Verdict::NoTruth
            | Verdict::InterruptionReplayed
            | Verdict::InterruptionNotReproduced => true,
        }
    }
}

/// Per-unit disposition: why a workload unit was never compared (not
/// attempted, or attempted but not countable as an outcome comparison).
/// The kebab-case string forms are the wire `disposition` value in
/// results.jsonl and the `buckets/<disposition>.jsonl` file names.
///
/// A unit carries either a disposition or a [`Verdict`], never both;
/// dispositions are assigned with precedence over verdicts (a unit that
/// was filtered out is never compared).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Disposition {
    /// Outside the campaign's scope filters (system, glob, feature
    /// exclude, limit, jobs file).
    Filtered,
    /// The archive marks the unit as failing at evaluation/recording
    /// time; there is nothing to build.
    EvalError,
    /// The recorder's fidelity gate found the unit's derivation identity
    /// divergent from the source it recorded; comparing it would compare
    /// different builds.
    IdentityDivergent,
    /// Under the leaf measurement policy the unit's outputs lie inside
    /// another in-scope unit's dependency closure, so it cannot be
    /// measured independently.
    NotAttemptable,
    /// The unit declares impure environment variables the engine does not
    /// forward; its recorded outputs are supplied like dependency outputs
    /// and it is not rebuilt.
    DemotedImpure,
    /// Already valid in the target store before the campaign started.
    CachedPrior,
    /// The target refused an upload required for this unit's closure, so
    /// the unit was not attempted.
    UploadRejected,
    /// The engine could not obtain or deliver required supply for reasons
    /// not attributable to the target.
    SupplyFailed,
    /// Completed without execution because the target substituted it from
    /// its own upstream during the run.
    TargetSubstituted,
    /// The run ended (deadline, pause, abort) before the unit was
    /// attempted.
    NotAttempted,
}

impl Disposition {
    /// The wire string for this disposition — the same kebab-case name
    /// the serde form uses, so record writers never hand-type it.
    pub fn as_str(&self) -> &'static str {
        match self {
            Disposition::Filtered => "filtered",
            Disposition::EvalError => "eval-error",
            Disposition::IdentityDivergent => "identity-divergent",
            Disposition::NotAttemptable => "not-attemptable",
            Disposition::DemotedImpure => "demoted-impure",
            Disposition::CachedPrior => "cached-prior",
            Disposition::UploadRejected => "upload-rejected",
            Disposition::SupplyFailed => "supply-failed",
            Disposition::TargetSubstituted => "target-substituted",
            Disposition::NotAttempted => "not-attempted",
        }
    }

    /// Every disposition, in assignment-precedence order: when more than
    /// one disposition could apply to a unit, the earliest entry wins.
    /// The classifier's disposition steps implement this order — see
    /// `classify()` in the classify module, the one precedence owner.
    pub const ALL: [Disposition; 10] = [
        Disposition::Filtered,
        Disposition::EvalError,
        Disposition::IdentityDivergent,
        Disposition::NotAttemptable,
        Disposition::DemotedImpure,
        Disposition::CachedPrior,
        Disposition::UploadRejected,
        Disposition::SupplyFailed,
        Disposition::TargetSubstituted,
        Disposition::NotAttempted,
    ];

    /// Inverse of [`Disposition::as_str`] for reading recorded
    /// dispositions back; `None` for a string outside the vocabulary (a
    /// record written by a different engine version).
    pub fn from_wire(disposition: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.as_str() == disposition)
    }

    /// Whether this disposition is evidence the cluster EXECUTED the
    /// unit's build — the work-evidence grade
    /// [`is_work_evidencing_terminal`] projects: NO disposition is.
    ///
    /// Dispositions are non-attempt classes by construction ("why a
    /// workload unit was never compared"): plan-time exclusions and
    /// supply-stage retirements never reached the cluster, and even
    /// `target-substituted` — the one attempted() disposition — records
    /// a completion WITHOUT execution. Several are minted BY outages
    /// (supply-failed / upload-rejected from the supply rollup during a
    /// dead-uploads window), so admitting any of them as work evidence
    /// would let an outage satisfy its own probe's success witness.
    /// Deliberately an exhaustive all-false match rather than a `_`:
    /// adding a disposition refuses to compile until its evidence grade
    /// is decided here, keeping this projection's totality test honest.
    pub fn evidences_cluster_work(self) -> bool {
        match self {
            Disposition::Filtered
            | Disposition::EvalError
            | Disposition::IdentityDivergent
            | Disposition::NotAttemptable
            | Disposition::DemotedImpure
            | Disposition::CachedPrior
            | Disposition::UploadRejected
            | Disposition::SupplyFailed
            | Disposition::TargetSubstituted
            | Disposition::NotAttempted => false,
        }
    }

    /// Whether a unit retiring with this disposition was actually submitted
    /// to the target (produced a rio observation) — the report's
    /// "attempted" denominator membership. Deliberately an exhaustive match
    /// with no wildcard: adding a disposition refuses to compile until its
    /// attempted-ness is decided here, so the report can never silently
    /// misclassify a new exclusion as an attempt.
    pub fn attempted(self) -> bool {
        match self {
            // Plan-time exclusions, supply-stage retirements, and the
            // deadline backfill: the unit was never submitted.
            Disposition::Filtered
            | Disposition::EvalError
            | Disposition::IdentityDivergent
            | Disposition::NotAttemptable
            | Disposition::DemotedImpure
            | Disposition::CachedPrior
            | Disposition::UploadRejected
            | Disposition::SupplyFailed
            | Disposition::NotAttempted => false,
            // Submitted and completed without execution (the target
            // substituted it mid-run): a real submission outcome.
            Disposition::TargetSubstituted => true,
        }
    }

    /// Whether records carrying this disposition describe jobs INSIDE the
    /// plan's post-filter in-scope set — the population every report ratio
    /// divides by (`completeness_pct`, progress `remaining`).
    ///
    /// results.jsonl is a single stream mixing two populations: in-scope
    /// outcomes/exclusions, and out-of-scope bookkeeping records. `filtered`
    /// is the one disposition recorded for jobs OUTSIDE the in-scope set
    /// (`plan_time_dispositions` classifies every `plan.skipped` job as
    /// filtered); every other producer is restricted to `plan.in_scope`
    /// before it classifies (the not-attemptable/cached-prior scans, the
    /// divergent/demoted filters, the supply rollup, the deadline
    /// backfill). A ratio numerator must therefore drop the
    /// out-of-population classes or it counts records its denominator
    /// never could — >100% completeness on any filtered campaign.
    ///
    /// Deliberately an exhaustive match with no wildcard, like
    /// [`Disposition::attempted`]: adding a disposition refuses to compile
    /// until its population membership is decided here.
    pub fn in_scope_population(self) -> bool {
        match self {
            // Recorded at plan time for every scope-filtered job — the
            // exact set the in-scope denominator excludes.
            Disposition::Filtered => false,
            Disposition::EvalError
            | Disposition::IdentityDivergent
            | Disposition::NotAttemptable
            | Disposition::DemotedImpure
            | Disposition::CachedPrior
            | Disposition::UploadRejected
            | Disposition::SupplyFailed
            | Disposition::TargetSubstituted
            | Disposition::NotAttempted => true,
        }
    }

    /// The gate accounting of this disposition. Exhaustive on purpose —
    /// see [`GateAccounting`].
    pub fn gate_accounting(self) -> GateAccounting {
        match self {
            // The target refused a required upload: charged to the target
            // by the §7.3 regression row even though the unit was never
            // attempted.
            Disposition::UploadRejected => GateAccounting::TripsRegression,
            // A real submission outcome: the target substituted the unit
            // mid-run. Cannot trip, but the campaign demonstrably
            // exercised it.
            Disposition::TargetSubstituted => GateAccounting::EvidenceOnly,
            // Plan-time exclusions: nothing about these units was observed
            // at the target.
            Disposition::Filtered
            | Disposition::EvalError
            | Disposition::IdentityDivergent
            | Disposition::NotAttemptable
            | Disposition::DemotedImpure
            | Disposition::CachedPrior => GateAccounting::Excluded,
            // Engine-side supply failure: counted against run confidence
            // via the supply-failed low-confidence flag (§7.2), never via
            // the gate — it is evidence about the engine's supply, not
            // about the target.
            Disposition::SupplyFailed => GateAccounting::Excluded,
            // Deadline backfill: the run ended before the unit was
            // touched. Counting it would mint coverage out of nothing.
            Disposition::NotAttempted => GateAccounting::Excluded,
        }
    }
}

/// How the regression gate accounts for one terminal class — the single
/// derivation surface for BOTH the gate's trip sets and its coverage
/// witness (`GateResult::checked`), so the two can never disagree about a
/// class and a new class cannot ship without a gate decision.
///
/// Same compile-forcing shape as [`Disposition::attempted`]: the
/// per-class methods ([`Verdict::gate_accounting`],
/// [`Disposition::gate_accounting`]) are exhaustive matches with no
/// wildcard, so adding a verdict or disposition refuses to compile until
/// its gate accounting is decided here. `attempted()` is deliberately NOT
/// reused for the gate: it answers a different question (submission
/// evidence for the report's rate denominators), and the gate's axes
/// disagree with it in both directions — `upload-rejected` is
/// not-attempted yet trips the gate, while the not-attempted deadline
/// backfill must never inflate the gate's coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateAccounting {
    /// Trips `fail_on: regression` (and therefore `divergence` too):
    /// charged to the target, or charged to run confidence with trip
    /// semantics — the design doc §7.3 regression row.
    TripsRegression,
    /// Trips only `fail_on: divergence`: informational divergence — the
    /// §7.3 divergence row's additions.
    TripsDivergence,
    /// Never trips, but is real campaign-observed evidence: the unit was
    /// exercised and classified, so it counts toward the gate's coverage
    /// witness — an untripped gate over such units verified something.
    EvidenceOnly,
    /// Never trips and carries no gate evidence: plan-time exclusions,
    /// engine-side supply failure (counted against run confidence via the
    /// supply-failed low-confidence flag, never via the gate), and the
    /// not-attempted deadline backfill. Excluded from `checked`, so an
    /// attempted-nothing campaign cannot mint a non-zero coverage witness
    /// out of backfill records.
    Excluded,
}

impl GateAccounting {
    /// Whether a class with this accounting counts toward the gate's
    /// coverage witness (`GateResult::checked`).
    pub fn counts_as_checked(self) -> bool {
        !matches!(self, GateAccounting::Excluded)
    }
}

impl Verdict {
    /// The gate accounting of this verdict. Exhaustive on purpose — see
    /// [`GateAccounting`].
    pub fn gate_accounting(self) -> GateAccounting {
        match self {
            // Charged to the target (§7.3 regression row) or to run
            // confidence with trip semantics (infra-indeterminate).
            Verdict::UnexpectedFailure
            | Verdict::UnexpectedDependencyFailure
            | Verdict::InfraIndeterminate => GateAccounting::TripsRegression,
            // Informational divergence (§7.3 divergence row).
            Verdict::OutputDivergence
            | Verdict::UnexpectedSuccess
            | Verdict::InterruptionNotReproduced => GateAccounting::TripsDivergence,
            // Every other verdict is a real classified observation of the
            // unit: it could not trip, but the gate demonstrably looked.
            Verdict::MatchBuilt
            | Verdict::MatchFailed
            | Verdict::SourceUnavailable
            | Verdict::TruthIndeterminate
            | Verdict::NoTruth
            | Verdict::InterruptionReplayed => GateAccounting::EvidenceOnly,
        }
    }
}

/// The single classification a workload unit ends a campaign with:
/// exactly one [`Verdict`] or exactly one [`Disposition`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnifiedClass {
    /// The unit was attempted and its replayed outcome compared.
    Verdict(Verdict),
    /// The unit was never compared.
    Disposition(Disposition),
}

impl UnifiedClass {
    /// The verdict, when this class is one.
    pub fn verdict(&self) -> Option<Verdict> {
        match self {
            UnifiedClass::Verdict(v) => Some(*v),
            UnifiedClass::Disposition(_) => None,
        }
    }

    /// The disposition, when this class is one.
    pub fn disposition(&self) -> Option<Disposition> {
        match self {
            UnifiedClass::Verdict(_) => None,
            UnifiedClass::Disposition(d) => Some(*d),
        }
    }

    /// The wire string of the inner verdict or disposition.
    pub fn as_str(&self) -> &'static str {
        match self {
            UnifiedClass::Verdict(v) => v.as_str(),
            UnifiedClass::Disposition(d) => d.as_str(),
        }
    }

    /// Whether this class is a final observation for its unit: every
    /// verdict is terminal, and so is every disposition except
    /// `not-attempted` (a unit never reached by this run may still be
    /// attempted by a later resume).
    pub fn is_terminal(&self) -> bool {
        match self {
            UnifiedClass::Verdict(_) => true,
            UnifiedClass::Disposition(d) => *d != Disposition::NotAttempted,
        }
    }
}

/// Map one legacy results.jsonl `bucket` string — plus whether any of the
/// record's per-output NAR comparisons reads `differs` — onto the unified
/// verdict/disposition vocabulary, or `None` for a string that was never
/// a legacy bucket.
///
/// This is the frozen mapping used to prove the vocabulary cutover
/// count-preserving against campaign artifacts written before it: the
/// function reads only legacy `bucket` strings and never new-schema
/// records, so it matches on the historical literals rather than any live
/// enum. The one data-dependent split is `match-built`, which becomes
/// `output-divergence` when a recorded output hash differs; the two timed
/// buckets (`interruption-replayed`, `interruption-not-reproduced`)
/// already carry their final names and map to themselves.
pub fn unified_from_legacy_bucket(bucket: &str, nar_differs: bool) -> Option<UnifiedClass> {
    let class = match bucket {
        "match-built" if nar_differs => UnifiedClass::Verdict(Verdict::OutputDivergence),
        "match-built" => UnifiedClass::Verdict(Verdict::MatchBuilt),
        "rio-only-failure" => UnifiedClass::Verdict(Verdict::UnexpectedFailure),
        "rio-dependency-failure" => UnifiedClass::Verdict(Verdict::UnexpectedDependencyFailure),
        "rio-infra-failure" => UnifiedClass::Verdict(Verdict::InfraIndeterminate),
        "upstream-source-unavailable" => UnifiedClass::Verdict(Verdict::SourceUnavailable),
        "hydra-only-failure" => UnifiedClass::Verdict(Verdict::UnexpectedSuccess),
        "both-failed" => UnifiedClass::Verdict(Verdict::MatchFailed),
        "hydra-unknown" => UnifiedClass::Verdict(Verdict::NoTruth),
        "interruption-replayed" => UnifiedClass::Verdict(Verdict::InterruptionReplayed),
        "interruption-not-reproduced" => UnifiedClass::Verdict(Verdict::InterruptionNotReproduced),
        "eval-divergence" => UnifiedClass::Disposition(Disposition::IdentityDivergent),
        "eval-error" => UnifiedClass::Disposition(Disposition::EvalError),
        "skipped" => UnifiedClass::Disposition(Disposition::Filtered),
        "not-attemptable" => UnifiedClass::Disposition(Disposition::NotAttemptable),
        "not-attempted" => UnifiedClass::Disposition(Disposition::NotAttempted),
        "cached-prior" => UnifiedClass::Disposition(Disposition::CachedPrior),
        "target-substituted" => UnifiedClass::Disposition(Disposition::TargetSubstituted),
        _ => return None,
    };
    Some(class)
}

/// Engine-observed lifecycle timestamps for one job.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Durations {
    pub submitted_at: Option<String>,
    pub first_active_at: Option<String>,
    pub terminal_at: Option<String>,
}

/// One rio-built output path with its NAR identity (when collected).
///
/// `nar_hash` is typed: it serializes to results.jsonl as bare lowercase
/// hex (the form rio-store reports) and reloads through
/// [`crate::narhash::NarHash::parse`], so older records in any spelling
/// keep loading.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RioOutput {
    pub path: String,
    pub nar_hash: Option<crate::narhash::NarHash>,
    pub nar_size: Option<u64>,
}

/// Everything observed on the rio side for one job.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RioSide {
    /// Kebab-case mirror of the [`RioOutcome`] variant ("built",
    /// "not-attempted", "target-failed", "dependency-failed") or, for
    /// plan-time exclusion records, the unit's [`Disposition`] string
    /// ("filtered", "not-attemptable", "cached-prior",
    /// "identity-divergent").
    pub outcome: String,
    /// Terminal status recorded for the target drv, when observed: the
    /// worker-protocol BuildStatus name from the in-band per-root result
    /// (e.g. "Built", "PermanentFailure"), written via [`build_status_name`].
    pub status: Option<String>,
    /// Execution id for the target drv. Nullable: the in-band collection
    /// path does not observe per-execution ids — it is populated only when
    /// a build-graph dump is taken for triage.
    pub exec_id: Option<String>,
    pub failing_drv: Option<String>,
    /// Captured relayed stderr reason line (durable copy that outlives the
    /// scheduler's poison-evidence TTL).
    pub reason: Option<String>,
    /// PG evidence (durable copy).
    pub failed_builders: Vec<String>,
    pub durations: Durations,
    pub outputs: BTreeMap<String, RioOutput>,
}

/// Expected NAR identity of one output, taken from the archive's recorded
/// truth. `narinfo_present` is false when the recorder captured no hash for
/// the output — the output is then merely not comparable.
///
/// `nar_hash` carries the typed digest straight from the archive record
/// (no re-parse between truth loading and comparison); it serializes as
/// bare lowercase hex and reloads through
/// [`crate::narhash::NarHash::parse`], so older records in any spelling
/// keep loading.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExpectedOutput {
    pub narinfo_present: bool,
    pub nar_hash: Option<crate::narhash::NarHash>,
    pub nar_size: Option<u64>,
}

/// The expected (recorded-truth) side of one job record: the unit's
/// expected outcome plus the expected per-output NAR identities.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExpectedSide {
    /// The [`ExpectedOutcome`] wire string (kebab-case, written via
    /// `as_str` — never hand-typed).
    pub outcome: String,
    pub outputs: BTreeMap<String, ExpectedOutput>,
}

/// One line of results.jsonl. Append-only: the LAST record per job wins
/// on reload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRecord {
    pub job: String,
    pub system: String,
    pub drv_path: String,
    pub mode: String,
    pub attempts: u32,
    pub build_ids: Vec<String>,
    pub rio: RioSide,
    pub expected: ExpectedSide,
    /// Per-output NAR comparison verdict: "equal" | "differs" | "not-comparable".
    pub nar_compare: BTreeMap<String, String>,
    /// Final comparison verdict for an attempted-and-compared unit, written
    /// via [`Verdict::as_str`] (never hand-typed). Exactly one of
    /// `verdict`/`disposition` is `Some` on a classified record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    /// Why the unit was never compared, written via [`Disposition::as_str`]
    /// (never hand-typed). Exactly one of `verdict`/`disposition` is `Some`
    /// on a classified record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<String>,
    /// True when this job is a cascaded dependent counted under an
    /// infra/source-rot root cause rather than charged as its own failure.
    #[serde(default)]
    pub cascaded: bool,
    /// The surviving failure cause for failure-class verdicts: the
    /// kebab-case serde form of the target's [`FailureKind`] (or the
    /// blocking dependency's [`RootCauseKind`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_cause: Option<String>,
    /// More than one attempt was needed before the final verdict settled on
    /// a match (`match-built` / `output-divergence`).
    #[serde(default)]
    pub flaky: bool,
    pub signature: Option<String>,
    pub log_key: Option<String>,
    /// The engine-native single-unit re-run for this record
    /// (`cargo xtask replay repro <campaign-id> <drv>`); empty for records
    /// written before any attempt (plan-time exclusions, backfills).
    pub repro: String,
    /// Evidence quality flag, e.g. "log-tail-only".
    #[serde(default)]
    pub evidence: Option<String>,
    pub updated_at: String,
}

impl JobRecord {
    /// The unified class this record carries: its verdict when set, else
    /// its disposition, parsed back through the enums' serde forms. `None`
    /// when neither field is set (an unclassified record) or the stored
    /// string is outside the vocabulary.
    pub fn class(&self) -> Option<UnifiedClass> {
        if let Some(verdict) = &self.verdict {
            return serde_json::from_value(serde_json::Value::String(verdict.clone()))
                .ok()
                .map(UnifiedClass::Verdict);
        }
        if let Some(disposition) = &self.disposition {
            return serde_json::from_value(serde_json::Value::String(disposition.clone()))
                .ok()
                .map(UnifiedClass::Disposition);
        }
        None
    }
}

/// Whether a record is terminal: any verdict is terminal, and so is any
/// disposition other than `not-attempted`; a record with neither field set
/// has not been classified yet and is not terminal.
///
/// Terminality is a SCHEDULING-LIVENESS fact ("this job needs no more
/// offers"), not an evidence grade: outage-minted classes (an
/// infra-indeterminate budget-exhaustion verdict, a supply-failed
/// exclusion) are terminal without the cluster ever executing anything.
/// A consumer asking "did the cluster demonstrably do work" must use
/// [`is_work_evidencing_terminal`] instead.
pub fn is_terminal_class(verdict: &Option<String>, disposition: &Option<String>) -> bool {
    if verdict.is_some() {
        return true;
    }
    disposition
        .as_deref()
        .is_some_and(|d| d != Disposition::NotAttempted.as_str())
}

/// Whether a record carries a WORK-EVIDENCING terminal class: a class
/// that can only exist because the cluster demonstrably executed (or
/// completed) the unit's build — the canary-probe scorer's success
/// witness, and the projection any future "did the cluster do work"
/// consumer must use instead of [`is_terminal_class`].
///
/// Parse-to-enum chokepoint: the record's wire strings are parsed back
/// through the closed [`Verdict`]/[`Disposition`] vocabularies and the
/// judgment is an exhaustive per-member match
/// ([`Verdict::evidences_cluster_work`] /
/// [`Disposition::evidences_cluster_work`]) — a new class refuses to
/// compile until its evidence grade is decided, so a new terminal
/// producer can never silently widen a work-evidence consumer the way
/// `is_terminal_class` consumers were widened. A string outside the
/// vocabulary (a record written by a different engine version) is an
/// explicit unknown: NON-evidencing, fail-closed, with a loud warning —
/// scoring an unknown class as proof of cluster work would let an
/// unrecognized outage-minted class defeat the very guard consuming
/// this predicate.
pub fn is_work_evidencing_terminal(verdict: &Option<String>, disposition: &Option<String>) -> bool {
    if let Some(raw) = verdict.as_deref() {
        return match Verdict::from_wire(raw) {
            Some(v) => v.evidences_cluster_work(),
            None => {
                tracing::warn!(
                    verdict = raw,
                    "record verdict is outside the vocabulary; treating it as unknown — \
                     NOT work-evidencing (fail-closed)"
                );
                false
            }
        };
    }
    if let Some(raw) = disposition.as_deref() {
        return match Disposition::from_wire(raw) {
            Some(d) => d.evidences_cluster_work(),
            None => {
                tracing::warn!(
                    disposition = raw,
                    "record disposition is outside the vocabulary; treating it as unknown — \
                     NOT work-evidencing (fail-closed)"
                );
                false
            }
        };
    }
    false
}

/// [`SupplyEntry::source`] value for an output of a workload unit — never
/// supplied by any mechanism, the campaign must produce it itself.
pub const SUPPLY_SOURCE_WORKLOAD_OUTPUT: &str = "workload-output";

/// [`SupplyEntry::source`] value for a path the target cluster's own
/// substituters cover (the cluster fetches it; the engine does not upload).
pub const SUPPLY_SOURCE_TARGET_SUBSTITUTER: &str = "target-substituter";

/// [`SupplyEntry::source`] value for a path whose bytes come from the
/// replay archive itself (embedded NAR or derivation text).
pub const SUPPLY_SOURCE_EMBEDDED: &str = "embedded";

/// [`SupplyEntry::source`] value for a path fetched from a relay
/// substituter listed by the archive and re-uploaded by the engine.
pub const SUPPLY_SOURCE_RELAY: &str = "relay";

/// [`SupplyEntry::source`] value for a path no source could provide (or
/// that the supply policy deliberately withholds).
pub const SUPPLY_SOURCE_NONE: &str = "none";

/// [`SupplyEntry::mechanism`] value for delivery delegated to the target
/// scheduler (prefetch submission / substitution) instead of an engine upload.
pub const SUPPLY_MECHANISM_DELEGATE: &str = "delegate";

/// [`SupplyEntry::mechanism`] value for an engine upload as part of a
/// multi-path AddMultipleToStore batch.
pub const SUPPLY_MECHANISM_UPLOAD_BATCH: &str = "upload-batch";

/// [`SupplyEntry::mechanism`] value for an engine upload of one large NAR
/// streamed individually via AddToStoreNar.
pub const SUPPLY_MECHANISM_UPLOAD_STREAM: &str = "upload-stream";

/// [`SupplyEntry::mechanism`] value when nothing was (or could be) sent
/// for the path.
pub const SUPPLY_MECHANISM_NONE: &str = "none";

/// [`SupplyEntry::outcome`] value for a path the engine uploaded and the
/// daemon accepted.
pub const SUPPLY_OUTCOME_DELIVERED: &str = "delivered";

/// [`SupplyEntry::outcome`] value for a path that was already valid in the
/// target store — nothing to deliver.
pub const SUPPLY_OUTCOME_ALREADY_PRESENT: &str = "already-present";

/// [`SupplyEntry::outcome`] value for a path the target cluster supplied
/// itself (prefetch substitution or fallback build).
pub const SUPPLY_OUTCOME_DELEGATED: &str = "delegated";

/// [`SupplyEntry::outcome`] value for an upload the daemon refused even
/// after the single fresh-channel retry.
pub const SUPPLY_OUTCOME_REFUSED: &str = "refused";

/// [`SupplyEntry::outcome`] value for a path no source could provide, so
/// nothing was delivered.
pub const SUPPLY_OUTCOME_UNAVAILABLE: &str = "unavailable";

/// [`SupplyEntry::outcome`] value for a delivery attempt that failed
/// (transport error, relay fetch failure, failed prefetch build).
pub const SUPPLY_OUTCOME_FAILED: &str = "failed";

/// [`SupplyEntry::outcome`] value for a planned delivery this invocation
/// skipped without making an attempt: the upload circuit breaker was open
/// (no transport call was made for the path), or another request still
/// held the path's upload claim — that holder's own settlement row is the
/// authoritative outcome. A skipped row is per-request bookkeeping, never
/// a settlement: it asserts nothing about whether the path is, or will
/// be, present on the target (see [`supply_outcome_is_settlement`]).
pub const SUPPLY_OUTCOME_SKIPPED: &str = "skipped";

/// Every [`SupplyEntry::outcome`] value, as data: tests iterate this array
/// instead of hand-copying the vocabulary, so a new outcome constant that
/// is not added here (and classified by
/// [`supply_outcome_is_settlement`]) fails the vocabulary tests rather
/// than silently splitting the journal's readers from its writers.
pub const SUPPLY_OUTCOMES: [&str; 7] = [
    SUPPLY_OUTCOME_DELIVERED,
    SUPPLY_OUTCOME_ALREADY_PRESENT,
    SUPPLY_OUTCOME_DELEGATED,
    SUPPLY_OUTCOME_REFUSED,
    SUPPLY_OUTCOME_UNAVAILABLE,
    SUPPLY_OUTCOME_FAILED,
    SUPPLY_OUTCOME_SKIPPED,
];

/// Whether a supply outcome SETTLES its path — i.e. records the result of
/// an actual delivery resolution (delivered / already-present / delegated,
/// or a claim-resolved refusal/failure) rather than per-request
/// bookkeeping (`unavailable`: nothing could provide the path when it was
/// planned; `skipped`: this invocation made no attempt at all).
///
/// Journal folds that derive a path's settled truth (the supply rollup,
/// the report's outcome counts) keep the LAST settlement row per path and
/// let bookkeeping rows count only for paths that never settled:
/// a bookkeeping row appended after a settlement (a skip-held row landing
/// after the claim holder's `delivered`, a breaker skip after an earlier
/// real failure) must never displace the settled outcome in either
/// direction. Unknown outcome strings (a newer engine's vocabulary read
/// by an older one) classify as bookkeeping, so they can neither retire
/// units nor displace a settled truth — new vocabulary falls through.
pub fn supply_outcome_is_settlement(outcome: &str) -> bool {
    matches!(
        outcome,
        SUPPLY_OUTCOME_DELIVERED
            | SUPPLY_OUTCOME_ALREADY_PRESENT
            | SUPPLY_OUTCOME_DELEGATED
            | SUPPLY_OUTCOME_REFUSED
            | SUPPLY_OUTCOME_FAILED
    )
}
/// [`SupplyEntry::detail`] value the supply stage writes (with outcome
/// [`SUPPLY_OUTCOME_UNAVAILABLE`]) for each planned upload it deliberately
/// defers to the per-submission inline top-up instead of delivering
/// before execution. The inline-resume gate reads this back: a path whose
/// LATEST journal row still carries the deferral was promised to a top-up
/// that no longer exists once the stage's process is gone, so resuming
/// past the completed stage with such paths outstanding (and jobs not yet
/// covered by a delivered top-up) must refuse — the journal, not the
/// current spec's wiring, is what survives a spec switch between
/// processes.
pub const SUPPLY_DETAIL_DEFERRED_INLINE: &str = "deferred to inline top-up";

/// One line of supply.jsonl — per-path supply outcome from the supply stage.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupplyEntry {
    pub path: String,
    /// One of the `SUPPLY_SOURCE_*` constants.
    pub source: String,
    /// One of the `SUPPLY_MECHANISM_*` constants.
    pub mechanism: String,
    /// One of the `SUPPLY_OUTCOME_*` constants.
    pub outcome: String,
    pub detail: Option<String>,
    pub batch_id: Option<u64>,
    pub bytes: Option<u64>,
    pub observed_at: String,
}

/// Per-path collapse of the supply journal under the settlement contract —
/// the fold OWNER. Every consumer that needs "the journal's truth for a
/// path" routes through one of these projections; none re-implements
/// latest-row-wins over raw rows, so the contract pinned by
/// [`supply_outcome_is_settlement`] ("a bookkeeping row appended after a
/// settlement must never displace the settled outcome") holds in EVERY
/// reader by construction instead of by per-consumer discipline. The
/// xtask `supply-fold-owner` lint enforces the routing: a production
/// load of supply.jsonl must name the projection it folds through.
///
/// One journal pass builds all three projections:
///
/// - [`Self::latest_settlements`]: the last SETTLEMENT row per path plus
///   its settled undelivered upload-attempt count — the supply rollup's
///   question. Bookkeeping rows are invisible here.
/// - [`Self::outstanding_inline_deferrals`]: paths still owed to the
///   per-submission inline top-up — the inline-resume gate's trigger
///   evidence. Only settlements and deferral rows participate; other
///   bookkeeping (a breaker/held `skipped`, a plain `unavailable`) can
///   neither redeem a deferral nor displace it.
/// - [`Self::report_outcomes`]: per-path display outcome for the stage
///   report — the settled outcome where one exists, else the latest
///   bookkeeping row, so a never-settled path still shows as
///   unavailable/skipped and a settled one is counted exactly once.
///
/// Temporal scope: [`Self::collapse`] folds the whole journal (the
/// forward question — what is settled NOW); [`Self::collapse_as_of`]
/// folds only rows observed at or before a cutoff (the backward question
/// — what WAS settled when a batch dispatched). Unknown future outcome
/// vocabulary classifies as bookkeeping ([`supply_outcome_is_settlement`]),
/// so it can neither retire units nor displace a settled truth in any
/// projection.
#[derive(Debug, Default)]
pub struct SupplyFold<'a> {
    /// Latest settlement row per path + settled undelivered attempt count.
    settled: BTreeMap<&'a str, SettledSupplyPath<'a>>,
    /// Latest settlement-or-deferral row per path (the deferral-evidence
    /// lattice: a deferral is owed until a settlement supersedes it, and a
    /// re-run stage's fresh deferral re-marks a settled path as owed).
    deferral_evidence: BTreeMap<&'a str, &'a SupplyEntry>,
    /// Latest bookkeeping row per path (report fallback for paths that
    /// never settled).
    bookkeeping: BTreeMap<&'a str, &'a SupplyEntry>,
}

/// One path's settled truth under [`SupplyFold::latest_settlements`].
#[derive(Debug)]
pub struct SettledSupplyPath<'a> {
    /// The path's last settlement row (its settled outcome and mechanism).
    pub entry: &'a SupplyEntry,
    /// How many settled refused/failed rows the path accumulated on the
    /// engine upload mechanisms — each one a claim-resolved delivery
    /// attempt. The supply rollup's attempt floor counts these; skipped
    /// bookkeeping rows are not attempts and never reach this fold.
    pub undelivered_upload_attempts: usize,
}

impl<'a> SupplyFold<'a> {
    /// Collapse the whole journal: the forward question ("what is settled
    /// NOW"), used by the process-start rollup, the inline-resume gate,
    /// and the report counts.
    pub fn collapse(entries: &'a [SupplyEntry]) -> Self {
        Self::fold(entries, None)
    }

    /// Collapse only rows observed at or before `cutoff_rfc3339`: the
    /// backward question ("what WAS settled then"), used by the collect
    /// pass's batch-settle rollup with the batch's `started_at` — claims
    /// are released on refusal/failure precisely so a LATER top-up can
    /// re-claim and deliver, and that later delivery must not rewrite
    /// what a batch dispatched without.
    ///
    /// Malformed timestamps degrade to visibility, never to silent
    /// dropping: an unparseable cutoff disables the scoping (whole-journal
    /// fold, with a warning) and a row with an unparseable `observed_at`
    /// stays visible — both are the pre-scoping behavior, and hiding a
    /// settlement over a corrupt timestamp would un-retire real
    /// starvation.
    pub fn collapse_as_of(entries: &'a [SupplyEntry], cutoff_rfc3339: &str) -> Self {
        match cutoff_rfc3339.parse::<jiff::Timestamp>() {
            Ok(cutoff) => Self::fold(entries, Some(cutoff)),
            Err(error) => {
                tracing::warn!(
                    cutoff = cutoff_rfc3339,
                    %error,
                    "unparseable as-of cutoff for the supply-journal fold; \
                     folding the whole journal instead"
                );
                Self::fold(entries, None)
            }
        }
    }

    fn fold(entries: &'a [SupplyEntry], cutoff: Option<jiff::Timestamp>) -> Self {
        let mut collapsed = Self::default();
        let mut unparseable_rows = 0usize;
        for entry in entries {
            if let Some(cutoff) = cutoff {
                match entry.observed_at.parse::<jiff::Timestamp>() {
                    // The cutoff is inclusive: a batch's own top-up rows
                    // are appended before its started_at is anchored, so
                    // <= keeps them in scope.
                    Ok(observed) if observed > cutoff => continue,
                    Ok(_) => {}
                    Err(_) => unparseable_rows += 1,
                }
            }
            let path = entry.path.as_str();
            let outcome = entry.outcome.as_str();
            if supply_outcome_is_settlement(outcome) {
                let is_undelivered_upload_attempt = (entry.mechanism
                    == SUPPLY_MECHANISM_UPLOAD_BATCH
                    || entry.mechanism == SUPPLY_MECHANISM_UPLOAD_STREAM)
                    && (outcome == SUPPLY_OUTCOME_REFUSED || outcome == SUPPLY_OUTCOME_FAILED);
                collapsed
                    .settled
                    .entry(path)
                    .and_modify(|settled| {
                        settled.entry = entry;
                        settled.undelivered_upload_attempts +=
                            usize::from(is_undelivered_upload_attempt);
                    })
                    .or_insert(SettledSupplyPath {
                        entry,
                        undelivered_upload_attempts: usize::from(is_undelivered_upload_attempt),
                    });
                collapsed.deferral_evidence.insert(path, entry);
            } else {
                collapsed.bookkeeping.insert(path, entry);
                if entry.detail.as_deref() == Some(SUPPLY_DETAIL_DEFERRED_INLINE) {
                    collapsed.deferral_evidence.insert(path, entry);
                }
            }
        }
        if unparseable_rows > 0 {
            tracing::warn!(
                rows = unparseable_rows,
                "supply rows with unparseable observedAt under an as-of fold are kept visible"
            );
        }
        collapsed
    }

    /// The last settlement row per path (bookkeeping rows invisible by
    /// construction), with the path's settled undelivered upload-attempt
    /// count alongside.
    pub fn latest_settlements(&self) -> &BTreeMap<&'a str, SettledSupplyPath<'a>> {
        &self.settled
    }

    /// Store paths whose latest deferral-evidence row is the
    /// inline-delivery deferral ([`SUPPLY_DETAIL_DEFERRED_INLINE`]):
    /// planned uploads a completed supply stage handed to the
    /// per-submission top-up that no settlement has superseded. A top-up
    /// that later delivered, refused, or failed the path — or a re-run
    /// stage that found it already present — clears the deferral; a
    /// breaker/held `skipped` row or a plain `unavailable` row asserts
    /// nothing about the promise and leaves the path owed (fail-closed:
    /// a gateway outage that skip-stamps the outstanding paths must not
    /// erase the resume gate's trigger evidence). Sorted (BTreeMap order)
    /// for stable error messages.
    ///
    /// This is the inline-resume gate's trigger evidence precisely
    /// because it is durable and substrate-independent: the current
    /// spec's wiring says what THIS process would do, while the journal
    /// says what the completed stage actually left undelivered — a spec
    /// switched from inline to prewarm between processes changes the
    /// former but not the latter.
    pub fn outstanding_inline_deferrals(&self) -> Vec<&'a str> {
        self.deferral_evidence
            .iter()
            .filter(|(_, entry)| {
                entry.detail.as_deref() == Some(SUPPLY_DETAIL_DEFERRED_INLINE)
                    && !supply_outcome_is_settlement(&entry.outcome)
            })
            .map(|(path, _)| *path)
            .collect()
    }

    /// Per-path display outcome for the stage report: the settled outcome
    /// where one exists (a settlement supersedes bookkeeping regardless of
    /// row order), else the latest bookkeeping row. Equivalently: a
    /// skip-held row appended after the claim holder's `delivered` leaves
    /// the path counted delivered, a breaker skip after a real failure
    /// leaves it counted failed, and a path nothing ever settled counts
    /// under its latest bookkeeping outcome.
    pub fn report_outcomes(&self) -> BTreeMap<&'a str, &'a str> {
        let mut outcomes: BTreeMap<&'a str, &'a str> = self
            .bookkeeping
            .iter()
            .map(|(path, entry)| (*path, entry.outcome.as_str()))
            .collect();
        for (path, settled) in &self.settled {
            outcomes.insert(path, settled.entry.outcome.as_str());
        }
        outcomes
    }
}

/// One line of dispatch.jsonl — per recorded request, written by the timed dispatcher.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchEntry {
    pub request_index: usize,
    pub session: i64,
    pub due_offset_s: f64,
    pub dispatched_at: String,
    pub dispatch_lateness_ms: u64,
    pub deadline_secs: u64,
    pub interruption_armed: bool,
    pub interruption_fired: bool,
    pub attempts: u32,
    pub batch_ids: Vec<u64>,
    pub drvs: Vec<String>,
}

/// [`BatchRecord::kind`] value for build-stage submissions.
pub const BATCH_KIND_SUBMIT: &str = "submit";

/// [`BatchRecord::kind`] value for submissions made by the timed dispatcher
/// (one recorded request per batch). Members of timed batches are never
/// re-offered to the timeless pending pool — the timed dispatcher owns its
/// own retries.
pub const BATCH_KIND_TIMED: &str = "timed";

/// Per-root in-band build result captured from one BuildPathsWithResults
/// submission (one entry per requested root, positional order preserved).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PathOutcome {
    pub drv_path: String,
    /// BuildStatus name as produced by [`build_status_name`] (e.g. "Built",
    /// "PermanentFailure"); writers must use that helper, never literals.
    pub status: String,
    pub error_msg: String,
    pub start_time: u64,
    pub stop_time: u64,
}

/// The recorded name of a worker-protocol [`BuildStatus`]: exactly the Rust
/// variant identifier (e.g. "Built", "PermanentFailure").
/// [`PathOutcome::status`] writers must go through this helper — never
/// hand-typed literals — so recorded statuses can never drift from the enum
/// vocabulary.
pub fn build_status_name(status: BuildStatus) -> &'static str {
    match status {
        BuildStatus::Built => "Built",
        BuildStatus::Substituted => "Substituted",
        BuildStatus::AlreadyValid => "AlreadyValid",
        BuildStatus::PermanentFailure => "PermanentFailure",
        BuildStatus::InputRejected => "InputRejected",
        BuildStatus::OutputRejected => "OutputRejected",
        BuildStatus::TransientFailure => "TransientFailure",
        BuildStatus::CachedFailure => "CachedFailure",
        BuildStatus::TimedOut => "TimedOut",
        BuildStatus::MiscFailure => "MiscFailure",
        BuildStatus::DependencyFailed => "DependencyFailed",
        BuildStatus::LogLimitExceeded => "LogLimitExceeded",
        BuildStatus::NotDeterministic => "NotDeterministic",
        BuildStatus::ResolvesToAlreadyValid => "ResolvesToAlreadyValid",
        BuildStatus::NoSubstituters => "NoSubstituters",
    }
}

/// Inverse of [`build_status_name`]: the [`BuildStatus`] a recorded status
/// string names, or `None` for anything the helper never writes (readers
/// treat unknown strings defensively, as a failure).
pub fn build_status_from_name(name: &str) -> Option<BuildStatus> {
    Some(match name {
        "Built" => BuildStatus::Built,
        "Substituted" => BuildStatus::Substituted,
        "AlreadyValid" => BuildStatus::AlreadyValid,
        "PermanentFailure" => BuildStatus::PermanentFailure,
        "InputRejected" => BuildStatus::InputRejected,
        "OutputRejected" => BuildStatus::OutputRejected,
        "TransientFailure" => BuildStatus::TransientFailure,
        "CachedFailure" => BuildStatus::CachedFailure,
        "TimedOut" => BuildStatus::TimedOut,
        "MiscFailure" => BuildStatus::MiscFailure,
        "DependencyFailed" => BuildStatus::DependencyFailed,
        "LogLimitExceeded" => BuildStatus::LogLimitExceeded,
        "NotDeterministic" => BuildStatus::NotDeterministic,
        "ResolvesToAlreadyValid" => BuildStatus::ResolvesToAlreadyValid,
        "NoSubstituters" => BuildStatus::NoSubstituters,
        _ => return None,
    })
}

/// Map the daemon's per-root keyed results from one `BuildPathsWithResults`
/// call onto the submitted roots positionally: the daemon answers in
/// submission order, so entry *i* belongs to `root_drvs[i]`. The recorded
/// `drv_path` is always the bare root drv path (what the result consumers
/// index by), never the echoed `DerivedPath` string, which carries the
/// output selector (`…!*`).
///
/// The positional contract obliges the correlating side to check that the
/// returned length matches the submission, so the check lives here — next to
/// the zip, where no caller can forget it. A mismatch only warns: a short
/// result vector still maps the prefix it can (the uncovered roots fall to
/// the caller's missing-result rule) and extra entries are ignored.
pub(crate) fn path_outcomes_from_keyed(
    root_drvs: &[String],
    results: &[KeyedBuildResult],
) -> Vec<PathOutcome> {
    if results.len() != root_drvs.len() {
        tracing::warn!(
            requested = root_drvs.len(),
            returned = results.len(),
            "BuildPathsWithResults returned a different result count than requested roots; \
             uncovered roots fall to the caller's missing-result rule"
        );
    }
    root_drvs
        .iter()
        .zip(results)
        .map(|(drv, keyed)| PathOutcome {
            drv_path: drv.clone(),
            status: build_status_name(keyed.result.status).to_string(),
            error_msg: keyed.result.error_msg.clone(),
            start_time: keyed.result.start_time,
            stop_time: keyed.result.stop_time,
        })
        .collect()
}

/// One line of batches.jsonl — engine-internal bookkeeping for resume and
/// build_id recovery (not part of the per-job results schema).
///
/// Records written before the client-ops cutover may carry an `exitCode`
/// key (the nix child's exit status); there is no child process any more,
/// so the field is gone and serde simply ignores it on read.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchRecord {
    pub batch_id: u64,
    /// One of the `BATCH_KIND_*` constants in this module; batches.jsonl
    /// writers must use the constants, never hand-typed literals. Records
    /// written by earlier engine versions may carry kinds that no longer
    /// have a writer (e.g. the retired warm stage's "warm"); they are kept
    /// readable and skipped by the collect loop.
    pub kind: String,
    pub jobs: Vec<String>,
    pub root_drvs: Vec<String>,
    pub est_nodes: usize,
    pub build_id: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
    /// In-band per-root results from the submission; empty for submitters
    /// that have none and on records written before this field existed.
    #[serde(default)]
    pub results: Vec<PathOutcome>,
    /// drv path → relayed failure reason (captured live from nix stderr).
    pub reasons: BTreeMap<String, String>,
    pub stderr_tail: Option<String>,
    /// True when the engine itself killed/cancelled this batch (timeout, abort).
    #[serde(default)]
    pub engine_cancelled: bool,
    /// True when the cancellation came from an armed disconnect-replay
    /// deadline (the channel was abandoned at the recorded relative
    /// instant) rather than the engine's own build budget. Set only by the
    /// submission chokepoint, which knows which typed deadline it handed
    /// the submitter; classification reads this bit so a build-budget cut
    /// on an interruption-armed request can never masquerade as the
    /// recorded interruption being reproduced. Defaults to false on records
    /// written before the bit existed (their armed cancellations predate
    /// the distinction and are not re-classified).
    #[serde(default)]
    pub disconnect_deadline_fired: bool,
    /// Root drvs in this submission for which a recorded interruption
    /// (cancellation or client disconnect) was armed by the timed
    /// dispatcher. Empty for every non-timed batch.
    #[serde(default)]
    pub interruption_drvs: Vec<String>,
    /// Interior input derivations the import walk skipped because the
    /// archive does not embed them (a non-conforming archive's
    /// import-gap set, sorted): the operator-facing union view — the
    /// per-root attribution collect consumes is
    /// `import_skipped_by_root`. Defaults to empty on records written
    /// before the field existed.
    #[serde(default)]
    pub import_skipped_drvs: Vec<String>,
    /// Per-root attribution of the import-gap set (root drv → the gaps
    /// reachable in ITS embedded-text closure): the breadcrumb collect
    /// CONSUMES — a failed root whose entry here is non-empty retires
    /// under the supply-failed disposition before two-signal
    /// classification, attributing the failure to the archive instead
    /// of charging the unit a regression. Defaults to empty on records
    /// written before the field existed: their skips predate the
    /// attribution path and their members are not re-classified.
    #[serde(default)]
    pub import_skipped_by_root: BTreeMap<String, Vec<String>>,
    /// True when this batch is a canary probe released by the submit loop
    /// while the infra-rate backpressure pause held: a single job sent to
    /// test whether the infrastructure recovered, whose infra-shaped
    /// failures collect re-offers without consuming the per-job retry
    /// budget (the failure is evidence about the outage, not the job).
    /// Defaults to false on records written before probing existed —
    /// pre-probe batches were all full-wave submissions.
    #[serde(default)]
    pub probe: bool,
    /// 1-based confirmation-retry index when this batch is one of the
    /// timed dispatcher's sanctioned re-confirmation submissions (an
    /// expected-built unit whose replayed result was a failure, re-checked
    /// on a fresh batch); 0 for every other writer. Collect's
    /// already-terminal belt admits a SUCCESS result from such a batch as
    /// a designed superseding write — the retry's verdict replaces the
    /// initial failure under latest-record-per-job semantics, with
    /// `attempts = confirmation_attempt + 1` carrying the flakiness on
    /// the verdict. Defaults to 0 on records written before the field
    /// existed: their retry successes were dropped by the belt (the
    /// defect this field exists to fix), and they are not re-classified.
    #[serde(default)]
    pub confirmation_attempt: u32,
    /// True when this batch's pre-submission supply top-up PROVED a
    /// complete delivery of everything its plan owed (the inline-delivery
    /// mechanism, or the prewarm-miss fallback) — the fail-closed
    /// collapse of the top-up's returned per-path outcome, never the
    /// call's Ok/Err shape: a top-up that ran, returned Ok, and left
    /// paths undelivered (breaker-skipped, refused, claim-held, or
    /// unsourceable) records false, like a failed or absent one. The
    /// inline-resume gate reads this as the delivery proof for the
    /// batch's jobs: membership in a batch without the proof tells the
    /// gate nothing about the jobs' deferred uploads. Defaults to false
    /// on records written before the field existed; records written
    /// while the bit was minted from bare Ok-ness may carry a true bit
    /// no delivery backs — re-running the supply stage (the gate's
    /// remedy) re-proves delivery either way.
    #[serde(default)]
    pub topup_delivered: bool,
}

/// Writer intent for one submission, recorded verbatim onto the
/// [`BatchRecord`] by the submission chokepoint
/// ([`super::submit::submit_one_batch`]) so collect can apply
/// writer-specific policy when the batch settles. Intent travels ON the
/// batch record because legitimacy of a write is a property of the
/// writing batch, never something a reader can re-derive from per-job
/// state after the fact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BatchIntent {
    /// Canary probe released while the infra-rate pause held (see
    /// [`BatchRecord::probe`]).
    pub probe: bool,
    /// 1-based confirmation-retry index for the timed dispatcher's
    /// re-confirmation submissions; 0 for every other writer (see
    /// [`BatchRecord::confirmation_attempt`]).
    pub confirmation_attempt: u32,
    /// The batch's pre-submission supply top-up proved complete delivery
    /// (see [`BatchRecord::topup_delivered`]). Set by the submit loops
    /// from the top-up's returned per-path outcome — its fail-closed
    /// `proves_delivery()` collapse — immediately before the submission.
    pub topup_delivered: bool,
}

impl BatchIntent {
    /// A canary-probe submission.
    pub fn probe() -> Self {
        Self {
            probe: true,
            ..Self::default()
        }
    }

    /// The timed dispatcher's `attempt`-th confirmation retry (1-based).
    pub fn confirmation(attempt: u32) -> Self {
        Self {
            confirmation_attempt: attempt,
            ..Self::default()
        }
    }
}

/// [`RequeueRecord::source`] value for collect-pass re-offers (a settled
/// batch's member returned to the timeless pending pool).
pub const REQUEUE_SOURCE_COLLECT: &str = "collect";

/// [`RequeueRecord::source`] value for the watchdog's single active-stall
/// auto-retry.
pub const REQUEUE_SOURCE_STALL: &str = "stall";

/// [`RequeueRecord::source`] value for a queued-watchdog re-enqueue (the
/// non-terminal QueuedRequeue ladder step). NOT an engine resubmission —
/// the job is already in the pending pool and nothing is re-offered — so
/// the resume fold routes this source into the queued-escalation ladder
/// counter, never into `resubmissions` (it must not consume the infra
/// auto-retry budget, trip fail-fast singleton isolation, or inflate
/// `attempts`).
pub const REQUEUE_SOURCE_QUEUED: &str = "queued";

/// Why the engine re-offered a job: the closed requeue-reason vocabulary,
/// journaled on every [`RequeueRecord`].
///
/// One enum because requeue history has two consumers with deliberately
/// different semantics, and string literals let them drift:
///
/// - The retry **budget** counts every reason (`collect::decide`'s
///   conservative-budget contract: any prior re-offer consumes auto-retry
///   headroom, so the budget can never multiply across reasons).
/// - The **measurement** — the `attempts` stamped on results.jsonl
///   records, the `flaky` flag, the report's first-attempt/after-retries
///   split — counts only reasons that represent a real cluster attempt,
///   through [`RequeueReason::counts_as_cluster_attempt`], the ONLY path
///   from reasons to the measurement.
///
/// Adding a reason refuses to compile until its measurement semantics are
/// decided in the predicate's exhaustive match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequeueReason {
    /// The engine itself cancelled the batch (deadline, abort) before
    /// results arrived. Documented at the carve-out as "the engine's own
    /// act, not evidence about the job".
    EngineCancelled,
    /// The submission failed engine-side (channel open, drv import, the
    /// build op erroring before any result) — the build never reached the
    /// cluster.
    EngineSubmissionFailure,
    /// The settled batch carried no in-band result for this root: a
    /// transport defect on a submission that did reach the cluster.
    NoInbandResult,
    /// A positively-identified infrastructure failure consumed the single
    /// auto-retry.
    InfraAutoRetry,
    /// An infra-shaped failure of a canary-probe batch: the probed outage
    /// answering, evidence about the outage rather than the job. The
    /// decision consumer routes probe-exempt re-offers around the journal
    /// entirely (no entry, no counter — the probe ladder owns the
    /// outage's convergence), so this reason is normally never journaled;
    /// it exists so the decision vocabulary stays closed and a journaled
    /// probe line, should one ever be written, reads back as
    /// not-a-cluster-attempt.
    InfraProbe,
    /// Fail-fast marked this job dependency-failed for a trigger outside
    /// its own closure: a cluster attempt denied a fair run by its
    /// batch-mates.
    FailfastBatchMate,
    /// A dependency-failed result with no identifiable trigger, treated
    /// like a fail-fast batch-mate.
    DependencyFailedNoTrigger,
    /// The watchdog's single active-stall auto-retry: the committed
    /// attempt ran (and stalled) on the cluster.
    ActiveStall,
}

impl RequeueReason {
    /// Every requeue reason. The vocabulary as data: the per-reason
    /// measurement-semantics test iterates this, so a new reason cannot
    /// ship without an expected-flakiness row.
    pub const ALL: [RequeueReason; 8] = [
        RequeueReason::EngineCancelled,
        RequeueReason::EngineSubmissionFailure,
        RequeueReason::NoInbandResult,
        RequeueReason::InfraAutoRetry,
        RequeueReason::InfraProbe,
        RequeueReason::FailfastBatchMate,
        RequeueReason::DependencyFailedNoTrigger,
        RequeueReason::ActiveStall,
    ];

    /// The journal/log string for this reason ([`RequeueRecord::why`]).
    /// Writers must use this — never literals — so the journal's reason
    /// vocabulary cannot drift from the enum.
    pub const fn as_str(self) -> &'static str {
        match self {
            RequeueReason::EngineCancelled => "engine-cancelled",
            RequeueReason::EngineSubmissionFailure => "engine-submission-failure",
            RequeueReason::NoInbandResult => "no-inband-result",
            RequeueReason::InfraAutoRetry => "infra-auto-retry",
            RequeueReason::InfraProbe => "infra-probe",
            RequeueReason::FailfastBatchMate => "failfast-batch-mate",
            RequeueReason::DependencyFailedNoTrigger => "dependency-failed-no-trigger",
            RequeueReason::ActiveStall => "active-stall",
        }
    }

    /// Inverse of [`RequeueReason::as_str`] for reading journaled reasons
    /// back; `None` for a string outside the vocabulary (a journal written
    /// by a different engine version).
    pub fn from_wire(why: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|reason| reason.as_str() == why)
    }

    /// Whether a re-offer for this reason means the PRECEDING submission
    /// was a real cluster attempt — evidence about the job — and so counts
    /// toward the user-facing measurement (`attempts`, `flaky`, the
    /// report's retry split).
    ///
    /// The two engine-side reasons do not count: an engine-cancelled batch
    /// is the engine's own scheduling act (its members may never have
    /// started), and an engine-side submission failure never reached the
    /// cluster at all. Counting either marks first-real-attempt successes
    /// flaky and reports a deadline-cut wave as "succeeded after retries".
    /// Every other reason describes an attempt that ran (or was denied a
    /// fair run) on the cluster, which is exactly what a flakiness
    /// measurement is about.
    ///
    /// The retry BUDGET deliberately ignores this distinction — see
    /// `collect::decide`.
    pub const fn counts_as_cluster_attempt(self) -> bool {
        match self {
            RequeueReason::EngineCancelled
            | RequeueReason::EngineSubmissionFailure
            | RequeueReason::InfraProbe => false,
            RequeueReason::NoInbandResult
            | RequeueReason::InfraAutoRetry
            | RequeueReason::FailfastBatchMate
            | RequeueReason::DependencyFailedNoTrigger
            | RequeueReason::ActiveStall => true,
        }
    }
}

/// One line of requeues.jsonl: an engine-initiated resubmission, journaled
/// by the job ledger at the transition site BEFORE the in-memory counter
/// moves.
///
/// This stream is the durable substrate of every bound the resubmission
/// counters back — the infra auto-retry budget, fail-fast singleton
/// isolation, the stall auto-retry gate, and the per-record `attempts`
/// accounting. Requeue decisions write no results.jsonl record and their
/// batches are skipped on resume (collected.json), so without this journal
/// a pod restart silently zeroed all consumed budget and every documented
/// convergence bound reopened at the restart edge. Resume rebuilds the
/// counters as a pure fold of this stream
/// ([`super::ledger::JobLedger::from_journals`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequeueRecord {
    pub job: String,
    /// Which transition counted it: one of the `REQUEUE_SOURCE_*` constants
    /// in this module (writers must use the constants, never literals —
    /// the resume fold derives the stall-retry counters from this field).
    pub source: String,
    /// The requeue reason: a [`RequeueReason`] wire string (writers go
    /// through [`RequeueReason::as_str`]). The BUDGET fold counts entries
    /// without parsing reasons (every reason consumes budget); the
    /// MEASUREMENT projection parses this field through
    /// [`RequeueReason::counts_as_cluster_attempt`] to derive the
    /// `attempts`/`flaky` stamped on records — an unrecognized string (a
    /// journal from a different engine version) counts, preserving the
    /// historical every-requeue semantics for foreign entries.
    /// One load-bearing value beyond the measurement: the resume fold
    /// counts collect-source entries whose why is
    /// [`RequeueReason::EngineCancelled`]'s wire string into the
    /// engine-cancel cycle budget (`max_engine_cancel_cycles`), so that
    /// bound survives restarts.
    pub why: String,
    pub at: String,
}

/// Cross-component pause flags: a manual operator pause and the
/// engine's own backpressure pause, OR-ed into one "submission paused"
/// signal — plus the canary-probe channel between the poller and the
/// submit loop.
///
/// The probe channel exists because the infra-rate backpressure pause
/// suppresses its own evidence: the rolling window is computed over
/// terminal records, and a paused submit loop produces none, so once
/// in-flight work drains the window freezes and the pause would re-assert
/// identically forever. The poller therefore grants one-shot probe tokens
/// ([`Self::grant_probe`]); the paused submit loop redeems a token for a
/// SINGLE one-job probe batch ([`Self::take_probe`]) and reports what it
/// released ([`Self::set_probe_batch`]) so the poller can score the cycle
/// once collect has classified it.
#[derive(Debug, Default)]
pub struct PauseState {
    manual: AtomicBool,
    backpressure: AtomicBool,
    probe: std::sync::Mutex<ProbePhase>,
}

/// Lifecycle of one canary-probe cycle through the [`PauseState`] channel.
/// One value, strictly forward (Idle → Granted → Redeemed → Released →
/// Idle, with Redeemed → Idle on abort), so a granted token can never
/// release more than one probe batch and the poller can never double-grant
/// while a probe is anywhere in flight.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum ProbePhase {
    /// No probe cycle in progress.
    #[default]
    Idle,
    /// The poller granted a token the submit loop has not redeemed yet.
    Granted,
    /// The submit loop redeemed the token and is releasing the probe.
    Redeemed,
    /// The probe batch was released; the poller scores the cycle once
    /// collect has classified it, then clears back to Idle.
    Released { batch_id: u64, jobs: Vec<String> },
}

impl PauseState {
    pub fn set_manual(&self, v: bool) {
        self.manual.store(v, Ordering::SeqCst);
    }

    pub fn set_backpressure(&self, v: bool) {
        self.backpressure.store(v, Ordering::SeqCst);
    }

    pub fn manual(&self) -> bool {
        self.manual.load(Ordering::SeqCst)
    }

    pub fn backpressure(&self) -> bool {
        self.backpressure.load(Ordering::SeqCst)
    }

    /// True when any pause source is set.
    pub fn paused(&self) -> bool {
        self.manual() || self.backpressure()
    }

    fn probe_lock(&self) -> std::sync::MutexGuard<'_, ProbePhase> {
        self.probe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Make one probe token available to the paused submit loop. A no-op
    /// unless the channel is idle — at most one probe cycle exists at a
    /// time.
    pub fn grant_probe(&self) {
        let mut probe = self.probe_lock();
        if *probe == ProbePhase::Idle {
            *probe = ProbePhase::Granted;
        }
    }

    /// Redeem the granted probe token, if any. Exactly one caller wins per
    /// grant.
    pub fn take_probe(&self) -> bool {
        let mut probe = self.probe_lock();
        if *probe == ProbePhase::Granted {
            *probe = ProbePhase::Redeemed;
            true
        } else {
            false
        }
    }

    /// The submit loop redeemed a token but dropped the probe without
    /// submitting it (no offerable job, operator pause, deadline): return
    /// the channel to idle so the poller can grant again.
    pub fn abort_probe(&self) {
        let mut probe = self.probe_lock();
        if *probe == ProbePhase::Redeemed {
            *probe = ProbePhase::Idle;
        }
    }

    /// The submit loop reports the probe batch it released against the
    /// redeemed token.
    pub fn set_probe_batch(&self, batch_id: u64, jobs: Vec<String>) {
        let mut probe = self.probe_lock();
        if *probe == ProbePhase::Redeemed {
            *probe = ProbePhase::Released { batch_id, jobs };
        }
    }

    /// True when no probe cycle is in progress (the poller may grant).
    pub fn probe_idle(&self) -> bool {
        *self.probe_lock() == ProbePhase::Idle
    }

    /// The released probe batch, if one is awaiting scoring.
    pub fn probe_batch(&self) -> Option<(u64, Vec<String>)> {
        match &*self.probe_lock() {
            ProbePhase::Released { batch_id, jobs } => Some((*batch_id, jobs.clone())),
            _ => None,
        }
    }

    /// The poller scored the released probe's cycle: channel back to idle.
    pub fn clear_probe(&self) {
        *self.probe_lock() = ProbePhase::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_strings_match_their_serde_forms() {
        for (rio, expected) in [
            (RioOutcome::NotAttempted, "not-attempted"),
            (RioOutcome::Built { executed: true }, "built"),
            (
                RioOutcome::TargetFailed {
                    kind: FailureKind::Genuine,
                },
                "target-failed",
            ),
            (
                RioOutcome::DependencyFailed {
                    root: RootCauseKind::Infra,
                    failing_drv: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-d.drv".into(),
                },
                "dependency-failed",
            ),
        ] {
            assert_eq!(rio.outcome_str(), expected);
            let value = serde_json::to_value(&rio).unwrap();
            assert_eq!(value["outcome"], expected, "{rio:?}");
        }
    }

    #[test]
    fn scheduler_status_vocabulary_is_fixed_and_distinct() {
        // The wire strings are owned by rio-scheduler's DerivationStatus
        // (rio-scheduler/src/state/derivation.rs); these constants must
        // track it exactly or collect would silently stop matching.
        let all = [
            STATUS_COMPLETED,
            STATUS_SKIPPED,
            STATUS_POISONED,
            STATUS_DEPENDENCY_FAILED,
            STATUS_CANCELLED,
        ];
        assert_eq!(
            all,
            [
                "completed",
                "skipped",
                "poisoned",
                "dependency_failed",
                "cancelled",
            ]
        );
        let unique: std::collections::BTreeSet<&str> = all.iter().copied().collect();
        assert_eq!(unique.len(), all.len());
    }

    #[test]
    fn supply_entry_wire_strings() {
        // Frozen wire strings: supply.jsonl is append-only across resumes,
        // so renaming a value would orphan prior entries.
        assert_eq!(SUPPLY_SOURCE_WORKLOAD_OUTPUT, "workload-output");
        assert_eq!(SUPPLY_SOURCE_TARGET_SUBSTITUTER, "target-substituter");
        assert_eq!(SUPPLY_SOURCE_EMBEDDED, "embedded");
        assert_eq!(SUPPLY_SOURCE_RELAY, "relay");
        assert_eq!(SUPPLY_SOURCE_NONE, "none");
        assert_eq!(SUPPLY_MECHANISM_DELEGATE, "delegate");
        assert_eq!(SUPPLY_MECHANISM_UPLOAD_BATCH, "upload-batch");
        assert_eq!(SUPPLY_MECHANISM_UPLOAD_STREAM, "upload-stream");
        assert_eq!(SUPPLY_MECHANISM_NONE, "none");
        assert_eq!(SUPPLY_OUTCOME_DELIVERED, "delivered");
        assert_eq!(SUPPLY_OUTCOME_ALREADY_PRESENT, "already-present");
        assert_eq!(SUPPLY_OUTCOME_DELEGATED, "delegated");
        assert_eq!(SUPPLY_OUTCOME_REFUSED, "refused");
        assert_eq!(SUPPLY_OUTCOME_UNAVAILABLE, "unavailable");
        assert_eq!(SUPPLY_OUTCOME_FAILED, "failed");
        assert_eq!(SUPPLY_OUTCOME_SKIPPED, "skipped");
        // The deferral detail is read back by the inline-resume gate from
        // journals written by EARLIER engine versions, so it is frozen the
        // same way the outcome vocabulary is.
        assert_eq!(SUPPLY_DETAIL_DEFERRED_INLINE, "deferred to inline top-up");

        let entry = SupplyEntry {
            path: "/nix/store/x".into(),
            source: SUPPLY_SOURCE_RELAY.into(),
            mechanism: SUPPLY_MECHANISM_UPLOAD_BATCH.into(),
            outcome: SUPPLY_OUTCOME_DELIVERED.into(),
            detail: None,
            batch_id: Some(3),
            bytes: Some(10),
            observed_at: now_rfc3339(),
        };
        let value = serde_json::to_value(&entry).unwrap();
        assert_eq!(value["source"], SUPPLY_SOURCE_RELAY);
        assert_eq!(value["mechanism"], SUPPLY_MECHANISM_UPLOAD_BATCH);
        assert_eq!(value["outcome"], SUPPLY_OUTCOME_DELIVERED);
        assert_eq!(value["batchId"], 3);
        assert_eq!(value["bytes"], 10);
        assert_eq!(value["observedAt"], entry.observed_at.as_str());
        let back: SupplyEntry = serde_json::from_value(value).unwrap();
        assert_eq!(back.path, entry.path);
        assert_eq!(back.source, entry.source);
        assert_eq!(back.mechanism, entry.mechanism);
        assert_eq!(back.outcome, entry.outcome);
        assert_eq!(back.detail, entry.detail);
        assert_eq!(back.batch_id, entry.batch_id);
        assert_eq!(back.bytes, entry.bytes);
        assert_eq!(back.observed_at, entry.observed_at);
    }

    /// The supply-outcome vocabulary is closed over [`SUPPLY_OUTCOMES`]
    /// (the class definition as data — folds and tests iterate it, never a
    /// hand-copied list), every value is unique, and the
    /// settlement/bookkeeping split is exactly the documented one: a
    /// settlement records a delivery resolution (delivered,
    /// already-present, delegated, refused, failed), bookkeeping records a
    /// per-request non-attempt (unavailable, skipped). Quantification
    /// domain: the SUPPLY_OUTCOME_* constants in this module via
    /// [`SUPPLY_OUTCOMES`]. Unknown (future) vocabulary must classify as
    /// bookkeeping so it can neither retire units nor displace a settled
    /// truth when an older reader folds a newer journal.
    #[test]
    fn supply_outcome_vocabulary_is_closed_and_split_into_settlement_vs_bookkeeping() {
        let unique: std::collections::BTreeSet<&str> = SUPPLY_OUTCOMES.iter().copied().collect();
        assert_eq!(unique.len(), SUPPLY_OUTCOMES.len());
        for outcome in SUPPLY_OUTCOMES {
            let is_settlement = supply_outcome_is_settlement(outcome);
            let is_bookkeeping =
                outcome == SUPPLY_OUTCOME_UNAVAILABLE || outcome == SUPPLY_OUTCOME_SKIPPED;
            assert!(
                is_settlement != is_bookkeeping,
                "{outcome} must be exactly one of settlement/bookkeeping"
            );
        }
        // Forward-compatibility direction: vocabulary this build does not
        // know falls through as bookkeeping.
        assert!(!supply_outcome_is_settlement("some-future-outcome"));
    }

    /// Journal row for the fold-owner tests. `observed_at` comes from the
    /// production clock helper unless a test pins it for as-of scoping.
    fn fold_row(path: &str, mechanism: &str, outcome: &str, detail: Option<&str>) -> SupplyEntry {
        SupplyEntry {
            path: path.to_string(),
            source: SUPPLY_SOURCE_EMBEDDED.to_string(),
            mechanism: mechanism.to_string(),
            outcome: outcome.to_string(),
            detail: detail.map(str::to_string),
            batch_id: None,
            bytes: None,
            observed_at: now_rfc3339(),
        }
    }

    /// The fold owner's displacement law, quantified over the FULL outcome
    /// vocabulary: for every member of [`SUPPLY_OUTCOMES`] (the closed
    /// class-as-data array — a new outcome constant joins this test by
    /// joining the array) appended AFTER an inline-deferral row, the
    /// projections must agree with [`supply_outcome_is_settlement`]:
    ///
    /// - the five settlements clear the deferral (the promise was
    ///   resolved) and become the path's settled truth;
    /// - the two bookkeeping outcomes (and any unknown future outcome)
    ///   leave the deferral OWED and stay invisible to
    ///   `latest_settlements` — a breaker-open `skipped` stamp on every
    ///   outstanding path must not erase the inline-resume gate's trigger
    ///   evidence (the fold this owner replaced kept raw latest-row-wins
    ///   and failed open exactly there).
    ///
    /// Both directions on the deferral lattice: a settlement clears the
    /// deferral, and a FRESH deferral row appended after a settlement (a
    /// re-run stage deferring the path again) re-marks it owed.
    #[test]
    fn supply_fold_projections_decide_every_outcome_against_a_deferral() {
        for outcome in SUPPLY_OUTCOMES {
            let path = "/nix/store/owed";
            let entries = vec![
                fold_row(
                    path,
                    SUPPLY_MECHANISM_NONE,
                    SUPPLY_OUTCOME_UNAVAILABLE,
                    Some(SUPPLY_DETAIL_DEFERRED_INLINE),
                ),
                fold_row(path, SUPPLY_MECHANISM_UPLOAD_BATCH, outcome, None),
            ];
            let fold = SupplyFold::collapse(&entries);
            let settles = supply_outcome_is_settlement(outcome);
            assert_eq!(
                fold.outstanding_inline_deferrals().is_empty(),
                settles,
                "{outcome}: a deferral is cleared by settlements only"
            );
            assert_eq!(
                fold.latest_settlements()
                    .get(path)
                    .map(|settled| settled.entry.outcome.as_str()),
                settles.then_some(outcome),
                "{outcome}: only settlements are visible as settled truth"
            );
            // The report projection sees every row class — the settled
            // outcome where one exists, else the latest bookkeeping row —
            // and in this two-row corpus both readings name the displacer.
            assert_eq!(
                fold.report_outcomes().get(path).copied(),
                Some(outcome),
                "{outcome}"
            );
        }
        // The PRODUCTION breaker shape, by the producer's own constant
        // (not a synthetic detail string): a deferral followed by the
        // breaker-open skip row the execute-stage top-up actually appends
        // (`skipped` + the gateway-unreachable detail) stays OWED. This is
        // the exact corpus that failed the inline-resume refusal open
        // before bookkeeping rows were filtered from deferral evidence.
        let path = "/nix/store/owed";
        let breaker_skipped = vec![
            fold_row(
                path,
                SUPPLY_MECHANISM_NONE,
                SUPPLY_OUTCOME_UNAVAILABLE,
                Some(SUPPLY_DETAIL_DEFERRED_INLINE),
            ),
            fold_row(
                path,
                SUPPLY_MECHANISM_UPLOAD_BATCH,
                SUPPLY_OUTCOME_SKIPPED,
                Some(crate::run::supply::exec::GATEWAY_UNREACHABLE),
            ),
        ];
        assert_eq!(
            SupplyFold::collapse(&breaker_skipped).outstanding_inline_deferrals(),
            vec![path],
            "a breaker-open skip row must not redeem the deferral"
        );
        // Unknown FUTURE vocabulary (a newer engine's journal read by this
        // build): bookkeeping in every projection — keeps the deferral
        // owed, invisible to settled truth.
        let future = vec![
            fold_row(
                path,
                SUPPLY_MECHANISM_NONE,
                SUPPLY_OUTCOME_UNAVAILABLE,
                Some(SUPPLY_DETAIL_DEFERRED_INLINE),
            ),
            fold_row(
                path,
                SUPPLY_MECHANISM_UPLOAD_BATCH,
                "some-future-outcome",
                None,
            ),
        ];
        let fold = SupplyFold::collapse(&future);
        assert_eq!(fold.outstanding_inline_deferrals(), vec![path]);
        assert!(fold.latest_settlements().is_empty());
        // Re-deferral direction: deferred → delivered → deferred again
        // (a re-run stage re-promising the path) is owed again.
        let redeferred = vec![
            fold_row(
                path,
                SUPPLY_MECHANISM_NONE,
                SUPPLY_OUTCOME_UNAVAILABLE,
                Some(SUPPLY_DETAIL_DEFERRED_INLINE),
            ),
            fold_row(
                path,
                SUPPLY_MECHANISM_UPLOAD_BATCH,
                SUPPLY_OUTCOME_DELIVERED,
                None,
            ),
            fold_row(
                path,
                SUPPLY_MECHANISM_NONE,
                SUPPLY_OUTCOME_UNAVAILABLE,
                Some(SUPPLY_DETAIL_DEFERRED_INLINE),
            ),
        ];
        assert_eq!(
            SupplyFold::collapse(&redeferred).outstanding_inline_deferrals(),
            vec![path],
            "a fresh deferral after a settlement re-marks the path owed"
        );
        // Empty journal: every projection is empty.
        let empty = SupplyFold::collapse(&[]);
        assert!(empty.outstanding_inline_deferrals().is_empty());
        assert!(empty.latest_settlements().is_empty());
        assert!(empty.report_outcomes().is_empty());
    }

    /// The settled undelivered-attempt count: refused/failed rows on the
    /// engine upload mechanisms each count one claim-resolved attempt —
    /// regardless of which row ends up latest — while delivered rows,
    /// non-upload mechanisms (a prefetch `failed` is not an upload
    /// attempt), and bookkeeping rows never count.
    #[test]
    fn supply_fold_counts_undelivered_upload_attempts_per_path() {
        let path = "/nix/store/retried";
        let entries = vec![
            fold_row(
                path,
                SUPPLY_MECHANISM_UPLOAD_BATCH,
                SUPPLY_OUTCOME_FAILED,
                None,
            ),
            fold_row(
                path,
                SUPPLY_MECHANISM_UPLOAD_STREAM,
                SUPPLY_OUTCOME_REFUSED,
                None,
            ),
            fold_row(path, SUPPLY_MECHANISM_DELEGATE, SUPPLY_OUTCOME_FAILED, None),
            fold_row(
                path,
                SUPPLY_MECHANISM_UPLOAD_BATCH,
                SUPPLY_OUTCOME_SKIPPED,
                None,
            ),
            fold_row(
                path,
                SUPPLY_MECHANISM_UPLOAD_BATCH,
                SUPPLY_OUTCOME_DELIVERED,
                None,
            ),
        ];
        let fold = SupplyFold::collapse(&entries);
        let settled = &fold.latest_settlements()[path];
        assert_eq!(settled.undelivered_upload_attempts, 2);
        assert_eq!(settled.entry.outcome, SUPPLY_OUTCOME_DELIVERED);
    }

    /// The as-of scope: rows observed after the cutoff are invisible to
    /// EVERY projection (settled truth, deferral evidence, report counts)
    /// — the backward question reads the journal as it stood — while the
    /// cutoff itself is inclusive (a batch's own top-up rows precede its
    /// started_at). Malformed timestamps degrade to visibility, never to
    /// silent dropping: an unparseable row stays visible and an
    /// unparseable cutoff disables the scoping — both are the pre-scoping
    /// whole-journal behavior.
    #[test]
    fn supply_fold_as_of_scopes_every_projection() {
        let early = "2026-06-03T10:00:00Z";
        let cutoff = "2026-06-03T10:05:00Z";
        let late = "2026-06-03T10:10:00Z";
        let at = |mut row: SupplyEntry, observed: &str| {
            row.observed_at = observed.to_string();
            row
        };
        let entries = vec![
            at(
                fold_row(
                    "/nix/store/masked",
                    SUPPLY_MECHANISM_UPLOAD_BATCH,
                    SUPPLY_OUTCOME_FAILED,
                    None,
                ),
                early,
            ),
            // A later delivery (a sibling batch's re-claimed top-up) must
            // not rewrite what stood at the cutoff.
            at(
                fold_row(
                    "/nix/store/masked",
                    SUPPLY_MECHANISM_UPLOAD_BATCH,
                    SUPPLY_OUTCOME_DELIVERED,
                    None,
                ),
                late,
            ),
            // Inclusive boundary: a row AT the cutoff is in scope.
            at(
                fold_row(
                    "/nix/store/boundary",
                    SUPPLY_MECHANISM_UPLOAD_BATCH,
                    SUPPLY_OUTCOME_DELIVERED,
                    None,
                ),
                cutoff,
            ),
            // A path that exists only after the cutoff: invisible.
            at(
                fold_row(
                    "/nix/store/later-only",
                    SUPPLY_MECHANISM_UPLOAD_BATCH,
                    SUPPLY_OUTCOME_DELIVERED,
                    None,
                ),
                late,
            ),
            at(
                fold_row(
                    "/nix/store/later-deferral",
                    SUPPLY_MECHANISM_NONE,
                    SUPPLY_OUTCOME_UNAVAILABLE,
                    Some(SUPPLY_DETAIL_DEFERRED_INLINE),
                ),
                late,
            ),
            // Unparseable observed_at: kept visible (degrade documented on
            // collapse_as_of).
            at(
                fold_row(
                    "/nix/store/unparseable",
                    SUPPLY_MECHANISM_UPLOAD_BATCH,
                    SUPPLY_OUTCOME_REFUSED,
                    None,
                ),
                "not-a-timestamp",
            ),
        ];
        let fold = SupplyFold::collapse_as_of(&entries, cutoff);
        let settled = fold.latest_settlements();
        assert_eq!(
            settled["/nix/store/masked"].entry.outcome, SUPPLY_OUTCOME_FAILED,
            "a post-cutoff delivery must not mask the at-cutoff failure"
        );
        assert_eq!(
            settled["/nix/store/boundary"].entry.outcome,
            SUPPLY_OUTCOME_DELIVERED
        );
        assert!(!settled.contains_key("/nix/store/later-only"));
        assert_eq!(
            settled["/nix/store/unparseable"].entry.outcome,
            SUPPLY_OUTCOME_REFUSED
        );
        assert!(
            fold.outstanding_inline_deferrals().is_empty(),
            "a post-cutoff deferral is invisible to the as-of fold"
        );
        assert!(!fold.report_outcomes().contains_key("/nix/store/later-only"));
        // Whole-journal fold over the same rows: the later rows ARE
        // visible (the forward question) — the two constructors answer
        // different questions over one journal.
        let unscoped = SupplyFold::collapse(&entries);
        assert_eq!(
            unscoped.latest_settlements()["/nix/store/masked"]
                .entry
                .outcome,
            SUPPLY_OUTCOME_DELIVERED
        );
        assert_eq!(
            unscoped.outstanding_inline_deferrals(),
            vec!["/nix/store/later-deferral"]
        );
        // Unparseable cutoff: scoping disabled, whole-journal semantics.
        let unparseable_cutoff = SupplyFold::collapse_as_of(&entries, "garbage");
        assert_eq!(
            unparseable_cutoff.latest_settlements()["/nix/store/masked"]
                .entry
                .outcome,
            SUPPLY_OUTCOME_DELIVERED
        );
        // Producer-format tie: the cutoff/row format this fold parses is
        // exactly what the production clock helper emits.
        let now_row = fold_row(
            "/nix/store/now",
            SUPPLY_MECHANISM_UPLOAD_BATCH,
            SUPPLY_OUTCOME_DELIVERED,
            None,
        );
        let now_entries = vec![now_row];
        let now_fold = SupplyFold::collapse_as_of(&now_entries, &now_rfc3339());
        assert!(now_fold.latest_settlements().contains_key("/nix/store/now"));
    }

    #[test]
    fn dispatch_entry_uses_camel_case_wire_names() {
        let entry = DispatchEntry {
            request_index: 4,
            session: 7,
            due_offset_s: 12.5,
            dispatched_at: "2026-05-28T00:00:00Z".into(),
            dispatch_lateness_ms: 250,
            deadline_secs: 1800,
            interruption_armed: true,
            interruption_fired: false,
            attempts: 2,
            batch_ids: vec![11, 12],
            drvs: vec!["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv".into()],
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains(r#""requestIndex":4"#), "{json}");
        assert!(json.contains(r#""dueOffsetS":12.5"#), "{json}");
        assert!(json.contains(r#""dispatchLatenessMs":250"#), "{json}");
        assert!(json.contains(r#""deadlineSecs":1800"#), "{json}");
        assert!(json.contains(r#""interruptionArmed":true"#), "{json}");
        assert!(json.contains(r#""interruptionFired":false"#), "{json}");
        assert!(json.contains(r#""batchIds":[11,12]"#), "{json}");
        assert!(!json.contains("request_index"), "{json}");
        let back: DispatchEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.request_index, 4);
        assert_eq!(back.session, 7);
        assert_eq!(back.attempts, 2);
        assert_eq!(back.batch_ids, vec![11, 12]);
    }

    #[test]
    fn batch_kind_vocabulary_is_fixed_and_distinct() {
        // Frozen wire strings: batches.jsonl is append-only across resumes,
        // so renaming a kind would orphan prior entries.
        assert_eq!(BATCH_KIND_SUBMIT, "submit");
        assert_eq!(BATCH_KIND_TIMED, "timed");
        assert_ne!(BATCH_KIND_SUBMIT, BATCH_KIND_TIMED);
    }

    #[test]
    fn build_status_name_round_trips_every_variant() {
        let all = [
            BuildStatus::Built,
            BuildStatus::Substituted,
            BuildStatus::AlreadyValid,
            BuildStatus::PermanentFailure,
            BuildStatus::InputRejected,
            BuildStatus::OutputRejected,
            BuildStatus::TransientFailure,
            BuildStatus::CachedFailure,
            BuildStatus::TimedOut,
            BuildStatus::MiscFailure,
            BuildStatus::DependencyFailed,
            BuildStatus::LogLimitExceeded,
            BuildStatus::NotDeterministic,
            BuildStatus::ResolvesToAlreadyValid,
            BuildStatus::NoSubstituters,
        ];
        for status in all {
            assert_eq!(
                build_status_from_name(build_status_name(status)),
                Some(status),
                "{status:?}"
            );
        }
        assert_eq!(build_status_from_name("nonsense"), None);
    }

    /// The shared keyed→outcome mapping is the single place the positional
    /// `BuildPathsWithResults` correlation contract is enforced: a result
    /// count differing from the submitted roots warns (the only breadcrumb
    /// every caller gets), a short vector still maps the prefix it can, and
    /// extra entries are ignored.
    #[test]
    #[tracing_test::traced_test]
    fn path_outcomes_from_keyed_warns_on_result_count_mismatch() {
        use rio_nix::protocol::build::BuildResult;

        let roots = vec![
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a.drv".to_string(),
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b.drv".to_string(),
        ];
        // Three keyed results for two roots: slices of this vector script
        // the matched, short, and oversized daemon answers.
        let keyed: Vec<KeyedBuildResult> = (0..3)
            .map(|i| KeyedBuildResult {
                derived_path: format!("/nix/store/{i:032}-r{i}.drv!*"),
                result: BuildResult {
                    status: BuildStatus::Built,
                    ..BuildResult::default()
                },
            })
            .collect();

        // Matched lengths: every root mapped, no warning.
        let full = path_outcomes_from_keyed(&roots, &keyed[..2]);
        assert_eq!(full.len(), 2);
        assert_eq!(full[0].drv_path, roots[0]);
        assert_eq!(full[1].drv_path, roots[1]);
        assert!(!logs_contain("different result count"));

        // Short answer: the prefix maps, the warning fires with both counts.
        let short = path_outcomes_from_keyed(&roots, &keyed[..1]);
        assert_eq!(short.len(), 1);
        assert_eq!(short[0].drv_path, roots[0]);
        assert!(logs_contain(
            "BuildPathsWithResults returned a different result count than requested roots"
        ));
        assert!(logs_contain("requested=2"));
        assert!(logs_contain("returned=1"));

        // Oversized answer: extra entries are ignored, and the mismatch
        // still warns.
        let extra = path_outcomes_from_keyed(&roots, &keyed);
        assert_eq!(extra.len(), 2);
        assert!(logs_contain("returned=3"));
    }

    #[test]
    fn path_outcome_serializes_camel_case() {
        let drv = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv";
        let rec = BatchRecord {
            batch_id: 7,
            kind: BATCH_KIND_SUBMIT.to_string(),
            jobs: vec!["x.x86_64-linux".into()],
            root_drvs: vec![drv.into()],
            est_nodes: 1,
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".into()),
            started_at: "2026-05-26T00:00:00Z".into(),
            finished_at: Some("2026-05-26T00:05:00Z".into()),
            results: vec![PathOutcome {
                drv_path: drv.into(),
                status: build_status_name(BuildStatus::Built).into(),
                error_msg: "".into(),
                start_time: 1,
                stop_time: 2,
            }],
            reasons: BTreeMap::new(),
            stderr_tail: None,
            engine_cancelled: false,
            disconnect_deadline_fired: false,
            interruption_drvs: Vec::new(),
            import_skipped_drvs: Vec::new(),
            import_skipped_by_root: BTreeMap::new(),
            probe: false,
            confirmation_attempt: 0,
            topup_delivered: true,
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains(r#""results":[{"drvPath":"#), "{json}");
        assert!(json.contains(r#""topupDelivered":true"#), "{json}");
        assert!(json.contains(r#""status":"Built""#), "{json}");
        assert!(json.contains(r#""errorMsg":"""#), "{json}");
        assert!(json.contains(r#""startTime":1"#), "{json}");
        assert!(json.contains(r#""stopTime":2"#), "{json}");
        assert!(!json.contains("drv_path"), "{json}");

        // A batches.jsonl line written before the client-ops cutover (no
        // `results` key, a stale `exitCode` key) still deserializes: the
        // array defaults to empty and the unknown key is ignored. Lines
        // written before timed scheduling existed lack `interruptionDrvs`
        // the same way (defaults to empty), lines written before the
        // deadline-cause bit existed lack `disconnectDeadlineFired`
        // (defaults to false), lines written before canary probing
        // existed lack `probe` (defaults to false — they were all
        // full-wave submissions), and lines written before confirmation
        // intent existed lack `confirmationAttempt` (defaults to 0 —
        // their retry successes were belt-dropped and stay that way).
        // Lines written before the delivery-proof bit existed lack
        // `topupDelivered` the same way (defaults to false: bare batch
        // membership stops counting as inline-delivery proof, so those
        // campaigns re-run the supply stage on their next inline resume
        // instead of submitting undelivered work).
        let old = r#"{"batchId":3,"kind":"submit","jobs":["x.x86_64-linux"],"rootDrvs":["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv"],"estNodes":1,"buildId":null,"startedAt":"2026-05-26T00:00:00Z","finishedAt":null,"exitCode":1,"reasons":{},"stderrTail":"tail","engineCancelled":false}"#;
        let parsed: BatchRecord = serde_json::from_str(old).unwrap();
        assert!(parsed.results.is_empty());
        assert!(parsed.interruption_drvs.is_empty());
        assert!(!parsed.disconnect_deadline_fired);
        // Lines written before the import-skip breadcrumb existed lack
        // `importSkippedDrvs` the same way (defaults to empty), and lines
        // written before the per-root attribution existed lack
        // `importSkippedByRoot` (defaults to empty: their skips predate
        // the attribution path and their members are not re-classified).
        assert!(parsed.import_skipped_drvs.is_empty());
        assert!(parsed.import_skipped_by_root.is_empty());
        assert!(!parsed.probe);
        assert_eq!(parsed.confirmation_attempt, 0);
        assert!(!parsed.topup_delivered);
        assert_eq!(parsed.batch_id, 3);
        assert_eq!(parsed.stderr_tail.as_deref(), Some("tail"));
    }

    #[test]
    fn job_record_uses_camel_case_wire_names() {
        let rec = JobRecord {
            job: "hello.x86_64-linux".into(),
            system: "x86_64-linux".into(),
            drv_path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.drv".into(),
            mode: "leaf".into(),
            attempts: 1,
            build_ids: vec!["0193e000-0000-7000-8000-000000000001".into()],
            rio: RioSide {
                outcome: "built".into(),
                ..RioSide::default()
            },
            expected: ExpectedSide {
                outcome: "built".into(),
                ..ExpectedSide::default()
            },
            nar_compare: BTreeMap::new(),
            verdict: Some(Verdict::MatchBuilt.as_str().into()),
            disposition: None,
            cascaded: false,
            failure_cause: None,
            flaky: false,
            signature: None,
            log_key: None,
            repro: "cargo xtask replay repro c1 \
                    /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.drv"
                .into(),
            evidence: None,
            updated_at: "2026-05-26T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains("\"drvPath\""), "{json}");
        assert!(json.contains("\"buildIds\""), "{json}");
        assert!(json.contains("\"narCompare\""), "{json}");
        assert!(json.contains("\"verdict\":\"match-built\""), "{json}");
        assert!(!json.contains("\"bucket\""), "{json}");
        assert!(!json.contains("\"drv_path\""), "{json}");
        // The recorded-truth side is keyed `expected` (the legacy `hydra`
        // key is gone with the truth-source-neutral rename).
        assert!(
            json.contains("\"expected\":{\"outcome\":\"built\""),
            "{json}"
        );
        assert!(!json.contains("\"hydra\""), "{json}");
        // Exactly one of verdict/disposition is set, so the unset side (and
        // the absent failure cause) never appear on the wire.
        assert!(!json.contains("\"disposition\""), "{json}");
        assert!(!json.contains("\"failureCause\""), "{json}");
        assert_eq!(
            rec.class(),
            Some(UnifiedClass::Verdict(Verdict::MatchBuilt))
        );

        // A failure-class record carries its cause under the camelCase key
        // and parses back to its verdict.
        let mut failed = rec.clone();
        failed.verdict = Some(Verdict::UnexpectedFailure.as_str().into());
        failed.failure_cause = Some("genuine".into());
        failed.flaky = false;
        let json = serde_json::to_string(&failed).unwrap();
        assert!(json.contains("\"failureCause\":\"genuine\""), "{json}");
        assert!(!json.contains("\"failure_cause\""), "{json}");
        assert_eq!(
            failed.class(),
            Some(UnifiedClass::Verdict(Verdict::UnexpectedFailure))
        );

        // A disposition-only record reads back as its disposition.
        let mut excluded = rec.clone();
        excluded.verdict = None;
        excluded.disposition = Some(Disposition::Filtered.as_str().into());
        let json = serde_json::to_string(&excluded).unwrap();
        assert!(json.contains("\"disposition\":\"filtered\""), "{json}");
        assert!(!json.contains("\"verdict\""), "{json}");
        assert_eq!(
            excluded.class(),
            Some(UnifiedClass::Disposition(Disposition::Filtered))
        );
    }

    #[test]
    fn pause_state_or_logic() {
        let p = PauseState::default();
        assert!(!p.paused());
        p.set_backpressure(true);
        assert!(p.paused());
        p.set_backpressure(false);
        p.set_manual(true);
        assert!(p.paused());
    }

    /// The probe channel is a single forward-only cycle: at most one token,
    /// exactly one redeemer per grant, no re-grant while a probe is
    /// anywhere in flight, and abort returns a redeemed-but-unreleased
    /// cycle to idle.
    #[test]
    fn pause_state_probe_channel_is_one_cycle_at_a_time() {
        let p = PauseState::default();
        assert!(p.probe_idle());
        assert!(!p.take_probe(), "no token granted yet");
        p.grant_probe();
        p.grant_probe(); // double-grant collapses to one token
        assert!(!p.probe_idle(), "a granted token blocks re-granting");
        assert!(p.take_probe(), "the granted token is redeemable once");
        assert!(!p.take_probe(), "a redeemed token cannot be redeemed again");
        assert!(
            !p.probe_idle(),
            "a redeemed-but-unreleased probe still blocks re-granting"
        );

        // Granting mid-cycle is a no-op: the channel stays in Redeemed, so
        // the released batch below is still accepted.
        p.grant_probe();
        assert!(!p.take_probe(), "mid-cycle grant must not mint a token");

        assert!(p.probe_batch().is_none());
        p.set_probe_batch(7, vec!["a.x86_64-linux".to_string()]);
        assert!(!p.probe_idle(), "a released probe blocks re-granting");
        assert_eq!(
            p.probe_batch(),
            Some((7, vec!["a.x86_64-linux".to_string()]))
        );
        p.clear_probe();
        assert!(p.probe_idle());

        // The abort path: a redeemed probe dropped without submission
        // returns the channel to idle (and aborting mid-Released is a
        // no-op — only clear_probe ends a released cycle).
        p.grant_probe();
        assert!(p.take_probe());
        p.abort_probe();
        assert!(p.probe_idle(), "aborted redemption frees the channel");
    }

    #[test]
    fn terminal_class_predicate() {
        // Any verdict is terminal.
        let verdict = Some(Verdict::MatchBuilt.as_str().to_string());
        assert!(is_terminal_class(&verdict, &None));
        let verdict = Some(Verdict::InfraIndeterminate.as_str().to_string());
        assert!(is_terminal_class(&verdict, &None));
        // Any disposition except not-attempted is terminal.
        let disposition = Some(Disposition::CachedPrior.as_str().to_string());
        assert!(is_terminal_class(&None, &disposition));
        let disposition = Some(Disposition::NotAttempted.as_str().to_string());
        assert!(!is_terminal_class(&None, &disposition));
        // A record with neither field has not been classified yet.
        assert!(!is_terminal_class(&None, &None));
    }

    /// Work-evidence projection over the WHOLE unified vocabulary, as
    /// data, through the wire-string chokepoint the production consumers
    /// use. Quantification domain: `Verdict::ALL` × `Disposition::ALL` —
    /// the same closed enums every record writer goes through — plus the
    /// out-of-vocabulary and unclassified rows.
    ///
    /// Every member is asserted on BOTH evidence axes so the two
    /// projections cannot silently merge again: terminality (scheduling
    /// liveness) and work evidence (did the cluster execute). The
    /// expected rows derive from the classes' own contracts:
    /// `infra-indeterminate` states the replayed outcome cannot be
    /// trusted (rio-side infrastructure failed — the class every
    /// budget-exhaustion terminal mints DURING an outage), and every
    /// disposition is a non-attempt class by construction (dispositions
    /// answer "why was the unit never compared"; supply-failed /
    /// upload-rejected are minted BY dead-uploads outages). A new
    /// verdict or disposition fails the exhaustive matches behind this
    /// test until its evidence grade is decided.
    #[test]
    #[tracing_test::traced_test]
    fn work_evidence_projection_covers_the_whole_vocabulary() {
        for verdict in Verdict::ALL {
            let expected = verdict != Verdict::InfraIndeterminate;
            let raw = Some(verdict.as_str().to_string());
            assert_eq!(
                is_work_evidencing_terminal(&raw, &None),
                expected,
                "{verdict:?}"
            );
            // Work evidence implies terminality, never the reverse.
            assert!(is_terminal_class(&raw, &None), "{verdict:?}");
            // The wire chokepoint round-trips the vocabulary.
            assert_eq!(Verdict::from_wire(verdict.as_str()), Some(verdict));
        }
        for disposition in Disposition::ALL {
            let raw = Some(disposition.as_str().to_string());
            assert!(
                !is_work_evidencing_terminal(&None, &raw),
                "{disposition:?}: dispositions are non-attempt classes — never work evidence"
            );
            assert_eq!(
                Disposition::from_wire(disposition.as_str()),
                Some(disposition)
            );
        }
        // Out-of-vocabulary strings are explicit unknowns: fail-closed
        // (NOT evidencing) with a loud warning — an unrecognized
        // outage-minted class must not satisfy a probe's success witness.
        assert!(!is_work_evidencing_terminal(
            &Some("some-future-verdict".to_string()),
            &None
        ));
        assert!(logs_contain("record verdict is outside the vocabulary"));
        assert!(!is_work_evidencing_terminal(
            &None,
            &Some("some-future-disposition".to_string())
        ));
        assert!(logs_contain("record disposition is outside the vocabulary"));
        // An unclassified record evidences nothing.
        assert!(!is_work_evidencing_terminal(&None, &None));
    }

    #[test]
    fn verdict_and_disposition_wire_strings_are_frozen() {
        let verdicts: Vec<&str> = Verdict::ALL.iter().map(|v| v.as_str()).collect();
        assert_eq!(
            verdicts,
            [
                "match-built",
                "output-divergence",
                "match-failed",
                "unexpected-failure",
                "unexpected-dependency-failure",
                "unexpected-success",
                "source-unavailable",
                "infra-indeterminate",
                "truth-indeterminate",
                "no-truth",
                "interruption-replayed",
                "interruption-not-reproduced",
            ]
        );
        let dispositions: Vec<&str> = Disposition::ALL.iter().map(|d| d.as_str()).collect();
        assert_eq!(
            dispositions,
            [
                "filtered",
                "eval-error",
                "identity-divergent",
                "not-attemptable",
                "demoted-impure",
                "cached-prior",
                "upload-rejected",
                "supply-failed",
                "target-substituted",
                "not-attempted",
            ]
        );
        // serde forms match as_str for every variant, both directions.
        for v in Verdict::ALL {
            let json = serde_json::to_string(&v).unwrap();
            assert_eq!(json, format!("\"{}\"", v.as_str()), "{v:?}");
            assert_eq!(serde_json::from_str::<Verdict>(&json).unwrap(), v);
        }
        for d in Disposition::ALL {
            let json = serde_json::to_string(&d).unwrap();
            assert_eq!(json, format!("\"{}\"", d.as_str()), "{d:?}");
            assert_eq!(serde_json::from_str::<Disposition>(&json).unwrap(), d);
        }
        // Verdict and disposition vocabularies never overlap.
        let overlap: Vec<&str> = verdicts
            .iter()
            .copied()
            .filter(|v| dispositions.contains(v))
            .collect();
        assert!(overlap.is_empty(), "{overlap:?}");
    }

    #[test]
    fn legacy_bucket_mapping_is_total_and_count_preserving_per_bucket() {
        // Every legacy bucket string maps to exactly one unified class; the
        // only data-dependent split is match-built with differing NAR hashes.
        let cases: [(&str, bool, UnifiedClass); 18] = [
            (
                "match-built",
                false,
                UnifiedClass::Verdict(Verdict::MatchBuilt),
            ),
            (
                "match-built",
                true,
                UnifiedClass::Verdict(Verdict::OutputDivergence),
            ),
            (
                "rio-only-failure",
                false,
                UnifiedClass::Verdict(Verdict::UnexpectedFailure),
            ),
            (
                "rio-dependency-failure",
                false,
                UnifiedClass::Verdict(Verdict::UnexpectedDependencyFailure),
            ),
            (
                "rio-infra-failure",
                false,
                UnifiedClass::Verdict(Verdict::InfraIndeterminate),
            ),
            (
                "upstream-source-unavailable",
                false,
                UnifiedClass::Verdict(Verdict::SourceUnavailable),
            ),
            (
                "hydra-only-failure",
                false,
                UnifiedClass::Verdict(Verdict::UnexpectedSuccess),
            ),
            (
                "both-failed",
                false,
                UnifiedClass::Verdict(Verdict::MatchFailed),
            ),
            (
                "hydra-unknown",
                false,
                UnifiedClass::Verdict(Verdict::NoTruth),
            ),
            (
                "interruption-replayed",
                false,
                UnifiedClass::Verdict(Verdict::InterruptionReplayed),
            ),
            (
                "interruption-not-reproduced",
                false,
                UnifiedClass::Verdict(Verdict::InterruptionNotReproduced),
            ),
            (
                "eval-divergence",
                false,
                UnifiedClass::Disposition(Disposition::IdentityDivergent),
            ),
            (
                "eval-error",
                false,
                UnifiedClass::Disposition(Disposition::EvalError),
            ),
            (
                "skipped",
                false,
                UnifiedClass::Disposition(Disposition::Filtered),
            ),
            (
                "not-attemptable",
                false,
                UnifiedClass::Disposition(Disposition::NotAttemptable),
            ),
            (
                "not-attempted",
                false,
                UnifiedClass::Disposition(Disposition::NotAttempted),
            ),
            (
                "cached-prior",
                false,
                UnifiedClass::Disposition(Disposition::CachedPrior),
            ),
            (
                "target-substituted",
                false,
                UnifiedClass::Disposition(Disposition::TargetSubstituted),
            ),
        ];
        for (bucket, nar_differs, expected) in cases {
            assert_eq!(
                unified_from_legacy_bucket(bucket, nar_differs),
                Some(expected),
                "{bucket} nar_differs={nar_differs}"
            );
        }
        assert_eq!(unified_from_legacy_bucket("no-such-bucket", false), None);
    }

    #[test]
    fn unified_class_terminal_predicate() {
        // Every verdict is terminal; every disposition except not-attempted is
        // terminal.
        for v in Verdict::ALL {
            assert!(UnifiedClass::Verdict(v).is_terminal(), "{v:?}");
        }
        for d in Disposition::ALL {
            assert_eq!(
                UnifiedClass::Disposition(d).is_terminal(),
                d != Disposition::NotAttempted,
                "{d:?}"
            );
        }
    }
}
