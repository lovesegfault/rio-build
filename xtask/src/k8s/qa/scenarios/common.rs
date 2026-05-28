//! Helpers shared across scenarios. Kept here (not on `QaCtx`) so the
//! sibling agent owning `ctx.rs` doesn't conflict.

use std::collections::HashSet;

use anyhow::{Context, Result};

use crate::k8s::qa::QaCtx;
use crate::k8s::status::{STORE_METRICS_PORT, Scrape, scrape_pod};
use crate::k8s::{NS_BUILDERS, NS_STORE};

/// `DebugListExecutors` row, via `rio-cli workers --actor`. The actor's
/// in-memory executor map — same source the dispatcher reads, no
/// gauge-tick lag (`housekeeping.rs` recomputes `workers_active` on a
/// tick, which can lie for one cycle after a leader-kill).
/// `has_stream=false` is an I-048b zombie: a heartbeat-only entry with
/// no `stream_tx` — there's no live stream for a keepalive to time out.
#[derive(serde::Deserialize)]
pub struct DebugExecutor {
    pub executor_id: String,
    pub has_stream: bool,
}

#[derive(serde::Deserialize)]
pub struct DebugList {
    pub executors: Vec<DebugExecutor>,
}

/// Live (`has_stream=true`) executor IDs from the actor's in-memory
/// map. Use as a snapshot-diff precondition: capture before a warmup
/// build, gate on a fresh ID appearing — `workers_active > 0` alone
/// can be satisfied by a leftover that's about to leave (the i048c
/// full-QA precondition race, 2026-05-13: satisfied at T+2.6s by a
/// prior scenario's mid-drain builder, long before the warmup builder
/// could spawn).
pub fn live_executor_ids(cli: &crate::k8s::eks::smoke::CliCtx) -> anyhow::Result<HashSet<String>> {
    let out = cli.run(&["--json", "workers", "--actor"])?;
    let dl: DebugList = serde_json::from_str(&out)
        .map_err(|e| anyhow::anyhow!("workers --actor json: {e}: {out}"))?;
    Ok(dl
        .executors
        .into_iter()
        .filter(|e| e.has_stream)
        .map(|e| e.executor_id)
        .collect())
}

/// Scrape one rio-store replica's `/metrics`. Multi-replica stores
/// don't aggregate — caller picks the pod (or sums across all via
/// [`scrape_all_stores`]).
pub async fn scrape_store(ctx: &QaCtx, pod: &str) -> Result<Scrape> {
    let body = scrape_pod(&ctx.kube, NS_STORE, pod, STORE_METRICS_PORT).await?;
    Ok(Scrape::parse(&body))
}

/// Scrape every store replica and return the per-pod scrapes.
pub async fn scrape_all_stores(ctx: &QaCtx) -> Result<Vec<(String, Scrape)>> {
    let pods = ctx.running_pods(NS_STORE, "app.kubernetes.io/name=rio-store")?;
    let mut out = Vec::with_capacity(pods.len());
    for p in pods {
        let s = scrape_store(ctx, &p).await?;
        out.push((p, s));
    }
    Ok(out)
}

/// First running builder pod's name, or None.
pub fn any_builder(ctx: &QaCtx) -> Result<Option<String>> {
    Ok(ctx
        .running_pods(NS_BUILDERS, QaCtx::BUILDER_LABEL)?
        .into_iter()
        .next())
}

// ─── Exclusive-tier helpers ───────────────────────────────────────────

use std::time::Duration;
use tokio::time::{Instant, sleep};

use crate::k8s::NS;

/// `kubectl delete pod` with `--grace-period=0 --force`. SIGKILL — no
/// SIGTERM, no graceful drain. For scenarios that test ungraceful loss.
pub fn kill_pod(ctx: &QaCtx, ns: &str, name: &str) -> Result<()> {
    ctx.kubectl(&[
        "-n",
        ns,
        "delete",
        "pod",
        name,
        "--grace-period=0",
        "--force",
        "--wait=false",
    ])?;
    Ok(())
}

/// First running pod matching `app.kubernetes.io/name=<label>` in `ns`.
/// Err if none — caller should precondition-Skip if absence is expected.
pub fn first_pod(ctx: &QaCtx, ns: &str, app: &str) -> Result<String> {
    ctx.running_pods(ns, &format!("app.kubernetes.io/name={app}"))?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no running {app} pod in {ns}"))
}

/// Wait until the scheduler-leader pod name differs from `old_leader`
/// (failover after a kill/restart). Returns the new leader name.
///
/// Any [`QaCtx::scheduler_leader`] Err during the transition is
/// transient by definition — no holder yet, holder Terminating, holder
/// pod gone — so all Err arms retry. The only failure mode is the
/// deadline elapsing.
pub async fn wait_new_leader(ctx: &QaCtx, old_leader: &str, deadline: Duration) -> Result<String> {
    poll_until(deadline, Duration::from_secs(2), || async {
        match ctx.scheduler_leader().await {
            Ok(cur) if cur != old_leader && !cur.is_empty() => Ok(Some(cur)),
            Ok(_) => Ok(None),
            Err(e) => {
                tracing::debug!("wait_new_leader: {e:#}");
                Ok(None)
            }
        }
    })
    .await?
    .ok_or_else(|| anyhow::anyhow!("no leader failover within {deadline:?}"))
}

/// Wait until the scheduler-leader's `rio_scheduler_recovery_total{outcome="success"}`
/// has incremented past `before` — i.e., recovery completed at least
/// once. Use after a leader kill to gate metric assertions on the new
/// leader having finished its PG load.
pub async fn wait_recovery_done(ctx: &QaCtx, before: f64, deadline: Duration) -> Result<bool> {
    let r = poll_until(deadline, Duration::from_secs(3), || async {
        // scrape may transiently fail mid-failover; treat as not-yet.
        let now = match ctx.scrape_scheduler().await {
            Ok(s) => s
                .labeled("rio_scheduler_recovery_total", "outcome", "success")
                .unwrap_or(0.0),
            Err(_) => return Ok(None),
        };
        Ok((now > before).then_some(()))
    })
    .await?;
    Ok(r.is_some())
}

/// Settle the cluster after a leader-kill so the *next* phase-2
/// scenario isn't blamed for this one's blast radius. Two slow paths
/// follow a leader-kill (live-measured 2026-05-13):
/// - The gateway's gRPC balancer takes ~3-10s to find the new leader.
///   `ResolveTenant` fails in that window → `SubmitBuild` carries no
///   JWT → `Unauthenticated`.
/// - Every executor stream dies with the old leader — the new leader
///   has 0 registered executors. The next build needs a cold spawn:
///   controller poll (10s) → Job → maybe a NodeClaim → container pull
///   → FUSE mount → connect → register. ~40s warm node, ~70-90s with
///   provisioning.
///
/// Waiting for a *fresh* smoke build to *complete* proves both:
/// gateway → new-leader routing AND cold-builder dispatch. Waiting
/// for `workers_active > 0` or `recovery_total > 0` proves neither
/// (i024's bg builder reconnects via the gateway's `WatchBuild`
/// retry with its pre-existing JWT — a different code path from a
/// fresh `SubmitBuild`; recovery is ~40ms, well before either path
/// is functional).
///
/// Caller's `meta().timeout` must budget ~90s for this — it's the
/// price of not poisoning the next scenario.
pub async fn phase2_settle_after_kill(ctx: &QaCtx) -> Result<()> {
    // Recovery gate: not load-bearing (recovery is ~40ms) but cheap
    // and turns a slow-recovery regression into a clean failure here
    // rather than a confusing one in the next scenario.
    if !wait_recovery_done(ctx, 0.0, Duration::from_secs(60)).await? {
        anyhow::bail!("post-kill settle: new leader never reported recovery success within 60s");
    }
    // Fresh build: proves end-to-end dispatch-readiness. 5s sleep,
    // 1 KiB output — small enough not to dominate the budget, real
    // enough to exercise the full pipeline.
    ctx.nix_build_via_gateway(0, "settle-after-kill", 5, 1)
        .await
        .context(
            "post-kill settle: fresh smoke build failed — \
             gateway → new-leader → builder pipeline not ready",
        )?;
    Ok(())
}

/// Poll `f` every `interval` until it returns `Some` or `deadline`
/// elapses. Returns the `Some` value or `None` on timeout. Errors from
/// `f` propagate immediately.
pub async fn poll_until<T, F, Fut>(
    deadline: Duration,
    interval: Duration,
    mut f: F,
) -> Result<Option<T>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Option<T>>>,
{
    let end = Instant::now() + deadline;
    loop {
        if let Some(v) = f().await? {
            return Ok(Some(v));
        }
        if Instant::now() >= end {
            return Ok(None);
        }
        sleep(interval).await;
    }
}

/// Grep recent (since `secs`s ago) logs of pod `name` for `needle`.
/// Returns matching lines.
pub fn logs_since_contain(
    ctx: &QaCtx,
    ns: &str,
    name: &str,
    secs: u32,
    needle: &str,
) -> Result<Vec<String>> {
    let since = format!("{secs}s");
    let out = ctx
        .kubectl(&["-n", ns, "logs", name, "--since", &since])
        .unwrap_or_default();
    Ok(out
        .lines()
        .filter(|l| l.contains(needle))
        .map(String::from)
        .collect())
}

/// PG-mutation cleanup guard. Runs `cleanup` exactly once when dropped
/// is awaited (no async Drop, so caller must `.cleanup().await`). For
/// scenarios that seed rows and must remove them even on Fail.
pub struct PgCleanup<'a> {
    pg: &'a sqlx::PgPool,
    sql: String,
}

impl<'a> PgCleanup<'a> {
    pub fn new(pg: &'a sqlx::PgPool, sql: impl Into<String>) -> Self {
        Self {
            pg,
            sql: sql.into(),
        }
    }
    pub async fn run(self) -> Result<()> {
        // AssertSqlSafe: cleanup SQL is authored by the scenario itself
        // (operator tooling), never derived from external input.
        sqlx::query(sqlx::AssertSqlSafe(self.sql))
            .execute(self.pg)
            .await?;
        Ok(())
    }
}

pub const NS_SYSTEM: &str = NS;
/// Controller metrics port (gateway=9090, scheduler=9091, store=9092,
/// worker=9093, controller=9094 — see rio-controller/src/main.rs).
pub const CONTROLLER_METRICS_PORT: u16 = 9094;

/// One scrape of the controller pod's `/metrics`.
pub async fn scrape_controller(ctx: &QaCtx) -> Result<Scrape> {
    let pod = first_pod(ctx, NS, "rio-controller")?;
    let body = scrape_pod(&ctx.kube, NS, &pod, CONTROLLER_METRICS_PORT).await?;
    Ok(Scrape::parse(&body))
}
