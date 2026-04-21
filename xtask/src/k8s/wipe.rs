//! `xtask k8s wipe` — reset a cluster to pristine state without
//! `destroy`+`up`.
//!
//! Clears the data plane (S3 chunks, PG schema, tenants/builds, builder
//! Jobs, gateway authorized_keys) and re-runs `up --deploy`. Infra
//! shape — RDS instance, S3 bucket, AMI, Karpenter NodePool
//! definitions, tofu-managed helm releases (cilium, karpenter, aws-lbc)
//! — is preserved. Target wall-clock: ~2min vs `destroy`+`up`'s ~20min.
//!
//! Secrets policy: `rio-gateway-ssh` (tenant keys) is wiped; internal
//! auth (`rio-jwt-signing`, `rio-service-hmac`, `rio-postgres*`) stays.
//! Those live in `rio-system`, which is the one namespace this command
//! does NOT delete.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{info, warn};

use super::eks::TF_DIR;
use super::eks::destroy::{k, uninstall_chart};
use super::provider::{Provider, ProviderKind};
use super::qa::ctx::PgHandle;
use super::{NS, NS_BUILDERS, NS_FETCHERS, NS_STORE, UpOpts, client as kube, run_up};
use crate::config::XtaskConfig;
use crate::sh::{self, cmd, shell};
use crate::{aws, tofu, ui};

/// Namespaces wiped wholesale. `rio-system` is excluded so internal
/// auth secrets survive.
const WIPE_NAMESPACES: &[&str] = &[NS_STORE, NS_BUILDERS, NS_FETCHERS];

pub async fn run(p: Arc<dyn Provider>, kind: ProviderKind, cfg: &XtaskConfig) -> Result<()> {
    let client = kube::client().await?;

    // ── 0. Capture PG URL BEFORE uninstall (eks) ────────────────────
    // On EKS, `rio-postgres` is an ExternalSecret-managed Secret —
    // `helm uninstall` removes the ExternalSecret CR and the operator
    // GCs the synced Secret. The schema-reset step (step 8) runs AFTER
    // namespace deletes (open conns block DROP CASCADE), so by then
    // the Secret is gone. Read it now; pass the URL forward.
    let pg_url = if matches!(kind, ProviderKind::Eks) {
        kube::get_secret_key(&client, NS, "rio-postgres", "url").await?
    } else {
        None
    };

    // ── 1–3. uninstall chart + strip CR finalizers ──────────────────
    // Shared with `destroy` — same ordering constraints (Pool delete
    // first so the controller starts draining; finalizer-strip after
    // helm uninstall so they're definitively orphaned).
    uninstall_chart().await?;

    // ── 4. Wipe tenant keys ─────────────────────────────────────────
    // The only `rio-system` Secret we touch. `up --deploy` recreates it
    // with just the operator's RIO_SSH_PUBKEY.
    ui::step("delete rio-gateway-ssh Secret", || async {
        kube::delete_secret(&client, NS, "rio-gateway-ssh").await
    })
    .await?;

    // ── 5. Delete data-plane namespaces ─────────────────────────────
    // Jobs (controller-created, not helm-owned), leftover pods,
    // store-side rio-postgres copy, PVCs — all go with the namespace.
    ui::step("delete rio data-plane namespaces", || async {
        for &ns in WIPE_NAMESPACES {
            k(&[
                "delete",
                "ns",
                ns,
                "--ignore-not-found",
                "--wait=true",
                "--timeout=300s",
            ])
            .await
            .with_context(|| format!("namespace {ns} stuck Terminating"))?;
        }
        Ok(())
    })
    .await?;

    // ── 6–8. Provider-specific data resets ──────────────────────────
    match kind {
        ProviderKind::Eks => {
            wait_rio_nodeclaims_gone().await?;
            empty_chunk_bucket().await?;
            // PG-schema reset MUST come after the namespace deletes:
            // store/scheduler pods hold connections that block DROP
            // SCHEMA on RDS until they're gone.
            match pg_url {
                Some(url) => reset_pg_schema(&url).await?,
                None => warn!(
                    "rio-postgres Secret was already gone before wipe started \
                     (prior partial wipe?) — skipping schema reset; \
                     `up --deploy` migration will fail if old tables remain"
                ),
            }
        }
        ProviderKind::K3s => {
            // bitnami PG is a subchart of `rio` in `rio-system`; helm
            // uninstall already removed it (and its PVC via the
            // chart's deleteClaim). S3 is rook-ceph — handled by the
            // store's own lifecycle on a fresh deploy.
            info!("k3s: PG/S3 are in-cluster; helm uninstall already cleared them");
        }
    }

    // ── 9. Redeploy ─────────────────────────────────────────────────
    // `up --push --deploy`: deploy's preflight rejects tags not in ECR,
    // and HEAD has typically moved since the last push (this is a dev
    // iteration tool). Push is idempotent — `nix build .#images` is
    // cached and the ECR tag-exists check skips re-upload. NOT full
    // `up` — provision/ami are infra-shape and untouched by wipe.
    ui::step("redeploy (up --push --deploy)", || {
        let cfg = cfg.clone();
        let opts = UpOpts {
            push: true,
            deploy: true,
            ..Default::default()
        };
        async move { run_up(p, kind, &cfg, opts).await }
    })
    .await
}

/// Karpenter reconciles deleted NodePools by terminating their
/// NodeClaims; we just wait. Unlike `destroy` (which `kubectl delete
/// nodeclaim --all` because Karpenter itself is about to be torn down),
/// `wipe` keeps Karpenter alive so it does the work.
///
/// Filters on `rio-` nodepool prefix in case the cluster ever carries
/// non-rio NodePools.
async fn wait_rio_nodeclaims_gone() -> Result<()> {
    ui::step("wait for rio-* NodeClaims to drain", || async {
        ui::poll(
            "rio-* NodeClaims gone",
            Duration::from_secs(10),
            60, // 10 min — builder nodes can take a while under load
            || async {
                // jsonpath braces collide with cmd!'s {} interpolation;
                // build the path as a separate var.
                let jp = r#"jsonpath={range .items[*]}{.metadata.labels.karpenter\.sh/nodepool}{"\n"}{end}"#;
                let sh = shell()?;
                let out = sh::try_read(cmd!(sh, "kubectl get nodeclaims -o {jp}"))
                    .unwrap_or_default();
                let n_rio = out.lines().filter(|l| l.starts_with("rio-")).count();
                if n_rio > 0 {
                    info!("{n_rio} rio-* NodeClaims still draining");
                }
                Ok((n_rio == 0).then_some(()))
            },
        )
        .await
    })
    .await
}

async fn empty_chunk_bucket() -> Result<()> {
    let region = tofu::output(TF_DIR, "region")?;
    let bucket = tofu::output(TF_DIR, "chunk_bucket_name")?;
    ui::step(&format!("empty s3://{bucket}"), || async {
        aws::empty_bucket(&region, &bucket).await.map(|_| ())
    })
    .await
}

/// `DROP SCHEMA public CASCADE; CREATE SCHEMA public;` so the
/// migration Job (run by `up --deploy`) starts from 001.
///
/// RDS lives in private VPC subnets — the operator's machine can't
/// reach it directly, and by this step we've deleted every rio pod
/// that could be port-forwarded through. So: spawn a one-shot psql
/// pod in `rio-system` (which still exists), pass the URL via env
/// (not argv — pod spec argv is logged by kubelet), wait for it to
/// exit 0. The bitnami postgresql image is already on the AMI
/// (prebaked via `nix/docker-pulled.nix` for the subchart).
async fn reset_pg_schema(url: &str) -> Result<()> {
    ui::step("reset PG schema", || async {
        // PgHandle::open_with_url spawns the socat relay → port-forward
        // → sqlx; rio-system (where the relay pod lands) is the one
        // namespace wipe preserves.
        let pg = PgHandle::open_with_url(url).await?;
        sqlx::query("DROP SCHEMA public CASCADE")
            .execute(&pg.pool)
            .await
            .context("DROP SCHEMA public CASCADE")?;
        sqlx::query("CREATE SCHEMA public")
            .execute(&pg.pool)
            .await
            .context("CREATE SCHEMA public")?;
        if let Err(e) = sqlx::query("GRANT ALL ON SCHEMA public TO public")
            .execute(&pg.pool)
            .await
        {
            warn!("GRANT ALL ON SCHEMA public TO public: {e:#} (continuing — owner has rights)");
        }
        Ok(())
    })
    .await
}
