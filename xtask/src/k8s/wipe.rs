//! `xtask k8s up --wipe` — reset the data plane to pristine.
//!
//! Clears S3 chunk buckets (standard + per-AZ Express One Zone hot
//! tier), PG schema, tenants/builds, builder Jobs, gateway
//! authorized_keys. Infra shape — RDS instance, S3 buckets, AMI,
//! Karpenter NodePools, tofu-managed helm releases — is preserved.
//! Target wall-clock: minutes vs `destroy`+`up`'s ~20.
//!
//! `rio-system` is the one namespace NOT deleted, so internal auth
//! Secrets (`rio-jwt-signing`, `rio-service-hmac`, `rio-postgres*`)
//! survive; `rio-gateway-ssh` (tenant keys) is wiped explicitly.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::future::try_join_all;
use tracing::{info, warn};

use super::eks::TF_DIR;
use super::eks::destroy::{k, uninstall_chart};
use super::provider::ProviderKind;
use super::qa::ctx::PgHandle;
use super::{NS, NS_BUILDERS, NS_FETCHERS, NS_STORE, client as kube};
use crate::sh::{self, cmd, shell};
use crate::{aws, tofu, ui};

/// Namespaces wiped wholesale. `rio-system` is excluded so internal
/// auth secrets survive.
const WIPE_NAMESPACES: &[&str] = &[NS_STORE, NS_BUILDERS, NS_FETCHERS];

pub(super) async fn run(kind: ProviderKind) -> Result<()> {
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

    // ── 3b. Delete leader-election Leases ───────────────────────────
    // helm uninstall removed the pods but Leases (created at runtime by
    // rio-lease, not chart-owned) survive in rio-system and keep naming
    // the now-deleted holder for `leaseDurationSeconds`. The deploy
    // phase's preflight (`status::gather` → `tunnel_grpc`) then burns
    // its full 30×2s poll budget on "lease holder X not found" before
    // giving up. Deleting the Lease lets `scheduler_leader`'s "lease
    // has no holder" path engage immediately. NotFound is benign (`k`).
    ui::step("delete stale leader Leases", || async {
        for lease in ["rio-scheduler-leader", "rio-controller-nodeclaim-pool"] {
            k(&["-n", NS, "delete", "lease", lease, "--ignore-not-found"]).await?;
        }
        Ok(())
    })
    .await?;

    // ── 4. Wipe tenant keys ─────────────────────────────────────────
    // The only `rio-system` Secret we touch. The deploy phase recreates
    // it with just the operator's RIO_SSH_PUBKEY.
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
            empty_chunk_buckets().await?;
            // PG-schema reset MUST come after the namespace deletes:
            // store/scheduler pods hold connections that block DROP
            // SCHEMA on RDS until they're gone.
            match pg_url {
                Some(url) => reset_pg_schema(&url).await?,
                None => warn!(
                    "rio-postgres Secret was already gone before wipe started \
                     (prior partial wipe?) — skipping schema reset; \
                     the deploy phase's migration will fail if old tables remain"
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

    Ok(())
}

/// Karpenter reconciles deleted NodePools by terminating their
/// NodeClaims; we just wait. Unlike `destroy` (which `kubectl delete
/// nodeclaim --all` because Karpenter itself is about to be torn down),
/// `up --wipe` keeps Karpenter alive so it does the work.
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

/// Empty the standard chunk bucket plus every per-AZ S3 Express One
/// Zone hot-tier bucket, concurrently — the standard bucket is the
/// long pole and [`aws::empty_bucket`] self-throttles via adaptive
/// retry. `express_buckets_json` is `get_opt` so wipe still works on
/// state that predates (or has dropped) the hot tier; directory
/// buckets speak the regular `ListObjectsV2`/`DeleteObjects` data
/// plane (SDK routes by the `--azid--x-s3` suffix), so `empty_bucket`
/// applies unchanged.
async fn empty_chunk_buckets() -> Result<()> {
    let tf = tofu::outputs(TF_DIR)?;
    let region = tf.get("region")?;
    let mut buckets = vec![tf.get("chunk_bucket_name")?];
    if let Some(json) = tf.get_opt("express_buckets_json") {
        let by_az: BTreeMap<String, String> =
            serde_json::from_str(&json).context("parse express_buckets_json")?;
        buckets.extend(by_az.into_values());
    }
    buckets.sort_unstable();
    buckets.dedup();
    ui::step("empty chunk buckets", || async {
        // ui::step prints only on completion (minutes at ~8 K obj/s on
        // a multi-million-object standard bucket) — log the work list now.
        info!(
            "emptying {} bucket(s): {}",
            buckets.len(),
            buckets.join(", ")
        );
        try_join_all(buckets.iter().map(|b| aws::empty_bucket(&region, b))).await?;
        Ok(())
    })
    .await
}

/// `DROP SCHEMA public CASCADE; CREATE SCHEMA public;` so the
/// migration Job (run by the deploy phase) starts from 001.
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
