//! `cargo xtask replay repro` — engine-native single-unit re-run.
//!
//! This is the command recorded in the `repro` field of every results.jsonl
//! record: it derives a one-unit campaign spec from the original campaign's
//! stored record (same archive pin, cluster endpoints, tenants, and knobs;
//! scope narrowed to exactly the named derivation), then applies it as a
//! fresh ConfigMap+Job through the same helpers `replay launch` uses. It
//! needs no local archive copy and no local Nix store — the engine pod
//! fetches the pinned archive and replays the unit over the same client-ops
//! transport and supply policy the campaign used.
//!
//! Tenant provisioning, SSH keys, the HMAC copy, and the pre-flight are all
//! skipped on purpose: the original campaign's tenants and Secrets already
//! exist in the rio-replay namespace, and the derived spec points at the
//! same mounted paths.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use clap::Args;
use k8s_openapi::api::batch::v1::Job;
use kube::api::Api;
use rio_replay::run::spec::{CampaignRecord, CampaignSpec, FailOn, ReportBlock, ReportPolicy};

use super::jobs::{self, EngineJobCommon};
use super::{NS_REPLAY, launch, s3};
use crate::k8s::client as kclient;
use crate::k8s::eks::{TF_DIR, push};
use crate::{git, tofu, ui};

#[derive(Args)]
pub struct ReproArgs {
    /// Campaign id of the original run (whose results.jsonl recorded this
    /// repro invocation).
    pub campaign: String,
    /// Derivation path to re-run (the results.jsonl record's `drvPath`).
    pub drv: String,
    /// RUST_LOG for the repro pod.
    #[arg(long, default_value = "info,rio_replay=debug")]
    pub log_level: String,
}

/// Derived repro campaign id: `<original>-repro-<8 hex>`. The random
/// suffix keeps repeated repros of the same unit from colliding on the Job
/// name; lowercase hex stays inside the campaign-id charset, so the derived
/// id passes the same RFC-1123 validation as any launch-chosen id.
fn repro_campaign_id(original: &str) -> String {
    format!("{original}-repro-{:08x}", rand::random::<u32>())
}

/// Map the requested drv path to its job name via the campaign's
/// results.jsonl. The job name — not the drv path — is the narrowing key:
/// the spec's `Filters` has no per-drv selector, and an exact job name used
/// as an include glob matches only itself.
fn job_for_drv(results_jsonl: &str, campaign: &str, drv: &str) -> Result<String> {
    for line in results_jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let record: serde_json::Value = serde_json::from_str(line)
            .with_context(|| format!("parse a results.jsonl line of campaign {campaign}"))?;
        if record["drvPath"].as_str() == Some(drv) {
            return record["job"]
                .as_str()
                .map(str::to_string)
                .with_context(|| format!("the results.jsonl record for {drv} has no job name"));
        }
    }
    bail!(
        "campaign {campaign} has no results.jsonl record for {drv} — pass the record's exact \
         `drvPath` (the per-class buckets/*.jsonl artifacts under the campaign's S3 prefix list \
         every record)"
    )
}

/// Derive the single-unit spec for the repro run from the original
/// campaign's record: identical archive pin, cluster endpoints, tenants,
/// and knobs; scope narrowed to exactly `job_name`; parity-only report; no
/// deadline.
fn derive_repro_spec(original: &CampaignRecord, job_name: &str, repro_id: &str) -> CampaignSpec {
    let mut spec = original.spec.clone();
    spec.campaign_id = Some(repro_id.to_owned());
    // Exactly one unit in scope: the job name as an exact-match glob, with
    // the limit/jobs-file narrowing of the original campaign cleared so they
    // cannot filter that one unit back out.
    spec.filters.include_globs = vec![job_name.to_owned()];
    spec.filters.limit = Some(1);
    spec.filters.jobs_file = None;
    // A repro is observational by definition: parity report only, no gate,
    // no deadline.
    spec.report = ReportBlock {
        policies: vec![ReportPolicy::Parity],
        fail_on: FailOn::None,
    };
    spec.deadline = None;
    spec
}

/// Engine argv for the repro Job: the same shape launch uses
/// (`run --spec <mounted spec>`), plus `--allow-unverified-tenants` when
/// the original campaign recorded its tenants as unverified — the derived
/// spec inherits that flag, and the engine refuses unverified-tenant specs
/// without the explicit override.
fn engine_args(spec: &CampaignSpec) -> Vec<String> {
    let mut args = vec!["run".into(), "--spec".into(), jobs::SPEC_MOUNT_PATH.into()];
    if !spec.tenants.upstreams_verified {
        args.push("--allow-unverified-tenants".into());
    }
    args
}

#[allow(clippy::print_stdout)]
pub async fn run(a: ReproArgs) -> Result<()> {
    let store = s3::CampaignStore::discover()?;

    // The original campaign's record and results are S3 artifacts; nothing
    // consults the cluster until the derived spec is ready to apply.
    let campaign_doc = ui::step("fetch campaign.json", || async {
        store
            .fetch_campaign_doc(&a.campaign, "campaign.json")
            .await?
            .with_context(|| {
                format!(
                    "{} not found — is {} a campaign id? (`cargo xtask replay status {}` shows \
                     Job + progress state)",
                    store.uri(&a.campaign, "campaign.json"),
                    a.campaign,
                    a.campaign
                )
            })
    })
    .await?;
    let original: CampaignRecord = serde_json::from_str(&campaign_doc)
        .with_context(|| format!("parse {}", store.uri(&a.campaign, "campaign.json")))?;

    let results_doc = ui::step("fetch results.jsonl", || async {
        store
            .fetch_campaign_doc(&a.campaign, "results.jsonl")
            .await?
            .with_context(|| {
                format!(
                    "{} not found — the campaign has not recorded any results yet",
                    store.uri(&a.campaign, "results.jsonl")
                )
            })
    })
    .await?;
    let job_name = job_for_drv(&results_doc, &a.campaign, &a.drv)?;

    // Derived one-unit spec, validated by the engine's own rules before
    // anything is created.
    let repro_id = repro_campaign_id(&a.campaign);
    launch::validate_campaign_id(&repro_id)?;
    let spec = derive_repro_spec(&original, &job_name, &repro_id);
    spec.validate()
        .context("derived repro spec failed engine validation")?;
    let spec_json = serde_json::to_string_pretty(&spec)?;

    println!("derived single-unit spec for {job_name} ({}):", a.drv);
    println!("{spec_json}");
    println!(
        "follow-up commands:\n  \
         cargo xtask replay status {repro_id} --watch\n  \
         cargo xtask replay report {repro_id}"
    );

    // The repro Job pulls <ecr>/rio-replay:<tag> for the CURRENT tree, same
    // assertion (and same refusal point) as launch.
    let tf = tofu::outputs(TF_DIR)?;
    let region = tf.get("region")?;
    let ecr = tf.get("ecr_registry")?;
    let bucket = tf.get("chunk_bucket_name")?;
    let role_arn = tf.get("replay_iam_role_arn")?;
    let tag = git::image_tag(&git::open()?)?;
    ui::step("rio-replay image in ECR", || {
        push::assert_in_ecr("rio-replay", &tag, &region)
    })
    .await?;

    let client = kclient::client().await?;
    ui::step("rio-replay namespace + ServiceAccount", || {
        jobs::ensure_base(&client, &role_arn)
    })
    .await?;

    // Same collision guard as launch: a repro id already in use must never
    // have its mounted spec swapped underneath it.
    let cm_name = jobs::spec_configmap_name(&repro_id);
    ui::step(
        &format!("campaign id {repro_id} not already in use"),
        || async {
            let jobs_api: Api<Job> = Api::namespaced(client.clone(), NS_REPLAY);
            let job_exists = jobs_api.get_opt(&repro_id).await?.is_some();
            let existing_spec =
                kclient::get_configmap_key(&client, NS_REPLAY, &cm_name, jobs::SPEC_FILENAME)
                    .await?;
            launch::guard_existing_campaign(
                &repro_id,
                job_exists,
                existing_spec.as_deref(),
                &spec_json,
            )
        },
    )
    .await?;

    let spec_data = BTreeMap::from([(jobs::SPEC_FILENAME.to_string(), spec_json.clone())]);
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

    let common = EngineJobCommon {
        image: format!("{ecr}/rio-replay:{tag}"),
        s3_bucket: bucket.clone(),
        region: region.clone(),
        log_level: a.log_level.clone(),
    };
    let job = jobs::campaign_job(&common, &repro_id, &engine_args(&spec))?;
    ui::step(&format!("apply repro Job {repro_id}"), || {
        jobs::create_job(&client, &job)
    })
    .await?;

    tracing::info!(
        "repro launched: {repro_id}\n  \
         unit:      {job_name} ({})\n  \
         original:  {}\n  \
         progress:  cargo xtask replay status {repro_id} --watch\n  \
         report:    cargo xtask replay report {repro_id}\n  \
         logs:      kubectl -n {NS_REPLAY} logs -f job/{repro_id}\n  \
         artifacts: s3://{bucket}/{}",
        a.drv,
        a.campaign,
        s3::campaign_key(&repro_id, "")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use rio_replay::run::spec::{
        ArchivePin, ArchiveRef, ClusterEndpoints, Filters, Mode, S3Target, TenantBlock,
    };

    use super::*;

    /// A stored campaign record shaped like what launch writes: a valid
    /// leaf-mode spec (passes `CampaignSpec::validate`) with non-default
    /// filters/report/deadline so the derivation's narrowing is observable.
    fn record_fixture() -> CampaignRecord {
        let archive_id = "deadbeef".repeat(8);
        let spec = CampaignSpec {
            campaign_id: Some("replay-leaf-20260601-ab12".into()),
            mode: Mode::Leaf,
            archive: ArchiveRef {
                s3_bucket: Some("rio-build-chunks-deadbeef".into()),
                s3_prefix: Some("replay/archives/deadbeefdeadbeef".into()),
                digest: archive_id.clone(),
            },
            s3: S3Target {
                bucket: Some("rio-build-chunks-deadbeef".into()),
                ..S3Target::default()
            },
            cluster: ClusterEndpoints {
                gateway_store_url: "ssh-ng://rio@rio-gateway.rio-system.svc:22?compress=true&\
                                    ssh-key=/etc/rio/replay-ssh/replay-leaf"
                    .into(),
                ssh_key_dir: Some("/etc/rio/replay-ssh".into()),
                scheduler_addr: "rio-scheduler.rio-system.svc:9001".into(),
                store_addr: "rio-store.rio-store.svc:9002".into(),
                service_hmac_key_path: None,
                gateway_host_key: Some(
                    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPlaceholder rio-gateway".into(),
                ),
            },
            tenants: TenantBlock {
                build_tenant: "replay-leaf".into(),
                warm_tenant: "replay-warm".into(),
                upstreams_verified: true,
                ..TenantBlock::default()
            },
            filters: Filters {
                include_globs: vec!["nixpkgs.*".into()],
                limit: Some(50),
                jobs_file: Some("/tmp/jobs.txt".into()),
                ..Filters::default()
            },
            report: ReportBlock {
                policies: vec![ReportPolicy::Parity, ReportPolicy::RegressionGate],
                fail_on: FailOn::Regression,
            },
            deadline: Some("2026-06-08T00:00:00Z".into()),
            ..CampaignSpec::default()
        };
        CampaignRecord::new(
            "replay-leaf-20260601-ab12".into(),
            "2026-06-01T12:00:00Z".into(),
            spec,
            ArchivePin {
                archive_id: archive_id.clone(),
                archive_id_short: archive_id[..16].to_string(),
            },
        )
    }

    #[test]
    fn repro_campaign_ids_are_fresh_and_label_safe() {
        let original = "replay-leaf-20260601-ab12";
        let id1 = repro_campaign_id(original);
        let id2 = repro_campaign_id(original);
        assert!(id1.starts_with("replay-leaf-20260601-ab12-repro-"), "{id1}");
        // 8-hex random suffix inside the campaign-id charset; two
        // derivations never collide on the same Job name.
        let suffix = id1.rsplit('-').next().unwrap();
        assert_eq!(suffix.len(), 8);
        assert!(
            suffix
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "{suffix}"
        );
        assert_ne!(id1, id2);
        // The derived id passes the same validation launch applies to
        // operator-chosen ids (Job-name / label-value / ConfigMap budget).
        launch::validate_campaign_id(&id1).unwrap();
    }

    #[test]
    fn job_for_drv_maps_the_drv_to_its_job_name() {
        let results = concat!(
            r#"{"job":"libfoo.x86_64-linux","drvPath":"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv","verdict":"match-built"}"#,
            "\n",
            r#"{"job":"app.x86_64-linux","drvPath":"/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-app-2.0.drv","verdict":"unexpected-failure"}"#,
            "\n",
        );
        assert_eq!(
            job_for_drv(
                results,
                "c1",
                "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-app-2.0.drv"
            )
            .unwrap(),
            "app.x86_64-linux"
        );
        // Unknown drv: the error names the campaign and the requested drv so
        // a typo is visible.
        let err = job_for_drv(results, "c1", "/nix/store/cccc-missing.drv")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("c1") && err.contains("/nix/store/cccc-missing.drv"),
            "{err}"
        );
    }

    #[test]
    fn derived_spec_narrows_scope_and_keeps_the_archive_pin() {
        let original = record_fixture();
        let repro_id = "replay-leaf-20260601-ab12-repro-0123abcd";
        let spec = derive_repro_spec(&original, "app.x86_64-linux", repro_id);

        // Fresh campaign id, never the original's.
        assert_eq!(spec.campaign_id.as_deref(), Some(repro_id));
        assert_ne!(spec.campaign_id, original.spec.campaign_id);
        // The same archive reference (bucket, prefix, digest) is pinned.
        assert_eq!(spec.archive.s3_bucket, original.spec.archive.s3_bucket);
        assert_eq!(spec.archive.s3_prefix, original.spec.archive.s3_prefix);
        assert_eq!(spec.archive.digest, original.spec.archive.digest);
        // Scope is narrowed to exactly the named unit: exact-name glob,
        // limit 1, the original's jobs-file narrowing cleared.
        assert_eq!(spec.filters.include_globs, vec!["app.x86_64-linux"]);
        assert_eq!(spec.filters.limit, Some(1));
        assert_eq!(spec.filters.jobs_file, None);
        // Cluster endpoints, tenants, and mode are inherited unchanged (the
        // repro reuses the original campaign's Secrets and key mounts).
        assert_eq!(
            spec.cluster.gateway_store_url,
            original.spec.cluster.gateway_store_url
        );
        assert_eq!(
            spec.tenants.build_tenant,
            original.spec.tenants.build_tenant
        );
        assert_eq!(spec.mode, original.spec.mode);
        // Repro runs are observational: parity-only report, no gate, no
        // deadline — even when the original requested a regression gate.
        assert_eq!(spec.report.policies, vec![ReportPolicy::Parity]);
        assert_eq!(spec.report.fail_on, FailOn::None);
        assert_eq!(spec.deadline, None);
        // The derived spec passes the engine's own validation as-is.
        spec.validate().unwrap();

        // Engine argv mirrors launch's shape; the unverified-tenants escape
        // hatch is inherited from the original record, not re-decided here.
        assert_eq!(
            engine_args(&spec),
            vec!["run", "--spec", "/etc/rio/replay/spec.json"]
        );
        let mut unverified = original.clone();
        unverified.spec.tenants.upstreams_verified = false;
        let spec = derive_repro_spec(&unverified, "app.x86_64-linux", repro_id);
        assert_eq!(
            engine_args(&spec),
            vec![
                "run",
                "--spec",
                "/etc/rio/replay/spec.json",
                "--allow-unverified-tenants"
            ]
        );
    }
}
