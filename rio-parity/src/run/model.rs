//! Shared campaign data model: per-job records (results.jsonl),
//! hydra-truth cache entries, warm dispositions, per-path supply
//! outcomes (supply.jsonl), timed dispatch records (dispatch.jsonl),
//! batch records, and the engine's pause state. Wire field names are
//! camelCase, matching the rest of the campaign artifacts.
//!
//! Convention: the stringly-typed `bucket`/`outcome` fields in the JSONL
//! record structs stay `String` on the wire, but they MUST be written via
//! the corresponding enums ([`Bucket::as_str`], [`HydraOutcome`] /
//! [`RioOutcome`] serde forms) — never hand-typed literals — so the
//! record values can never drift from the enum vocabulary.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

use rio_nix::protocol::build::BuildStatus;
use serde::{Deserialize, Serialize};

/// Wall-clock now as RFC3339 (UTC). Single helper so records stay uniform.
pub fn now_rfc3339() -> String {
    jiff::Timestamp::now().to_string()
}

/// Parse an RFC3339 timestamp into unix seconds; None on parse failure.
pub fn rfc3339_to_unix(ts: &str) -> Option<i64> {
    ts.parse::<jiff::Timestamp>().ok().map(|t| t.as_second())
}

/// Hydra-side outcome for one job (derived from cache.nixos.org narinfo
/// presence, plus exact buildstatus for scoped campaigns).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HydraOutcome {
    Built,
    Failed,
    Unknown,
}

impl HydraOutcome {
    /// The [`HydraSide::outcome`] wire string — the same kebab-case name
    /// this enum's serde form uses, so record writers never hand-type it.
    pub fn as_str(&self) -> &'static str {
        match self {
            HydraOutcome::Built => "built",
            HydraOutcome::Failed => "failed",
            HydraOutcome::Unknown => "unknown",
        }
    }
}

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

/// Final comparison bucket for one job. The string forms are the
/// kebab-case names used as the `bucket` field in results.jsonl and as
/// the `buckets/<bucket>.jsonl` file names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum Bucket {
    MatchBuilt,
    RioOnlyFailure,
    RioDependencyFailure,
    RioInfraFailure,
    UpstreamSourceUnavailable,
    TargetSubstituted,
    CachedPrior,
    NotAttemptable,
    NotAttempted,
    HydraUnknown,
    EvalDivergence,
    HydraOnlyFailure,
    BothFailed,
    EvalError,
    Skipped,
}

impl Bucket {
    pub fn as_str(&self) -> &'static str {
        match self {
            Bucket::MatchBuilt => "match-built",
            Bucket::RioOnlyFailure => "rio-only-failure",
            Bucket::RioDependencyFailure => "rio-dependency-failure",
            Bucket::RioInfraFailure => "rio-infra-failure",
            Bucket::UpstreamSourceUnavailable => "upstream-source-unavailable",
            Bucket::TargetSubstituted => "target-substituted",
            Bucket::CachedPrior => "cached-prior",
            Bucket::NotAttemptable => "not-attemptable",
            Bucket::NotAttempted => "not-attempted",
            Bucket::HydraUnknown => "hydra-unknown",
            Bucket::EvalDivergence => "eval-divergence",
            Bucket::HydraOnlyFailure => "hydra-only-failure",
            Bucket::BothFailed => "both-failed",
            Bucket::EvalError => "eval-error",
            Bucket::Skipped => "skipped",
        }
    }

    pub const ALL: [Bucket; 15] = [
        Bucket::MatchBuilt,
        Bucket::RioOnlyFailure,
        Bucket::RioDependencyFailure,
        Bucket::RioInfraFailure,
        Bucket::UpstreamSourceUnavailable,
        Bucket::TargetSubstituted,
        Bucket::CachedPrior,
        Bucket::NotAttemptable,
        Bucket::NotAttempted,
        Bucket::HydraUnknown,
        Bucket::EvalDivergence,
        Bucket::HydraOnlyFailure,
        Bucket::BothFailed,
        Bucket::EvalError,
        Bucket::Skipped,
    ];
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
    /// "not-attempted", "target-failed", "dependency-failed") or a
    /// plan-time exclusion value ("skipped", "not-attemptable",
    /// "cached-prior", "eval-error").
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

/// One Hydra-published output path with its NAR identity.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HydraOutput {
    pub narinfo_present: bool,
    pub nar_hash: Option<String>,
    pub nar_size: Option<u64>,
}

/// Everything observed on the Hydra side for one job.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HydraSide {
    /// "built" | "failed" | "unknown"
    pub outcome: String,
    pub buildstatus: Option<i64>,
    pub outputs: BTreeMap<String, HydraOutput>,
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
    pub hydra: HydraSide,
    /// Per-output NAR comparison verdict: "equal" | "differs" | "not-comparable".
    pub nar_compare: BTreeMap<String, String>,
    pub bucket: String,
    /// True when this job is a cascaded dependent counted under an
    /// infra/source-rot root cause rather than charged as its own failure.
    #[serde(default)]
    pub cascaded: bool,
    pub signature: Option<String>,
    pub log_key: Option<String>,
    pub repro: String,
    /// Evidence quality flag, e.g. "log-tail-only".
    #[serde(default)]
    pub evidence: Option<String>,
    pub updated_at: String,
}

/// Whether a record is terminal: every classified bucket except
/// `not-attempted` is terminal (an empty bucket means the record has not
/// been classified yet).
pub fn is_terminal_bucket(bucket: &str) -> bool {
    bucket != Bucket::NotAttempted.as_str() && !bucket.is_empty()
}

/// One line of hydra.jsonl — raw narinfo fetch cache keyed by output path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HydraEntry {
    pub path: String,
    pub found: bool,
    pub nar_hash: Option<String>,
    pub nar_size: Option<u64>,
    pub deriver: Option<String>,
    pub fetched_at: String,
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
    fn bucket_strings_match_design_names() {
        assert_eq!(Bucket::MatchBuilt.as_str(), "match-built");
        assert_eq!(
            Bucket::UpstreamSourceUnavailable.as_str(),
            "upstream-source-unavailable"
        );
        assert_eq!(Bucket::ALL.len(), 15);
        // serde uses the same kebab-case names
        assert_eq!(
            serde_json::to_string(&Bucket::CachedPrior).unwrap(),
            "\"cached-prior\""
        );
        let b: Bucket = serde_json::from_str("\"rio-infra-failure\"").unwrap();
        assert_eq!(b, Bucket::RioInfraFailure);
    }

    #[test]
    fn bucket_serde_form_matches_as_str_for_every_variant() {
        for bucket in Bucket::ALL {
            let json = serde_json::to_string(&bucket).unwrap();
            assert_eq!(json, format!("\"{}\"", bucket.as_str()), "{bucket:?}");
            let back: Bucket = serde_json::from_str(&json).unwrap();
            assert_eq!(back, bucket);
        }
    }

    #[test]
    fn outcome_strings_match_their_serde_forms() {
        for hydra in [
            HydraOutcome::Built,
            HydraOutcome::Failed,
            HydraOutcome::Unknown,
        ] {
            assert_eq!(
                serde_json::to_string(&hydra).unwrap(),
                format!("\"{}\"", hydra.as_str()),
                "{hydra:?}"
            );
        }
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
        // Frozen wire string: batches.jsonl is append-only across resumes,
        // so renaming a kind would orphan prior entries.
        assert_eq!(BATCH_KIND_SUBMIT, "submit");
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
        // array defaults to empty and the unknown key is ignored.
        let old = r#"{"batchId":3,"kind":"submit","jobs":["x.x86_64-linux"],"rootDrvs":["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv"],"estNodes":1,"buildId":null,"startedAt":"2026-05-26T00:00:00Z","finishedAt":null,"exitCode":1,"reasons":{},"stderrTail":"tail","engineCancelled":false}"#;
        let parsed: BatchRecord = serde_json::from_str(old).unwrap();
        assert!(parsed.results.is_empty());
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
            hydra: HydraSide {
                outcome: "built".into(),
                ..HydraSide::default()
            },
            nar_compare: BTreeMap::new(),
            bucket: Bucket::MatchBuilt.as_str().into(),
            cascaded: false,
            signature: None,
            log_key: None,
            repro: "nix build ...".into(),
            evidence: None,
            updated_at: "2026-05-26T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains("\"drvPath\""), "{json}");
        assert!(json.contains("\"buildIds\""), "{json}");
        assert!(json.contains("\"narCompare\""), "{json}");
        assert!(!json.contains("\"drv_path\""), "{json}");
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
    fn terminal_bucket_predicate() {
        assert!(is_terminal_bucket("match-built"));
        assert!(is_terminal_bucket("rio-infra-failure"));
        assert!(!is_terminal_bucket("not-attempted"));
        assert!(!is_terminal_bucket(""));
    }
}
