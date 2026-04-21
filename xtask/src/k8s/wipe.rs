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
            reset_pg_schema(&client).await?;
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
    // Exactly `up --deploy`: ensure_namespaces, secrets, helm install,
    // migration job. NOT `up` (all phases) — push/ami/provision are
    // infra-shape and untouched by wipe.
    ui::step("redeploy (up --deploy)", || {
        let cfg = cfg.clone();
        let opts = UpOpts {
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
/// migration Job (run by `up --deploy`) starts from 001. RDS instance
/// itself is untouched.
async fn reset_pg_schema(client: &kube::Client) -> Result<()> {
    ui::step("reset PG schema", || async {
        let pg = PgHandle::open(client).await?;
        // Single batch; CASCADE handles all dependent objects.
        sqlx::query("DROP SCHEMA public CASCADE")
            .execute(&pg.pool)
            .await
            .context("DROP SCHEMA public CASCADE")?;
        sqlx::query("CREATE SCHEMA public")
            .execute(&pg.pool)
            .await
            .context("CREATE SCHEMA public")?;
        // The connecting role (from rio-postgres URL) created the
        // schema, so it already owns it. Explicit GRANT for the
        // generic `public` role mirrors what `initdb` does.
        if let Err(e) = sqlx::query("GRANT ALL ON SCHEMA public TO public")
            .execute(&pg.pool)
            .await
        {
            warn!("GRANT ALL ON SCHEMA public TO public: {e:#} (continuing — owner already has rights)");
        }
        Ok(())
    })
    .await
}
