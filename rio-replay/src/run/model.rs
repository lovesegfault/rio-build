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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RioOutput {
    pub path: String,
    pub nar_hash: Option<String>,
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExpectedOutput {
    pub narinfo_present: bool,
    pub nar_hash: Option<String>,
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
pub fn is_terminal_class(verdict: &Option<String>, disposition: &Option<String>) -> bool {
    if verdict.is_some() {
        return true;
    }
    disposition
        .as_deref()
        .is_some_and(|d| d != Disposition::NotAttempted.as_str())
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
    /// Root drvs in this submission for which a recorded interruption
    /// (cancellation or client disconnect) was armed by the timed
    /// dispatcher. Empty for every non-timed batch.
    #[serde(default)]
    pub interruption_drvs: Vec<String>,
}

/// Cross-component pause flags: a manual operator pause and the
/// engine's own backpressure pause, OR-ed into one "submission paused"
/// signal.
#[derive(Debug, Default)]
pub struct PauseState {
    manual: AtomicBool,
    backpressure: AtomicBool,
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
            interruption_drvs: Vec::new(),
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains(r#""results":[{"drvPath":"#), "{json}");
        assert!(json.contains(r#""status":"Built""#), "{json}");
        assert!(json.contains(r#""errorMsg":"""#), "{json}");
        assert!(json.contains(r#""startTime":1"#), "{json}");
        assert!(json.contains(r#""stopTime":2"#), "{json}");
        assert!(!json.contains("drv_path"), "{json}");

        // A batches.jsonl line written before the client-ops cutover (no
        // `results` key, a stale `exitCode` key) still deserializes: the
        // array defaults to empty and the unknown key is ignored. Lines
        // written before timed scheduling existed lack `interruptionDrvs`
        // the same way; it defaults to empty.
        let old = r#"{"batchId":3,"kind":"submit","jobs":["x.x86_64-linux"],"rootDrvs":["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv"],"estNodes":1,"buildId":null,"startedAt":"2026-05-26T00:00:00Z","finishedAt":null,"exitCode":1,"reasons":{},"stderrTail":"tail","engineCancelled":false}"#;
        let parsed: BatchRecord = serde_json::from_str(old).unwrap();
        assert!(parsed.results.is_empty());
        assert!(parsed.interruption_drvs.is_empty());
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
