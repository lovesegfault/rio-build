//! Campaign spec (operator input), engine knobs, and the campaign.json record.
//!
//! [`CampaignSpec`] is what `xtask replay launch` writes (or a developer
//! writes by hand for local runs); [`CampaignRecord`] is the engine-owned
//! campaign.json artifact carrying the spec plus the plan-stage output and
//! the comparability block every report leads with.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

/// Conventional warm-stage tenant name (leaf mode): the tenant whose
/// upstreams point at the public binary cache so dependency warming
/// substitutes instead of rebuilding.
pub const WARM_TENANT: &str = "replay-warm";

/// How the campaign builds: against upstream caches (leaf) or entirely
/// from source inside the cluster (self-hosted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    Leaf,
    SelfHosted,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Leaf => "leaf",
            Mode::SelfHosted => "self-hosted",
        }
    }

    /// The build tenant conventionally paired with each mode.
    pub fn expected_build_tenant(&self) -> &'static str {
        match self {
            Mode::Leaf => "replay-leaf",
            Mode::SelfHosted => "replay-selfhosted",
        }
    }
}

/// Which replay archive the campaign runs against.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ArchiveRef {
    /// S3 bucket holding `replay/archives/...` (None = same bucket as `s3.bucket`).
    pub s3_bucket: Option<String>,
    /// Full key prefix of the archive, e.g. `replay/archives/<archive_id_short>`.
    pub s3_prefix: Option<String>,
    /// The full 64-hex archive id (lowercase hex SHA-256 of the archive's
    /// `manifest.json` bytes).
    pub digest: String,
}

/// Where campaign artifacts are synced.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct S3Target {
    /// Campaign artifact bucket (the existing chunk bucket). None = S3 sync disabled.
    pub bucket: Option<String>,
    /// Key prefix for campaign artifacts, default `replay/campaigns`.
    pub prefix: String,
}

impl Default for S3Target {
    fn default() -> Self {
        Self {
            bucket: None,
            prefix: "replay/campaigns".into(),
        }
    }
}

/// Cluster endpoints the engine talks to.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ClusterEndpoints {
    /// ssh-ng URL for the build tenant (replay-leaf / replay-selfhosted),
    /// e.g. `ssh-ng://rio@rio-gateway.rio-system.svc:22?compress=true&ssh-key=/secrets/replay-leaf`.
    pub gateway_store_url: String,
    /// Directory holding one passphrase-less SSH private key per campaign
    /// tenant, file name = tenant name; the prefetch arm dials the gateway
    /// with `<ssh_key_dir>/<tenants.warm_tenant>`. Required whenever the
    /// effective supply policy includes the prefetch arm — leaf mode (and
    /// any spec with `supply.dependencies = "substituters"`) cannot run
    /// its supply stage without it.
    pub ssh_key_dir: Option<PathBuf>,
    /// Scheduler AdminService address, e.g. `rio-scheduler.rio-system.svc:9001`.
    pub scheduler_addr: String,
    /// Store StoreService address, e.g. `rio-store.rio-store.svc:9002`.
    pub store_addr: String,
    /// Path to the HMAC key for `x-rio-service-token` (None = no token, dev mode).
    pub service_hmac_key_path: Option<PathBuf>,
    /// Gateway SSH host key the engine's transport pins: an OpenSSH
    /// public-key line (`ssh-ed25519 AAAA… comment`) or a `SHA256:…`
    /// fingerprint. Required — the launcher reads it from the gateway
    /// host-key Secret; [`CampaignSpec::validate`] rejects specs without it
    /// because omitting it would disable SSH host-key verification.
    pub gateway_host_key: Option<String>,
}

/// Tenant names plus the launch-time upstream-set assertion.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TenantBlock {
    /// `replay-leaf` or `replay-selfhosted`.
    pub build_tenant: String,
    /// `replay-warm` (unused in self-hosted mode).
    pub warm_tenant: String,
    /// Set by `xtask replay launch` after it asserted the campaign tenants'
    /// upstream sets via rio-cli. The engine cannot perform that assertion
    /// itself: the ListTenants/ListUpstreams admin RPCs are allowlisted to
    /// operator CLIs and exclude `rio-replay`.
    pub upstreams_verified: bool,
    pub upstreams_verified_at: Option<String>,
    /// Snapshot recorded by launch: tenant name → upstream URLs.
    pub upstream_snapshot: BTreeMap<String, Vec<String>>,
}

/// Job-scope filters applied by the plan stage.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Filters {
    /// Systems to keep (empty = keep all systems present in the manifest).
    pub systems: Vec<String>,
    /// Job-name include globs (empty = include all).
    pub include_globs: Vec<String>,
    /// Exclude jobs whose requiredFeatures intersect this set (e.g. ["kvm"]).
    pub exclude_features: Vec<String>,
    /// Keep only the first N in-scope jobs (after deterministic sort).
    pub limit: Option<usize>,
    /// Path to an explicit job list (overrides include_globs): one job
    /// name per line, `#` starting a whole-line or trailing comment —
    /// the shared [`crate::jobsfile`] grammar, identical to what the
    /// recorder's `--scope jobs-file:` accepts, so one file serves both
    /// record and replay.
    pub jobs_file: Option<PathBuf>,
}

/// Engine tuning knobs. The defaults are the locked starting values for
/// the first campaigns; every knob is overridable per campaign via the
/// spec.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Knobs {
    pub batch_max_jobs: usize,
    pub batch_max_nodes: usize,
    pub submit_concurrency: usize,
    pub narinfo_concurrency: usize,
    pub s3_sync_interval_secs: u64,
    pub collect_poll_secs: u64,
    pub cluster_status_poll_secs: u64,
    pub spawn_intents_poll_secs: u64,
    pub active_stall_hours: f64,
    pub queued_watchdog_hours: f64,
    pub max_queued_requeues: u32,
    pub max_auto_retries: u32,
    /// Re-offers granted to a job through the engine-cancelled carve-out
    /// (a batch the engine itself cancelled at its deadline or abort)
    /// before the next cancellation terminalizes it. Each cycle can hold
    /// the job for up to `batch_timeout_hours` of cluster time, so this
    /// budget bounds the engine-cancel requeue loop explicitly — at
    /// defaults (2 cycles x 24h) a deadline-cycling job is retired after
    /// roughly two days instead of spinning.
    pub max_engine_cancel_cycles: u32,
    pub failfast_singleton_after: u32,
    pub batch_timeout_hours: f64,
    pub log_tail_bytes: usize,
    pub idle_polls_for_suspend: u32,
    pub ice_masked_cells_threshold: usize,
    pub dispatch_gap_threshold: i64,
    pub dispatch_gap_polls: u32,
    pub pause_queue_depth: Option<u32>,
    pub infra_pause_pct: f64,
    /// Low-confidence threshold (percent) on the infra-indeterminate rate:
    /// above it the report flags the campaign's headline as low-confidence.
    pub infra_low_confidence_pct: f64,
    /// Low-confidence threshold (percent) on the no-truth rate: above it
    /// the report flags the campaign's truth coverage as low-confidence.
    pub no_truth_threshold_pct: f64,
    pub report_top_n: usize,
    /// SSH connections the gateway transport pool dials. `None` derives the
    /// count from the scheduling mode's peak channel demand — timed: the
    /// larger of `submit_concurrency` and `max_sessions`; timeless:
    /// `submit_concurrency` — as ceil(demand / `CHANNELS_PER_CONNECTION`),
    /// minimum 1. The divisor is the transport's own per-connection fan-out
    /// — a client-side blast-radius choice, not a gateway limit. An
    /// explicit value must cover that demand in timed mode; spec validation
    /// rejects one that cannot.
    pub connections: Option<usize>,
    /// Deadline in seconds for each probe / upload / path-info client op on
    /// a gateway channel (build submissions use the batch timeout instead).
    pub op_timeout_secs: u64,
    /// Store paths per `QueryValidPaths` probe call.
    pub probe_chunk: usize,
    /// Channels held for validity probing. Reserved for the supply planner;
    /// nothing reads it yet — the submitter probes on its own channel.
    pub probe_concurrency: usize,
    /// Prefetch shortfall — missing plus unavailable paths, as a
    /// percentage of the whole wanted set (planned plus unavailable) —
    /// above this threshold pauses the campaign before execution starts;
    /// below it the shortfall is recorded as a low-confidence flag. An
    /// entirely undeliverable wanted set reads as 100%, never as a skipped
    /// gate. The default is a starting point, not a calibrated value.
    pub prefetch_shortfall_pause_pct: f64,
    /// Concurrently admitted requests in timed mode (= concurrently held
    /// build channels under the dispatcher's FIFO admission).
    pub max_sessions: usize,
    /// Floor in minutes on a timed request's build deadline; it applies
    /// when the archive records no durations for the request's units.
    pub build_timeout_floor_mins: u64,
    /// Cap in hours on a timed request's build deadline.
    pub build_timeout_cap_hours: f64,
    /// Re-confirmation budget for unexpected failures in timed mode: total
    /// submission attempts for a unit whose expected outcome is built but
    /// whose replayed result is a failure.
    pub confirm_attempts: u32,
    /// Minutes a timed request waits on another request's upload claim for
    /// a shared path before re-claiming it once.
    pub claim_wait_mins: u64,
    /// Recorded request offsets are divided by this factor when building
    /// the timed schedule; must be finite and positive.
    pub speedup: f64,
    /// Reproduce recorded cancellations/disconnects in timed mode by
    /// abandoning the build channel at the recorded relative time.
    pub replay_interruptions: bool,
    /// Worker tasks in the prewarm/client-upload pool.
    pub upload_workers: usize,
    /// Byte cap in MiB on one client upload batch.
    pub upload_batch_max_mib: u64,
    /// Entry cap on one client upload batch.
    pub upload_batch_max_entries: usize,
    /// NARs at or above this many MiB are streamed individually via
    /// `AddToStoreNar` instead of riding a multi-path upload batch.
    pub large_nar_threshold_mib: u64,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            batch_max_jobs: 50,
            batch_max_nodes: 4500,
            submit_concurrency: 8,
            narinfo_concurrency: 64,
            s3_sync_interval_secs: 300,
            collect_poll_secs: 60,
            cluster_status_poll_secs: 60,
            spawn_intents_poll_secs: 300,
            active_stall_hours: 6.0,
            queued_watchdog_hours: 2.0,
            max_queued_requeues: 2,
            max_auto_retries: 1,
            max_engine_cancel_cycles: 2,
            failfast_singleton_after: 3,
            batch_timeout_hours: 24.0,
            log_tail_bytes: 65536,
            idle_polls_for_suspend: 3,
            ice_masked_cells_threshold: 3,
            dispatch_gap_threshold: 50,
            dispatch_gap_polls: 5,
            pause_queue_depth: None,
            infra_pause_pct: 25.0,
            infra_low_confidence_pct: 5.0,
            no_truth_threshold_pct: 5.0,
            report_top_n: 20,
            connections: None,
            op_timeout_secs: 120,
            probe_chunk: 2000,
            probe_concurrency: 3,
            prefetch_shortfall_pause_pct: 10.0,
            max_sessions: 32,
            build_timeout_floor_mins: 30,
            build_timeout_cap_hours: 2.0,
            confirm_attempts: 3,
            claim_wait_mins: 10,
            speedup: 1.0,
            replay_interruptions: true,
            upload_workers: 8,
            upload_batch_max_mib: 256,
            upload_batch_max_entries: 500,
            large_nar_threshold_mib: 64,
        }
    }
}

/// When submissions happen: queue-driven (timeless) or at the recorded
/// request offsets (timed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScheduleMode {
    /// Queue-driven dispatch: attemptable units are packed into batches and
    /// submitted as capacity allows, ignoring recorded timing.
    #[default]
    Timeless,
    /// Recorded requests fire at their recorded offsets divided by the
    /// `speedup` knob, under FIFO admission capped at `max_sessions`.
    Timed,
}

impl ScheduleMode {
    /// The wire string (matches the serde kebab-case form); used wherever
    /// the mode is recorded as data, e.g. the comparability block.
    pub fn as_str(&self) -> &'static str {
        match self {
            ScheduleMode::Timeless => "timeless",
            ScheduleMode::Timed => "timed",
        }
    }
}

/// Scheduling block of the campaign spec; part of the campaign's identity
/// and recorded in the comparability block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SchedulingBlock {
    pub mode: ScheduleMode,
}

/// Where dependency outputs may come from (the supply planner's source
/// ladder policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SupplyDependencies {
    /// Full ladder: target substituters first, then archive-embedded
    /// content, then relay from recorded source substituters.
    Substituters,
    /// Hermetic replay: skip the target-substituter rung — even paths the
    /// target's substituters could provide are uploaded from the archive
    /// or relayed.
    EmbeddedOnly,
    /// Self-hosted measurement: no dependency outputs are delivered by any
    /// mechanism; only derivation texts and embedded input sources are
    /// uploaded, and the target builds the entire closure itself.
    None,
}

impl SupplyDependencies {
    /// The wire string (matches the serde kebab-case form); used wherever
    /// the policy is recorded as data, e.g. the comparability block.
    pub fn as_str(&self) -> &'static str {
        match self {
            SupplyDependencies::Substituters => "substituters",
            SupplyDependencies::EmbeddedOnly => "embedded-only",
            SupplyDependencies::None => "none",
        }
    }
}

/// When planned supply is delivered relative to execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SupplyDelivery {
    /// All planned supply is delivered before the execution clock starts
    /// (required for timed runs).
    #[default]
    Prewarm,
    /// Client uploads happen per submission as gaps are discovered; allowed
    /// only for timeless runs.
    Inline,
}

impl SupplyDelivery {
    /// The wire string (matches the serde kebab-case form); used wherever
    /// the policy is named as data, e.g. in validation errors.
    pub fn as_str(&self) -> &'static str {
        match self {
            SupplyDelivery::Prewarm => "prewarm",
            SupplyDelivery::Inline => "inline",
        }
    }
}

/// Supply policy block of the campaign spec; part of the campaign's
/// identity and recorded in the comparability block.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SupplyBlock {
    /// None = derived from the campaign mode (leaf → substituters,
    /// self-hosted → none).
    pub dependencies: Option<SupplyDependencies>,
    pub delivery: SupplyDelivery,
    /// Target-cluster substituter URLs the planner probes for ladder rung 1.
    /// Empty = use the archive manifest's advisory target-substituter list.
    pub target_substituters: Vec<String>,
}

impl SupplyBlock {
    /// The dependency policy in effect: the explicit spec value when set,
    /// otherwise derived from the campaign mode (leaf → substituters,
    /// self-hosted → none).
    pub fn effective_dependencies(&self, mode: Mode) -> SupplyDependencies {
        self.dependencies.unwrap_or(match mode {
            Mode::Leaf => SupplyDependencies::Substituters,
            Mode::SelfHosted => SupplyDependencies::None,
        })
    }
}

/// Campaign-level aggregation policy applied at report time. Policies are
/// chosen at launch and recorded in the spec; they are never baked into the
/// replay archive, and both may be requested for one campaign.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReportPolicy {
    /// The headline build-outcome agreement report (the parity report):
    /// the verdict-based numerator/denominator plus the comparability block.
    Parity,
    /// The regression gate: a tripped/not-tripped result over the `fail_on`
    /// trip set, written to `report/gate.json` and mirrored in progress.json.
    RegressionGate,
}

impl ReportPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReportPolicy::Parity => "parity",
            ReportPolicy::RegressionGate => "regression-gate",
        }
    }
}

/// What trips the regression gate. The gate result is data consumed by the
/// operator CLI (`report --check`); it never changes the engine's exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailOn {
    /// Observational run: the gate never trips.
    None,
    /// Trip on anything charged to the target or to run confidence:
    /// unexpected-failure, unexpected-dependency-failure, upload-rejected,
    /// infra-indeterminate.
    Regression,
    /// Everything in `regression`, plus the informational divergence
    /// classes: output-divergence, unexpected-success,
    /// interruption-not-reproduced.
    Divergence,
}

impl FailOn {
    pub fn as_str(&self) -> &'static str {
        match self {
            FailOn::None => "none",
            FailOn::Regression => "regression",
            FailOn::Divergence => "divergence",
        }
    }

    /// Every trip condition, in spec order — the iteration surface for
    /// consumer-side sweeps over the `fail_on` axis (the gate document
    /// is wire data, so each variant's wire string must be exercised at
    /// the consumption point, not just the one a fixture happened to
    /// pin). Completeness is compile-forced by `fail_on_all_is_total`:
    /// a new variant breaks its match until indexed, and the index
    /// range breaks until the variant joins this list.
    pub const ALL: [FailOn; 3] = [FailOn::None, FailOn::Regression, FailOn::Divergence];
}

/// Report-policy block of the campaign spec: which aggregation policies the
/// report stage applies and what trips the regression gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReportBlock {
    /// Policies applied at report time. Defaults to the parity report alone.
    pub policies: Vec<ReportPolicy>,
    /// Regression-gate trip condition; only meaningful when `policies`
    /// includes the regression-gate policy (validation enforces that).
    pub fail_on: FailOn,
}

impl Default for ReportBlock {
    fn default() -> Self {
        Self {
            policies: vec![ReportPolicy::Parity],
            fail_on: FailOn::None,
        }
    }
}

/// The operator-provided campaign spec (input to `rio-replay run --spec`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CampaignSpec {
    pub campaign_id: Option<String>,
    pub mode: Mode,
    pub archive: ArchiveRef,
    pub s3: S3Target,
    pub cluster: ClusterEndpoints,
    pub tenants: TenantBlock,
    pub filters: Filters,
    pub scheduling: SchedulingBlock,
    pub supply: SupplyBlock,
    pub report: ReportBlock,
    pub knobs: Knobs,
    /// Deployed image versions verified by launch pre-flight (recorded verbatim).
    pub cluster_versions: Option<serde_json::Value>,
    /// Deadline (RFC3339); also settable via --deadline.
    pub deadline: Option<String>,
}

impl Default for CampaignSpec {
    fn default() -> Self {
        Self {
            campaign_id: None,
            mode: Mode::Leaf,
            archive: ArchiveRef::default(),
            s3: S3Target::default(),
            cluster: ClusterEndpoints::default(),
            tenants: TenantBlock::default(),
            filters: Filters::default(),
            scheduling: SchedulingBlock::default(),
            supply: SupplyBlock::default(),
            report: ReportBlock::default(),
            knobs: Knobs::default(),
            cluster_versions: None,
            deadline: None,
        }
    }
}

impl CampaignSpec {
    /// Read and parse a campaign spec JSON file, then [`validate`](Self::validate) it.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read campaign spec {}", path.display()))?;
        let spec: Self = serde_json::from_str(&text)
            .with_context(|| format!("parse campaign spec {}", path.display()))?;
        spec.validate()
            .with_context(|| format!("invalid campaign spec {}", path.display()))?;
        Ok(spec)
    }

    /// Reject specs missing anything a campaign cannot run without. Every
    /// field is defaultable at the serde level (so partial specs parse),
    /// but these must be filled in before the engine will accept the spec;
    /// each error names the offending field as it appears in the JSON.
    pub fn validate(&self) -> anyhow::Result<()> {
        let non_empty = [
            (
                "cluster.gateway_store_url",
                self.cluster.gateway_store_url.as_str(),
            ),
            (
                "cluster.scheduler_addr",
                self.cluster.scheduler_addr.as_str(),
            ),
            ("cluster.store_addr", self.cluster.store_addr.as_str()),
            ("tenants.build_tenant", self.tenants.build_tenant.as_str()),
        ];
        for (field, value) in non_empty {
            anyhow::ensure!(
                !value.trim().is_empty(),
                "campaign spec field {field} must not be empty"
            );
        }
        // The archive id pins exactly which replay archive the campaign runs
        // against (it is checked against the fetched manifest at bootstrap),
        // so the spec must carry the full 64-hex form, never a prefix.
        anyhow::ensure!(
            self.archive.digest.len() == 64
                && self
                    .archive
                    .digest
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "spec.archive.digest must be the 64-hex archive id"
        );
        // The transport pins the gateway host key against this value on every
        // dial; a spec without one would silently disable SSH host-key
        // verification, so it is rejected here rather than at first dial.
        anyhow::ensure!(
            self.cluster
                .gateway_host_key
                .as_deref()
                .is_some_and(|key| !key.trim().is_empty()),
            "campaign spec field cluster.gateway_host_key must be set; omitting it would disable \
             SSH host-key verification (the launcher populates it from the gateway host-key Secret)"
        );
        let nonzero_knobs = [
            ("knobs.batch_max_jobs", self.knobs.batch_max_jobs),
            ("knobs.batch_max_nodes", self.knobs.batch_max_nodes),
            ("knobs.submit_concurrency", self.knobs.submit_concurrency),
            ("knobs.narinfo_concurrency", self.knobs.narinfo_concurrency),
            ("knobs.probe_chunk", self.knobs.probe_chunk),
            ("knobs.probe_concurrency", self.knobs.probe_concurrency),
        ];
        for (field, value) in nonzero_knobs {
            anyhow::ensure!(value != 0, "campaign spec field {field} must be nonzero");
        }
        anyhow::ensure!(
            self.knobs.op_timeout_secs != 0,
            "campaign spec field knobs.op_timeout_secs must be nonzero"
        );
        // Zero would terminalize a job on its FIRST engine cancellation —
        // charging the engine's own deadline cut to the job with no
        // re-offer at all.
        anyhow::ensure!(
            self.knobs.max_engine_cancel_cycles != 0,
            "campaign spec field knobs.max_engine_cancel_cycles must be nonzero"
        );
        // `None` means "derive from submit_concurrency"; an explicit zero
        // would leave the transport with no connections at all.
        anyhow::ensure!(
            self.knobs.connections != Some(0),
            "campaign spec field knobs.connections must be nonzero"
        );
        // The batch timeout becomes the per-submission abandon deadline;
        // zero, NaN, or a negative value would cancel every submission the
        // moment it starts.
        anyhow::ensure!(
            self.knobs.batch_timeout_hours.is_finite() && self.knobs.batch_timeout_hours > 0.0,
            "campaign spec field knobs.batch_timeout_hours must be a positive finite number of hours"
        );
        // The engine-cancelled requeue arm is bounded by its own explicit
        // cycle budget (max_engine_cancel_cycles, validated nonzero above)
        // — the stall ladder is no longer that loop's bound. The ordering
        // below still matters for the ladder's two remaining roles: the
        // active-stall AUTO-RETRY can only fire mid-batch while a batch
        // outlives the stall threshold (inverted, every settle flips the
        // phase and resets the clock before the rescue can fire), and the
        // terminal stalled-active arm's batch-timeout floor is only a
        // meaningful tightening while the floor sits above the threshold
        // (the engine must not retire a batch member on a residence the
        // batch's own budget still permits).
        anyhow::ensure!(
            self.knobs.batch_timeout_hours > self.knobs.active_stall_hours,
            "campaign spec field knobs.batch_timeout_hours ({}) must exceed \
             knobs.active_stall_hours ({}): the active-stall auto-retry can only fire while a \
             batch outlives the stall threshold (with the inverted ordering — accepted by \
             earlier engine versions, rejected now — every settle resets the clock first), and \
             the terminal stall arm's batch-timeout floor must sit above the threshold to mean \
             anything",
            self.knobs.batch_timeout_hours,
            self.knobs.active_stall_hours,
        );
        // Recorded offsets are divided by the speedup; zero, NaN, infinity,
        // or a negative value would collapse or invert the schedule.
        anyhow::ensure!(
            self.knobs.speedup.is_finite() && self.knobs.speedup > 0.0,
            "campaign spec field knobs.speedup must be a positive finite number"
        );
        // The schedule's duration math divides recorded offsets by the
        // speedup before Duration::from_secs_f64. The engine's schedule
        // wiring (run/mod.rs converters — the archive reader itself only
        // floors request offsets at zero) bounds every recorded number
        // the schedule consumes by MAX_RECORDED_OFFSET_S (one year):
        // `recorded_request_from` clamps request offsets into
        // [0, MAX_RECORDED_OFFSET_S] (non-finite → 0), and
        // `recorded_timing_from` drops non-finite and over-cap stop
        // offsets to None — filtered, not clamped: a dropped stop falls
        // back to the scaled default gap (a 60s constant), and any gap
        // from a stop below its request offset is floored at zero by the
        // timeline's `(stop - offset).max(0.0)`. So the worst-case
        // quotient is MAX_RECORDED_OFFSET_S / speedup — demand here that
        // it stays a representable Duration, as a named-field refusal
        // like every other bad knob, instead of letting an absurdly
        // small stretch factor panic schedule construction and
        // crash-loop the campaign Job. The bound is derived from those
        // contracts, not picked: anything that survives this check
        // cannot overflow any of the schedule's division sites.
        anyhow::ensure!(
            std::time::Duration::try_from_secs_f64(
                super::MAX_RECORDED_OFFSET_S / self.knobs.speedup
            )
            .is_ok(),
            "campaign spec field knobs.speedup ({}) is too small: a recorded offset at the \
             one-year cap divided by it exceeds the representable Duration range — use a larger \
             speedup",
            self.knobs.speedup
        );
        // The build-deadline cap is a duration; it must be a usable number
        // of hours for the same reason as the batch timeout above.
        anyhow::ensure!(
            self.knobs.build_timeout_cap_hours.is_finite()
                && self.knobs.build_timeout_cap_hours > 0.0,
            "campaign spec field knobs.build_timeout_cap_hours must be a positive finite number of hours"
        );
        // The cap's hours-to-seconds conversion
        // (`TimelineConfig::from_knobs` multiplies by 3600 before the
        // float→Duration conversion) must stay representable — the same
        // derived-bound discipline as the speedup quotient above. Two
        // finite classes fail it: hours whose seconds exceed Duration's
        // u64 range (above ~5.1e15 hours), and hours large enough that
        // the ×3600 multiplication itself overflows to +inf (~5e304).
        // Both die here as a named-field refusal instead of wiring a
        // campaign whose every build deadline is the meaningless
        // saturated maximum.
        anyhow::ensure!(
            std::time::Duration::try_from_secs_f64(self.knobs.build_timeout_cap_hours * 3600.0)
                .is_ok(),
            "campaign spec field knobs.build_timeout_cap_hours ({}) is too large: the cap in \
             seconds (hours × 3600) exceeds the representable Duration range — use a smaller cap",
            self.knobs.build_timeout_cap_hours
        );
        // Zero would disable the supply/upload arms or produce zero-length
        // deadlines and empty admission windows downstream.
        let nonzero_supply_knobs = [
            ("knobs.max_sessions", self.knobs.max_sessions as u64),
            (
                "knobs.confirm_attempts",
                u64::from(self.knobs.confirm_attempts),
            ),
            ("knobs.upload_workers", self.knobs.upload_workers as u64),
            (
                "knobs.upload_batch_max_mib",
                self.knobs.upload_batch_max_mib,
            ),
            (
                "knobs.upload_batch_max_entries",
                self.knobs.upload_batch_max_entries as u64,
            ),
            (
                "knobs.large_nar_threshold_mib",
                self.knobs.large_nar_threshold_mib,
            ),
            (
                "knobs.build_timeout_floor_mins",
                self.knobs.build_timeout_floor_mins,
            ),
        ];
        for (field, value) in nonzero_supply_knobs {
            anyhow::ensure!(value != 0, "campaign spec field {field} must be nonzero");
        }
        // Timed-only knobs silently do nothing in timeless mode, so a
        // non-default value there is a configuration mistake worth refusing
        // rather than ignoring.
        if self.scheduling.mode == ScheduleMode::Timeless {
            let defaults = Knobs::default();
            let timed_only_overridden = [
                (
                    "max_sessions",
                    self.knobs.max_sessions != defaults.max_sessions,
                ),
                (
                    "confirm_attempts",
                    self.knobs.confirm_attempts != defaults.confirm_attempts,
                ),
                (
                    "claim_wait_mins",
                    self.knobs.claim_wait_mins != defaults.claim_wait_mins,
                ),
                ("speedup", self.knobs.speedup != defaults.speedup),
                (
                    "build_timeout_floor_mins",
                    self.knobs.build_timeout_floor_mins != defaults.build_timeout_floor_mins,
                ),
                (
                    "build_timeout_cap_hours",
                    self.knobs.build_timeout_cap_hours != defaults.build_timeout_cap_hours,
                ),
            ];
            for (name, overridden) in timed_only_overridden {
                anyhow::ensure!(
                    !overridden,
                    "campaign spec knob knobs.{name} is only meaningful in timed mode \
                     (scheduling.mode = \"timed\")"
                );
            }
        }
        // Legality of the (scheduling.mode × supply.delivery) combination
        // is defined by the engine's mode-wiring table — the same source
        // that sizes the gateway transport pool and wires the
        // pre-submission supply hook — so a combination cannot pass
        // validation while having no execution path.
        let Some(wiring) =
            super::mode_wiring(self.scheduling.mode, self.supply.delivery, &self.knobs)
        else {
            anyhow::bail!(
                "campaign spec field supply.delivery = \"{}\" has no engine wiring when \
                 scheduling.mode is \"{}\" (timed runs deliver all planned supply before the \
                 execution clock starts; a per-submission top-up would corrupt the recorded \
                 cadence)",
                self.supply.delivery.as_str(),
                self.scheduling.mode.as_str(),
            );
        };
        // An explicitly pinned knobs.connections must still cover the
        // scheduling mode's peak channel demand in timed mode: the pool's
        // channel capacity is a hard ceiling on concurrently held build
        // channels, and dispatch lateness is stamped at admission — before
        // the channel wait — so an undersized pool would silently
        // serialize the recorded cadence with no trace in the timing
        // statistics. Timeless campaigns have no cadence contract: there an
        // explicit undersized pool merely throttles throughput, which can
        // be a deliberate choice.
        if self.scheduling.mode == ScheduleMode::Timed
            && let Some(connections) = self.knobs.connections
        {
            let capacity = connections.saturating_mul(super::transport::CHANNELS_PER_CONNECTION);
            anyhow::ensure!(
                capacity >= wiring.channel_demand,
                "campaign spec field knobs.connections = {} yields {} concurrent channels \
                 ({} per connection), below the timed scheduling mode's channel demand of {} \
                 (the larger of knobs.submit_concurrency and knobs.max_sessions); raise \
                 knobs.connections or lower knobs.max_sessions",
                connections,
                capacity,
                super::transport::CHANNELS_PER_CONNECTION,
                wiring.channel_demand,
            );
        }
        // The dependency policy must not contradict what the campaign mode
        // measures: self-hosted builds the whole closure itself, leaf
        // measures against upstream-substituted dependencies.
        match (self.mode, self.supply.dependencies) {
            (Mode::SelfHosted, Some(SupplyDependencies::Substituters)) => anyhow::bail!(
                "campaign spec field supply.dependencies = \"substituters\" contradicts \
                 mode \"self-hosted\" (the self-hosted measurement builds the entire closure itself)"
            ),
            (Mode::Leaf, Some(SupplyDependencies::None)) => anyhow::bail!(
                "campaign spec field supply.dependencies = \"none\" contradicts mode \"leaf\" \
                 (the leaf measurement substitutes dependencies from the target's upstreams)"
            ),
            _ => {}
        }
        // A fail-on condition is only ever evaluated by the regression-gate
        // policy; requesting one without the other would silently produce a
        // campaign whose gate never exists, so it is refused up front.
        anyhow::ensure!(
            self.report.fail_on == FailOn::None
                || self.report.policies.contains(&ReportPolicy::RegressionGate),
            "report.fail_on requires the regression-gate policy to be requested"
        );
        // The converse combination — regression-gate requested with
        // fail_on "none" — stays representable: it records an
        // accounting-only gate document (counts can never populate, the
        // coverage witness still accrues), and specs persisted by
        // pre-acknowledgment launches carry it, so a hard refusal here
        // would brick their resumes. It is still the silent-inert trap
        // the rationale above exists for — the gate's trip predicate is
        // the constant false — so it is called out loudly: launch
        // demands an explicit `--fail-on none` acknowledgment for new
        // campaigns, and `report --check` (the design-named single CI
        // consumption point, §7.3) treats the resulting pass as vacuous.
        if self.report.policies.contains(&ReportPolicy::RegressionGate)
            && self.report.fail_on == FailOn::None
        {
            tracing::warn!(
                "campaign spec requests the regression-gate report policy with fail_on \
                 \"none\": the recorded gate can never trip, so a `report --check` pass \
                 verifies nothing (use fail_on \"regression\" or \"divergence\" for a gate \
                 that can fire)"
            );
        }
        Ok(())
    }
}

/// Generate a campaign id when the spec doesn't pin one:
/// `c<UTC yyyymmddthhmmssz>-<8 hex>` — sortable, unique, k8s-name safe.
/// The timestamp is truncated to whole seconds, so sub-second precision in
/// the input cannot change the id's documented shape.
pub fn generate_campaign_id(now_rfc3339: &str) -> String {
    let whole_seconds = now_rfc3339
        .parse::<jiff::Timestamp>()
        .ok()
        .and_then(|ts| jiff::Timestamp::from_second(ts.as_second()).ok())
        .map(|ts| ts.to_string())
        .unwrap_or_else(|| now_rfc3339.to_string());
    let compact: String = whole_seconds
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    format!("c{}-{}", compact, &suffix[..8])
}

/// Comparability block: every report leads with it so two campaign
/// reports can only be compared when their eval set, mode, tenant,
/// filters, and engine/signature versions actually line up.
///
/// Every field added after the block first shipped is serde-defaulted, so
/// campaign.json artifacts written by older engines keep parsing.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ComparabilityBlock {
    pub eval_set: String,
    pub manifest_sha256: String,
    pub mode: String,
    pub build_tenant: String,
    pub filters: Filters,
    pub engine_version: String,
    pub signature_table_version: String,
    pub in_scope: usize,
    pub attemptable: usize,
    pub attempted: usize,
    pub excluded: BTreeMap<String, usize>,
    pub completeness_pct: f64,
    pub low_confidence: Vec<String>,
    /// When the replay archive was recorded (the archive manifest's creation
    /// timestamp, RFC3339); `None` for campaign records written before this
    /// field existed.
    pub archive_created_at: Option<String>,
    /// Age of the archive, in days, at campaign creation time (campaign
    /// `created_at` minus archive `created_at`). Campaigns replaying the
    /// same archive at very different ages measure different upstream
    /// worlds (sources drift, caches garbage-collect), so the age is part
    /// of the comparability story.
    pub archive_age_days: Option<f64>,
    /// Count of recorder-side exclusion records carried by the archive.
    /// `None` means the archive has no exclusions member at all, so no
    /// completeness penalty applies; `Some(0)` means the recorder declared
    /// an empty exclusion set.
    pub exclusions_recorded: Option<usize>,
    /// Workload units whose session-scoped truth records disagree on the
    /// CONSUMED truth — the outcome, or the recorded output hashes of
    /// outcome-equal records — with no session-less record to supersede
    /// them: the units whose one truth slot the reader's cross-session
    /// collapse resolved by informativeness rank or, within a rank
    /// class, by record content order, discarding recorded information
    /// (see `ReplayArchive::truth_collapse_conflicts`). Part of
    /// comparability because the headline's truth for these units is a
    /// policy choice, not a plain recording. `None` means NOT MEASURED:
    /// a campaign record written before the field existed, or an
    /// archive without the `expected_outcomes` capability — the reader
    /// withholds the count there, since no truth is collapse-resolved
    /// when the recorder vouched for no outcomes (every unit's truth is
    /// Unknown).
    pub truth_collapse_conflicts: Option<usize>,
    /// The supply stage's planned-but-missing prefetch percentage, recorded
    /// even when it stayed below the pause threshold (any nonzero shortfall
    /// changes what the headline measured).
    pub prefetch_shortfall_pct: Option<f64>,
    /// True when a timed run's recorded cadence was not honored (resume
    /// re-anchoring, or a pause/dispatch suspension window during timed
    /// execution); mirrors the timed dispatcher's degradation flag.
    pub timing_degraded: bool,
    /// The campaign's scheduling mode ([`ScheduleMode`] wire string), copied
    /// from the spec because it is part of the campaign's identity.
    pub scheduling_mode: Option<String>,
    /// The campaign's effective dependency-supply policy
    /// ([`SupplyDependencies`] wire string, resolved against the campaign
    /// mode when the spec leaves it unset), copied from the spec because it
    /// is part of the campaign's identity.
    pub supply_policy: Option<String>,
}

impl ComparabilityBlock {
    /// Record when the replay archive was created and how old it was (in
    /// days) when the campaign started.
    ///
    /// An unparsable campaign timestamp leaves the age unset rather than
    /// failing campaign bootstrap; the archive timestamp is still recorded.
    pub fn record_archive_provenance(
        &mut self,
        archive_created_at: jiff::Timestamp,
        campaign_created_at: &str,
    ) {
        self.archive_created_at = Some(archive_created_at.to_string());
        self.archive_age_days =
            campaign_created_at
                .parse::<jiff::Timestamp>()
                .ok()
                .map(|campaign_created| {
                    (campaign_created.as_second() - archive_created_at.as_second()) as f64
                        / 86_400.0
                });
    }
}

/// Key of [`PlanOutput::counts`] holding the number of in-scope jobs.
/// Shared between the plan stage (which writes the counts map) and the
/// report path (which reads it into the comparability block) so the two
/// can never drift apart on the key spelling.
pub const PLAN_COUNT_IN_SCOPE: &str = "inScope";

/// Key of [`PlanOutput::counts`] holding the number of attemptable jobs
/// (in scope minus not-attemptable). See [`PLAN_COUNT_IN_SCOPE`].
pub const PLAN_COUNT_ATTEMPTABLE: &str = "attemptable";

/// Key of [`PlanOutput::counts`] holding the engine's resident-set size in
/// MiB just before the plan stage loads the closure graph. Paired with
/// [`PLAN_COUNT_RSS_PEAK`] to measure the plan-time closure-graph memory
/// cost; absent when the measurement was unavailable.
pub const PLAN_COUNT_RSS_BEFORE: &str = "planRssMibBefore";

/// Key of [`PlanOutput::counts`] holding the engine's peak resident-set
/// size in MiB after the closure/overlap computation. See
/// [`PLAN_COUNT_RSS_BEFORE`].
pub const PLAN_COUNT_RSS_PEAK: &str = "planRssPeakMib";

/// Plan-stage output persisted inside campaign.json: warm and
/// not-attemptable membership plus the plan-time validity snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PlanOutput {
    pub planned_at: String,
    pub in_scope: Vec<String>,
    pub skipped: BTreeMap<String, String>,
    pub not_attemptable: Vec<String>,
    pub warm_set: Vec<String>,
    pub cached_prior_paths: Vec<String>,
    pub cached_prior_jobs: Vec<String>,
    pub counts: BTreeMap<String, usize>,
    /// In-scope jobs the plan demoted out of the workload because the
    /// archive lists impure environment variables for their derivations
    /// (sorted). Pinned here so the classification is decided ONCE, at
    /// first plan: a resume reads this membership instead of re-deriving
    /// it from the archive, so an engine upgrade that changes the
    /// demotion rule mid-campaign cannot silently move units between
    /// demoted-impure and buildable (the supply protection set and the
    /// never-supply rule key on this membership). `None` on campaign
    /// records written before the pin existed — those re-derive on
    /// resume, loudly when the re-derivation disagrees with units already
    /// retired demoted-impure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub demoted_impure: Option<Vec<String>>,
}

/// The campaign.json artifact: identity, the spec as launched, the
/// pinned replay archive, the comparability block, and (once planned) the
/// plan-stage output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CampaignRecord {
    pub campaign_id: String,
    pub created_at: String,
    pub engine_version: String,
    pub signature_table_version: String,
    pub spec: CampaignSpec,
    pub archive: ArchivePin,
    pub comparability: ComparabilityBlock,
    pub plan: Option<PlanOutput>,
}

/// The replay archive a campaign ran against, pinned by the full archive id
/// and its 16-char short form (the short id names the archive's S3 prefix).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ArchivePin {
    pub archive_id: String,
    pub archive_id_short: String,
}

pub const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Bumped when the failure-signature rules change. The current table
/// captures raw evidence only (no signature normalization yet).
pub const SIGNATURE_TABLE_VERSION: &str = "m1-raw-evidence";

impl CampaignRecord {
    pub fn new(
        campaign_id: String,
        created_at: String,
        spec: CampaignSpec,
        pin: ArchivePin,
    ) -> Self {
        // The comparability block keeps its historical `evalSet` /
        // `manifestSha256` field names; their values now carry the archive
        // identity (short id and full id respectively) until the wire
        // vocabulary itself is renamed.
        let comparability = ComparabilityBlock {
            eval_set: pin.archive_id_short.clone(),
            manifest_sha256: pin.archive_id.clone(),
            mode: spec.mode.as_str().to_string(),
            build_tenant: spec.tenants.build_tenant.clone(),
            filters: spec.filters.clone(),
            engine_version: ENGINE_VERSION.to_string(),
            signature_table_version: SIGNATURE_TABLE_VERSION.to_string(),
            // Scheduling mode and the effective supply policy are campaign
            // identity: two reports are only comparable when both match.
            scheduling_mode: Some(spec.scheduling.mode.as_str().to_string()),
            supply_policy: Some(
                spec.supply
                    .effective_dependencies(spec.mode)
                    .as_str()
                    .to_string(),
            ),
            ..ComparabilityBlock::default()
        };
        Self {
            campaign_id,
            created_at,
            engine_version: ENGINE_VERSION.to_string(),
            signature_table_version: SIGNATURE_TABLE_VERSION.to_string(),
            spec,
            archive: pin,
            comparability,
            plan: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An otherwise-valid spec, parsed from JSON that also carries an
    /// unknown field. Tests that need `validate()` to reach a later rule
    /// (instead of tripping on an empty cluster/tenant field first) start
    /// from this fixture; `spec_tolerates_unknown_fields` shares it to prove
    /// forward compatibility.
    fn valid_spec() -> CampaignSpec {
        let json = format!(
            r#"{{
            "mode": "leaf",
            "archive": {{"digest": "{digest}"}},
            "cluster": {{"gateway_store_url": "ssh-ng://rio@rio-gateway.rio-system.svc:22?ssh-key=/k",
                        "scheduler_addr": "rio-scheduler.rio-system.svc:9001",
                        "store_addr": "rio-store.rio-store.svc:9002",
                        "gateway_host_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPlaceholder gateway"}},
            "tenants": {{"build_tenant": "replay-leaf", "warm_tenant": "replay-warm",
                        "upstreams_verified": true}},
            "bogus_future_field": {{"nested": [1, 2, 3]}}
        }}"#,
            digest = "ab".repeat(32)
        );
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn knob_defaults_match_design() {
        let k = Knobs::default();
        assert_eq!((k.batch_max_jobs, k.batch_max_nodes), (50, 4500));
        assert_eq!(k.narinfo_concurrency, 64);
        assert_eq!(k.max_queued_requeues, 2);
        assert_eq!(k.max_auto_retries, 1);
        assert_eq!(k.max_engine_cancel_cycles, 2);
        assert_eq!(k.active_stall_hours, 6.0);
        assert_eq!(k.infra_low_confidence_pct, 5.0);
        assert_eq!(k.no_truth_threshold_pct, 5.0);
        assert_eq!(k.idle_polls_for_suspend, 3);
        assert_eq!(k.ice_masked_cells_threshold, 3);
        assert_eq!(k.dispatch_gap_threshold, 50);
        assert_eq!(k.dispatch_gap_polls, 5);
    }

    #[test]
    fn spec_minimal_json_roundtrip() {
        // A minimal spec as xtask launch would write it; unknown fields tolerated,
        // missing fields defaulted.
        let json = format!(
            r#"{{
            "mode": "leaf",
            "archive": {{"digest": "{digest}", "s3_prefix": "replay/archives/0123456789abcdef"}},
            "cluster": {{"gateway_store_url": "ssh-ng://rio@rio-gateway.rio-system.svc:22?ssh-key=/k",
                        "scheduler_addr": "rio-scheduler.rio-system.svc:9001",
                        "store_addr": "rio-store.rio-store.svc:9002"}},
            "tenants": {{"build_tenant": "replay-leaf", "warm_tenant": "replay-warm",
                        "upstreams_verified": true}}
        }}"#,
            digest = "ab".repeat(32)
        );
        let spec: CampaignSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec.mode, Mode::Leaf);
        assert_eq!(spec.archive.digest, "ab".repeat(32));
        assert_eq!(spec.knobs.batch_max_jobs, 50);
        assert_eq!(spec.tenants.build_tenant, "replay-leaf");
        assert_eq!(spec.mode.expected_build_tenant(), "replay-leaf");
        // Round-trips.
        let re: CampaignSpec =
            serde_json::from_str(&serde_json::to_string(&spec).unwrap()).unwrap();
        assert_eq!(re.archive.digest, "ab".repeat(32));
        assert_eq!(
            re.archive.s3_prefix.as_deref(),
            Some("replay/archives/0123456789abcdef")
        );
    }

    #[test]
    fn archive_digest_must_be_the_full_hex_id() {
        // Otherwise-valid spec whose archive pin is empty / truncated /
        // non-hex: validation names the field with the exact rule.
        let valid = format!(
            r#"{{
            "mode": "leaf",
            "archive": {{"digest": "{digest}"}},
            "cluster": {{"gateway_store_url": "ssh-ng://rio@gw:22?ssh-key=/k",
                        "scheduler_addr": "s:9001", "store_addr": "st:9002",
                        "gateway_host_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPlaceholder gateway"}},
            "tenants": {{"build_tenant": "replay-leaf", "warm_tenant": "replay-warm"}}
        }}"#,
            digest = "ab".repeat(32)
        );
        let spec: CampaignSpec = serde_json::from_str(&valid).unwrap();
        spec.validate().unwrap();
        for bad in ["", "ab12cd34", &"AB".repeat(32), &"zz".repeat(32)] {
            let mut spec = spec.clone();
            spec.archive.digest = bad.to_string();
            let err = spec.validate().unwrap_err();
            assert_eq!(
                err.to_string(),
                "spec.archive.digest must be the 64-hex archive id",
                "digest {bad:?}"
            );
        }
    }

    #[test]
    fn client_ops_knob_defaults_and_validation() {
        let k = Knobs::default();
        assert_eq!(k.connections, None);
        assert_eq!(k.op_timeout_secs, 120);
        assert_eq!(k.probe_chunk, 2000);
        assert_eq!(k.probe_concurrency, 3);
        // Explicit zero overrides are rejected, naming the knob.
        let json = format!(
            r#"{{
            "mode": "leaf",
            "archive": {{"digest": "{digest}"}},
            "cluster": {{"gateway_store_url": "ssh-ng://rio@gw:22?ssh-key=/k",
                        "scheduler_addr": "s:9001", "store_addr": "st:9002",
                        "gateway_host_key": "SHA256:0000000000000000000000000000000000000000000"}},
            "tenants": {{"build_tenant": "replay-leaf", "warm_tenant": "replay-warm"}},
            "knobs": {{"connections": 0}}
        }}"#,
            digest = "ab".repeat(32)
        );
        let spec: CampaignSpec = serde_json::from_str(&json).unwrap();
        assert!(
            spec.validate()
                .unwrap_err()
                .to_string()
                .contains("knobs.connections")
        );
    }

    /// Cross-crate calibration: every batch shape the engine's assembler
    /// can legally emit under the default knobs must fit the protocol
    /// client's stderr drain budget. The budget constants live in rio-nix
    /// and the decision of how many roots (and how large a merged closure)
    /// one op carries is made HERE, by these knobs and `assemble_batches` —
    /// so this is the test that fails when either crate moves its end of
    /// the calibration.
    ///
    /// Universe: the budget's true input universe is the SUBMISSION
    /// CHOKEPOINT'S feeders, one level above the assembler — wave batches,
    /// fail-fast singletons, canary probes, AND the timed dispatcher's two
    /// literal constructions, which never pass through the assembler (the
    /// round-3 calibration quantified over assembler output and those two
    /// feeders escaped it; the submitter now derives the budget workload
    /// itself from the realized import closure, pinned through the real
    /// submitter by the chokepoint tests in `run::submitter` and
    /// `run::timeline`). For a conforming archive the assembler's
    /// `est_nodes` IS that realized closure (plan-verified adjacency), so
    /// the corners of the legal (roots, est_nodes) space at
    /// `Knobs::default()` remain the calibration shapes, ENUMERATED FROM
    /// THE ADMISSION CODE — each corner batch is produced by
    /// `assemble_batches` itself, not hand-assumed. Budget is monotone in
    /// each axis while volume follows nodes alone, so the binding corner
    /// is min-roots × max-nodes, not the diagonal:
    ///
    /// 1. max-roots × max-nodes (the packed default batch),
    /// 2. ONE root carrying `batch_max_nodes` (a wave-tail batch),
    /// 3. the oversized singleton (one job whose own closure exceeds
    ///    `batch_max_nodes` — admitted alone, est_nodes above the cap),
    /// 4. the fail-fast/canary singleton admission `(1, usize::MAX)`,
    ///    whose REALIZED est_nodes is the job's actual closure union (the
    ///    admission cap is not a workload),
    /// 5. the timed feeder, which has NO admission caps at all (a recorded
    ///    request is submitted verbatim): checked at the formula's node
    ///    multiplier cap — the largest workload the budget still scales
    ///    for — plus the explicit beyond-cap clamp row (above the cap the
    ///    belt deliberately stops scaling and the wall-clock deadline is
    ///    the liveness bound; rio-nix pins the same clamp from its side).
    #[test]
    fn default_batch_shape_fits_the_stderr_drain_budget() {
        use rio_nix::protocol::client::{
            STDERR_BUDGET_NODE_MULTIPLIER_CAP, STDERR_BUDGET_PER_CLOSURE_NODE,
            STDERR_BUDGET_ROOT_MULTIPLIER_CAP, stderr_budget_for_workload,
        };

        use crate::run::batch::{PendingJob, assemble_batches};

        let knobs = Knobs::default();
        // Neither multiplier cap may bind for shapes the default packing
        // emits — otherwise raising the knob would silently stop scaling
        // the budget.
        assert!(
            knobs.batch_max_jobs <= STDERR_BUDGET_ROOT_MULTIPLIER_CAP,
            "default batch_max_jobs ({}) exceeds the drain-budget root multiplier cap ({}) — \
             the per-root calibration would silently stop scaling; split batches or raise the \
             cap in rio-nix",
            knobs.batch_max_jobs,
            STDERR_BUDGET_ROOT_MULTIPLIER_CAP,
        );
        assert!(
            knobs.batch_max_nodes <= STDERR_BUDGET_NODE_MULTIPLIER_CAP,
            "default batch_max_nodes ({}) exceeds the drain-budget node multiplier cap ({}) — \
             the per-node calibration would silently stop scaling; split batches or raise the \
             cap in rio-nix",
            knobs.batch_max_nodes,
            STDERR_BUDGET_NODE_MULTIPLIER_CAP,
        );

        // Per-closure-node log allowance the budget must absorb: 10k lines
        // per merged-closure node is ~4.5× the mid-range from-source
        // nixpkgs average (~2.2k lines/drv) — a deliberately log-heavy but
        // healthy batch must never trip the count belt (the belt exists
        // for a daemon that streams FOREVER, and wall-clock deadlines are
        // the liveness bound). rio-nix's STDERR_BUDGET_PER_CLOSURE_NODE is
        // documented against this exact model.
        const LOG_LINES_PER_CLOSURE_NODE: usize = 10_000;

        let job = |name: &str, deps: usize| PendingJob {
            job: name.to_string(),
            drv_path: format!("/nix/store/{:0>32}-{name}.drv", name.len()),
            dep_drvs: (0..deps)
                .map(|i| format!("/nix/store/{i:0>32}-dep-{name}-{i}.drv"))
                .collect(),
        };

        // Corner 1+2: jobs with disjoint closures packed under the default
        // caps. The first batch fills to the node cap with multiple roots;
        // forcing per-job closures near the cap also yields the one-root/
        // max-nodes wave-tail shape.
        let per_job_nodes = knobs.batch_max_nodes / 10;
        let packed: Vec<PendingJob> = (0..30)
            .map(|i| job(&format!("packed{i}"), per_job_nodes - 1))
            .collect();
        let mut corner_batches =
            assemble_batches(&packed, knobs.batch_max_jobs, knobs.batch_max_nodes);
        let tail: Vec<PendingJob> = vec![job("wavetail", knobs.batch_max_nodes - 1)];
        corner_batches.extend(assemble_batches(
            &tail,
            knobs.batch_max_jobs,
            knobs.batch_max_nodes,
        ));
        // Corner 3: the oversized singleton — one job whose own closure
        // exceeds batch_max_nodes is still admitted, alone (the same shape
        // the batch module's own oversized_single_job_becomes_singleton
        // test pins at 9001 nodes).
        let oversized: Vec<PendingJob> = vec![job("texlive", 2 * knobs.batch_max_nodes)];
        corner_batches.extend(assemble_batches(
            &oversized,
            knobs.batch_max_jobs,
            knobs.batch_max_nodes,
        ));
        // Corner 4: the fail-fast/canary singleton admission shape — one
        // job assembled with (1, usize::MAX) caps. The admission cap is
        // unbounded but the REALIZED est_nodes is the job's actual closure.
        let isolated = job("isolated", 2 * knobs.batch_max_nodes);
        corner_batches.extend(assemble_batches(
            std::slice::from_ref(&isolated),
            1,
            usize::MAX,
        ));

        let one_root_corner_seen = corner_batches
            .iter()
            .any(|batch| batch.roots.len() == 1 && batch.est_nodes >= knobs.batch_max_nodes);
        assert!(
            one_root_corner_seen,
            "corner enumeration must include the min-roots/max-nodes shape; got {:?}",
            corner_batches
                .iter()
                .map(|b| (b.roots.len(), b.est_nodes))
                .collect::<Vec<_>>()
        );

        for batch in &corner_batches {
            let healthy_volume = batch.est_nodes * LOG_LINES_PER_CLOSURE_NODE;
            let budget = stderr_budget_for_workload(batch.roots.len(), batch.est_nodes);
            assert!(
                budget >= healthy_volume,
                "a legal batch shape ({} roots × {} nodes; {healthy_volume} healthy lines) \
                 would trip the drain budget ({budget}) — healthy log-heavy batches must never \
                 be cut off mid-DAG",
                batch.roots.len(),
                batch.est_nodes,
            );
        }

        // Corner 5: the timed feeder. A recorded request is submitted
        // verbatim (no assembler, no caps), so the chokepoint can realize
        // a one-root workload of ANY size; the budget must absorb the
        // healthy-volume model all the way up to the formula's node
        // multiplier cap...
        assert!(
            stderr_budget_for_workload(1, STDERR_BUDGET_NODE_MULTIPLIER_CAP)
                >= STDERR_BUDGET_NODE_MULTIPLIER_CAP * LOG_LINES_PER_CLOSURE_NODE,
            "a one-root timed workload at the node multiplier cap must still fit its \
             healthy log volume"
        );
        // ...and beyond the cap it must CLAMP, not scale: the count belt
        // stays a real bound on a runaway daemon, and past this point the
        // wall-clock batch deadline is the liveness bound. Asserted
        // explicitly (not assumed unreachable) because the timed feeder
        // has no admission cap that would keep workloads below it.
        assert_eq!(
            stderr_budget_for_workload(1, usize::MAX),
            STDERR_BUDGET_NODE_MULTIPLIER_CAP * STDERR_BUDGET_PER_CLOSURE_NODE,
            "an over-cap workload estimate must clamp at the node multiplier cap"
        );
    }

    #[test]
    fn cluster_gateway_host_key_is_required_and_round_trips() {
        let spec: CampaignSpec = serde_json::from_str(
            r#"{"cluster": {"gateway_store_url": "ssh-ng://rio@gw:22?ssh-key=/k",
                            "scheduler_addr": "s:9001", "store_addr": "st:9002",
                            "gateway_host_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPlaceholder gateway"}}"#,
        )
        .unwrap();
        assert!(
            spec.cluster
                .gateway_host_key
                .as_deref()
                .unwrap()
                .starts_with("ssh-ed25519")
        );
        let re: CampaignSpec =
            serde_json::from_str(&serde_json::to_string(&spec).unwrap()).unwrap();
        assert_eq!(re.cluster.gateway_host_key, spec.cluster.gateway_host_key);

        // An otherwise-valid spec without the pin is rejected naming the
        // field: omitting it would disable SSH host-key verification.
        let json = format!(
            r#"{{
            "mode": "leaf",
            "archive": {{"digest": "{digest}"}},
            "cluster": {{"gateway_store_url": "ssh-ng://rio@gw:22?ssh-key=/k",
                        "scheduler_addr": "s:9001", "store_addr": "st:9002"}},
            "tenants": {{"build_tenant": "replay-leaf", "warm_tenant": "replay-warm"}}
        }}"#,
            digest = "ab".repeat(32)
        );
        let unpinned: CampaignSpec = serde_json::from_str(&json).unwrap();
        let err = unpinned.validate().unwrap_err();
        assert!(
            err.to_string().contains("cluster.gateway_host_key"),
            "{err:#}"
        );
        // An explicitly empty pin is rejected the same way.
        let mut empty_pin = unpinned;
        empty_pin.cluster.gateway_host_key = Some("  ".into());
        assert!(
            empty_pin
                .validate()
                .unwrap_err()
                .to_string()
                .contains("cluster.gateway_host_key")
        );
    }

    #[test]
    fn campaign_id_generation_shape() {
        let id = generate_campaign_id("2026-05-26T12:34:56Z");
        assert!(id.starts_with("c20260526t123456z-"), "{id}");
        assert_eq!(id.len(), "c20260526t123456z-".len() + 8);
        // k8s-name safe: lowercase alnum + dashes.
        assert!(
            id.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        );
        // Sub-second precision (what `now_rfc3339()` emits) is truncated to
        // whole seconds so the id keeps the same documented shape.
        let id = generate_campaign_id("2026-05-26T12:34:56.789012345Z");
        assert!(id.starts_with("c20260526t123456z-"), "{id}");
        assert_eq!(id.len(), "c20260526t123456z-".len() + 8);
    }

    #[test]
    fn empty_spec_is_rejected_naming_the_first_missing_field() {
        let spec: CampaignSpec = serde_json::from_str("{}").unwrap();
        let err = spec.validate().unwrap_err();
        assert!(
            err.to_string().contains("cluster.gateway_store_url"),
            "expected the first missing field to be named, got: {err:#}"
        );
    }

    #[test]
    fn nonpositive_batch_timeout_is_rejected() {
        // Otherwise-valid spec with a zero batch timeout: validation must
        // name the offending knob instead of letting it become a 0-second
        // child deadline downstream.
        let json = format!(
            r#"{{
            "mode": "leaf",
            "archive": {{"digest": "{digest}"}},
            "cluster": {{"gateway_store_url": "ssh-ng://rio@rio-gateway.rio-system.svc:22?ssh-key=/k",
                        "scheduler_addr": "rio-scheduler.rio-system.svc:9001",
                        "store_addr": "rio-store.rio-store.svc:9002",
                        "gateway_host_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPlaceholder gateway"}},
            "tenants": {{"build_tenant": "replay-leaf", "warm_tenant": "replay-warm",
                        "upstreams_verified": true}},
            "knobs": {{"batch_timeout_hours": 0.0}}
        }}"#,
            digest = "ab".repeat(32)
        );
        let spec: CampaignSpec = serde_json::from_str(&json).unwrap();
        let err = spec.validate().unwrap_err();
        assert!(err.to_string().contains("batch_timeout_hours"), "{err:#}");
    }

    /// The stall ladder's ordering requirement, re-pointed at its current
    /// contract: the engine-cancelled requeue loop is bounded by its OWN
    /// explicit budget (max_engine_cancel_cycles — pinned below and in the
    /// fold tests), so the ordering no longer carries that loop. It still
    /// backs the ladder's two remaining roles: the active-stall AUTO-RETRY
    /// can only fire mid-batch while a batch outlives the stall threshold,
    /// and the terminal stall arm's batch-timeout floor must sit above the
    /// threshold to be a tightening at all. A spec whose batch timeout is
    /// at or below active_stall_hours is rejected (accepted by earlier
    /// engine versions — a deliberate validation tightening, named in the
    /// error). The defaults (24h > 6h) satisfy the ordering.
    #[test]
    fn batch_timeout_must_exceed_the_active_stall_threshold() {
        let defaults = Knobs::default();
        assert!(
            defaults.batch_timeout_hours > defaults.active_stall_hours,
            "default knobs must satisfy the ordering"
        );
        assert!(
            defaults.max_engine_cancel_cycles > 0,
            "the cancel loop's own bound exists — the ordering no longer carries it"
        );

        let mut inverted = valid_spec();
        inverted.knobs.batch_timeout_hours = 2.0;
        inverted.knobs.active_stall_hours = 6.0;
        let err = inverted.validate().unwrap_err().to_string();
        assert!(err.contains("must exceed"), "{err}");
        assert!(err.contains("active_stall_hours"), "{err}");
        assert!(
            err.contains("auto-retry") && err.contains("floor"),
            "the error must name the bounds that depend on the ordering: {err}"
        );

        // Equality is rejected too: the stall clock needs the batch to
        // OUTLIVE the threshold to fire.
        let mut equal = valid_spec();
        equal.knobs.batch_timeout_hours = 6.0;
        equal.knobs.active_stall_hours = 6.0;
        assert!(equal.validate().is_err());

        let mut ordered = valid_spec();
        ordered.knobs.batch_timeout_hours = 7.0;
        ordered.knobs.active_stall_hours = 6.0;
        ordered.validate().unwrap();

        // The cycle budget itself is validated nonzero: zero would charge
        // the engine's own first deadline cut to the job.
        let mut zero_cycles = valid_spec();
        zero_cycles.knobs.max_engine_cancel_cycles = 0;
        let err = zero_cycles.validate().unwrap_err().to_string();
        assert!(err.contains("max_engine_cancel_cycles"), "{err}");
    }

    #[test]
    fn spec_tolerates_unknown_fields() {
        // Forward compatibility: a spec written by a newer launch tool (or
        // carrying operator annotations) still parses and validates. The
        // shared fixture's JSON carries the unknown field.
        let spec = valid_spec();
        spec.validate().unwrap();
        assert_eq!(spec.archive.digest, "ab".repeat(32));
    }

    #[test]
    fn report_block_defaults_and_wire_strings() {
        let spec: CampaignSpec = serde_json::from_str(r#"{"mode":"leaf"}"#).unwrap();
        assert_eq!(spec.report.policies, vec![ReportPolicy::Parity]);
        assert_eq!(spec.report.fail_on, FailOn::None);
        assert_eq!(
            serde_json::to_value(ReportPolicy::RegressionGate).unwrap(),
            serde_json::json!("regression-gate")
        );
        assert_eq!(
            serde_json::to_value(FailOn::Divergence).unwrap(),
            serde_json::json!("divergence")
        );
        // fail_on != none requires the regression-gate policy to be requested.
        // valid_spec() is an otherwise-valid fixture so validate() reaches the
        // report rule instead of tripping on an empty cluster/tenant field
        // first (validate checks those before anything else).
        let mut bad = valid_spec();
        bad.report = ReportBlock {
            policies: vec![ReportPolicy::Parity],
            fail_on: FailOn::Regression,
        };
        let err = bad.validate().unwrap_err().to_string();
        assert!(err.contains("regression-gate"), "{err}");
        bad.report.policies.push(ReportPolicy::RegressionGate);
        bad.validate().unwrap();

        // The converse — regression-gate with fail_on "none" — VALIDATES
        // (warn-only): specs persisted by launches that predate the
        // explicit-acknowledgment flow carry exactly this block, and a
        // resume re-validates the persisted spec, so a hard refusal here
        // would brick old campaigns. The vacuity is enforced where it is
        // consumed: launch demands the acknowledgment for new campaigns,
        // and report --check calls the pass vacuous.
        let mut observational = valid_spec();
        observational.report = ReportBlock {
            policies: vec![ReportPolicy::Parity, ReportPolicy::RegressionGate],
            fail_on: FailOn::None,
        };
        observational.validate().unwrap();
    }

    /// `FailOn::ALL` is total: a new variant breaks the index match
    /// below until it is indexed, and the index range breaks until the
    /// variant joins `ALL` — so consumer-side sweeps over the wire axis
    /// (the report --check fixtures) cannot silently miss one.
    #[test]
    fn fail_on_all_is_total() {
        let index = |f: FailOn| -> usize {
            match f {
                FailOn::None => 0,
                FailOn::Regression => 1,
                FailOn::Divergence => 2,
            }
        };
        let mut seen: Vec<usize> = FailOn::ALL.iter().map(|f| index(*f)).collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..FailOn::ALL.len()).collect::<Vec<_>>());
        // The wire strings are the serde forms verbatim — the consumer
        // sweeps key on them.
        for f in FailOn::ALL {
            assert_eq!(
                serde_json::to_value(f).unwrap(),
                serde_json::json!(f.as_str())
            );
        }
    }

    #[test]
    fn knobs_partial_override_keeps_other_defaults() {
        let spec: CampaignSpec =
            serde_json::from_str(r#"{"knobs": {"batch_max_jobs": 10}}"#).unwrap();
        assert_eq!(spec.knobs.batch_max_jobs, 10);
        let defaults = Knobs::default();
        assert_eq!(spec.knobs.batch_max_nodes, defaults.batch_max_nodes);
        assert_eq!(spec.knobs.submit_concurrency, defaults.submit_concurrency);
        assert_eq!(
            spec.knobs.s3_sync_interval_secs,
            defaults.s3_sync_interval_secs
        );
    }

    #[test]
    fn archive_ref_round_trips_and_validates() {
        let json = serde_json::json!({
            "s3_bucket": "rio-chunks",
            "s3_prefix": "replay/archives/0123456789abcdef",
            "digest": "ab".repeat(32),
        });
        let r: ArchiveRef = serde_json::from_value(json).unwrap();
        assert_eq!(r.digest.len(), 64);
        assert!(ArchiveRef::default().digest.is_empty());
        // Round-trips through serialization without losing the pin fields.
        let re: ArchiveRef = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(re.digest, r.digest);
        assert_eq!(re.s3_bucket.as_deref(), Some("rio-chunks"));
        assert_eq!(
            re.s3_prefix.as_deref(),
            Some("replay/archives/0123456789abcdef")
        );
    }

    #[test]
    fn archive_pin_serializes_camel_case() {
        let pin = ArchivePin {
            archive_id: "ab".repeat(32),
            archive_id_short: "ab".repeat(8),
        };
        let v = serde_json::to_value(&pin).unwrap();
        assert!(v.get("archiveId").is_some());
        assert!(v.get("archiveIdShort").is_some());
        // campaign.json is re-read on resume, so the pin must come back from
        // its camelCase wire form unchanged.
        let re: ArchivePin = serde_json::from_value(v).unwrap();
        assert_eq!(re, pin);
    }

    #[test]
    fn phase4_knob_defaults_match_design() {
        let k = Knobs::default();
        assert_eq!(k.prefetch_shortfall_pause_pct, 10.0);
        assert_eq!(k.max_sessions, 32);
        assert_eq!(k.connections, None);
        assert_eq!(k.op_timeout_secs, 120);
        assert_eq!(k.build_timeout_floor_mins, 30);
        assert_eq!(k.build_timeout_cap_hours, 2.0);
        assert_eq!(k.confirm_attempts, 3);
        assert_eq!(k.claim_wait_mins, 10);
        assert_eq!(k.speedup, 1.0);
        assert!(k.replay_interruptions);
        assert_eq!(k.upload_workers, 8);
        assert_eq!(k.upload_batch_max_mib, 256);
        assert_eq!(k.upload_batch_max_entries, 500);
        assert_eq!(k.large_nar_threshold_mib, 64);
        assert_eq!(k.probe_chunk, 2000);
        assert_eq!(k.probe_concurrency, 3);
    }

    #[test]
    fn scheduling_and_supply_blocks_default_and_roundtrip() {
        let spec: CampaignSpec = serde_json::from_str("{}").unwrap();
        assert_eq!(spec.scheduling.mode, ScheduleMode::Timeless);
        assert_eq!(spec.supply.dependencies, None);
        assert_eq!(spec.supply.delivery, SupplyDelivery::Prewarm);
        assert!(spec.supply.target_substituters.is_empty());
        // Mode-derived effective dependencies.
        assert_eq!(
            spec.supply.effective_dependencies(Mode::Leaf),
            SupplyDependencies::Substituters
        );
        assert_eq!(
            spec.supply.effective_dependencies(Mode::SelfHosted),
            SupplyDependencies::None
        );
        let json = r#"{"scheduling":{"mode":"timed"},
                       "supply":{"dependencies":"embedded-only","delivery":"prewarm",
                                 "target_substituters":["https://cache.nixos.org"]}}"#;
        let spec: CampaignSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.scheduling.mode, ScheduleMode::Timed);
        assert_eq!(
            spec.supply.dependencies,
            Some(SupplyDependencies::EmbeddedOnly)
        );
        let re: CampaignSpec =
            serde_json::from_str(&serde_json::to_string(&spec).unwrap()).unwrap();
        assert_eq!(re.scheduling.mode, ScheduleMode::Timed);
    }

    /// Base valid spec for validation tests: an archive pin (any 64-char
    /// lowercase-hex digest) plus the required cluster and tenant blocks.
    /// Unterminated — append `, ...}` overrides (or just `}`) to close it.
    fn spec_base() -> &'static str {
        r#"{
            "mode": "leaf",
            "archive": {"digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
            "cluster": {"gateway_store_url": "ssh-ng://rio@rio-gateway.rio-system.svc:22?ssh-key=/k",
                        "scheduler_addr": "rio-scheduler.rio-system.svc:9001",
                        "store_addr": "rio-store.rio-store.svc:9002",
                        "gateway_host_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPlaceholder gateway"},
            "tenants": {"build_tenant": "replay-leaf", "warm_tenant": "replay-warm",
                        "upstreams_verified": true}"#
    }

    #[test]
    fn speedup_too_small_for_the_offset_cap_is_refused() {
        let base = spec_base();
        // The timed schedule divides recorded offsets — bounded by the
        // one-year MAX_RECORDED_OFFSET_S at the engine's conversion
        // points (`recorded_request_from` clamps request offsets,
        // `recorded_timing_from` drops over-cap stops) — by the
        // speedup before Duration::from_secs_f64. A divisor small enough
        // to push that worst-case quotient past Duration's representable
        // range must die here as a named-field refusal like every other
        // bad knob, not as a schedule-construction panic that crash-loops
        // the campaign Job. 1.5e-12 stretches one clamped year past
        // Duration::MAX; 2e-12 keeps it representable and stays admitted
        // (the bound is derived, not a round number — both sides of it
        // are pinned so it cannot silently widen).
        let spec: CampaignSpec = serde_json::from_str(&format!(
            "{base}, \"scheduling\": {{\"mode\": \"timed\"}}, \"knobs\": {{\"speedup\": 1.5e-12}}}}"
        ))
        .unwrap();
        let err = spec.validate().unwrap_err().to_string();
        assert!(err.contains("speedup"), "{err}");

        let spec: CampaignSpec = serde_json::from_str(&format!(
            "{base}, \"scheduling\": {{\"mode\": \"timed\"}}, \"knobs\": {{\"speedup\": 2e-12}}}}"
        ))
        .unwrap();
        spec.validate()
            .expect("a speedup whose worst-case quotient fits a Duration is admitted");
    }

    #[test]
    fn build_timeout_cap_overflowing_duration_is_refused() {
        let base = spec_base();
        // The cap converts hours → seconds (×3600) → Duration at the timed
        // wiring point; Duration's seconds are u64, so the derived bound is
        // u64::MAX seconds ≈ 5.124e15 hours. Both sides of it are pinned —
        // 6e15 h refused (2.16e19 s > u64::MAX ≈ 1.845e19), 5e15 h
        // admitted (1.8e19 s fits) — so the bound cannot silently widen.
        let timed_cap = |hours: &str| -> CampaignSpec {
            serde_json::from_str(&format!(
                "{base}, \"scheduling\": {{\"mode\": \"timed\"}}, \
                 \"knobs\": {{\"build_timeout_cap_hours\": {hours}}}}}"
            ))
            .unwrap()
        };
        let err = timed_cap("6e15").validate().unwrap_err().to_string();
        assert!(err.contains("build_timeout_cap_hours"), "{err}");
        timed_cap("5e15")
            .validate()
            .expect("a cap whose seconds value fits a Duration is admitted");
        // 1e306 h is finite but its ×3600 multiplication overflows to +inf
        // BEFORE the conversion — the bound must be checked on the
        // multiplied value, not as a pre-multiplication finiteness check.
        let err = timed_cap("1e306").validate().unwrap_err().to_string();
        assert!(err.contains("build_timeout_cap_hours"), "{err}");
        // The default passes (every other validation test covers it via
        // specs that never override the knob); pin it here explicitly so
        // the bound provably admits the shipped default.
        timed_cap("2.0").validate().expect("the default cap passes");
    }

    /// Knob-universe enumeration for the float→Duration conversion family
    /// (the panicking `Duration::from_secs_f64` class): every f64-typed
    /// knob — the universe is read from the serialized [`Knobs`] schema,
    /// not hand-listed — must be classified here against that family, with
    /// the contract that keeps its downstream conversion total. A new f64
    /// knob fails this test until a row names its bound (or its
    /// never-converted role), so a sibling of the speedup/cap overflow
    /// class cannot ship unexamined. The conversion-site half of the same
    /// audit lives in `timeline::tests::
    /// duration_from_secs_f64_sites_are_enumerated`.
    #[test]
    fn every_float_knob_is_classified_against_the_duration_conversion_family() {
        const CLASSIFIED: &[(&str, &str)] = &[
            (
                "active_stall_hours",
                "float comparison threshold (watchdog/stall ladders); never converted to Duration",
            ),
            (
                "queued_watchdog_hours",
                "float comparison threshold (watchdog); never converted to Duration",
            ),
            (
                "batch_timeout_hours",
                "validate() requires finite>0; converted via saturating `as u64` cast \
                 (submit.rs), which cannot panic",
            ),
            (
                "infra_pause_pct",
                "rate threshold; never converted to Duration",
            ),
            (
                "infra_low_confidence_pct",
                "rate threshold; never converted to Duration",
            ),
            (
                "no_truth_threshold_pct",
                "rate threshold; never converted to Duration",
            ),
            (
                "prefetch_shortfall_pause_pct",
                "rate threshold; never converted to Duration",
            ),
            (
                "build_timeout_cap_hours",
                "validate() demands try_from_secs_f64(hours*3600) representability \
                 (named-field refusal); from_knobs converts via try_from_secs_f64 \
                 saturating to Duration::MAX",
            ),
            (
                "speedup",
                "validate() demands try_from_secs_f64(MAX_RECORDED_OFFSET_S/speedup) \
                 representability; every schedule division site's numerator is bounded \
                 by the recorded-offset domain clamp",
            ),
        ];
        let knobs = serde_json::to_value(Knobs::default()).unwrap();
        let f64_fields: std::collections::BTreeSet<String> = knobs
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, value)| value.is_f64())
            .map(|(field, _)| field.clone())
            .collect();
        let classified: std::collections::BTreeSet<String> = CLASSIFIED
            .iter()
            .map(|(field, _)| field.to_string())
            .collect();
        assert_eq!(
            f64_fields, classified,
            "every f64 knob must carry a conversion-family classification row \
             (and every row must name a live knob)"
        );
    }

    #[test]
    fn timed_knob_validation() {
        let base = spec_base();
        // Non-positive speedup rejected outright.
        let spec: CampaignSpec =
            serde_json::from_str(&format!("{base}, \"knobs\": {{\"speedup\": 0.0}}}}")).unwrap();
        assert!(spec.validate().unwrap_err().to_string().contains("speedup"));
        // Zero upload sizing knob rejected outright.
        let spec: CampaignSpec =
            serde_json::from_str(&format!("{base}, \"knobs\": {{\"upload_workers\": 0}}}}"))
                .unwrap();
        assert!(
            spec.validate()
                .unwrap_err()
                .to_string()
                .contains("upload_workers")
        );
        // Timed-only knob set to a non-default value in a timeless spec is rejected, naming the knob.
        let spec: CampaignSpec =
            serde_json::from_str(&format!("{base}, \"knobs\": {{\"max_sessions\": 16}}}}"))
                .unwrap();
        let err = spec.validate().unwrap_err().to_string();
        assert!(
            err.contains("max_sessions") && err.contains("timed"),
            "{err}"
        );
        // The same value with scheduling.mode=timed is accepted.
        let spec: CampaignSpec = serde_json::from_str(&format!(
            "{base}, \"scheduling\": {{\"mode\": \"timed\"}}, \"knobs\": {{\"max_sessions\": 16}}}}"
        ))
        .unwrap();
        spec.validate().unwrap();
        // Inline delivery is rejected in timed mode (no entry in the
        // mode-wiring table), naming both offending fields.
        let spec: CampaignSpec = serde_json::from_str(&format!(
            "{base}, \"scheduling\": {{\"mode\": \"timed\"}}, \"supply\": {{\"delivery\": \"inline\"}}}}"
        ))
        .unwrap();
        let err = spec.validate().unwrap_err().to_string();
        assert!(
            err.contains("supply.delivery") && err.contains("inline") && err.contains("timed"),
            "{err}"
        );
        // Self-hosted + explicit substituters dependencies contradiction is rejected.
        let json = r#"{
            "mode": "self-hosted",
            "archive": {"digest": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},
            "cluster": {"gateway_store_url": "ssh-ng://rio@gw:22?ssh-key=/k",
                        "scheduler_addr": "s:9001", "store_addr": "st:9002",
                        "gateway_host_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPlaceholder gateway"},
            "tenants": {"build_tenant": "replay-selfhosted", "upstreams_verified": true},
            "supply": {"dependencies": "substituters"}}"#;
        let spec: CampaignSpec = serde_json::from_str(json).unwrap();
        assert!(
            spec.validate()
                .unwrap_err()
                .to_string()
                .contains("dependencies")
        );
    }

    /// A timed spec that pins `knobs.connections` must still cover the
    /// dispatcher's admission bound (the mode-wiring table's channel
    /// demand): the pool's channel capacity is the hard ceiling on
    /// concurrently held build channels, and an undersized pool would
    /// silently serialize the recorded cadence.
    #[test]
    fn timed_explicit_connections_must_cover_the_admission_bound() {
        // 3 connections × 4 channels = 12 < max_sessions 16 → rejected,
        // naming the knob, the capacity, and the demand.
        let spec: CampaignSpec = serde_json::from_str(&format!(
            "{}, \"scheduling\": {{\"mode\": \"timed\"}}, \
             \"knobs\": {{\"max_sessions\": 16, \"connections\": 3}}}}",
            spec_base()
        ))
        .unwrap();
        let err = spec.validate().unwrap_err().to_string();
        assert!(
            err.contains("knobs.connections") && err.contains("12") && err.contains("16"),
            "{err}"
        );
        // 4 connections × 4 channels = 16 ≥ max_sessions 16 → accepted.
        let spec: CampaignSpec = serde_json::from_str(&format!(
            "{}, \"scheduling\": {{\"mode\": \"timed\"}}, \
             \"knobs\": {{\"max_sessions\": 16, \"connections\": 4}}}}",
            spec_base()
        ))
        .unwrap();
        spec.validate().unwrap();
        // The demand is the larger of submit_concurrency and max_sessions:
        // submit_concurrency 24 demands 24 channels even with max_sessions
        // at 16, so 4 connections (16 channels) no longer cover it.
        let spec: CampaignSpec = serde_json::from_str(&format!(
            "{}, \"scheduling\": {{\"mode\": \"timed\"}}, \
             \"knobs\": {{\"max_sessions\": 16, \"connections\": 4, \"submit_concurrency\": 24}}}}",
            spec_base()
        ))
        .unwrap();
        let err = spec.validate().unwrap_err().to_string();
        assert!(err.contains("24"), "{err}");
        // Timeless campaigns have no cadence contract: an explicitly
        // undersized pool merely throttles throughput and stays accepted.
        let spec: CampaignSpec = serde_json::from_str(&format!(
            "{}, \"knobs\": {{\"connections\": 1, \"submit_concurrency\": 8}}}}",
            spec_base()
        ))
        .unwrap();
        spec.validate().unwrap();
    }

    #[test]
    fn campaign_record_comparability_seeded() {
        let spec: CampaignSpec = serde_json::from_str(
            r#"{"mode":"self-hosted","tenants":{"build_tenant":"replay-selfhosted"}}"#,
        )
        .unwrap();
        let rec = CampaignRecord::new(
            "c1".into(),
            "2026-05-26T00:00:00Z".into(),
            spec,
            ArchivePin {
                archive_id: "ab".repeat(32),
                archive_id_short: "ab".repeat(8),
            },
        );
        assert_eq!(rec.comparability.mode, "self-hosted");
        // The historical comparability field names carry the archive
        // identity: short id under `evalSet`, full id under `manifestSha256`.
        assert_eq!(rec.comparability.eval_set, "ab".repeat(8));
        assert_eq!(rec.comparability.manifest_sha256, "ab".repeat(32));
        assert_eq!(rec.archive.archive_id, "ab".repeat(32));
        assert_eq!(rec.comparability.engine_version, ENGINE_VERSION);
        // Campaign identity copied from the spec: the scheduling mode and
        // the effective supply policy (self-hosted mode derives "none" when
        // the spec leaves the dependency policy unset).
        assert_eq!(
            rec.comparability.scheduling_mode.as_deref(),
            Some("timeless")
        );
        assert_eq!(rec.comparability.supply_policy.as_deref(), Some("none"));
        // Archive provenance is recorded at bootstrap (once the archive
        // manifest is open), not here.
        assert_eq!(rec.comparability.archive_created_at, None);
        assert_eq!(rec.comparability.archive_age_days, None);
        assert_eq!(rec.comparability.exclusions_recorded, None);
    }

    #[test]
    fn comparability_block_context_fields_default_and_use_camel_case() {
        // A comparability block written before the archive/scheduling/supply
        // context fields existed still parses, with every new field at its
        // absent default.
        let pre_existing = r#"{
            "evalSet": "0123456789abcdef",
            "manifestSha256": "0123",
            "mode": "leaf",
            "buildTenant": "replay-leaf",
            "inScope": 10, "attemptable": 8, "attempted": 8,
            "excluded": {}, "completenessPct": 100.0, "lowConfidence": []
        }"#;
        let block: ComparabilityBlock = serde_json::from_str(pre_existing).unwrap();
        assert_eq!(block.archive_created_at, None);
        assert_eq!(block.archive_age_days, None);
        assert_eq!(block.exclusions_recorded, None);
        assert_eq!(block.truth_collapse_conflicts, None);
        assert_eq!(block.prefetch_shortfall_pct, None);
        assert!(!block.timing_degraded);
        assert_eq!(block.scheduling_mode, None);
        assert_eq!(block.supply_policy, None);
        // The new fields use the block's camelCase wire form and round-trip.
        let mut block = block;
        block.archive_created_at = Some("2026-05-01T00:00:00Z".into());
        block.archive_age_days = Some(25.0);
        block.exclusions_recorded = Some(2);
        block.truth_collapse_conflicts = Some(1);
        block.prefetch_shortfall_pct = Some(1.5);
        block.timing_degraded = true;
        block.scheduling_mode = Some("timed".into());
        block.supply_policy = Some("substituters".into());
        let v = serde_json::to_value(&block).unwrap();
        for key in [
            "archiveCreatedAt",
            "archiveAgeDays",
            "exclusionsRecorded",
            "truthCollapseConflicts",
            "prefetchShortfallPct",
            "timingDegraded",
            "schedulingMode",
            "supplyPolicy",
        ] {
            assert!(v.get(key).is_some(), "missing wire key {key}: {v}");
        }
        let re: ComparabilityBlock = serde_json::from_value(v).unwrap();
        assert_eq!(re, block);
    }

    #[test]
    fn archive_provenance_records_age_in_days() {
        let mut block = ComparabilityBlock::default();
        block.record_archive_provenance(
            "2026-05-01T00:00:00Z".parse().unwrap(),
            "2026-05-31T12:00:00Z",
        );
        assert_eq!(
            block.archive_created_at.as_deref(),
            Some("2026-05-01T00:00:00Z")
        );
        assert!((block.archive_age_days.unwrap() - 30.5).abs() < 1e-9);
        // An unparsable campaign timestamp leaves the age unset (never fails
        // bootstrap); the archive timestamp is still recorded.
        let mut block = ComparabilityBlock::default();
        block.record_archive_provenance("2026-05-01T00:00:00Z".parse().unwrap(), "not-a-time");
        assert_eq!(
            block.archive_created_at.as_deref(),
            Some("2026-05-01T00:00:00Z")
        );
        assert_eq!(block.archive_age_days, None);
    }

    #[test]
    fn schedule_mode_and_supply_dependency_strings_match_serde_forms() {
        for mode in [ScheduleMode::Timeless, ScheduleMode::Timed] {
            assert_eq!(
                serde_json::to_value(mode).unwrap(),
                serde_json::json!(mode.as_str())
            );
        }
        for deps in [
            SupplyDependencies::Substituters,
            SupplyDependencies::EmbeddedOnly,
            SupplyDependencies::None,
        ] {
            assert_eq!(
                serde_json::to_value(deps).unwrap(),
                serde_json::json!(deps.as_str())
            );
        }
    }
}
