//! pol01: per-tenant keep_going over ssh-ng (`r[gw.build.per-tenant-policy]`).
//!
//! First scenario to exercise keep_going=true end-to-end. The deployed
//! `gateway.buildPolicy` (values.yaml) ships an entry for the fixed
//! tenant `qa-keep-going`, so the gateway stamps
//! `SubmitBuildRequest.keep_going = true` onto builds submitted with
//! that tenant's key. One merged DAG with two INDEPENDENT leaves under
//! one top — `fail` exits 1 instantly, `slow` takes ~30s — must let
//! `slow` finish after `fail` fails. Without the policy the scheduler
//! fail-fast cancels the in-flight sibling (it ends 'cancelled'); with
//! it, `slow` ends 'completed'. The failing leaf itself ends 'poisoned'
//! and the top 'dependency_failed' — neither is asserted.
//!
//! The overall `nix build` exit is EXPECTED to be non-zero (the fail
//! leaf poisons the top): scheduler PG decides the verdict, not the
//! client exit code.
//!
//! Unlike other Tenant scenarios this one does NOT use its pool slot —
//! the deployed policy is keyed on the fixed tenant name, so the
//! scenario provisions `qa-keep-going` plus its own SSH key and removes
//! both before returning.

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use sqlx::Row;
use tokio::time::{Instant, sleep};

use crate::k8s::NS;
use crate::k8s::eks::smoke::{self, BUSYBOX_LET};
use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};
use crate::k8s::shared;

/// Fixed tenant name the deployed `gateway.buildPolicy` keys on
/// (`infra/helm/rio-build/values.yaml` → `gateway.buildPolicy.qa-keep-going`).
const TENANT: &str = "qa-keep-going";

/// Derivation statuses that end the PG poll. Mirrors the scheduler's
/// terminal set; 'cancelled' is what the fail-fast sweep leaves behind
/// when keep_going is NOT applied.
const TERMINAL: &[&str] = &[
    "completed",
    "failed",
    "poisoned",
    "dependency_failed",
    "cancelled",
    "skipped",
];

pub struct KeepGoingPolicy;

#[async_trait]
impl Scenario for KeepGoingPolicy {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "pol01-keep-going",
            i_ref: None,
            // Same shape as i013: submits builds via the gateway,
            // mutates nothing cluster-wide. The pool slot is unused —
            // the deployed gateway.buildPolicy keys on this scenario's
            // own fixed-name tenant, not an ephemeral pool tenant.
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(420),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        // 1. Fixed-name tenant + key. The gateway resolves the tenant
        //    from the key's authorized_keys comment, and the build
        //    policy is keyed on that tenant name. Pre-clean any key a
        //    crashed prior run left behind so the Secret doesn't
        //    accumulate lines — the pool's stale-key sweep only matches
        //    `qa-{ts}-` prefixes, never this fixed name.
        smoke::step_tenant(&ctx.cli, TENANT).await?;
        smoke::step_upstream(&ctx.cli, TENANT).await?;
        shared::remove_authorized_keys_by_comment_prefix(&ctx.kube, TENANT).await?;
        let (priv_pem, pub_line) = crate::ssh::generate(TENANT)?;
        let key_dir = tempfile::Builder::new().prefix("rio-qa-pol01-").tempdir()?;
        let key = key_dir.path().join(TENANT);
        std::fs::write(&key, priv_pem)?;
        // ssh refuses keys with group/other-readable perms.
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))?;
        shared::merge_authorized_keys_batch(&ctx.kube, &[pub_line.as_str()]).await?;

        // 2. Wait for the gateway to accept the new key. The pool's
        //    keys were hot-reloaded before phase 1 started, but THIS
        //    key was written just now: kubelet Secret projection (≤60s)
        //    + gateway 10s poll ≈ 70s ceiling, 120s budget. Same
        //    `nix store ping` probe as the tenant pool / i109.
        let (port, _gw_guard) = shared::port_forward(NS, "svc/rio-gateway", 0, 22).await?;
        let store = format!(
            "ssh-ng://rio@localhost:{port}?compress=true&ssh-key={}",
            key.display()
        );
        let sshopts = format!("{} -o ConnectTimeout=5", shared::NIX_SSHOPTS_BASE);
        crate::ui::poll(
            "pol01 qa-keep-going key hot-reload",
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
        .context("gateway never accepted the qa-keep-going key (authorized_keys hot-reload)")?;

        // 3. The DAG: busybox FOD + two INDEPENDENT leaves + one top
        //    referencing both. Same busybox-builder shape as SMOKE_EXPR
        //    / i047 (`/bin/sh` does not exist in the build sandbox; the
        //    `read -t N < /dev/zero` idiom is the sandbox-safe sleep).
        //    The nonce keeps re-runs from deduping onto already-terminal
        //    drvs from a previous run.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let expr = format!(
            r#"{BUSYBOX_LET}
            let
              mk = name: script: builtins.derivation {{
                inherit name;
                system = "x86_64-linux";
                builder = "${{busybox}}";
                args = ["sh" "-c" script];
              }};
              fail = mk "rio-qa-pol01-fail-{nonce}" "exit 1";
              slow = mk "rio-qa-pol01-slow-{nonce}"
                "read -t 30 x < /dev/zero || true; echo ok > $out";
            in mk "rio-qa-pol01-top-{nonce}" "echo ${{slow}} ${{fail}} > $out""#
        );

        // 4. Submit over the same tunnel (single ssh-ng session, one
        //    merged 4-node DAG). The overall nix build failing is
        //    EXPECTED — don't `?` it; PG decides the verdict.
        let build = smoke::build_expr(&expr, &store).await;
        match &build {
            Ok(()) => tracing::warn!("pol01: nix build succeeded — fail leaf did not fail?"),
            Err(e) => tracing::info!("pol01: nix build failed as expected: {e:#}"),
        }

        // 5. PG: the slow leaf must have been allowed to finish. Poll
        //    briefly — with keep_going applied the build only returns
        //    once every drv is terminal, but a fail-fast return (or a
        //    status-persist lag) can hand control back while the row is
        //    still moving.
        let slow_pat = format!("%rio-qa-pol01-slow-{nonce}%");
        let deadline = Instant::now() + Duration::from_secs(90);
        let slow_status: Option<String> = loop {
            let row = sqlx::query(
                "SELECT status FROM derivations WHERE drv_path LIKE $1 \
                 ORDER BY updated_at DESC LIMIT 1",
            )
            .bind(&slow_pat)
            .fetch_optional(ctx.pg())
            .await?;
            let status = row.map(|r| r.try_get::<String, _>("status")).transpose()?;
            let terminal = status.as_deref().is_some_and(|s| TERMINAL.contains(&s));
            if terminal || Instant::now() >= deadline {
                break status;
            }
            sleep(Duration::from_secs(5)).await;
        };

        // Sibling statuses aren't asserted, but log them — a FAIL is
        // much easier to triage knowing what the fail/top rows ended as
        // (expected: fail → poisoned, top → dependency_failed).
        if let Ok(rows) =
            sqlx::query("SELECT drv_path, status FROM derivations WHERE drv_path LIKE $1")
                .bind(format!("%rio-qa-pol01-%{nonce}%"))
                .fetch_all(ctx.pg())
                .await
        {
            for r in &rows {
                let p: String = r.try_get("drv_path").unwrap_or_default();
                let s: String = r.try_get("status").unwrap_or_default();
                tracing::info!("pol01 {} → {s}", p.rsplit('/').next().unwrap_or(&p));
            }
        }

        // 6. Cleanup before the verdict (best-effort): drop the fixed
        //    tenant and strip its key so re-runs start clean.
        if let Err(e) = ctx.cli.run(&["delete-tenant", TENANT]) {
            tracing::warn!("pol01 cleanup: delete-tenant {TENANT}: {e:#}");
        }
        if let Err(e) = shared::remove_authorized_keys_by_comment_prefix(&ctx.kube, TENANT).await {
            tracing::warn!("pol01 cleanup: removing {TENANT} authorized_keys line: {e:#}");
        }

        let verdict = if build.is_ok() {
            Verdict::Fail(
                "nix build succeeded — the fail leaf never failed, so keep_going was not \
                 exercised (busybox `exit 1` should always fail)"
                    .into(),
            )
        } else {
            match slow_status {
                Some(s) if s == "completed" => Verdict::Pass,
                Some(s) => Verdict::Fail(format!(
                    "rio-qa-pol01-slow-{nonce} ended '{s}' instead of 'completed' — \
                     keep_going not applied (fail-fast cancelled the independent sibling?)"
                )),
                None => Verdict::Fail(format!(
                    "rio-qa-pol01-slow-{nonce} not found in scheduler PG derivations — \
                     was the DAG submitted at all?"
                )),
            }
        };
        Ok(verdict)
    }
}
