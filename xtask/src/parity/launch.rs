//! `cargo xtask parity launch` — provision the campaign tenants, keys and
//! Secrets, run the launch pre-flight, and apply the campaign Job.
//!
//! The in-cluster engine (`rio-parity run`) is driven entirely by a
//! campaign spec file: launch builds that spec with the engine's own
//! [`CampaignSpec`] types, validates it locally, ships it as the
//! `<campaign-id>-spec` ConfigMap (key [`jobs::SPEC_FILENAME`], mounted at
//! [`jobs::SPEC_MOUNT_DIR`]), and points the Job argv at the mounted copy
//! (`run --spec /etc/rio/parity/spec.json`). The spec pins `campaign_id`
//! to the Job name so a rescheduled pod resumes from the campaign's S3
//! prefix, and records the deployed component versions the pre-flight
//! verified (`cluster_versions`) plus the tenant-upstream snapshot.
//!
//! Tenant provisioning is direct: `rio-cli create-tenant` with the
//! campaign GC retention, `upstream add` for the substituting tenants
//! only, and an authorized-keys merge whose key comments equal the tenant
//! names (the gateway routes builds by that comment). Never `k8s grant` —
//! it unconditionally adds cache.nixos.org, which would corrupt
//! parity-selfhosted's deliberately empty upstream set.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use k8s_openapi::api::batch::v1::Job;
use kube::api::Api;
use rio_parity::run::evalset_input::EvalSetMeta;
use rio_parity::run::spec::{
    CampaignSpec, ClusterEndpoints, EvalSetRef, Filters, S3Target, TenantBlock,
};

use super::jobs::{self, EngineJobCommon};
use super::{NS_PARITY, TENANT_MATRIX, TENANT_RETENTION_HOURS, TENANT_WARM, preflight, s3};
use crate::k8s::client as kclient;
use crate::k8s::eks::smoke::{CliCtx, step_restart_gateway, step_upstream};
use crate::k8s::eks::{TF_DIR, push};
use crate::k8s::shared;
use crate::{git, ssh, tofu, ui};

#[derive(Args)]
pub struct LaunchArgs {
    /// Hydra evaluation id whose eval set the campaign consumes
    /// (must have been produced by `parity eval` first).
    #[arg(long)]
    pub eval: u64,
    /// Eval-set key digest (or unambiguous prefix) to pin when more than
    /// one eval set exists for --eval. Discover them with
    /// `aws s3 ls s3://<chunk-bucket>/parity/evals/<eval>/`.
    #[arg(long)]
    pub eval_digest: Option<String>,
    /// Dependency mode: leaf = dependencies substituted from
    /// cache.nixos.org and roots force-built; self-hosted = full closure
    /// built by rio.
    #[arg(long, value_enum, default_value_t = Mode::Leaf)]
    pub mode: Mode,
    /// Campaign id (Job name + S3 prefix). Default:
    /// `parity-<mode>-<YYYYMMDD>-<4 hex>`.
    #[arg(long)]
    pub campaign_id: Option<String>,
    /// Cap on attempted jobs (smoke runs: 10-50). Recorded in the
    /// campaign spec's filters.
    #[arg(long)]
    pub limit: Option<usize>,
    /// Extra args appended verbatim to the engine `run` invocation
    /// (escape hatch while the engine CLI stabilises; must be valid
    /// `rio-parity run` flags, e.g. `--deadline <rfc3339>`).
    #[arg(long = "engine-arg", allow_hyphen_values = true)]
    pub engine_args: Vec<String>,
    /// Proceed when the deployed gateway/scheduler tags don't match this
    /// tree's tag (the skew is recorded in the spec's cluster_versions
    /// and the run is low-confidence).
    #[arg(long)]
    pub allow_version_skew: bool,
    /// Fail if the QueryDerivationStatuses AdminService RPC is absent
    /// (its absence is otherwise only a warning; collect falls back to
    /// GetBuildGraph under the 4,500-node batch cap).
    #[arg(long)]
    pub require_qds: bool,
    /// Rollout-restart the gateway after merging the campaign tenant
    /// keys instead of waiting ~70s for the authorized_keys hot reload.
    #[arg(long)]
    pub restart_gateway: bool,
    /// Skip pre-flight checks (debugging only — the run will not be
    /// comparable; the spec records the tenants as unverified and the
    /// engine is started with --allow-unverified-tenants).
    #[arg(long)]
    pub skip_preflight: bool,
    /// RUST_LOG for the campaign pod.
    #[arg(long, default_value = "info,rio_parity=debug")]
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
    pub fn engine(self) -> rio_parity::run::spec::Mode {
        match self {
            Mode::Leaf => rio_parity::run::spec::Mode::Leaf,
            Mode::SelfHosted => rio_parity::run::spec::Mode::SelfHosted,
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

/// Default campaign id: `parity-<mode>-<YYYYMMDD>-<4 hex>`. Lowercase
/// RFC-1123 so it is a valid Job name.
pub fn default_campaign_id(mode: Mode, now: jiff::Zoned, nonce: u16) -> String {
    format!(
        "parity-{}-{}-{:04x}",
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
fn validate_campaign_id(id: &str) -> Result<()> {
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

/// Correlation annotations for the campaign Job: which Hydra eval / eval
/// set it consumes and which dependency mode it runs, so a Job seen in
/// `kubectl` can be tied back to its inputs without reading the spec
/// ConfigMap.
pub fn campaign_annotations(eval: u64, short_digest: &str, mode: Mode) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("rio.build/hydra-eval-id".to_string(), eval.to_string()),
        ("rio.build/eval-set".to_string(), short_digest.to_string()),
        ("rio.build/mode".to_string(), mode.as_str().to_string()),
    ])
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

/// The eval set a campaign will run against, as resolved from S3.
pub struct EvalSetLocation {
    /// Full key digest as recorded in evalset.json.
    pub key_digest: String,
    /// 16-char short digest — the S3 prefix segment.
    pub short_digest: String,
    /// Eval-set key prefix in the chunk bucket (no trailing slash),
    /// e.g. `parity/evals/1824219/8b919129046e0f60`.
    pub s3_prefix: String,
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
    eval_set: &EvalSetLocation,
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
        eval_set: EvalSetRef {
            hydra_eval_id: a.eval,
            key_digest: eval_set.key_digest.clone(),
            // Same bucket as the campaign artifacts (s3.bucket below).
            s3_bucket: None,
            s3_prefix: Some(eval_set.s3_prefix.clone()),
        },
        s3: S3Target {
            bucket: Some(bucket.to_owned()),
            // Default prefix (`parity/campaigns`) — also what
            // `super::s3::campaign_key` renders for status/report.
            ..S3Target::default()
        },
        cluster: ClusterEndpoints {
            gateway_store_url: gateway_store_url(mode.expected_build_tenant()),
            warm_store_url: matches!(a.mode, Mode::Leaf).then(|| gateway_store_url(TENANT_WARM)),
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
        // Deployed image versions verified by the pre-flight, recorded
        // verbatim; None when --skip-preflight left them unverified.
        cluster_versions: preflight
            .and_then(|p| serde_json::to_value(&p.deployed_tags).ok())
            .filter(|v| v.as_object().is_some_and(|m| !m.is_empty())),
        // Knobs / hydra / deadline stay at the engine defaults; operators
        // override per campaign via --engine-arg or a hand-edited spec.
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

    let tf = tofu::outputs(TF_DIR)?;
    let region = tf.get("region")?;
    let ecr = tf.get("ecr_registry")?;
    let bucket = tf.get("chunk_bucket_name")?;
    let role_arn = tf.get("parity_iam_role_arn")?;

    // The campaign Job pulls <ecr>/rio-parity:<tag> for the CURRENT tree;
    // refuse before creating anything if that tag was never pushed.
    let tag = git::image_tag(&git::open()?)?;
    ui::step("rio-parity image in ECR", || {
        push::assert_in_ecr("rio-parity", &tag, &region)
    })
    .await?;

    // Resolve the eval set first: a typo'd --eval (or an eval Job that
    // hasn't finished uploading) should fail in seconds, before any
    // cluster mutation.
    let eval_set = ui::step("resolve eval set in S3", || {
        resolve_eval_set(&region, &bucket, a.eval, a.eval_digest.as_deref())
    })
    .await?;

    let client = kclient::client().await?;
    ui::step("rio-parity namespace + ServiceAccount", || {
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
    let hmac_present = ui::step("copy service-HMAC Secret into rio-parity", || {
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
                 --deploy --deploy-parity` sets gateway.ssh.hostKeySecret=rio-gateway-host-key \
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
        &eval_set,
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
            let jobs_api: Api<Job> = Api::namespaced(client.clone(), NS_PARITY);
            let job_exists = jobs_api.get_opt(&campaign_id).await?.is_some();
            let existing_spec =
                kclient::get_configmap_key(&client, NS_PARITY, &cm_name, jobs::SPEC_FILENAME)
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
            NS_PARITY,
            &cm_name,
            spec_data,
            jobs::labels("parity-campaign"),
        )
    })
    .await?;

    // Campaign Job.
    let common = EngineJobCommon {
        image: format!("{ecr}/rio-parity:{tag}"),
        s3_bucket: bucket.clone(),
        region: region.clone(),
        log_level: a.log_level.clone(),
    };
    let mut job = jobs::campaign_job(&common, &campaign_id, &engine_args(&a))?;
    job.metadata
        .annotations
        .get_or_insert_with(Default::default)
        .extend(campaign_annotations(a.eval, &eval_set.short_digest, a.mode));
    ui::step(&format!("apply campaign Job {campaign_id}"), || {
        jobs::create_job(&client, &job)
    })
    .await?;

    tracing::info!(
        "campaign launched: {campaign_id}\n  \
         eval set:  {}/{} (mode {})\n  \
         progress:  cargo xtask parity status {campaign_id} --watch\n  \
         report:    cargo xtask parity report {campaign_id}\n  \
         logs:      kubectl -n {NS_PARITY} logs -f job/{campaign_id}\n  \
         artifacts: s3://{bucket}/{}",
        a.eval,
        eval_set.short_digest,
        a.mode.as_str(),
        s3::campaign_key(&campaign_id, "")
    );
    Ok(())
}

/// Find the eval set for `eval` under `parity/evals/<eval>/` in the chunk
/// bucket and read its evalset.json (the upload-completeness marker) for
/// the full key digest the spec records. More than one eval set for the
/// same Hydra eval requires `--eval-digest` to disambiguate.
async fn resolve_eval_set(
    region: &str,
    bucket: &str,
    eval: u64,
    requested_digest: Option<&str>,
) -> Result<EvalSetLocation> {
    let prefix = s3::evals_prefix(eval);
    let digests = s3::list_subprefixes(region, bucket, &prefix).await?;
    if digests.is_empty() {
        bail!(
            "no eval set found under s3://{bucket}/{prefix} — run `cargo xtask parity eval \
             --eval {eval} …` first and wait for its Job to complete"
        );
    }
    let chosen = choose_digest(&digests, requested_digest)
        .with_context(|| format!("choose an eval set under s3://{bucket}/{prefix}"))?;
    let set_prefix = format!("{prefix}{chosen}");
    let meta_key = format!("{set_prefix}/evalset.json");
    let text = s3::get_text(region, bucket, &meta_key)
        .await?
        .with_context(|| {
            format!(
                "s3://{bucket}/{meta_key} not found — evalset.json is uploaded last, so the eval \
             Job is still running or failed mid-upload (check `kubectl -n {NS_PARITY} get jobs`)"
            )
        })?;
    let meta: EvalSetMeta =
        serde_json::from_str(&text).with_context(|| format!("parse s3://{bucket}/{meta_key}"))?;
    ensure!(
        meta.hydra_eval_id == eval,
        "evalset.json at s3://{bucket}/{meta_key} records hydra_eval_id {} (expected {eval})",
        meta.hydra_eval_id
    );
    ensure!(
        !meta.key_digest.is_empty(),
        "evalset.json at s3://{bucket}/{meta_key} has an empty key_digest"
    );
    Ok(EvalSetLocation {
        key_digest: meta.key_digest,
        short_digest: chosen.to_owned(),
        s3_prefix: set_prefix,
    })
}

/// Pick which eval-set `<key-digest>/` prefix to use out of the ones
/// found in S3. `requested` may be the full key digest recorded in
/// evalset.json or any prefix of the short (16-char) digest segment; it
/// must match exactly one candidate — an ambiguous prefix is an error,
/// never a silent first-match. Without `requested`, a single candidate
/// is picked automatically and several candidates ask the operator to
/// disambiguate with `--eval-digest`.
fn choose_digest<'a>(candidates: &'a [String], requested: Option<&str>) -> Result<&'a str> {
    match requested {
        Some(want) => {
            let matches: Vec<&str> = candidates
                .iter()
                .map(String::as_str)
                .filter(|d| d.starts_with(want) || want.starts_with(d))
                .collect();
            match matches.as_slice() {
                [one] => Ok(*one),
                [] => bail!(
                    "--eval-digest {want} matches none of the eval sets (found: {})",
                    candidates.join(", ")
                ),
                more => bail!(
                    "--eval-digest {want} is ambiguous — it matches {} eval sets ({}); pass more \
                     of the digest",
                    more.len(),
                    more.join(", ")
                ),
            }
        }
        None => match candidates {
            [one] => Ok(one.as_str()),
            [] => bail!("no eval sets found"),
            more => bail!(
                "{} eval sets exist ({}) — pass --eval-digest to pick one",
                more.len(),
                more.join(", ")
            ),
        },
    }
}

/// Refuse to (re)write the `<campaign-id>-spec` ConfigMap when the
/// campaign id is already in use: a campaign Job with that name exists
/// (its pod mounts the ConfigMap and re-reads it on container restart),
/// or a leftover spec ConfigMap holds a different spec than this launch
/// would apply. Pure so the refusal logic is unit-testable; the caller
/// supplies the cluster facts.
fn guard_existing_campaign(
    campaign_id: &str,
    job_exists: bool,
    existing_spec: Option<&str>,
    new_spec: &str,
) -> Result<()> {
    let cm = jobs::spec_configmap_name(campaign_id);
    if job_exists {
        bail!(
            "campaign Job {NS_PARITY}/{campaign_id} already exists — re-using its campaign id \
             would overwrite ConfigMap {NS_PARITY}/{cm}, the spec that campaign mounts. Pick a \
             different --campaign-id, or — only if you mean to relaunch/resume THIS campaign — \
             delete the Job first (`kubectl -n {NS_PARITY} delete job {campaign_id}`) and re-run \
             launch."
        );
    }
    if existing_spec.is_some_and(|old| old != new_spec) {
        bail!(
            "ConfigMap {NS_PARITY}/{cm} already exists with a different campaign spec (campaign \
             id {campaign_id} was launched before). Refusing to overwrite it: pick a fresh \
             --campaign-id, or — if you are deliberately relaunching this campaign after deleting \
             its Job — delete the stale ConfigMap too (`kubectl -n {NS_PARITY} delete configmap \
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
/// rio-parity-ssh Secret (data key = tenant name, the file name
/// [`jobs::ssh_key_path`] points the spec's `ssh-key=` parameters at);
/// public halves are merged into rio-gateway-ssh with comment = tenant
/// (the gateway maps the key comment to `SubmitBuild.tenant_name`).
/// Existing private keys are REUSED so re-running launch neither grows
/// authorized_keys with orphans nor strands a running campaign's mounted
/// keys.
async fn ensure_tenant_keys(client: &kclient::Client, restart_gateway: bool) -> Result<()> {
    use k8s_openapi::api::core::v1::Secret;

    let api: Api<Secret> = Api::namespaced(client.clone(), NS_PARITY);
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
                     Secret {NS_PARITY}/{}) — delete that key from the Secret to have launch mint \
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
    kclient::apply_secret(client, NS_PARITY, jobs::SSH_SECRET_NAME, secret_data).await?;

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
    kclient::apply_secret_bytes(client, NS_PARITY, jobs::HMAC_SECRET_NAME, data).await?;
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
/// gateway build-policy entries for the campaign tenants, per-tenant
/// upstream sets, and the QueryDerivationStatuses probe. Runs ALL checks
/// and reports every failure at once — a red pre-flight should hand the
/// operator the complete fix list, not its first item.
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
             deployed without the gateway build-policy; redeploy with parity.enabled=true \
             (`cargo xtask k8s -p eks up --deploy --deploy-parity`)"
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

    // 4. QueryDerivationStatuses presence: warning unless --require-qds,
    //    in which case both "absent" and "could not determine" refuse.
    match preflight::probe_query_derivation_statuses(&cli.sched_addr()).await {
        Ok(true) => tracing::info!("AdminService.QueryDerivationStatuses: present"),
        Ok(false) if a.require_qds => failures.push(
            "AdminService.QueryDerivationStatuses is not implemented on the deployed scheduler \
             (required by --require-qds; deploy that RPC first or drop the flag)"
                .to_string(),
        ),
        Ok(false) => tracing::warn!(
            "AdminService.QueryDerivationStatuses not implemented — collect falls back to \
             GetBuildGraph under the 4,500-node batch cap"
        ),
        Err(e) if a.require_qds => failures.push(format!(
            "QueryDerivationStatuses probe failed: {e:#} (presence required by --require-qds)"
        )),
        Err(e) => tracing::warn!("QueryDerivationStatuses probe failed ({e:#}); continuing"),
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
    use rio_parity::run::spec::Mode as EngineMode;
    use serde_json::json;

    /// Fixture gateway host-key pin (what `gateway_host_key_pin` derives
    /// from the deployed host-key Secret on a real launch).
    const HOST_KEY_PIN: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPlaceholder rio-gateway";

    fn args(mode: Mode) -> LaunchArgs {
        LaunchArgs {
            eval: 1824219,
            eval_digest: None,
            mode,
            campaign_id: None,
            limit: Some(50),
            engine_args: vec![],
            allow_version_skew: false,
            require_qds: false,
            restart_gateway: false,
            skip_preflight: false,
            log_level: "info".into(),
        }
    }

    fn eval_loc() -> EvalSetLocation {
        // Obviously-fake digests (the engine only requires a non-empty
        // key_digest); the short form is the leading 16 chars, mirroring
        // how the real S3 prefix segment is derived from the full digest.
        let key_digest = "deadbeef".repeat(8);
        let short_digest = key_digest[..16].to_string();
        EvalSetLocation {
            s3_prefix: format!("parity/evals/1824219/{short_digest}"),
            key_digest,
            short_digest,
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
                    "parity-leaf".to_string(),
                    vec!["https://cache.nixos.org".to_string()],
                ),
                ("parity-selfhosted".to_string(), vec![]),
                (
                    "parity-warm".to_string(),
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
            ("parity-leaf", &["https://cache.nixos.org"][..], true)
        );
        assert_eq!(m[1], ("parity-selfhosted", &[][..], false));
        assert_eq!(
            m[2],
            ("parity-warm", &["https://cache.nixos.org"][..], false)
        );
    }

    #[test]
    fn default_campaign_id_shape() {
        let now = jiff::civil::date(2026, 6, 1)
            .at(12, 0, 0, 0)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .unwrap();
        let id = default_campaign_id(Mode::Leaf, now.clone(), 0xab12);
        assert_eq!(id, "parity-leaf-20260601-ab12");
        validate_campaign_id(&id).unwrap();
        assert!(id.len() <= 63);
        assert!(
            id.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        );
        // The self-hosted segment stays hyphen-free so the id remains easy
        // to eyeball-split on '-'.
        let id = default_campaign_id(Mode::SelfHosted, now, 0x1);
        assert_eq!(id, "parity-selfhosted-20260601-0001");
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
        validate_campaign_id("parity-leaf-20260601-ab12").unwrap();
        validate_campaign_id(&"a".repeat(58)).unwrap();
    }

    #[test]
    fn engine_args_point_at_the_mounted_spec() {
        let a = args(Mode::Leaf);
        assert_eq!(
            engine_args(&a),
            vec!["run", "--spec", "/etc/rio/parity/spec.json"]
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
                "/etc/rio/parity/spec.json",
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
            "parity-leaf-20260601-ab12",
            &eval_loc(),
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
            Some("parity-leaf-20260601-ab12")
        );
        assert_eq!(parsed.mode, EngineMode::Leaf);
        assert_eq!(parsed.eval_set.hydra_eval_id, 1824219);
        assert_eq!(parsed.eval_set.key_digest, eval_loc().key_digest);
        assert_eq!(
            parsed.eval_set.s3_prefix.as_deref(),
            Some("parity/evals/1824219/deadbeefdeadbeef")
        );
        assert_eq!(parsed.eval_set.s3_bucket, None);
        assert_eq!(
            parsed.s3.bucket.as_deref(),
            Some("rio-build-chunks-deadbeef")
        );
        assert_eq!(parsed.s3.prefix, "parity/campaigns");
        assert_eq!(
            parsed.cluster.gateway_store_url,
            "ssh-ng://rio@rio-gateway.rio-system.svc:22?compress=true&ssh-key=/etc/rio/parity-ssh/parity-leaf"
        );
        assert_eq!(
            parsed.cluster.warm_store_url.as_deref(),
            Some(
                "ssh-ng://rio@rio-gateway.rio-system.svc:22?compress=true&ssh-key=/etc/rio/parity-ssh/parity-warm"
            )
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
        assert_eq!(parsed.tenants.build_tenant, "parity-leaf");
        assert_eq!(parsed.tenants.warm_tenant, "parity-warm");
        assert!(parsed.tenants.upstreams_verified);
        assert_eq!(
            parsed.tenants.upstreams_verified_at.as_deref(),
            Some("2026-06-01T12:00:00Z")
        );
        assert_eq!(
            parsed.tenants.upstream_snapshot.get("parity-selfhosted"),
            Some(&vec![])
        );
        assert_eq!(parsed.filters.limit, Some(50));
        assert_eq!(
            parsed.cluster_versions,
            Some(json!({"rio-gateway": "abc123", "rio-scheduler": "abc123"}))
        );
        // Engine knob/hydra defaults are left untouched.
        assert_eq!(parsed.knobs.batch_max_jobs, 50);
        assert_eq!(parsed.knobs.batch_max_nodes, 4500);
        assert_eq!(parsed.hydra.cache_url, "https://cache.nixos.org");
        assert_eq!(parsed.deadline, None);
    }

    #[test]
    fn self_hosted_or_skipped_preflight_spec_shape() {
        // Self-hosted: no warm store URL, the self-hosted build tenant.
        // hmac_present=false and no pre-flight outcome (--skip-preflight):
        // tokenless Admin reads, tenants recorded as unverified, no
        // cluster_versions claim.
        let spec = build_campaign_spec(
            &args(Mode::SelfHosted),
            "parity-selfhosted-20260601-0001",
            &eval_loc(),
            "rio-build-chunks-deadbeef",
            false,
            HOST_KEY_PIN,
            None,
        );
        spec.validate().unwrap();
        assert_eq!(spec.cluster.gateway_host_key.as_deref(), Some(HOST_KEY_PIN));
        assert_eq!(spec.mode, EngineMode::SelfHosted);
        assert_eq!(spec.tenants.build_tenant, "parity-selfhosted");
        assert_eq!(
            spec.cluster.gateway_store_url,
            gateway_store_url("parity-selfhosted")
        );
        assert!(spec.cluster.warm_store_url.is_none());
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
        let ann = campaign_annotations(1824219, "8b919129046e0f60", Mode::Leaf);
        assert_eq!(ann.get("rio.build/hydra-eval-id").unwrap(), "1824219");
        assert_eq!(ann.get("rio.build/eval-set").unwrap(), "8b919129046e0f60");
        assert_eq!(ann.get("rio.build/mode").unwrap(), "leaf");
        assert_eq!(
            campaign_annotations(1, "d", Mode::SelfHosted)
                .get("rio.build/mode")
                .unwrap(),
            "self-hosted"
        );
    }

    #[test]
    fn choose_digest_unrequested_needs_exactly_one_candidate() {
        let one = vec!["8b919129046e0f60".to_string()];
        assert_eq!(choose_digest(&one, None).unwrap(), "8b919129046e0f60");

        let two = vec![
            "8b919129046e0f60".to_string(),
            "9c02aabbccddeeff".to_string(),
        ];
        let err = choose_digest(&two, None).unwrap_err().to_string();
        assert!(err.contains("--eval-digest"), "{err}");
        assert!(err.contains("8b919129046e0f60"), "{err}");

        assert!(choose_digest(&[], None).is_err());
    }

    #[test]
    fn choose_digest_prefix_full_digest_and_ambiguity() {
        let candidates = vec![
            "8b919129046e0f60".to_string(),
            "8b02aabbccddeeff".to_string(),
        ];
        // Unambiguous short prefix.
        assert_eq!(
            choose_digest(&candidates, Some("8b91")).unwrap(),
            "8b919129046e0f60"
        );
        // Full 64-char key digest matches the candidate that is its
        // 16-char prefix.
        let full = format!("8b919129046e0f60{}", "ab".repeat(24));
        assert_eq!(
            choose_digest(&candidates, Some(&full)).unwrap(),
            "8b919129046e0f60"
        );
        // Ambiguous prefix → error naming every match, never first-match.
        let err = choose_digest(&candidates, Some("8b"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("ambiguous"), "{err}");
        assert!(
            err.contains("8b919129046e0f60") && err.contains("8b02aabbccddeeff"),
            "{err}"
        );
        // No match → error listing what exists.
        let err = choose_digest(&candidates, Some("ffff"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("matches none"), "{err}");
        assert!(err.contains("8b02aabbccddeeff"), "{err}");
    }

    #[test]
    fn guard_refuses_existing_job_or_differing_spec() {
        let id = "parity-leaf-20260601-ab12";
        let spec = r#"{"campaignId":"parity-leaf-20260601-ab12"}"#;

        // Existing Job: refuse regardless of ConfigMap state, with the
        // delete-Job escape hatch spelled out.
        let err = guard_existing_campaign(id, true, None, spec)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "{err}");
        assert!(
            err.contains("delete job parity-leaf-20260601-ab12"),
            "{err}"
        );

        // Leftover ConfigMap with a different spec: refuse and name the
        // ConfigMap to delete.
        let err = guard_existing_campaign(id, false, Some(r#"{"campaignId":"other"}"#), spec)
            .unwrap_err()
            .to_string();
        assert!(err.contains("different campaign spec"), "{err}");
        assert!(
            err.contains("delete configmap parity-leaf-20260601-ab12-spec"),
            "{err}"
        );

        // Identical leftover spec (e.g. a launch that failed after the
        // ConfigMap apply) and the fresh-id case are both fine.
        guard_existing_campaign(id, false, Some(spec), spec).unwrap();
        guard_existing_campaign(id, false, None, spec).unwrap();
    }
}
