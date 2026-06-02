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
    /// Path to a newline-separated explicit job list (overrides include_globs).
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
    /// count from the in-flight channel budget as
    /// ceil(`submit_concurrency` / `CHANNELS_PER_CONNECTION`), minimum 1.
    /// The divisor is the transport's own per-connection fan-out — a
    /// client-side blast-radius choice, not a gateway limit.
    pub connections: Option<usize>,
    /// Deadline in seconds for each probe / upload / path-info client op on
    /// a gateway channel (build submissions use the batch timeout instead).
    pub op_timeout_secs: u64,
    /// Store paths per `QueryValidPaths` probe call.
    pub probe_chunk: usize,
    /// Channels held for validity probing. Reserved for the supply planner;
    /// nothing reads it yet — the submitter probes on its own channel.
    pub probe_concurrency: usize,
    /// Planned-but-missing prefetch paths above this percentage of the
    /// planned prefetch set pause the campaign before execution starts;
    /// below it the shortfall is recorded as a low-confidence flag. The
    /// default is a starting point, not a calibrated value.
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
        // Recorded offsets are divided by the speedup; zero, NaN, infinity,
        // or a negative value would collapse or invert the schedule.
        anyhow::ensure!(
            self.knobs.speedup.is_finite() && self.knobs.speedup > 0.0,
            "campaign spec field knobs.speedup must be a positive finite number"
        );
        // The build-deadline cap is a duration; it must be a usable number
        // of hours for the same reason as the batch timeout above.
        anyhow::ensure!(
            self.knobs.build_timeout_cap_hours.is_finite()
                && self.knobs.build_timeout_cap_hours > 0.0,
            "campaign spec field knobs.build_timeout_cap_hours must be a positive finite number of hours"
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
        // Timed runs deliver all planned supply before the clock starts;
        // inline top-up during execution would corrupt the recorded timing.
        if self.scheduling.mode == ScheduleMode::Timed {
            anyhow::ensure!(
                self.supply.delivery == SupplyDelivery::Prewarm,
                "campaign spec field supply.delivery must be \"prewarm\" when scheduling.mode is \"timed\""
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

    #[test]
    fn timed_knob_validation() {
        // Base valid spec: an archive pin (any 64-char lowercase-hex digest)
        // plus the required cluster and tenant blocks.
        let base = r#"{
            "mode": "leaf",
            "archive": {"digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
            "cluster": {"gateway_store_url": "ssh-ng://rio@rio-gateway.rio-system.svc:22?ssh-key=/k",
                        "scheduler_addr": "rio-scheduler.rio-system.svc:9001",
                        "store_addr": "rio-store.rio-store.svc:9002",
                        "gateway_host_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPlaceholder gateway"},
            "tenants": {"build_tenant": "replay-leaf", "warm_tenant": "replay-warm",
                        "upstreams_verified": true}"#;
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
        // Inline delivery is rejected in timed mode.
        let spec: CampaignSpec = serde_json::from_str(&format!(
            "{base}, \"scheduling\": {{\"mode\": \"timed\"}}, \"supply\": {{\"delivery\": \"inline\"}}}}"
        ))
        .unwrap();
        assert!(
            spec.validate()
                .unwrap_err()
                .to_string()
                .contains("delivery")
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
        assert_eq!(block.prefetch_shortfall_pct, None);
        assert!(!block.timing_degraded);
        assert_eq!(block.scheduling_mode, None);
        assert_eq!(block.supply_policy, None);
        // The new fields use the block's camelCase wire form and round-trip.
        let mut block = block;
        block.archive_created_at = Some("2026-05-01T00:00:00Z".into());
        block.archive_age_days = Some(25.0);
        block.exclusions_recorded = Some(2);
        block.prefetch_shortfall_pct = Some(1.5);
        block.timing_degraded = true;
        block.scheduling_mode = Some("timed".into());
        block.supply_policy = Some("substituters".into());
        let v = serde_json::to_value(&block).unwrap();
        for key in [
            "archiveCreatedAt",
            "archiveAgeDays",
            "exclusionsRecorded",
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
