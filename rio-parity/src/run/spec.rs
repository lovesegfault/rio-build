//! Campaign spec (operator input), engine knobs, and the campaign.json record.
//!
//! [`CampaignSpec`] is what `xtask parity launch` writes (or a developer
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
pub const WARM_TENANT: &str = "parity-warm";

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
            Mode::Leaf => "parity-leaf",
            Mode::SelfHosted => "parity-selfhosted",
        }
    }
}

/// Which replay archive the campaign runs against.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ArchiveRef {
    /// S3 bucket holding `parity/archives/...` (None = same bucket as `s3.bucket`).
    pub s3_bucket: Option<String>,
    /// Full key prefix of the archive, e.g. `parity/archives/<archive_id_short>`.
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
    /// Key prefix for campaign artifacts, default `parity/campaigns`.
    pub prefix: String,
}

impl Default for S3Target {
    fn default() -> Self {
        Self {
            bucket: None,
            prefix: "parity/campaigns".into(),
        }
    }
}

/// Cluster endpoints the engine talks to.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ClusterEndpoints {
    /// ssh-ng URL for the build tenant (parity-leaf / parity-selfhosted),
    /// e.g. `ssh-ng://rio@rio-gateway.rio-system.svc:22?compress=true&ssh-key=/secrets/parity-leaf`.
    pub gateway_store_url: String,
    /// ssh-ng URL for the parity-warm tenant (leaf mode only).
    pub warm_store_url: Option<String>,
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
    /// `parity-leaf` or `parity-selfhosted`.
    pub build_tenant: String,
    /// `parity-warm` (unused in self-hosted mode).
    pub warm_tenant: String,
    /// Set by `xtask parity launch` after it asserted the parity tenants'
    /// upstream sets via rio-cli. The engine cannot perform that assertion
    /// itself: the ListTenants/ListUpstreams admin RPCs are allowlisted to
    /// operator CLIs and exclude `rio-parity`.
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
    pub evidence_ttl_hours: f64,
    pub batch_timeout_hours: f64,
    pub log_tail_bytes: usize,
    pub idle_polls_for_suspend: u32,
    pub ice_masked_cells_threshold: usize,
    pub dispatch_gap_threshold: i64,
    pub dispatch_gap_polls: u32,
    pub pause_queue_depth: Option<u32>,
    pub infra_pause_pct: f64,
    pub infra_low_confidence_pct: f64,
    pub hydra_unknown_threshold_pct: f64,
    pub report_top_n: usize,
    /// SSH connections the gateway transport pool dials. `None` derives the
    /// count from the in-flight channel budget as
    /// ceil(`submit_concurrency` / 4), minimum 1 (the gateway caps channels
    /// per connection at 4).
    pub connections: Option<usize>,
    /// Deadline in seconds for each probe / upload / path-info client op on
    /// a gateway channel (build submissions use the batch timeout instead).
    pub op_timeout_secs: u64,
    /// Store paths per `QueryValidPaths` probe call.
    pub probe_chunk: usize,
    /// Channels held for validity probing. Reserved for the supply planner;
    /// nothing reads it yet — the submitter probes on its own channel.
    pub probe_concurrency: usize,
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
            evidence_ttl_hours: 24.0,
            batch_timeout_hours: 24.0,
            log_tail_bytes: 65536,
            idle_polls_for_suspend: 3,
            ice_masked_cells_threshold: 3,
            dispatch_gap_threshold: 50,
            dispatch_gap_polls: 5,
            pause_queue_depth: None,
            infra_pause_pct: 25.0,
            infra_low_confidence_pct: 5.0,
            hydra_unknown_threshold_pct: 5.0,
            report_top_n: 20,
            connections: None,
            op_timeout_secs: 120,
            probe_chunk: 2000,
            probe_concurrency: 3,
        }
    }
}

/// The operator-provided campaign spec (input to `rio-parity run --spec`).
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
        // The batch timeout becomes the per-child kill deadline; zero, NaN,
        // or a negative value would kill every `nix build` child the moment
        // it spawns.
        anyhow::ensure!(
            self.knobs.batch_timeout_hours.is_finite() && self.knobs.batch_timeout_hours > 0.0,
            "campaign spec field knobs.batch_timeout_hours must be a positive finite number of hours"
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
}

/// Key of [`PlanOutput::counts`] holding the number of in-scope jobs.
/// Shared between the plan stage (which writes the counts map) and the
/// report path (which reads it into the comparability block) so the two
/// can never drift apart on the key spelling.
pub const PLAN_COUNT_IN_SCOPE: &str = "inScope";

/// Key of [`PlanOutput::counts`] holding the number of attemptable jobs
/// (in scope minus not-attemptable). See [`PLAN_COUNT_IN_SCOPE`].
pub const PLAN_COUNT_ATTEMPTABLE: &str = "attemptable";

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

    #[test]
    fn knob_defaults_match_design() {
        let k = Knobs::default();
        assert_eq!((k.batch_max_jobs, k.batch_max_nodes), (50, 4500));
        assert_eq!(k.narinfo_concurrency, 64);
        assert_eq!(k.max_queued_requeues, 2);
        assert_eq!(k.max_auto_retries, 1);
        assert_eq!(k.active_stall_hours, 6.0);
        assert_eq!(k.evidence_ttl_hours, 24.0);
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
            "archive": {{"digest": "{digest}", "s3_prefix": "parity/archives/0123456789abcdef"}},
            "cluster": {{"gateway_store_url": "ssh-ng://rio@rio-gateway.rio-system.svc:22?ssh-key=/k",
                        "scheduler_addr": "rio-scheduler.rio-system.svc:9001",
                        "store_addr": "rio-store.rio-store.svc:9002"}},
            "tenants": {{"build_tenant": "parity-leaf", "warm_tenant": "parity-warm",
                        "upstreams_verified": true}}
        }}"#,
            digest = "ab".repeat(32)
        );
        let spec: CampaignSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec.mode, Mode::Leaf);
        assert_eq!(spec.archive.digest, "ab".repeat(32));
        assert_eq!(spec.knobs.batch_max_jobs, 50);
        assert_eq!(spec.tenants.build_tenant, "parity-leaf");
        assert_eq!(spec.mode.expected_build_tenant(), "parity-leaf");
        // Round-trips.
        let re: CampaignSpec =
            serde_json::from_str(&serde_json::to_string(&spec).unwrap()).unwrap();
        assert_eq!(re.archive.digest, "ab".repeat(32));
        assert_eq!(
            re.archive.s3_prefix.as_deref(),
            Some("parity/archives/0123456789abcdef")
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
            "tenants": {{"build_tenant": "parity-leaf", "warm_tenant": "parity-warm"}}
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
            "tenants": {{"build_tenant": "parity-leaf", "warm_tenant": "parity-warm"}},
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
            "tenants": {{"build_tenant": "parity-leaf", "warm_tenant": "parity-warm"}}
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
            "tenants": {{"build_tenant": "parity-leaf", "warm_tenant": "parity-warm",
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
        // carrying operator annotations) still parses and validates.
        let json = format!(
            r#"{{
            "mode": "leaf",
            "archive": {{"digest": "{digest}"}},
            "cluster": {{"gateway_store_url": "ssh-ng://rio@rio-gateway.rio-system.svc:22?ssh-key=/k",
                        "scheduler_addr": "rio-scheduler.rio-system.svc:9001",
                        "store_addr": "rio-store.rio-store.svc:9002",
                        "gateway_host_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPlaceholder gateway"}},
            "tenants": {{"build_tenant": "parity-leaf", "warm_tenant": "parity-warm",
                        "upstreams_verified": true}},
            "bogus_future_field": {{"nested": [1, 2, 3]}}
        }}"#,
            digest = "ab".repeat(32)
        );
        let spec: CampaignSpec = serde_json::from_str(&json).unwrap();
        spec.validate().unwrap();
        assert_eq!(spec.archive.digest, "ab".repeat(32));
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
            "s3_prefix": "parity/archives/0123456789abcdef",
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
            Some("parity/archives/0123456789abcdef")
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
    fn campaign_record_comparability_seeded() {
        let spec: CampaignSpec = serde_json::from_str(
            r#"{"mode":"self-hosted","tenants":{"build_tenant":"parity-selfhosted"}}"#,
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
    }
}
