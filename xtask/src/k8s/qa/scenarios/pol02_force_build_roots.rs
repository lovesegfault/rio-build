//! pol02: per-tenant force_build_roots over ssh-ng
//! (`r[sched.merge.force-build-roots]`).
//!
//! Live-cluster PLUMBING check, not a substitution-behavior check — the
//! `force-build-roots-not-substituted` VM subtest
//! (`nix/tests/scenarios/substitute.nix`) owns proving the scheduler
//! refuses to substitute force-build roots. The deployed
//! `gateway.buildPolicy` (values.yaml) ships an entry for the fixed
//! tenant `qa-force-build`, so the gateway must stamp
//! `SubmitBuildRequest.force_build_roots = true` onto builds submitted
//! with that tenant's key and the scheduler must persist it to
//! `builds.force_build_roots` (migration 062). One standard busybox
//! smoke build is submitted; the verdict is decided by that PG column
//! on the latest `qa-force-build` build row.
//!
//! Like pol01, this scenario does NOT use its pool slot — the deployed
//! policy is keyed on the fixed tenant name, so the scenario provisions
//! `qa-force-build` plus its own SSH key and removes both before
//! returning.

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use sqlx::Row;

use crate::k8s::NS;
use crate::k8s::eks::smoke;
use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};
use crate::k8s::shared;

/// Fixed tenant name the deployed `gateway.buildPolicy` keys on
/// (`infra/helm/rio-build/values.yaml` → `gateway.buildPolicy.qa-force-build`).
const TENANT: &str = "qa-force-build";

pub struct ForceBuildRootsPolicy;

#[async_trait]
impl Scenario for ForceBuildRootsPolicy {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "pol02-force-build-roots",
            i_ref: None,
            // Same shape as pol01: submits one build via the gateway,
            // mutates nothing cluster-wide. The pool slot is unused —
            // the deployed gateway.buildPolicy keys on this scenario's
            // own fixed-name tenant, not an ephemeral pool tenant.
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(420),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // 1. Fixed-name tenant + key, exactly like pol01: the gateway
        //    resolves the tenant from the key's authorized_keys comment
        //    and looks the build policy up by that name. Pre-clean any
        //    key a crashed prior run left behind so the Secret doesn't
        //    accumulate lines — the pool's stale-key sweep only matches
        //    `qa-{ts}-` prefixes, never this fixed name.
        smoke::step_tenant(&ctx.cli, TENANT).await?;
        smoke::step_upstream(&ctx.cli, TENANT).await?;
        shared::remove_authorized_keys_by_comment_prefix(&ctx.kube, TENANT).await?;
        let (priv_pem, pub_line) = crate::ssh::generate(TENANT)?;
        let key_dir = tempfile::Builder::new().prefix("rio-qa-pol02-").tempdir()?;
        let key = key_dir.path().join(TENANT);
        std::fs::write(&key, priv_pem)?;
        // ssh refuses keys with group/other-readable perms.
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))?;
        shared::merge_authorized_keys_batch(&ctx.kube, &[pub_line.as_str()]).await?;

        // 2. Wait for the gateway to accept the new key. Same probe and
        //    budget as pol01: kubelet Secret projection (≤60s) + gateway
        //    10s poll ≈ 70s ceiling, 120s budget.
        let (port, _gw_guard) = shared::port_forward(NS, "svc/rio-gateway", 0, 22).await?;
        let store = format!(
            "ssh-ng://rio@localhost:{port}?compress=true&ssh-key={}",
            key.display()
        );
        let sshopts = format!("{} -o ConnectTimeout=5", shared::NIX_SSHOPTS_BASE);
        crate::ui::poll(
            "pol02 qa-force-build key hot-reload",
            Duration::from_secs(5),
            24,
            || {
                let store = store.clone();
                let sshopts = sshopts.clone();
                async move {
                    let s = crate::sh::shell()?;
                    let ok = crate::sh::try_read(
                        crate::sh::cmd!(s, "timeout 10 nix store ping --store {store}")
                            .env("NIX_SSHOPTS", &sshopts),
                    )
                    .is_ok();
                    Ok(ok.then_some(()))
                }
            },
        )
        .await
        .context("gateway never accepted the qa-force-build key (authorized_keys hot-reload)")?;

        // 3. Standard busybox smoke expr (`builtins.currentTime` in the
        //    name keeps re-runs from deduping onto a prior run's drv),
        //    submitted over the same tunnel. The build itself should
        //    succeed — the smoke output is unique so force_build_roots
        //    changes nothing about HOW it completes; what this scenario
        //    proves is the stamp.
        let build = smoke::build_expr(&smoke::smoke_expr("pol02", 5, 1), &store).await;
        if let Err(e) = &build {
            tracing::warn!("pol02: nix build failed (verdict still decided by PG): {e:#}");
        }

        // 4. PG: the latest build row attributed to qa-force-build must
        //    carry force_build_roots = TRUE (gateway policy → proto →
        //    scheduler insert_build, migration 062). Stale rows from
        //    prior runs don't match: cleanup deletes the tenant, which
        //    SET NULLs their tenant_id, and a re-created tenant gets a
        //    fresh UUID anyway.
        let row = sqlx::query(
            "SELECT b.force_build_roots FROM builds b \
             JOIN tenants t ON t.tenant_id = b.tenant_id \
             WHERE t.tenant_name = $1 \
             ORDER BY b.submitted_at DESC LIMIT 1",
        )
        .bind(TENANT)
        .fetch_optional(ctx.pg())
        .await?;
        let stamped = row
            .map(|r| r.try_get::<bool, _>("force_build_roots"))
            .transpose()?;

        // 5. Cleanup before the verdict (best-effort): drop the fixed
        //    tenant and strip its key so re-runs start clean.
        if let Err(e) = ctx.cli.run(&["delete-tenant", TENANT]) {
            tracing::warn!("pol02 cleanup: delete-tenant {TENANT}: {e:#}");
        }
        if let Err(e) = shared::remove_authorized_keys_by_comment_prefix(&ctx.kube, TENANT).await {
            tracing::warn!("pol02 cleanup: removing {TENANT} authorized_keys line: {e:#}");
        }

        let build_err = build.err();
        let verdict = match stamped {
            None => {
                let extra = build_err
                    .as_ref()
                    .map(|e| format!(" (nix build also failed: {e:#})"))
                    .unwrap_or_default();
                Verdict::Fail(format!(
                    "no builds row attributed to tenant {TENANT} after submitting one{extra} — \
                     was the submission rejected before insert_build?"
                ))
            }
            Some(false) => Verdict::Fail(
                "builds.force_build_roots not stamped for the qa-force-build submission — \
                 gateway build-policy → SubmitBuildRequest → scheduler insert_build plumbing \
                 broken"
                    .into(),
            ),
            Some(true) => match build_err {
                None => Verdict::Pass,
                Some(e) => Verdict::Fail(format!(
                    "force_build_roots was stamped, but the qa-force-build smoke build itself \
                     failed: {e:#}"
                )),
            },
        };
        Ok(verdict)
    }
}
