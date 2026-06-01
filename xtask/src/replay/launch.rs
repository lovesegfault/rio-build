//! `cargo xtask replay launch` — provision the campaign tenants, keys and
//! Secrets, run the launch pre-flight, and apply the campaign Job.
//!
//! The in-cluster engine (`rio-replay run`) is driven entirely by a
//! campaign spec file: launch builds that spec with the engine's own
//! [`CampaignSpec`] types, validates it locally, ships it as the
//! `<campaign-id>-spec` ConfigMap (key [`jobs::SPEC_FILENAME`], mounted at
//! [`jobs::SPEC_MOUNT_DIR`]), and points the Job argv at the mounted copy
//! (`run --spec /etc/rio/replay/spec.json`). The spec pins `campaign_id`
//! to the Job name so a rescheduled pod resumes from the campaign's S3
//! prefix, and records the deployed component versions the pre-flight
//! verified (`cluster_versions`) plus the tenant-upstream snapshot.
//!
//! Tenant provisioning is direct: `rio-cli create-tenant` with the
//! campaign GC retention, `upstream add` for the substituting tenants
//! only, and an authorized-keys merge whose key comments equal the tenant
//! names (the gateway routes builds by that comment). Never `k8s grant` —
//! it unconditionally adds cache.nixos.org, which would corrupt
//! replay-selfhosted's deliberately empty upstream set.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use k8s_openapi::api::batch::v1::Job;
use kube::api::Api;
use rio_replay::archive::reader::ReplayArchive;
use rio_replay::archive::s3::{
    ARCHIVE_COMPLETE_OBJECT, ARCHIVE_MANIFEST_OBJECT, ArchiveStore, CompleteMarker,
};
use rio_replay::archive::schema::{Capabilities, Manifest};
use rio_replay::archive::writer::pack_with_mkdwarfs;
use rio_replay::run::spec::{
    ArchiveRef, CampaignSpec, ClusterEndpoints, FailOn, Filters, Knobs, Mode as EngineMode,
    ReportBlock, ReportPolicy, S3Target, ScheduleMode, SchedulingBlock, TenantBlock,
};

use super::jobs::{self, EngineJobCommon};
use super::{NS_REPLAY, TENANT_MATRIX, TENANT_RETENTION_HOURS, TENANT_WARM, preflight, s3};
use crate::k8s::client as kclient;
use crate::k8s::eks::smoke::{CliCtx, step_restart_gateway, step_upstream};
use crate::k8s::eks::{TF_DIR, push};
use crate::k8s::shared;
use crate::{git, ssh, tofu, ui};

#[derive(Args)]
pub struct LaunchArgs {
    /// Hydra evaluation id whose recorded replay archive the campaign
    /// consumes (must have been produced by `replay record` first). This
    /// is the recorder-convenience alias for Hydra-derived archives;
    /// exactly one of --eval / --archive names the campaign input.
    #[arg(long)]
    pub eval: Option<u64>,
    /// Recipe digest (or unambiguous prefix) to pin when more than one
    /// archive exists for --eval. A full 64-hex digest is resolved
    /// directly via the recorder's by-recipe pointer; a shorter value
    /// narrows the listing under `replay/archives/`. Discover candidates
    /// with `aws s3 ls s3://<chunk-bucket>/replay/archives/`.
    #[arg(long)]
    pub eval_digest: Option<String>,
    /// Campaign input archive named by location instead of by recorder
    /// address: a local packed `.dwarfs` image, a local directory-form
    /// archive (packed with mkdwarfs first), or the `s3://bucket/prefix`
    /// of an already-published archive. Local archives are published
    /// write-once under `replay/archives/` before launch; the upload is
    /// skipped when the content-addressed digest is already published.
    /// Mutually exclusive with --eval / --eval-digest.
    #[arg(long)]
    pub archive: Option<String>,
    /// Dependency mode: leaf = dependencies substituted from
    /// cache.nixos.org and roots force-built; self-hosted = full closure
    /// built by rio.
    #[arg(long, value_enum, default_value_t = Mode::Leaf)]
    pub mode: Mode,
    /// Campaign id (Job name + S3 prefix). Default:
    /// `replay-<mode>-<YYYYMMDD>-<4 hex>`.
    #[arg(long)]
    pub campaign_id: Option<String>,
    /// Cap on attempted jobs (smoke runs: 10-50). Recorded in the
    /// campaign spec's filters.
    #[arg(long)]
    pub limit: Option<usize>,
    /// Extra args appended verbatim to the engine `run` invocation
    /// (escape hatch while the engine CLI stabilises; must be valid
    /// `rio-replay run` flags, e.g. `--deadline <rfc3339>`).
    #[arg(long = "engine-arg", allow_hyphen_values = true)]
    pub engine_args: Vec<String>,
    /// Proceed when the deployed gateway/scheduler tags don't match this
    /// tree's tag (the skew is recorded in the spec's cluster_versions
    /// and the run is low-confidence).
    #[arg(long)]
    pub allow_version_skew: bool,
    /// Rollout-restart the gateway after merging the campaign tenant
    /// keys instead of waiting ~70s for the authorized_keys hot reload.
    #[arg(long)]
    pub restart_gateway: bool,
    /// Skip pre-flight checks (debugging only — the run will not be
    /// comparable; the spec records the tenants as unverified and the
    /// engine is started with --allow-unverified-tenants).
    #[arg(long)]
    pub skip_preflight: bool,
    /// Report policy applied at report time (repeatable; default: parity
    /// alone). The regression-gate policy makes the engine write
    /// report/gate.json, which `replay report --check` maps to its exit
    /// code.
    #[arg(long = "report-policy", value_enum, default_values_t = [ReportPolicyArg::Parity])]
    pub report_policy: Vec<ReportPolicyArg>,
    /// Regression-gate trip condition recorded in the spec. Requires
    /// --report-policy regression-gate (the engine's spec validation
    /// enforces it); never an engine exit-code knob — the gate is consumed
    /// by `replay report --check`.
    #[arg(long, value_enum, default_value_t = FailOnArg::None)]
    pub fail_on: FailOnArg,
    /// When submissions happen: timeless = queue-driven dispatch ignoring
    /// recorded timing; timed = recorded request offsets divided by
    /// --speedup. Timed scheduling requires an archive recorded with the
    /// `timed` capability (launch refuses early; the engine re-validates).
    #[arg(long, value_enum, default_value_t = Schedule::Timeless)]
    pub schedule: Schedule,
    /// Divisor applied to recorded request offsets when building the timed
    /// schedule (recorded into the spec's knobs). Only meaningful with
    /// --schedule timed — the engine's spec validation refuses a
    /// non-default value in timeless mode.
    #[arg(long, default_value_t = 1.0)]
    pub speedup: f64,
    /// RUST_LOG for the campaign pod.
    #[arg(long, default_value = "info,rio_replay=debug")]
    pub log_level: String,
}

/// Campaign dependency mode (CLI surface). Mirrors the engine's spec-level
/// mode enum; [`Mode::engine`] converts.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Leaf,
    SelfHosted,
}

impl Mode {
    /// Spelling used by the engine (`leaf` / `self-hosted`).
    pub fn as_str(self) -> &'static str {
        self.engine().as_str()
    }

    /// The engine-side mode this CLI value maps to.
    pub fn engine(self) -> rio_replay::run::spec::Mode {
        match self {
            Mode::Leaf => rio_replay::run::spec::Mode::Leaf,
            Mode::SelfHosted => rio_replay::run::spec::Mode::SelfHosted,
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Matches the clap ValueEnum rendering (`--mode self-hosted`).
        clap::ValueEnum::to_possible_value(self)
            .expect("no skipped variants")
            .get_name()
            .fmt(f)
    }
}

/// Campaign scheduling mode (CLI surface). Mirrors the engine's spec-level
/// [`ScheduleMode`]; [`Schedule::engine`] converts.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Schedule {
    Timeless,
    Timed,
}

impl Schedule {
    /// The engine-side scheduling mode this CLI value maps to.
    pub fn engine(self) -> ScheduleMode {
        match self {
            Schedule::Timeless => ScheduleMode::Timeless,
            Schedule::Timed => ScheduleMode::Timed,
        }
    }
}

impl std::fmt::Display for Schedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Matches the clap ValueEnum rendering (`--schedule timeless`),
        // which is also what clap prints for the default value in --help.
        clap::ValueEnum::to_possible_value(self)
            .expect("no skipped variants")
            .get_name()
            .fmt(f)
    }
}

/// Report policy (CLI surface). Mirrors the engine's spec-level
/// [`ReportPolicy`]; [`ReportPolicyArg::engine`] converts.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportPolicyArg {
    Parity,
    RegressionGate,
}

impl ReportPolicyArg {
    /// The engine-side report policy this CLI value maps to.
    pub fn engine(self) -> ReportPolicy {
        match self {
            ReportPolicyArg::Parity => ReportPolicy::Parity,
            ReportPolicyArg::RegressionGate => ReportPolicy::RegressionGate,
        }
    }
}

impl std::fmt::Display for ReportPolicyArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Matches the clap ValueEnum rendering (`--report-policy parity`),
        // which is also what clap prints for the default value in --help.
        clap::ValueEnum::to_possible_value(self)
            .expect("no skipped variants")
            .get_name()
            .fmt(f)
    }
}

/// Regression-gate trip condition (CLI surface). Mirrors the engine's
/// [`FailOn`]; [`FailOnArg::engine`] converts.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailOnArg {
    None,
    Regression,
    Divergence,
}

impl FailOnArg {
    /// The engine-side fail-on value this CLI value maps to.
    pub fn engine(self) -> FailOn {
        match self {
            FailOnArg::None => FailOn::None,
            FailOnArg::Regression => FailOn::Regression,
            FailOnArg::Divergence => FailOn::Divergence,
        }
    }
}

impl std::fmt::Display for FailOnArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Matches the clap ValueEnum rendering (`--fail-on regression`).
        clap::ValueEnum::to_possible_value(self)
            .expect("no skipped variants")
            .get_name()
            .fmt(f)
    }
}

/// Default campaign id: `replay-<mode>-<YYYYMMDD>-<4 hex>`. Lowercase
/// RFC-1123 so it is a valid Job name.
pub fn default_campaign_id(mode: Mode, now: jiff::Zoned, nonce: u16) -> String {
    format!(
        "replay-{}-{}-{:04x}",
        match mode {
            Mode::Leaf => "leaf",
            Mode::SelfHosted => "selfhosted",
        },
        now.strftime("%Y%m%d"),
        nonce
    )
}

/// Campaign ids become the Job name (which the Job controller also
/// stamps onto the campaign pods as the `job-name` label, so it must fit
/// the 63-char label-value limit), the spec-ConfigMap name
/// (`<id>-spec`), and the S3 prefix segment — they must be lowercase
/// RFC-1123 labels. The cap leaves room for the `-spec` suffix inside
/// the same 63-char budget so every derived name stays label-safe.
/// Shared with `replay repro`, whose derived ids face the same limits.
pub(super) fn validate_campaign_id(id: &str) -> Result<()> {
    let max = 63 - jobs::spec_configmap_name("").len();
    let charset_ok = id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    let ends_ok = id.starts_with(|c: char| c.is_ascii_alphanumeric())
        && id.ends_with(|c: char| c.is_ascii_alphanumeric());
    ensure!(
        !id.is_empty() && charset_ok && ends_ok && id.len() <= max,
        "campaign id {id:?} must be a lowercase RFC-1123 label (a-z, 0-9, '-', \
         alphanumeric first/last character) of at most {max} characters — it becomes the \
         campaign Job name (and so its pods' 63-char-capped `job-name` label) and the `{}` \
         ConfigMap name",
        jobs::spec_configmap_name("<id>")
    );
    Ok(())
}

/// Engine argv for the campaign Job. Everything campaign-specific (eval
/// set, mode, tenants, endpoints, limit) travels in the mounted spec, not
/// the argv: the argv only names the spec path, plus
/// `--allow-unverified-tenants` when the operator skipped the pre-flight
/// (the spec then records `upstreams_verified: false` and the engine
/// would otherwise refuse to plan). `--engine-arg` values are appended
/// verbatim.
pub fn engine_args(a: &LaunchArgs) -> Vec<String> {
    let mut args = vec!["run".into(), "--spec".into(), jobs::SPEC_MOUNT_PATH.into()];
    if a.skip_preflight {
        args.push("--allow-unverified-tenants".into());
    }
    args.extend(a.engine_args.iter().cloned());
    args
}

/// Correlation annotations for the campaign Job: which replay archive (by
/// its 16-char short id) it consumes, which dependency mode it runs, and —
/// when the archive was resolved through the recorder path — which Hydra
/// eval it was recorded from, so a Job seen in `kubectl` can be tied back
/// to its inputs without reading the spec ConfigMap. The
/// `rio.build/eval-set` annotation key keeps its historical name; its
/// value is the archive id short form. Takes the engine-side mode so both
/// Job creators (launch from its CLI flags, repro from a stored campaign
/// spec) annotate from the same type.
pub fn campaign_annotations(
    eval: Option<u64>,
    archive_id_short: &str,
    mode: EngineMode,
) -> BTreeMap<String, String> {
    let mut annotations = BTreeMap::from([
        (
            "rio.build/eval-set".to_string(),
            archive_id_short.to_string(),
        ),
        ("rio.build/mode".to_string(), mode.as_str().to_string()),
    ]);
    // Archives named by --archive carry no Hydra eval id (non-recorder
    // archives have none); the annotation is written only when known.
    if let Some(eval) = eval {
        annotations.insert("rio.build/hydra-eval-id".to_string(), eval.to_string());
    }
    annotations
}

/// In-cluster scheduler AdminService address recorded in the campaign
/// spec (ClusterIP Service, gRPC).
fn scheduler_addr() -> String {
    format!("rio-scheduler.{}.svc:9001", crate::k8s::NS)
}

/// In-cluster store StoreService address recorded in the campaign spec.
fn store_addr() -> String {
    format!("rio-store.{}.svc:9002", crate::k8s::NS_STORE)
}

/// ssh-ng store URL for one campaign tenant: the in-cluster gateway
/// Service with the tenant's mounted private key
/// ([`jobs::ssh_key_path`]) as the `ssh-key=` query parameter.
fn gateway_store_url(tenant: &str) -> String {
    format!(
        "ssh-ng://rio@rio-gateway.{}.svc:22?compress=true&ssh-key={}",
        crate::k8s::NS,
        jobs::ssh_key_path(tenant)
    )
}

/// The replay archive a campaign will run against, as resolved (and, for
/// local `--archive` inputs, published) from the launch flags.
pub struct ArchiveLocation {
    /// Full 64-hex archive id (SHA-256 of the archive's manifest.json).
    pub archive_id: String,
    /// 16-char short id — the S3 prefix segment.
    pub archive_id_short: String,
    /// Archive key prefix (no trailing slash),
    /// e.g. `replay/archives/8b919129046e0f60`.
    pub s3_prefix: String,
    /// Bucket holding the archive prefix. `None` = the campaign chunk
    /// bucket (recorder-resolved and locally-published archives);
    /// `Some` only for `--archive s3://…` references to another bucket.
    pub s3_bucket: Option<String>,
    /// Recipe digest from the archive's provenance (the eval recipe the
    /// recorder ran), used for --eval-digest narrowing and operator audit.
    /// `None` for archives not resolved through the recorder path.
    pub recipe_digest: Option<String>,
    /// Hydra eval id from the archive's provenance. `None` for archives
    /// not resolved through the recorder path.
    pub hydra_eval_id: Option<u64>,
}

/// How the operator named the campaign's input archive — the classified
/// `--eval` / `--eval-digest` / `--archive` flag combination.
#[derive(Debug)]
pub enum ArchiveInput {
    /// `--eval` (plus optional `--eval-digest`): the recorder-convenience
    /// alias, resolved through the recorder's S3 layout (by-recipe pointer
    /// or listing).
    Recorded {
        eval: u64,
        eval_digest: Option<String>,
    },
    /// `--archive s3://<bucket>/<prefix>`: an already-published archive,
    /// verified against its completion marker.
    S3 { bucket: String, prefix: String },
    /// `--archive <path to a .dwarfs file>`: a local packed image,
    /// published write-once before launch.
    LocalImage(PathBuf),
    /// `--archive <path to a directory>`: a local directory-form archive,
    /// packed with mkdwarfs and then published write-once before launch.
    LocalDir(PathBuf),
}

/// Classify the `--eval` / `--eval-digest` / `--archive` flags into an
/// [`ArchiveInput`]. Exactly one of `--eval` / `--archive` must name the
/// campaign input; every refusal names the accepted forms. The clap fields
/// stay plain `Option`s so this helper owns all input validation and stays
/// unit-testable (its only I/O is the local-path existence/file-type
/// probe).
fn archive_input(
    eval: Option<u64>,
    eval_digest: Option<&str>,
    archive: Option<&str>,
) -> Result<ArchiveInput> {
    match (eval, archive) {
        (None, None) => bail!(
            "the campaign input archive must be named: pass --eval <hydra eval id> (a \
             recorder-produced archive) or --archive <local path | s3://bucket/prefix>"
        ),
        (Some(_), Some(_)) => bail!(
            "--eval and --archive are mutually exclusive ways to name the campaign input \
             archive — pass exactly one"
        ),
        (None, Some(_)) if eval_digest.is_some() => {
            bail!("--eval-digest narrows --eval and is mutually exclusive with --archive")
        }
        (Some(eval), None) => Ok(ArchiveInput::Recorded {
            eval,
            eval_digest: eval_digest.map(str::to_string),
        }),
        (None, Some(archive)) => {
            if let Some(rest) = archive.strip_prefix("s3://") {
                let (bucket, prefix) = rest
                    .split_once('/')
                    .map(|(bucket, prefix)| (bucket, prefix.trim_end_matches('/')))
                    .filter(|(bucket, prefix)| !bucket.is_empty() && !prefix.is_empty())
                    .with_context(|| {
                        format!(
                            "--archive {archive:?} must name a published archive prefix as \
                             s3://<bucket>/<prefix> (e.g. \
                             s3://<chunk-bucket>/replay/archives/<archive-id-short>)"
                        )
                    })?;
                return Ok(ArchiveInput::S3 {
                    bucket: bucket.to_string(),
                    prefix: prefix.to_string(),
                });
            }
            // Any other URL scheme cannot be fetched: archives are either
            // already in S3 or local files.
            ensure!(
                !archive.contains("://"),
                "--archive {archive:?} is not an accepted archive form — pass the s3:// prefix \
                 of a published archive, a local .dwarfs image, or a local archive directory"
            );
            let path = Path::new(archive);
            ensure!(
                path.exists(),
                "--archive {archive:?} does not exist — pass the s3:// prefix of a published \
                 archive, a local .dwarfs image, or a local archive directory"
            );
            if path.is_dir() {
                Ok(ArchiveInput::LocalDir(path.to_path_buf()))
            } else {
                Ok(ArchiveInput::LocalImage(path.to_path_buf()))
            }
        }
    }
}

/// Refuse `--schedule timed` against an archive that was not recorded with
/// the `timed` capability (per-request offsets). Launch only fails fast
/// here — the engine re-validates the same gate at bootstrap.
fn ensure_timed_capability(schedule: Schedule, caps: &Capabilities) -> Result<()> {
    ensure!(
        schedule != Schedule::Timed || caps.timed,
        "--schedule timed requires an archive recorded with the `timed` capability \
         (per-request offsets), and this archive's manifest does not declare it — launch with \
         --schedule timeless, or record a timed archive"
    );
    Ok(())
}

/// What the pre-flight verified, recorded into the campaign spec: the
/// deployed image tags (→ `cluster_versions`) and the tenant upstream
/// snapshot (→ `tenants.*`).
pub struct PreflightOutcome {
    /// Deployment name → image tag as read from the cluster.
    pub deployed_tags: BTreeMap<String, String>,
    /// Tenant → upstream URLs listed at verification time.
    pub upstream_snapshot: BTreeMap<String, Vec<String>>,
    /// RFC3339 timestamp of the verification.
    pub verified_at: String,
}

/// Build the campaign spec the engine consumes. Pure so the unit tests
/// can prove the output round-trips through the engine's own
/// `CampaignSpec` parser and validator.
fn build_campaign_spec(
    a: &LaunchArgs,
    campaign_id: &str,
    archive: &ArchiveLocation,
    bucket: &str,
    hmac_present: bool,
    gateway_host_key: &str,
    preflight: Option<&PreflightOutcome>,
) -> CampaignSpec {
    let mode = a.mode.engine();
    CampaignSpec {
        // Pinned to the Job name so a pod rescheduled onto a fresh volume
        // resumes from this campaign's S3 prefix instead of minting a new
        // campaign.
        campaign_id: Some(campaign_id.to_owned()),
        mode,
        archive: ArchiveRef {
            // The archive may live in a different bucket than the campaign
            // artifacts (an --archive s3://… reference); recorder-resolved
            // and locally-published archives live in the chunk bucket.
            s3_bucket: Some(
                archive
                    .s3_bucket
                    .clone()
                    .unwrap_or_else(|| bucket.to_owned()),
            ),
            s3_prefix: Some(archive.s3_prefix.clone()),
            digest: archive.archive_id.clone(),
        },
        s3: S3Target {
            bucket: Some(bucket.to_owned()),
            // Default prefix (`replay/campaigns`) — also what
            // `super::s3::campaign_key` renders for status/report.
            ..S3Target::default()
        },
        cluster: ClusterEndpoints {
            gateway_store_url: gateway_store_url(mode.expected_build_tenant()),
            ssh_key_dir: Some(PathBuf::from(jobs::SSH_KEY_MOUNT_DIR)),
            scheduler_addr: scheduler_addr(),
            store_addr: store_addr(),
            service_hmac_key_path: hmac_present.then(|| PathBuf::from(jobs::HMAC_KEY_MOUNT_PATH)),
            gateway_host_key: Some(gateway_host_key.to_owned()),
        },
        tenants: TenantBlock {
            build_tenant: mode.expected_build_tenant().to_owned(),
            warm_tenant: TENANT_WARM.to_owned(),
            upstreams_verified: preflight.is_some(),
            upstreams_verified_at: preflight.map(|p| p.verified_at.clone()),
            upstream_snapshot: preflight
                .map(|p| p.upstream_snapshot.clone())
                .unwrap_or_default(),
        },
        filters: Filters {
            limit: a.limit,
            ..Filters::default()
        },
        // Scheduling mode and the speedup knob from the launch flags;
        // every other knob stays at the engine default (operators override
        // via --engine-arg or a hand-edited spec). The engine's spec
        // validation rejects a non-default speedup in timeless mode.
        scheduling: SchedulingBlock {
            mode: a.schedule.engine(),
        },
        knobs: Knobs {
            speedup: a.speedup,
            ..Knobs::default()
        },
        // Report policies chosen at launch; the engine's spec validation
        // (run below) rejects a fail-on condition without the
        // regression-gate policy.
        report: ReportBlock {
            policies: a.report_policy.iter().map(|p| p.engine()).collect(),
            fail_on: a.fail_on.engine(),
        },
        // Deployed image versions verified by the pre-flight, recorded
        // verbatim; None when --skip-preflight left them unverified.
        cluster_versions: preflight
            .and_then(|p| serde_json::to_value(&p.deployed_tags).ok())
            .filter(|v| v.as_object().is_some_and(|m| !m.is_empty())),
        // The deadline stays at the engine default; operators override per
        // campaign via --engine-arg or a hand-edited spec.
        ..CampaignSpec::default()
    }
}

pub async fn run(a: LaunchArgs) -> Result<()> {
    // An operator-chosen campaign id is validated up front so a bad label
    // fails before any S3 read or cluster mutation; the generated default
    // is always valid and is minted (and re-checked) further down.
    if let Some(id) = &a.campaign_id {
        validate_campaign_id(id)?;
    }
    // Classify the --eval/--archive flags before anything else: a missing
    // or contradictory input combination needs no AWS or cluster traffic
    // to be refused.
    let input = archive_input(a.eval, a.eval_digest.as_deref(), a.archive.as_deref())?;

    let tf = tofu::outputs(TF_DIR)?;
    let region = tf.get("region")?;
    let ecr = tf.get("ecr_registry")?;
    let bucket = tf.get("chunk_bucket_name")?;
    let role_arn = tf.get("replay_iam_role_arn")?;

    // The campaign Job pulls <ecr>/rio-replay:<tag> for the CURRENT tree;
    // refuse before creating anything if that tag was never pushed.
    let tag = git::image_tag(&git::open()?)?;
    ui::step("rio-replay image in ECR", || {
        push::assert_in_ecr("rio-replay", &tag, &region)
    })
    .await?;

    // Resolve (and for local inputs publish) the replay archive first: a
    // typo'd --eval, an eval Job that hasn't finished uploading, or a bad
    // --archive should fail in seconds, before any cluster mutation.
    let (archive, local_capabilities) = ui::step("resolve replay archive", || {
        resolve_archive_input(&region, &bucket, &input)
    })
    .await?;

    // Timed scheduling needs the archive's `timed` capability; check it
    // before any cluster mutation. Local inputs were opened above; for S3
    // and recorder-resolved archives the published standalone manifest is
    // fetched. The engine re-validates the same gate at bootstrap.
    if a.schedule == Schedule::Timed {
        let capabilities = match local_capabilities {
            Some(capabilities) => capabilities,
            None => {
                ui::step("fetch archive capabilities", || {
                    archive_capabilities_from_s3(&region, &bucket, &archive)
                })
                .await?
            }
        };
        ensure_timed_capability(a.schedule, &capabilities)?;
    }

    let client = kclient::client().await?;
    ui::step("rio-replay namespace + ServiceAccount", || {
        jobs::ensure_base(&client, &role_arn)
    })
    .await?;

    // Tenant provisioning + per-tenant SSH keys + namespace Secrets.
    let cli = ui::step("open scheduler/store tunnel", || {
        CliCtx::open(&client, 0, 0)
    })
    .await?;
    ui::step("campaign tenants (create + upstreams)", || {
        provision_tenants(&cli)
    })
    .await?;
    ui::step("campaign tenant SSH keys", || {
        ensure_tenant_keys(&client, a.restart_gateway)
    })
    .await?;
    let hmac_present = ui::step("copy service-HMAC Secret into rio-replay", || {
        copy_hmac_secret(&client)
    })
    .await?;
    // The engine pins the gateway's SSH host key (the spec field is
    // required by engine validation), so launch must be able to read it
    // from the deployed chart — independently of --skip-preflight.
    let gateway_host_key = ui::step("gateway SSH host-key pin", || async {
        match gateway_host_key_pin(&client).await? {
            Some(pin) => Ok(pin),
            None => bail!(
                "the deployed rio-gateway runs on an auto-generated (emptyDir) SSH host key, so \
                 there is nothing to pin and the campaign engine refuses to run without a pinned \
                 host key. Redeploy with a persistent host key — `cargo xtask k8s -p eks up \
                 --deploy` sets gateway.ssh.hostKeySecret=rio-gateway-host-key \
                 (Secret data key `host_key`) — then re-run launch."
            ),
        }
    })
    .await?;

    // Pre-flight: the deployed cluster — not this tree — is what the
    // campaign measures, so verify it before submitting anything.
    let preflight_outcome = if a.skip_preflight {
        ui::step_skip(
            "pre-flight",
            "--skip-preflight passed (run will not be comparable; engine gets --allow-unverified-tenants)",
        );
        None
    } else {
        Some(ui::step("pre-flight", || preflight_checks(&client, &cli, &a, &tag)).await?)
    };

    // Campaign id → Job name, spec-ConfigMap name, S3 prefix.
    let campaign_id = match &a.campaign_id {
        Some(id) => id.clone(),
        None => default_campaign_id(a.mode, jiff::Zoned::now(), rand::random::<u16>()),
    };
    validate_campaign_id(&campaign_id)?;

    // Campaign spec, built with the engine's own types and validated
    // locally so a malformed spec fails here, not minutes later in the
    // pod logs.
    let spec = build_campaign_spec(
        &a,
        &campaign_id,
        &archive,
        &bucket,
        hmac_present,
        &gateway_host_key,
        preflight_outcome.as_ref(),
    );
    spec.validate()
        .context("constructed campaign spec failed engine validation (launch bug)")?;
    let spec_json = serde_json::to_string_pretty(&spec)?;
    let cm_name = jobs::spec_configmap_name(&campaign_id);

    // A re-used campaign id must never swap the spec mounted by an
    // existing campaign: refuse before touching the ConfigMap when the
    // Job already exists, or when a leftover spec ConfigMap differs from
    // what this launch would write.
    ui::step(
        &format!("campaign id {campaign_id} not already in use"),
        || async {
            let jobs_api: Api<Job> = Api::namespaced(client.clone(), NS_REPLAY);
            let job_exists = jobs_api.get_opt(&campaign_id).await?.is_some();
            let existing_spec =
                kclient::get_configmap_key(&client, NS_REPLAY, &cm_name, jobs::SPEC_FILENAME)
                    .await?;
            guard_existing_campaign(
                &campaign_id,
                job_exists,
                existing_spec.as_deref(),
                &spec_json,
            )
        },
    )
    .await?;

    let spec_data = BTreeMap::from([(jobs::SPEC_FILENAME.to_string(), spec_json)]);
    ui::step(&format!("apply spec ConfigMap {cm_name}"), || {
        kclient::apply_configmap(
            &client,
            NS_REPLAY,
            &cm_name,
            spec_data,
            jobs::labels("replay-campaign"),
        )
    })
    .await?;

    // Campaign Job.
    let common = EngineJobCommon {
        image: format!("{ecr}/rio-replay:{tag}"),
        s3_bucket: bucket.clone(),
        region: region.clone(),
        log_level: a.log_level.clone(),
    };
    let mut job = jobs::campaign_job(&common, &campaign_id, &engine_args(&a))?;
    job.metadata
        .annotations
        .get_or_insert_with(Default::default)
        .extend(campaign_annotations(
            archive.hydra_eval_id,
            &archive.archive_id_short,
            a.mode.engine(),
        ));
    ui::step(&format!("apply campaign Job {campaign_id}"), || {
        jobs::create_job(&client, &job)
    })
    .await?;

    // Recorder provenance (Hydra eval + recipe) is shown only when the
    // archive was resolved through the recorder path; --archive inputs
    // have neither.
    let provenance = match (archive.hydra_eval_id, &archive.recipe_digest) {
        (Some(eval), Some(digest)) => {
            format!(
                ", hydra eval {eval}, recipe {}…",
                &digest[..digest.len().min(16)]
            )
        }
        _ => String::new(),
    };
    tracing::info!(
        "campaign launched: {campaign_id}\n  \
         archive:   {} (mode {}{provenance})\n  \
         progress:  cargo xtask replay status {campaign_id} --watch\n  \
         report:    cargo xtask replay report {campaign_id}\n  \
         logs:      kubectl -n {NS_REPLAY} logs -f job/{campaign_id}\n  \
         artifacts: s3://{bucket}/{}",
        archive.archive_id_short,
        a.mode.as_str(),
        s3::campaign_key(&campaign_id, "")
    );
    Ok(())
}

/// Resolve whichever [`ArchiveInput`] the operator named into the campaign
/// archive's location:
///
/// - `Recorded` goes through the recorder's S3 layout (by-recipe pointer or
///   listing) exactly as before;
/// - `S3` references must already carry the completion marker;
/// - local images/directories are published write-once under
///   `replay/archives/` first (the upload is skipped when the
///   content-addressed digest is already published).
///
/// For local inputs the archive's capability flags are returned alongside
/// (the archive was opened locally anyway), so the timed-capability gate
/// needs no extra S3 round trip.
async fn resolve_archive_input(
    region: &str,
    bucket: &str,
    input: &ArchiveInput,
) -> Result<(ArchiveLocation, Option<Capabilities>)> {
    match input {
        ArchiveInput::Recorded { eval, eval_digest } => {
            let location = resolve_archive(region, bucket, *eval, eval_digest.as_deref()).await?;
            Ok((location, None))
        }
        ArchiveInput::S3 {
            bucket: archive_bucket,
            prefix,
        } => {
            let location = resolve_published_archive(region, archive_bucket, prefix).await?;
            Ok((location, None))
        }
        ArchiveInput::LocalImage(path) => {
            let (location, capabilities) =
                publish_local_archive(region, bucket, path, false).await?;
            Ok((location, Some(capabilities)))
        }
        ArchiveInput::LocalDir(path) => {
            let (location, capabilities) =
                publish_local_archive(region, bucket, path, true).await?;
            Ok((location, Some(capabilities)))
        }
    }
}

/// Resolve an `--archive s3://…` reference: the prefix must carry the
/// completion marker (`complete.json` is uploaded last, so its absence
/// means the archive is not — or not yet — published), and the marker
/// names the archive id the spec pins.
async fn resolve_published_archive(
    region: &str,
    bucket: &str,
    prefix: &str,
) -> Result<ArchiveLocation> {
    let complete_key = format!("{prefix}/{ARCHIVE_COMPLETE_OBJECT}");
    let Some(text) = s3::get_text(region, bucket, &complete_key).await? else {
        bail!(
            "s3://{bucket}/{prefix} has no {ARCHIVE_COMPLETE_OBJECT} — the completion marker is \
             uploaded last, so this archive is not (yet) published; wait for its publisher to \
             finish, or check the prefix for a typo"
        );
    };
    let marker: CompleteMarker = serde_json::from_str(&text)
        .with_context(|| format!("parse s3://{bucket}/{complete_key}"))?;
    Ok(ArchiveLocation {
        archive_id: marker.archive_id,
        archive_id_short: marker.archive_id_short,
        s3_prefix: prefix.to_string(),
        s3_bucket: Some(bucket.to_string()),
        // Referenced archives are not resolved through the recorder path;
        // whatever provenance they carry stays in their manifest.
        recipe_digest: None,
        hydra_eval_id: None,
    })
}

/// Publish a local archive (a packed `.dwarfs` image, or a directory-form
/// archive packed here first) into the write-once `replay/archives/`
/// layout of the chunk bucket, returning where it landed plus its
/// capability flags. Content-addressed: when the archive id is already
/// published the upload is skipped entirely, so re-launching from the same
/// local archive is idempotent and cheap.
async fn publish_local_archive(
    region: &str,
    bucket: &str,
    path: &Path,
    is_dir: bool,
) -> Result<(ArchiveLocation, Capabilities)> {
    // Open (and validate) the local archive off the async executor —
    // DwarFS open is blocking work. Only v1 archives can be published:
    // v0 archives have no content-addressed identity to key the S3
    // layout by.
    let open_path = path.to_path_buf();
    let archive = tokio::task::spawn_blocking(move || ReplayArchive::open(&open_path))
        .await
        .context("archive open task panicked or was cancelled")?
        .with_context(|| format!("open replay archive {}", path.display()))?;
    let Some(archive_id) = archive.archive_id().map(str::to_string) else {
        bail!(
            "{} is a v0 archive, which has no content-addressed identity and cannot be \
             published to the write-once S3 layout — re-record it as a v1 archive",
            path.display()
        );
    };
    let archive_id_short = archive
        .archive_id_short()
        .expect("v1 archives always have a short id");
    let capabilities = *archive.capabilities();
    // The exact stored manifest bytes are the published standalone
    // manifest.json (the identity bytes) — never a re-serialization.
    let manifest_bytes = archive.manifest_bytes()?;

    // The published at-rest form is always the DwarFS image: directories
    // (the recorder/dev staging form) are packed into a temporary image
    // first. The TempDir guard must outlive the upload below.
    let _packed_image_dir;
    let image_path = if is_dir {
        let tempdir = tempfile::TempDir::new().context("create a staging dir for mkdwarfs")?;
        let image = tempdir.path().join("archive.dwarfs");
        _packed_image_dir = tempdir;
        let staging = path.to_path_buf();
        let packed = image.clone();
        // mkdwarfs is a long-running external process driven synchronously
        // by the packer; run it off the async executor.
        tokio::task::spawn_blocking(move || pack_with_mkdwarfs(&staging, &packed))
            .await
            .context("mkdwarfs packing task panicked or was cancelled")??;
        image
    } else {
        path.to_path_buf()
    };

    // Write-once publish, skipped when the content-addressed digest is
    // already there (`complete.json` is the authoritative marker).
    let client = aws_sdk_s3::Client::new(crate::aws::config(Some(region)).await);
    let store = ArchiveStore::new(bucket, super::S3_PREFIX);
    if store.is_complete(&client, &archive_id_short).await? {
        tracing::info!(
            "archive {archive_id_short} is already published — skipping the upload \
             (content-addressed)"
        );
    } else {
        store
            .publish(&client, &image_path, &manifest_bytes, "xtask-replay-launch")
            .await
            .with_context(|| format!("publish {} to s3://{bucket}", path.display()))?;
    }

    Ok((
        ArchiveLocation {
            archive_id,
            s3_prefix: s3::archive_prefix(&archive_id_short),
            archive_id_short,
            // Local archives are published into the campaign chunk bucket.
            s3_bucket: None,
            // Publishing is not the recorder path; provenance stays in the
            // archive manifest.
            recipe_digest: None,
            hydra_eval_id: None,
        },
        capabilities,
    ))
}

/// Fetch a published archive's standalone `manifest.json` and return its
/// capability flags — the timed-capability gate for archives that were not
/// opened locally (recorder-resolved and `--archive s3://…` inputs).
async fn archive_capabilities_from_s3(
    region: &str,
    chunk_bucket: &str,
    location: &ArchiveLocation,
) -> Result<Capabilities> {
    let bucket = location.s3_bucket.as_deref().unwrap_or(chunk_bucket);
    let manifest_key = format!("{}/{ARCHIVE_MANIFEST_OBJECT}", location.s3_prefix);
    let text = s3::get_text(region, bucket, &manifest_key)
        .await?
        .with_context(|| {
            format!(
                "s3://{bucket}/{manifest_key} does not exist — the archive prefix carries a \
                 completion marker but no standalone manifest"
            )
        })?;
    let manifest: Manifest = serde_json::from_str(&text)
        .with_context(|| format!("parse s3://{bucket}/{manifest_key}"))?;
    Ok(manifest.capabilities)
}

/// One archive prefix found under `replay/archives/`, with the provenance
/// fields candidate filtering reads from its manifest.json.
#[derive(Debug)]
struct ArchiveCandidate {
    archive_id_short: String,
    s3_prefix: String,
    archive_id: String,
    recipe_digest: String,
    hydra_eval_id: u64,
    created_at: String,
}

/// Whether `--eval-digest` is a full lowercase-hex recipe digest (64
/// chars) — the form the recorder keys its by-recipe pointers by. Shorter
/// values are treated as digest prefixes and resolved by listing.
fn is_full_recipe_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Find the replay archive recorded for `eval` under `replay/archives/`
/// in the chunk bucket. A full 64-hex `--eval-digest` takes the by-recipe
/// fast path (one GET of the recorder's idempotency pointer names the
/// archive prefix directly); otherwise the per-archive prefixes are
/// listed and each candidate's `manifest.json` provenance is read. Either
/// way [`pick_candidate`] must keep exactly one candidate, and the chosen
/// prefix must carry `complete.json` — the recorder uploads it last, so
/// its absence means the upload has not finished.
async fn resolve_archive(
    region: &str,
    bucket: &str,
    eval: u64,
    requested_digest: Option<&str>,
) -> Result<ArchiveLocation> {
    let candidates = match requested_digest {
        Some(digest) if is_full_recipe_digest(digest) => {
            by_recipe_candidates(region, bucket, eval, digest).await?
        }
        _ => listed_candidates(region, bucket).await?,
    };
    let chosen = pick_candidate(&candidates, eval, requested_digest).with_context(|| {
        format!(
            "resolve a replay archive in s3://{bucket}/{}",
            s3::archives_prefix()
        )
    })?;
    // complete.json is uploaded last: without it the prefix is a partial
    // upload and the engine would refuse it anyway.
    let complete_key = format!("{}/complete.json", chosen.s3_prefix);
    ensure!(
        s3::get_text(region, bucket, &complete_key).await?.is_some(),
        "s3://{bucket}/{} has no complete.json — the recorder Job has not finished \
         (complete.json is uploaded last)",
        chosen.s3_prefix
    );
    Ok(ArchiveLocation {
        archive_id: chosen.archive_id.clone(),
        archive_id_short: chosen.archive_id_short.clone(),
        s3_prefix: chosen.s3_prefix.clone(),
        // Recorder archives live in the campaign chunk bucket.
        s3_bucket: None,
        recipe_digest: Some(chosen.recipe_digest.clone()),
        hydra_eval_id: Some(chosen.hydra_eval_id),
    })
}

/// Read one archive prefix's standalone `manifest.json` and extract the
/// fields candidate filtering needs. `Ok(None)` when the manifest is not
/// there yet (an upload in flight, or a foreign prefix).
async fn read_candidate(
    region: &str,
    bucket: &str,
    archive_id_short: &str,
) -> Result<Option<ArchiveCandidate>> {
    let prefix = s3::archive_prefix(archive_id_short);
    let manifest_key = format!("{prefix}/manifest.json");
    let Some(text) = s3::get_text(region, bucket, &manifest_key).await? else {
        return Ok(None);
    };
    let manifest: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parse s3://{bucket}/{manifest_key}"))?;
    let provenance = &manifest["provenance"];
    Ok(Some(ArchiveCandidate {
        archive_id_short: archive_id_short.to_string(),
        s3_prefix: prefix,
        // The archive id is the SHA-256 over the manifest.json bytes; the
        // engine re-verifies it against the spec pin and the downloaded
        // complete.json marker before planning anything.
        archive_id: sha256_hex(text.as_bytes()),
        recipe_digest: provenance["recipe_digest"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        hydra_eval_id: provenance["source"]["hydra_eval_id"].as_u64().unwrap_or(0),
        created_at: manifest["created_at"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    }))
}

/// Listing path: every per-archive prefix under `replay/archives/` whose
/// standalone manifest is already uploaded becomes a candidate.
async fn listed_candidates(region: &str, bucket: &str) -> Result<Vec<ArchiveCandidate>> {
    let archives_prefix = s3::archives_prefix();
    let shorts = s3::list_subprefixes(region, bucket, &archives_prefix).await?;
    let mut candidates = Vec::new();
    for short in shorts {
        // The recorder-owned idempotency pointers live under by-recipe/;
        // they are not archive prefixes.
        if short == rio_replay::s3::BY_RECIPE_SEGMENT {
            continue;
        }
        if let Some(candidate) = read_candidate(region, bucket, &short).await? {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

/// By-recipe fast path: a full recipe digest names the recorder's
/// idempotency pointer (`replay/archives/by-recipe/<digest>.json`)
/// directly, so resolution costs one GET for the pointer plus one for the
/// archive's manifest instead of listing and reading every candidate.
async fn by_recipe_candidates(
    region: &str,
    bucket: &str,
    eval: u64,
    recipe_digest: &str,
) -> Result<Vec<ArchiveCandidate>> {
    let pointer_key = format!("{}{recipe_digest}.json", s3::by_recipe_prefix());
    let Some(text) = s3::get_text(region, bucket, &pointer_key).await? else {
        bail!(
            "recipe {recipe_digest} has not been recorded: s3://{bucket}/{pointer_key} does not \
             exist — run `cargo xtask replay record --eval {eval} …` first and wait for its Job to \
             complete (the recorder writes the by-recipe pointer after publishing the archive)"
        );
    };
    let pointer: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parse s3://{bucket}/{pointer_key}"))?;
    let short = pointer["archive_id_short"].as_str().unwrap_or_default();
    // A pointer that does not actually name an archive (garbage or empty
    // fields) is unusable — never probe a malformed archive prefix off it.
    ensure!(
        !short.is_empty(),
        "s3://{bucket}/{pointer_key} does not name an archive — re-record with \
         `cargo xtask replay record --eval {eval} … --force`, or pass --eval-digest as a shorter \
         prefix to resolve by listing instead"
    );
    let candidate = read_candidate(region, bucket, short)
        .await?
        .with_context(|| {
            format!(
                "the by-recipe pointer s3://{bucket}/{pointer_key} names archive {short}, but \
                 s3://{bucket}/{}/manifest.json does not exist — re-record with \
                 `cargo xtask replay record --eval {eval} … --force`",
                s3::archive_prefix(short)
            )
        })?;
    Ok(vec![candidate])
}

/// Choose exactly one archive among `cands` for `--eval` and the optional
/// `--eval-digest` recipe-digest prefix. Pure so the single / zero /
/// ambiguous outcomes are unit-testable without S3.
fn pick_candidate<'a>(
    cands: &'a [ArchiveCandidate],
    eval: u64,
    eval_digest: Option<&str>,
) -> Result<&'a ArchiveCandidate> {
    let matches: Vec<&ArchiveCandidate> = cands
        .iter()
        .filter(|c| c.hydra_eval_id == eval)
        .filter(|c| match eval_digest {
            Some(want) => c.recipe_digest.starts_with(want),
            None => true,
        })
        .collect();
    match matches.len() {
        0 => bail!(
            "no replay archive recorded from hydra eval {eval}{} under {} — run \
             `cargo xtask replay record --eval {eval} …` first and wait for its Job to complete",
            eval_digest
                .map(|d| format!(" with recipe digest {d}…"))
                .unwrap_or_default(),
            s3::archives_prefix()
        ),
        1 => Ok(matches[0]),
        n => {
            let listing: Vec<String> = matches
                .iter()
                .map(|c| {
                    format!(
                        "{} (recipe {}…, created {})",
                        c.archive_id_short,
                        &c.recipe_digest[..c.recipe_digest.len().min(16)],
                        c.created_at
                    )
                })
                .collect();
            bail!(
                "{n} replay archives were recorded from hydra eval {eval} ({}) — pass \
                 --eval-digest <recipe digest prefix> to pick one",
                listing.join(", ")
            );
        }
    }
}

/// Lowercase-hex SHA-256 of a byte slice — the archive id is this hash over
/// the standalone manifest.json bytes.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// Refuse to (re)write the `<campaign-id>-spec` ConfigMap when the
/// campaign id is already in use: a campaign Job with that name exists
/// (its pod mounts the ConfigMap and re-reads it on container restart),
/// or a leftover spec ConfigMap holds a different spec than this launch
/// would apply. Pure so the refusal logic is unit-testable; the caller
/// supplies the cluster facts. Shared with `replay repro`, which applies
/// the same ConfigMap+Job pair.
pub(super) fn guard_existing_campaign(
    campaign_id: &str,
    job_exists: bool,
    existing_spec: Option<&str>,
    new_spec: &str,
) -> Result<()> {
    let cm = jobs::spec_configmap_name(campaign_id);
    if job_exists {
        bail!(
            "campaign Job {NS_REPLAY}/{campaign_id} already exists — re-using its campaign id \
             would overwrite ConfigMap {NS_REPLAY}/{cm}, the spec that campaign mounts. Pick a \
             different --campaign-id, or — only if you mean to relaunch/resume THIS campaign — \
             delete the Job first (`kubectl -n {NS_REPLAY} delete job {campaign_id}`) and re-run \
             launch."
        );
    }
    if existing_spec.is_some_and(|old| old != new_spec) {
        bail!(
            "ConfigMap {NS_REPLAY}/{cm} already exists with a different campaign spec (campaign \
             id {campaign_id} was launched before). Refusing to overwrite it: pick a fresh \
             --campaign-id, or — if you are deliberately relaunching this campaign after deleting \
             its Job — delete the stale ConfigMap too (`kubectl -n {NS_REPLAY} delete configmap \
             {cm}`) and re-run; resume state lives under the campaign's S3 prefix, not in the old \
             ConfigMap."
        );
    }
    Ok(())
}

/// `rio-cli create-tenant` (with the campaign GC retention) for every
/// campaign tenant, plus `upstream add` for the substituting ones — the
/// upstream sets per tenant come from [`TENANT_MATRIX`]. Idempotent.
async fn provision_tenants(cli: &CliCtx) -> Result<()> {
    for (tenant, upstreams, _) in TENANT_MATRIX {
        create_tenant(cli, tenant).await?;
        if !upstreams.is_empty() {
            // step_upstream is idempotent and adds exactly cache.nixos.org
            // with the canonical trusted key — the only upstream the
            // substituting campaign tenants may have.
            step_upstream(cli, tenant).await?;
        }
    }
    Ok(())
}

/// `rio-cli create-tenant <tenant> --gc-retention-hours <…>`, tolerating
/// AlreadyExists so launch re-runs are idempotent (an existing tenant's
/// retention is left as is).
async fn create_tenant(cli: &CliCtx, tenant: &str) -> Result<()> {
    let retention = TENANT_RETENTION_HOURS.to_string();
    match cli.run(&["create-tenant", tenant, "--gc-retention-hours", &retention]) {
        Ok(out) => {
            ensure!(
                out.contains(&format!("tenant {tenant}")),
                "create-tenant {tenant} failed: {out}"
            );
            Ok(())
        }
        Err(e) if format!("{e:#}").to_lowercase().contains("already exists") => {
            tracing::info!("tenant '{tenant}' already exists (idempotent re-run)");
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("create-tenant {tenant}")),
    }
}

/// One ed25519 keypair per campaign tenant. Private halves live in the
/// rio-replay-ssh Secret (data key = tenant name, the file name
/// [`jobs::ssh_key_path`] points the spec's `ssh-key=` parameters at);
/// public halves are merged into rio-gateway-ssh with comment = tenant
/// (the gateway maps the key comment to `SubmitBuild.tenant_name`).
/// Existing private keys are REUSED so re-running launch neither grows
/// authorized_keys with orphans nor strands a running campaign's mounted
/// keys.
///
/// Shared with `replay setup` (pub(super)): setup provisions the same
/// Secrets up front so the cluster is campaign-ready before any launch;
/// launch keeps calling it so a launch on a never-setup cluster still
/// self-heals.
pub(super) async fn ensure_tenant_keys(
    client: &kclient::Client,
    restart_gateway: bool,
) -> Result<()> {
    use k8s_openapi::api::core::v1::Secret;

    let api: Api<Secret> = Api::namespaced(client.clone(), NS_REPLAY);
    let existing: BTreeMap<String, String> = api
        .get_opt(jobs::SSH_SECRET_NAME)
        .await?
        .and_then(|s| s.data)
        .map(|d| {
            d.into_iter()
                .filter_map(|(k, v)| String::from_utf8(v.0).ok().map(|s| (k, s)))
                .collect()
        })
        .unwrap_or_default();

    let mut secret_data = BTreeMap::new();
    let mut pub_lines = Vec::new();
    for (tenant, _, _) in TENANT_MATRIX {
        let (private, reused) = match existing.get(tenant) {
            Some(p) => (p.clone(), true),
            None => (ssh::generate(tenant)?.0, false),
        };
        // Re-derive the public half from whichever private key we ended up
        // with (fresh or reused) and force comment = tenant — the comment
        // is the gateway's tenant-routing key.
        let parsed = ssh_key::PrivateKey::from_openssh(&private).with_context(|| {
            if reused {
                format!(
                    "parse the existing private key for tenant {tenant} (data key {tenant:?} of \
                     Secret {NS_REPLAY}/{}) — delete that key from the Secret to have launch mint \
                     a fresh one",
                    jobs::SSH_SECRET_NAME
                )
            } else {
                format!("parse the freshly generated private key for {tenant}")
            }
        })?;
        let mut public = parsed.public_key().clone();
        public.set_comment(tenant);
        pub_lines.push(public.to_openssh()? + "\n");
        secret_data.insert(tenant.to_string(), private);
    }
    kclient::apply_secret(client, NS_REPLAY, jobs::SSH_SECRET_NAME, secret_data).await?;

    let refs: Vec<&str> = pub_lines.iter().map(String::as_str).collect();
    shared::merge_authorized_keys_batch(client, &refs).await?;

    if restart_gateway {
        step_restart_gateway(client).await?;
    } else {
        ui::step_skip(
            "rollout restart rio-gateway",
            "the gateway hot-reloads authorized_keys within ~70s; pass --restart-gateway to force it now",
        );
    }
    Ok(())
}

/// Copy the AdminService HMAC token Secret into the campaign namespace so
/// the Job's `service-hmac` volume resolves (accepted v1 risk; the M2
/// cleanup deletes it). Returns whether the key exists. On HMAC-less dev
/// clusters an EMPTY Secret is applied instead — the volume must still
/// mount or the pod never starts — and the spec records no
/// `service_hmac_key_path`, so the engine sends tokenless Admin reads
/// (matching a verifier-less scheduler).
async fn copy_hmac_secret(client: &kclient::Client) -> Result<bool> {
    let bytes = shared::secret_bytes(jobs::HMAC_SECRET_NAME, jobs::HMAC_KEY_FILENAME).await?;
    let present = bytes.is_some();
    let data = match bytes {
        Some(b) => BTreeMap::from([(jobs::HMAC_KEY_FILENAME.to_string(), b)]),
        None => {
            tracing::warn!(
                "Secret {}/{} not found — applying an empty Secret so the campaign pod can still \
                 mount it; engine Admin reads will be tokenless (fine on dev clusters, wrong on EKS)",
                crate::k8s::NS,
                jobs::HMAC_SECRET_NAME
            );
            BTreeMap::new()
        }
    };
    kclient::apply_secret_bytes(client, NS_REPLAY, jobs::HMAC_SECRET_NAME, data).await?;
    Ok(present)
}

/// Chart-defined name of the gateway pod volume holding the SSH host key
/// (`infra/helm/rio-build/templates/gateway.yaml`): a `secret` volume named
/// by `gateway.ssh.hostKeySecret` when that value is set, an `emptyDir`
/// (auto-generated key) otherwise.
const GATEWAY_STATE_VOLUME: &str = "gateway-state";

/// Data key holding the OpenSSH private host key inside the gateway
/// host-key Secret (mounted by the chart at /var/lib/rio/gateway/host_key).
const GATEWAY_HOST_KEY_DATA_KEY: &str = "host_key";

/// Read the gateway SSH host-key pin from the deployed cluster: find the
/// host-key Secret the rio-gateway Deployment mounts (its
/// [`GATEWAY_STATE_VOLUME`] volume is a `secret` volume when the chart's
/// `gateway.ssh.hostKeySecret` is set) and derive the OpenSSH public-key
/// line from the private key it holds. Returns `Ok(None)` when the gateway
/// runs on the auto-generated emptyDir host key (nothing stable to pin);
/// errors reading the Deployment or the Secret are returned as errors.
async fn gateway_host_key_pin(client: &kclient::Client) -> Result<Option<String>> {
    use k8s_openapi::api::apps::v1::Deployment;

    let api: Api<Deployment> = Api::namespaced(client.clone(), crate::k8s::NS);
    let deployment = api
        .get_opt("rio-gateway")
        .await
        .with_context(|| format!("read Deployment {}/rio-gateway", crate::k8s::NS))?
        .with_context(|| {
            format!(
                "Deployment {}/rio-gateway not found — deploy the rio-build chart before \
                 launching a campaign",
                crate::k8s::NS
            )
        })?;
    let volumes = deployment
        .spec
        .and_then(|spec| spec.template.spec)
        .and_then(|pod| pod.volumes)
        .unwrap_or_default();
    let Some(state_volume) = volumes
        .iter()
        .find(|volume| volume.name == GATEWAY_STATE_VOLUME)
    else {
        return Ok(None);
    };
    let Some(secret_name) = state_volume
        .secret
        .as_ref()
        .and_then(|source| source.secret_name.as_deref())
    else {
        // emptyDir (or anything that is not a secret volume): the host key
        // is auto-generated per pod and cannot be pinned.
        return Ok(None);
    };
    let bytes = shared::secret_bytes(secret_name, GATEWAY_HOST_KEY_DATA_KEY)
        .await?
        .with_context(|| {
            format!(
                "the rio-gateway Deployment mounts host-key Secret {}/{secret_name}, but that \
                 Secret does not exist (is the external-secrets sync healthy?)",
                crate::k8s::NS
            )
        })?;
    let private_key = String::from_utf8(bytes).with_context(|| {
        format!(
            "Secret {}/{secret_name} data key {GATEWAY_HOST_KEY_DATA_KEY:?} is not UTF-8 — \
             expected an OpenSSH private key",
            crate::k8s::NS
        )
    })?;
    let line = derive_openssh_public_key_line(&private_key).with_context(|| {
        format!(
            "derive the gateway host-key pin from Secret {}/{secret_name} (data key \
             {GATEWAY_HOST_KEY_DATA_KEY:?})",
            crate::k8s::NS
        )
    })?;
    Ok(Some(line))
}

/// Derive the OpenSSH public-key line (`ssh-ed25519 AAAA…`) from an
/// OpenSSH-format private key — the pin recorded in the campaign spec.
/// Pure so the derivation is unit-testable without a cluster.
fn derive_openssh_public_key_line(private_key_pem: &str) -> Result<String> {
    let key = ssh_key::PrivateKey::from_openssh(private_key_pem)
        .context("parse the gateway host key as an OpenSSH private key")?;
    Ok(key.public_key().to_openssh()?)
}

/// Launch pre-flight: deployed gateway/scheduler image tags vs this tree,
/// gateway build-policy entries for the campaign tenants, and per-tenant
/// upstream sets. Runs ALL checks and reports every failure at once — a
/// red pre-flight should hand the operator the complete fix list, not its
/// first item.
async fn preflight_checks(
    client: &kclient::Client,
    cli: &CliCtx,
    a: &LaunchArgs,
    tree_tag: &str,
) -> Result<PreflightOutcome> {
    let mut failures: Vec<String> = Vec::new();

    // 1. Deployed image tags vs this tree (the campaign measures the
    //    deployed cluster, not this checkout).
    let deployed_tags = match preflight::deployed_image_tags(client).await {
        Ok(tags) => {
            for (component, got) in &tags {
                if let Err(e) = preflight::check_image_tag(component, got, tree_tag) {
                    if a.allow_version_skew {
                        tracing::warn!(
                            "{e:#} (continuing under --allow-version-skew; the run is low-confidence)"
                        );
                    } else {
                        failures.push(format!(
                            "{e:#}\n   (or pass --allow-version-skew to record the skew and continue)"
                        ));
                    }
                }
            }
            tags
        }
        Err(e) => {
            failures.push(format!(
                "read deployed rio-gateway/rio-scheduler image tags: {e:#}"
            ));
            BTreeMap::new()
        }
    };

    // 2. Gateway build-policy entries for the campaign tenants (the
    //    gateway.toml rendered into the rio-gateway-config ConfigMap).
    match preflight::read_build_policy(client).await {
        Ok(Some(policy)) => {
            for (tenant, _, force_build_roots) in TENANT_MATRIX {
                if let Err(e) = preflight::check_build_policy(&policy, tenant, force_build_roots) {
                    failures.push(format!("{e:#}"));
                }
            }
        }
        Ok(None) => failures.push(
            "ConfigMap rio-system/rio-gateway-config (key gateway.toml) not found — the chart was \
             deployed without the gateway build-policy; enable replay on the release \
             (`cargo xtask replay setup`)"
                .to_string(),
        ),
        Err(e) => failures.push(format!("read deployed gateway build-policy: {e:#}")),
    }

    // 3. Tenant upstream sets (all three campaign tenants).
    let mut upstream_snapshot = BTreeMap::new();
    for (tenant, expected, _) in TENANT_MATRIX {
        let listed = match cli.run(&["upstream", "list", "--tenant", tenant, "--json"]) {
            Ok(out) => out,
            Err(e) => {
                failures.push(format!("list upstreams for tenant '{tenant}': {e:#}"));
                continue;
            }
        };
        match preflight::upstream_urls(&listed) {
            Ok(got) => {
                if let Err(e) = preflight::check_upstreams(tenant, &got, expected) {
                    failures.push(format!("{e:#}"));
                }
                upstream_snapshot.insert(tenant.to_string(), got.into_iter().collect());
            }
            Err(e) => failures.push(format!("parse upstream list for tenant '{tenant}': {e:#}")),
        }
    }

    if !failures.is_empty() {
        let list = failures
            .iter()
            .enumerate()
            .map(|(i, f)| format!("{}. {f}", i + 1))
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "pre-flight failed ({} problem{}):\n{list}\n\
             fix the above (or pass --skip-preflight for a non-comparable debug run) and re-run",
            failures.len(),
            if failures.len() == 1 { "" } else { "s" }
        );
    }

    Ok(PreflightOutcome {
        deployed_tags,
        upstream_snapshot,
        verified_at: jiff::Timestamp::now().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Fixture gateway host-key pin (what `gateway_host_key_pin` derives
    /// from the deployed host-key Secret on a real launch).
    const HOST_KEY_PIN: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPlaceholder rio-gateway";

    fn args(mode: Mode) -> LaunchArgs {
        LaunchArgs {
            eval: Some(1824219),
            eval_digest: None,
            archive: None,
            mode,
            campaign_id: None,
            limit: Some(50),
            engine_args: vec![],
            allow_version_skew: false,
            restart_gateway: false,
            skip_preflight: false,
            report_policy: vec![ReportPolicyArg::Parity],
            fail_on: FailOnArg::None,
            schedule: Schedule::Timeless,
            speedup: 1.0,
            log_level: "info".into(),
        }
    }

    fn archive_loc() -> ArchiveLocation {
        // Obviously-fake ids (the engine only requires a 64-hex digest);
        // the short form is the leading 16 chars, mirroring how the real S3
        // prefix segment is derived from the full archive id.
        let archive_id = "deadbeef".repeat(8);
        let archive_id_short = archive_id[..16].to_string();
        ArchiveLocation {
            s3_prefix: s3::archive_prefix(&archive_id_short),
            archive_id,
            archive_id_short,
            s3_bucket: None,
            recipe_digest: Some("feedc0de".repeat(8)),
            hydra_eval_id: Some(1824219),
        }
    }

    /// Candidate fixture for the pure archive-filtering tests; `archive_id`
    /// is irrelevant to filtering and left obviously fake.
    fn candidate(
        short: &str,
        eval: u64,
        recipe_digest: &str,
        created_at: &str,
    ) -> ArchiveCandidate {
        ArchiveCandidate {
            archive_id_short: short.to_string(),
            s3_prefix: s3::archive_prefix(short),
            archive_id: "ab".repeat(32),
            recipe_digest: recipe_digest.to_string(),
            hydra_eval_id: eval,
            created_at: created_at.to_string(),
        }
    }

    fn outcome() -> PreflightOutcome {
        PreflightOutcome {
            deployed_tags: BTreeMap::from([
                ("rio-gateway".to_string(), "abc123".to_string()),
                ("rio-scheduler".to_string(), "abc123".to_string()),
            ]),
            upstream_snapshot: BTreeMap::from([
                (
                    "replay-leaf".to_string(),
                    vec!["https://cache.nixos.org".to_string()],
                ),
                ("replay-selfhosted".to_string(), vec![]),
                (
                    "replay-warm".to_string(),
                    vec!["https://cache.nixos.org".to_string()],
                ),
            ]),
            verified_at: "2026-06-01T12:00:00Z".into(),
        }
    }

    #[test]
    fn tenant_matrix_matches_design_modes() {
        let m = TENANT_MATRIX;
        assert_eq!(
            m[0],
            ("replay-leaf", &["https://cache.nixos.org"][..], true)
        );
        assert_eq!(m[1], ("replay-selfhosted", &[][..], false));
        assert_eq!(
            m[2],
            ("replay-warm", &["https://cache.nixos.org"][..], false)
        );
    }

    #[test]
    fn default_campaign_id_shape() {
        let now = jiff::civil::date(2026, 6, 1)
            .at(12, 0, 0, 0)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .unwrap();
        let id = default_campaign_id(Mode::Leaf, now.clone(), 0xab12);
        assert_eq!(id, "replay-leaf-20260601-ab12");
        validate_campaign_id(&id).unwrap();
        assert!(id.len() <= 63);
        assert!(
            id.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        );
        // The self-hosted segment stays hyphen-free so the id remains easy
        // to eyeball-split on '-'.
        let id = default_campaign_id(Mode::SelfHosted, now, 0x1);
        assert_eq!(id, "replay-selfhosted-20260601-0001");
        validate_campaign_id(&id).unwrap();
    }

    #[test]
    fn campaign_id_validation_rejects_bad_labels() {
        for bad in [
            "",
            "Has-Caps",
            "ends-with-dash-",
            "-starts-with-dash",
            "under_score",
            // 59 chars: a valid label on its own, but the 63-char budget
            // (Job name → `job-name` label value) leaves no room for the
            // derived `-spec` suffix we keep label-safe as well.
            &"a".repeat(59),
        ] {
            assert!(
                validate_campaign_id(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
        validate_campaign_id("replay-leaf-20260601-ab12").unwrap();
        validate_campaign_id(&"a".repeat(58)).unwrap();
    }

    #[test]
    fn engine_args_point_at_the_mounted_spec() {
        let a = args(Mode::Leaf);
        assert_eq!(
            engine_args(&a),
            vec!["run", "--spec", "/etc/rio/replay/spec.json"]
        );
        // Skipping the pre-flight produces an unverified-tenants spec; the
        // engine must be told that is intentional.
        let mut skipped = args(Mode::Leaf);
        skipped.skip_preflight = true;
        skipped.engine_args = vec!["--deadline".into(), "2026-06-08T00:00:00Z".into()];
        assert_eq!(
            engine_args(&skipped),
            vec![
                "run",
                "--spec",
                "/etc/rio/replay/spec.json",
                "--allow-unverified-tenants",
                "--deadline",
                "2026-06-08T00:00:00Z",
            ]
        );
    }

    #[test]
    fn derive_openssh_public_key_line_round_trips() {
        // Same generation path the gateway uses for its own host key.
        let (private, public) = ssh::generate("rio-gateway").unwrap();
        let line = derive_openssh_public_key_line(&private).unwrap();
        assert!(line.starts_with("ssh-ed25519 "), "{line}");
        // Key material matches the public half (comments may differ — the
        // transport compares key material only).
        let derived = ssh_key::PublicKey::from_openssh(&line).unwrap();
        let expected = ssh_key::PublicKey::from_openssh(public.trim()).unwrap();
        assert_eq!(derived.key_data(), expected.key_data());
        // Garbage is rejected with a parse error, never a bogus pin.
        assert!(derive_openssh_public_key_line("not a private key").is_err());
    }

    #[test]
    fn campaign_spec_round_trips_through_the_engine_types() {
        let spec = build_campaign_spec(
            &args(Mode::Leaf),
            "replay-leaf-20260601-ab12",
            &archive_loc(),
            "rio-build-chunks-deadbeef",
            true,
            HOST_KEY_PIN,
            Some(&outcome()),
        );
        // What the engine does on startup: parse + validate.
        let json_text = serde_json::to_string_pretty(&spec).unwrap();
        let parsed: CampaignSpec = serde_json::from_str(&json_text).unwrap();
        parsed.validate().unwrap();

        assert_eq!(
            parsed.campaign_id.as_deref(),
            Some("replay-leaf-20260601-ab12")
        );
        assert_eq!(parsed.mode, EngineMode::Leaf);
        assert_eq!(parsed.archive.digest, archive_loc().archive_id);
        assert_eq!(
            parsed.archive.s3_prefix.as_deref(),
            Some("replay/archives/deadbeefdeadbeef")
        );
        assert_eq!(
            parsed.archive.s3_bucket.as_deref(),
            Some("rio-build-chunks-deadbeef")
        );
        assert_eq!(
            parsed.s3.bucket.as_deref(),
            Some("rio-build-chunks-deadbeef")
        );
        assert_eq!(parsed.s3.prefix, "replay/campaigns");
        assert_eq!(
            parsed.cluster.gateway_store_url,
            "ssh-ng://rio@rio-gateway.rio-system.svc:22?compress=true&ssh-key=/etc/rio/replay-ssh/replay-leaf"
        );
        assert_eq!(
            parsed.cluster.ssh_key_dir.as_deref(),
            Some(std::path::Path::new("/etc/rio/replay-ssh"))
        );
        assert_eq!(
            parsed.cluster.scheduler_addr,
            "rio-scheduler.rio-system.svc:9001"
        );
        assert_eq!(parsed.cluster.store_addr, "rio-store.rio-store.svc:9002");
        assert_eq!(
            parsed.cluster.service_hmac_key_path,
            Some(PathBuf::from("/etc/rio/hmac/service-hmac.key"))
        );
        assert_eq!(
            parsed.cluster.gateway_host_key.as_deref(),
            Some(HOST_KEY_PIN)
        );
        assert_eq!(parsed.tenants.build_tenant, "replay-leaf");
        assert_eq!(parsed.tenants.warm_tenant, "replay-warm");
        assert!(parsed.tenants.upstreams_verified);
        assert_eq!(
            parsed.tenants.upstreams_verified_at.as_deref(),
            Some("2026-06-01T12:00:00Z")
        );
        assert_eq!(
            parsed.tenants.upstream_snapshot.get("replay-selfhosted"),
            Some(&vec![])
        );
        assert_eq!(parsed.filters.limit, Some(50));
        assert_eq!(
            parsed.cluster_versions,
            Some(json!({"rio-gateway": "abc123", "rio-scheduler": "abc123"}))
        );
        // Engine knob defaults are left untouched.
        assert_eq!(parsed.knobs.batch_max_jobs, 50);
        assert_eq!(parsed.knobs.batch_max_nodes, 4500);
        assert_eq!(parsed.deadline, None);
        // Default report block: the parity policy alone, observational gate.
        assert_eq!(parsed.report.policies, vec![ReportPolicy::Parity]);
        assert_eq!(parsed.report.fail_on, FailOn::None);
    }

    #[test]
    fn report_policy_flags_populate_the_spec_report_block() {
        // --report-policy regression-gate --fail-on regression lands in
        // spec.report with the engine's kebab-case wire strings, and the
        // engine validation launch always runs accepts it.
        let mut a = args(Mode::Leaf);
        a.report_policy = vec![ReportPolicyArg::RegressionGate];
        a.fail_on = FailOnArg::Regression;
        let spec = build_campaign_spec(
            &a,
            "replay-leaf-20260601-ab12",
            &archive_loc(),
            "rio-build-chunks-deadbeef",
            true,
            HOST_KEY_PIN,
            Some(&outcome()),
        );
        spec.validate().unwrap();
        let v = serde_json::to_value(&spec).unwrap();
        assert_eq!(v["report"]["policies"], json!(["regression-gate"]));
        assert_eq!(v["report"]["fail_on"], json!("regression"));

        // Both policies may be requested for one campaign (repeatable flag).
        let mut a = args(Mode::Leaf);
        a.report_policy = vec![ReportPolicyArg::Parity, ReportPolicyArg::RegressionGate];
        a.fail_on = FailOnArg::Divergence;
        let spec = build_campaign_spec(
            &a,
            "replay-leaf-20260601-ab12",
            &archive_loc(),
            "rio-build-chunks-deadbeef",
            true,
            HOST_KEY_PIN,
            Some(&outcome()),
        );
        spec.validate().unwrap();
        let v = serde_json::to_value(&spec).unwrap();
        assert_eq!(
            v["report"]["policies"],
            json!(["parity", "regression-gate"])
        );
        assert_eq!(v["report"]["fail_on"], json!("divergence"));

        // A fail-on condition without the regression-gate policy is refused
        // by the engine's own spec validation, which launch runs before
        // touching the cluster — no launch-side duplicate of that rule.
        let mut a = args(Mode::Leaf);
        a.fail_on = FailOnArg::Regression;
        let spec = build_campaign_spec(
            &a,
            "replay-leaf-20260601-ab12",
            &archive_loc(),
            "rio-build-chunks-deadbeef",
            true,
            HOST_KEY_PIN,
            Some(&outcome()),
        );
        let err = spec.validate().unwrap_err().to_string();
        assert!(err.contains("regression-gate"), "{err}");

        // The clap value names are the engine wire strings, so --help and
        // the spec JSON can never disagree on the vocabulary.
        for (arg, engine) in [
            (ReportPolicyArg::Parity, ReportPolicy::Parity),
            (
                ReportPolicyArg::RegressionGate,
                ReportPolicy::RegressionGate,
            ),
        ] {
            assert_eq!(arg.to_string(), engine.as_str());
            assert_eq!(arg.engine(), engine);
        }
        for (arg, engine) in [
            (FailOnArg::None, FailOn::None),
            (FailOnArg::Regression, FailOn::Regression),
            (FailOnArg::Divergence, FailOn::Divergence),
        ] {
            assert_eq!(arg.to_string(), engine.as_str());
            assert_eq!(arg.engine(), engine);
        }
    }

    #[test]
    fn self_hosted_or_skipped_preflight_spec_shape() {
        // Self-hosted: the self-hosted build tenant, same per-tenant SSH key
        // directory as leaf (the field is mode-independent).
        // hmac_present=false and no pre-flight outcome (--skip-preflight):
        // tokenless Admin reads, tenants recorded as unverified, no
        // cluster_versions claim.
        let spec = build_campaign_spec(
            &args(Mode::SelfHosted),
            "replay-selfhosted-20260601-0001",
            &archive_loc(),
            "rio-build-chunks-deadbeef",
            false,
            HOST_KEY_PIN,
            None,
        );
        spec.validate().unwrap();
        assert_eq!(spec.cluster.gateway_host_key.as_deref(), Some(HOST_KEY_PIN));
        assert_eq!(spec.mode, EngineMode::SelfHosted);
        assert_eq!(spec.tenants.build_tenant, "replay-selfhosted");
        assert_eq!(
            spec.cluster.gateway_store_url,
            gateway_store_url("replay-selfhosted")
        );
        assert_eq!(
            spec.cluster.ssh_key_dir.as_deref(),
            Some(std::path::Path::new("/etc/rio/replay-ssh"))
        );
        assert!(spec.cluster.service_hmac_key_path.is_none());
        assert!(!spec.tenants.upstreams_verified);
        assert_eq!(spec.tenants.upstreams_verified_at, None);
        assert!(spec.tenants.upstream_snapshot.is_empty());
        assert_eq!(spec.cluster_versions, None);
    }

    #[test]
    fn campaign_s3_layout_matches_engine_default_prefix() {
        // xtask's status/report S3 helpers and the engine's default
        // campaign prefix must name the same place.
        assert_eq!(
            format!("{}/campaigns", super::super::S3_PREFIX),
            S3Target::default().prefix
        );
        assert!(
            s3::campaign_key("c1", "progress.json")
                .starts_with(&format!("{}/c1/", S3Target::default().prefix))
        );
    }

    #[test]
    fn campaign_annotations_carry_eval_set_and_mode() {
        let ann = campaign_annotations(Some(1824219), "8b919129046e0f60", EngineMode::Leaf);
        assert_eq!(ann.get("rio.build/hydra-eval-id").unwrap(), "1824219");
        assert_eq!(ann.get("rio.build/eval-set").unwrap(), "8b919129046e0f60");
        assert_eq!(ann.get("rio.build/mode").unwrap(), "leaf");
        assert_eq!(
            campaign_annotations(Some(1), "d", EngineMode::SelfHosted)
                .get("rio.build/mode")
                .unwrap(),
            "self-hosted"
        );
        // Archives named by --archive (not resolved through the recorder
        // path) have no Hydra eval id; the annotation is simply absent.
        let ann = campaign_annotations(None, "8b919129046e0f60", EngineMode::Leaf);
        assert!(!ann.contains_key("rio.build/hydra-eval-id"));
        assert_eq!(ann.get("rio.build/eval-set").unwrap(), "8b919129046e0f60");
    }

    #[test]
    fn full_recipe_digests_take_the_by_recipe_fast_path() {
        // Exactly 64 lowercase hex characters — the recorder's pointer key.
        assert!(is_full_recipe_digest(&"ab".repeat(32)));
        // Anything else resolves by listing: prefixes, uppercase hex,
        // non-hex, wrong length.
        assert!(!is_full_recipe_digest("ab12"));
        assert!(!is_full_recipe_digest(&"AB".repeat(32)));
        assert!(!is_full_recipe_digest(&"zz".repeat(32)));
        assert!(!is_full_recipe_digest(&"ab".repeat(33)));
        assert!(!is_full_recipe_digest(""));
    }

    #[test]
    fn pick_candidate_requires_exactly_one_match() {
        let cands = vec![
            candidate(
                "aaaaaaaaaaaaaaaa",
                1824219,
                &"11".repeat(32),
                "2026-05-01T00:00:00Z",
            ),
            candidate(
                "bbbbbbbbbbbbbbbb",
                1824219,
                &"22".repeat(32),
                "2026-05-02T00:00:00Z",
            ),
            candidate(
                "cccccccccccccccc",
                999,
                &"33".repeat(32),
                "2026-05-03T00:00:00Z",
            ),
        ];

        // Single match: an eval recorded exactly once needs no digest.
        let only = pick_candidate(&cands, 999, None).unwrap();
        assert_eq!(only.archive_id_short, "cccccccccccccccc");

        // A recipe-digest prefix narrows same-eval candidates down to one.
        let narrowed = pick_candidate(&cands, 1824219, Some("22")).unwrap();
        assert_eq!(narrowed.archive_id_short, "bbbbbbbbbbbbbbbb");
        // The full digest narrows the same way (the by-recipe fast path
        // hands pick_candidate a single candidate, but the filter must
        // still hold for it).
        let full = pick_candidate(&cands, 1824219, Some(&"22".repeat(32))).unwrap();
        assert_eq!(full.archive_id_short, "bbbbbbbbbbbbbbbb");

        // Zero matches: the error names the eval, the listing prefix, and
        // the recorder command that produces an archive.
        let err = format!("{:#}", pick_candidate(&cands, 4242, None).unwrap_err());
        assert!(
            err.contains("no replay archive recorded from hydra eval 4242"),
            "{err}"
        );
        assert!(err.contains("replay/archives/"), "{err}");
        assert!(
            err.contains("cargo xtask replay record --eval 4242"),
            "{err}"
        );
        // Zero matches because the digest narrowed everything away: the
        // requested digest is named so the typo is visible.
        let err = format!(
            "{:#}",
            pick_candidate(&cands, 1824219, Some("ff")).unwrap_err()
        );
        assert!(err.contains("recipe digest ff"), "{err}");

        // Ambiguity: every match is listed with its short id, recipe-digest
        // prefix and creation time, plus the --eval-digest way out.
        let err = format!("{:#}", pick_candidate(&cands, 1824219, None).unwrap_err());
        assert!(err.contains("2 replay archives"), "{err}");
        assert!(
            err.contains("aaaaaaaaaaaaaaaa") && err.contains("bbbbbbbbbbbbbbbb"),
            "{err}"
        );
        assert!(
            err.contains(&"11".repeat(8)) && err.contains(&"22".repeat(8)),
            "{err}"
        );
        assert!(
            err.contains("2026-05-01T00:00:00Z") && err.contains("2026-05-02T00:00:00Z"),
            "{err}"
        );
        assert!(err.contains("--eval-digest"), "{err}");
        assert!(!err.contains("cccccccccccccccc"), "{err}");
    }

    #[test]
    fn archive_input_classification_and_exclusivity() {
        // Exactly one of --eval / --archive names the campaign input.
        let err = archive_input(None, None, None).unwrap_err().to_string();
        assert!(err.contains("--eval") && err.contains("--archive"), "{err}");
        let err = archive_input(
            Some(1824219),
            None,
            Some("s3://b/replay/archives/0123456789abcdef"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("mutually exclusive"), "{err}");
        // Recorder-convenience alias path is untouched.
        match archive_input(Some(1824219), Some("ab".repeat(32).as_str()), None).unwrap() {
            ArchiveInput::Recorded { eval, eval_digest } => {
                assert_eq!(eval, 1824219);
                assert_eq!(eval_digest.as_deref(), Some("ab".repeat(32).as_str()));
            }
            other => panic!("expected Recorded, got {other:?}"),
        }
        // s3:// URIs split into bucket + prefix; trailing slash tolerated.
        match archive_input(
            None,
            None,
            Some("s3://rio-chunks/replay/archives/0123456789abcdef/"),
        )
        .unwrap()
        {
            ArchiveInput::S3 { bucket, prefix } => {
                assert_eq!(bucket, "rio-chunks");
                assert_eq!(prefix, "replay/archives/0123456789abcdef");
            }
            other => panic!("expected S3, got {other:?}"),
        }
        // Local paths classify by form: .dwarfs image vs directory archive.
        let dir = tempfile::tempdir().unwrap();
        match archive_input(None, None, Some(dir.path().to_str().unwrap())).unwrap() {
            ArchiveInput::LocalDir(p) => assert_eq!(p, dir.path()),
            other => panic!("expected LocalDir, got {other:?}"),
        }
        let image = dir.path().join("a.dwarfs");
        std::fs::write(&image, b"placeholder").unwrap();
        match archive_input(None, None, Some(image.to_str().unwrap())).unwrap() {
            ArchiveInput::LocalImage(p) => assert_eq!(p, image),
            other => panic!("expected LocalImage, got {other:?}"),
        }
        // Anything else is rejected naming the accepted forms.
        let err = archive_input(None, None, Some("https://example.org/a"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("s3://") && err.contains(".dwarfs"), "{err}");
        let err = archive_input(None, None, Some("/no/such/path"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn scheduling_flags_populate_the_spec_and_validate() {
        // Defaults stay timeless / 1.0 so existing launches are byte-identical.
        let spec = build_campaign_spec(
            &args(Mode::Leaf),
            "c1",
            &archive_loc(),
            "rio-chunks",
            true,
            HOST_KEY_PIN,
            None,
        );
        assert_eq!(spec.scheduling.mode, ScheduleMode::Timeless);
        assert_eq!(spec.knobs.speedup, 1.0);
        spec.validate().unwrap();
        // --schedule timed --speedup 50 lands in scheduling.mode + knobs.speedup.
        let mut a = args(Mode::Leaf);
        a.schedule = Schedule::Timed;
        a.speedup = 50.0;
        let spec = build_campaign_spec(
            &a,
            "c1",
            &archive_loc(),
            "rio-chunks",
            true,
            HOST_KEY_PIN,
            None,
        );
        assert_eq!(spec.scheduling.mode, ScheduleMode::Timed);
        assert_eq!(spec.knobs.speedup, 50.0);
        spec.validate().unwrap();
        // A non-default speedup without timed scheduling is refused by the
        // engine's own spec validation, which launch already runs before
        // applying anything.
        let mut a = args(Mode::Leaf);
        a.speedup = 50.0;
        let spec = build_campaign_spec(
            &a,
            "c1",
            &archive_loc(),
            "rio-chunks",
            true,
            HOST_KEY_PIN,
            None,
        );
        let err = spec.validate().unwrap_err().to_string();
        assert!(err.contains("speedup") && err.contains("timed"), "{err}");
    }

    #[test]
    fn timed_schedule_requires_the_archive_timed_capability() {
        use rio_replay::archive::schema::Capabilities;
        let caps = Capabilities {
            timed: false,
            ..Capabilities::default()
        };
        let err = ensure_timed_capability(Schedule::Timed, &caps)
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed"), "{err}");
        ensure_timed_capability(Schedule::Timeless, &caps).unwrap();
        let caps = Capabilities {
            timed: true,
            ..Capabilities::default()
        };
        ensure_timed_capability(Schedule::Timed, &caps).unwrap();
    }

    #[test]
    fn guard_refuses_existing_job_or_differing_spec() {
        let id = "replay-leaf-20260601-ab12";
        let spec = r#"{"campaignId":"replay-leaf-20260601-ab12"}"#;

        // Existing Job: refuse regardless of ConfigMap state, with the
        // delete-Job escape hatch spelled out.
        let err = guard_existing_campaign(id, true, None, spec)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "{err}");
        assert!(
            err.contains("delete job replay-leaf-20260601-ab12"),
            "{err}"
        );

        // Leftover ConfigMap with a different spec: refuse and name the
        // ConfigMap to delete.
        let err = guard_existing_campaign(id, false, Some(r#"{"campaignId":"other"}"#), spec)
            .unwrap_err()
            .to_string();
        assert!(err.contains("different campaign spec"), "{err}");
        assert!(
            err.contains("delete configmap replay-leaf-20260601-ab12-spec"),
            "{err}"
        );

        // Identical leftover spec (e.g. a launch that failed after the
        // ConfigMap apply) and the fresh-id case are both fine.
        guard_existing_campaign(id, false, Some(spec), spec).unwrap();
        guard_existing_campaign(id, false, None, spec).unwrap();
    }
}
