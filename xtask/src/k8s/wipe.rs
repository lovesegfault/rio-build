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

use anyhow::{Context, Result};
use futures_util::future::try_join_all;
use tracing::{info, warn};

use super::eks::TF_DIR;
use super::eks::destroy::{k, uninstall_chart};
use super::provider::ProviderKind;
use super::qa::ctx::PgHandle;
use super::{NS, NS_BUILDERS, NS_FETCHERS, NS_STORE, client as kube};
use crate::{aws, tofu, ui};

/// Namespaces deleted wholesale. `rio-system` excluded — see module doc.
const WIPE_NAMESPACES: &[&str] = &[NS_STORE, NS_BUILDERS, NS_FETCHERS];

pub(super) async fn run(kind: ProviderKind) -> Result<()> {
    let client = kube::client().await?;

    // ── 0. Capture PG URL BEFORE uninstall (eks) ────────────────────
    // `rio-postgres` is ExternalSecret-managed: helm uninstall removes
    // the CR, the operator GCs the Secret. The schema reset runs last
    // (see its call site for why), so read the URL while it exists.
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
    // rio-lease-created (not chart-owned) — they survive uninstall
    // naming a dead holder, and the deploy preflight's `tunnel_grpc`
    // then burns its full poll budget on "holder not found" before its
    // no-holder fast path can engage.
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
            // Karpenter (tofu-managed) survives wipe and reaps the
            // chart's NodePool-backed claims in the background once
            // their pools are gone — no need to block on the drain.
            // Kick a non-blocking delete on shim-pool claims so any
            // controller-minted straggler that raced uninstall_chart's
            // step-1b sweep is marked for GC before the next deploy.
            ui::step("delete shim-pool NodeClaims (non-blocking)", || {
                k(&[
                    "delete",
                    "nodeclaim",
                    "-l",
                    "karpenter.sh/nodepool=rio-nodeclaim-shim",
                    "--ignore-not-found",
                    "--wait=false",
                ])
            })
            .await?;
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
            // PG (bitnami subchart, deleteClaim PVC) and S3 (rook-ceph)
            // are in-cluster — helm uninstall already cleared both.
            info!("k3s: PG/S3 are in-cluster; helm uninstall already cleared them");
        }
    }

    Ok(())
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

/// `DROP SCHEMA public CASCADE; CREATE SCHEMA public;` so the deploy
/// phase's migration Job starts from 001. RDS is in private subnets
/// and every rio pod is gone by now; [`PgHandle::open_with_url`]
/// spawns the socat relay in `rio-system` (the one namespace wipe
/// preserves) and port-forwards into sqlx, so the URL never lands in
/// a kubelet-logged argv.
async fn reset_pg_schema(url: &str) -> Result<()> {
    ui::step("reset PG schema", || async {
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
