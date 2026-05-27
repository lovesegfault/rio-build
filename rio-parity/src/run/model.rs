//! Shared campaign data model: per-job records (results.jsonl),
//! hydra-truth cache entries, warm dispositions, batch records, and the
//! engine's pause state. Wire field names are camelCase, matching the
//! rest of the campaign artifacts.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

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
/// the buckets/<bucket>.jsonl file names.
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
    /// Raw scheduler derivations.status for the target drv, when observed.
    pub status: Option<String>,
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

/// One line of warm.jsonl — per-path warm-stage disposition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WarmEntry {
    pub path: String,
    pub drv_path: Option<String>,
    /// "not-found-upstream" | "already-present" | "substituted" |
    /// "failed-after-retries" | "built-fallback"
    pub disposition: String,
    pub batch_id: Option<u64>,
    pub observed_at: String,
}

/// One line of batches.jsonl — engine-internal bookkeeping for resume and
/// build_id recovery (not part of the per-job results schema).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchRecord {
    pub batch_id: u64,
    /// "submit" | "warm"
    pub kind: String,
    pub jobs: Vec<String>,
    pub root_drvs: Vec<String>,
    pub est_nodes: usize,
    pub build_id: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub exit_code: Option<i32>,
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
