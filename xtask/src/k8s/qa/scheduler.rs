//! Two-phase scenario scheduler.
//!
//! Phase 1: `Shared` + `Tenant` concurrently. Shared is unbounded;
//! Tenant is bounded by the tenant-pool semaphore (`acquire(count)`).
//!
//! Phase 2: `Exclusive`, greedy-scheduled by disjoint `mutates`. A
//! scenario is runnable when none of its components are held by an
//! in-flight Exclusive. Re-scan on every completion.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::task::JoinSet;
use tracing::{info, warn};

use super::ctx::{PgHandle, QaCtx, TenantPool};
use super::{Component, Isolation, Scenario, ScenarioMeta, Verdict};
use crate::config::XtaskConfig;
use crate::k8s::client as kube;
use crate::k8s::eks::smoke::CliCtx;
use crate::k8s::provider::ProviderKind;
use crate::ui;

#[derive(Debug)]
pub struct Outcome {
    pub id: &'static str,
    pub verdict: Verdict,
    pub elapsed: Duration,
}

pub async fn run(
    registry: &'static [&'static dyn Scenario],
    only: &[String],
    tenant_pool_size: usize,
    _kind: ProviderKind,
    cfg: &XtaskConfig,
) -> Result<()> {
    let scenarios: Vec<_> = registry
        .iter()
        .copied()
        .filter(|s| only.is_empty() || only.iter().any(|f| s.meta().id.contains(f)))
        .collect();
    if scenarios.is_empty() {
        anyhow::bail!("no scenarios match filter {only:?}");
    }

    let _ = cfg; // reserved for future per-scenario config
    let kube = kube::Client::try_default().await?;
    let cli = Arc::new(CliCtx::open(&kube, 0, 0).await?);
    // PG handle held here (not in QaCtx) so the port-forward guard
    // outlives every scenario.
    let pg = PgHandle::open(&kube).await?;
    let pg_pool = Arc::new(pg.pool.clone());
    let pool = Arc::new(TenantPool::new(&kube, &cli, tenant_pool_size).await?);

    let (p1, p2): (Vec<_>, Vec<_>) = scenarios
        .into_iter()
        .partition(|s| !matches!(s.meta().isolation, Isolation::Exclusive { .. }));

    let mut outcomes = Vec::new();

    ui::step("qa scenarios — phase 1 (shared + tenant)", || async {
        outcomes.extend(run_phase1(p1, &kube, &cli, &pg_pool, &pool).await);
        Ok::<_, anyhow::Error>(())
    })
    .await?;

    ui::step("qa scenarios — phase 2 (exclusive)", || async {
        outcomes.extend(run_phase2(p2, &kube, &cli, &pg_pool, &pool).await);
        Ok::<_, anyhow::Error>(())
    })
    .await?;

    Arc::into_inner(pool)
        .expect("all leases released")
        .cleanup(&kube, &cli)
        .await?;
    drop(pg);

    report(&outcomes);
    let fails = outcomes
        .iter()
        .filter(|o| matches!(o.verdict, Verdict::Fail(_)))
        .count();
    if fails > 0 {
        anyhow::bail!("{fails} scenario(s) failed");
    }
    Ok(())
}

async fn run_phase1(
    scenarios: Vec<&'static dyn Scenario>,
    kube: &kube::Client,
    cli: &Arc<CliCtx>,
    pg: &Arc<sqlx::PgPool>,
    pool: &Arc<TenantPool>,
) -> Vec<Outcome> {
    let mut set = JoinSet::new();
    for s in scenarios {
        let kube = kube.clone();
        let cli = cli.clone();
        let pg = pg.clone();
        let pool = pool.clone();
        set.spawn(async move {
            let meta = s.meta();
            let lease = match meta.isolation {
                Isolation::Tenant { count } => Some(pool.acquire(count).await),
                _ => None,
            };
            let tenants = lease
                .as_ref()
                .map(|l| l.tenants().to_vec())
                .unwrap_or_default();
            let out = exec(s, &meta, kube, cli, pg, tenants).await;
            if let Some(l) = lease {
                l.release().await;
            }
            out
        });
    }
    collect(set).await
}

/// Greedy disjoint-mutates scheduler. Each Exclusive gets ONE tenant
/// from the pool — phase 1 has drained so the pool is full; pool size
/// (default 8) ≥ max concurrent Exclusives (~3-4 by component-disjoint
/// distribution), so `acquire(1)` never blocks in practice. Scenarios
/// that don't need the tenant just ignore `ctx.tenants[0]`.
async fn run_phase2(
    scenarios: Vec<&'static dyn Scenario>,
    kube: &kube::Client,
    cli: &Arc<CliCtx>,
    pg: &Arc<sqlx::PgPool>,
    pool: &Arc<TenantPool>,
) -> Vec<Outcome> {
    let mut pending: VecDeque<_> = scenarios.into_iter().collect();
    let mut held: HashSet<Component> = HashSet::new();
    let mut set: JoinSet<(Outcome, &'static [Component])> = JoinSet::new();
    let mut out = Vec::new();

    loop {
        // Launch everything currently runnable.
        let mut i = 0;
        while i < pending.len() {
            let meta = pending[i].meta();
            let Isolation::Exclusive { mutates } = meta.isolation else {
                unreachable!("phase 2 is exclusive-only")
            };
            if mutates.iter().all(|c| !held.contains(c)) {
                held.extend(mutates.iter().copied());
                let s = pending.remove(i).expect("i < len");
                let kube = kube.clone();
                let cli = cli.clone();
                let pg = pg.clone();
                let pool = pool.clone();
                set.spawn(async move {
                    let lease = pool.acquire(1).await;
                    let tenants = lease.tenants().to_vec();
                    let o = exec(s, &meta, kube, cli, pg, tenants).await;
                    lease.release().await;
                    (o, mutates)
                });
            } else {
                i += 1;
            }
        }
        // Drain one completion, release its components, re-scan.
        match set.join_next().await {
            Some(res) => {
                let (o, mutates) = res.expect("scenario task panicked");
                for c in mutates {
                    held.remove(c);
                }
                out.push(o);
            }
            None => break,
        }
    }
    out
}

async fn exec(
    s: &'static dyn Scenario,
    meta: &ScenarioMeta,
    kube: kube::Client,
    cli: Arc<CliCtx>,
    pg: Arc<sqlx::PgPool>,
    tenants: Vec<super::ctx::Tenant>,
) -> Outcome {
    let start = Instant::now();
    let mut ctx = QaCtx {
        kube,
        cli,
        pg,
        tenants,
    };
    let verdict = match tokio::time::timeout(meta.timeout, s.run(&mut ctx)).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => Verdict::Fail(format!("error: {e:#}")),
        Err(_) => Verdict::Fail(format!("timeout after {:?}", meta.timeout)),
    };
    Outcome {
        id: meta.id,
        verdict,
        elapsed: start.elapsed(),
    }
}

async fn collect(mut set: JoinSet<Outcome>) -> Vec<Outcome> {
    let mut out = Vec::new();
    while let Some(r) = set.join_next().await {
        out.push(r.expect("scenario task panicked"));
    }
    out
}

fn report(outcomes: &[Outcome]) {
    for o in outcomes {
        let (mark, msg) = match &o.verdict {
            Verdict::Pass => ("PASS", String::new()),
            Verdict::Skip(m) => ("SKIP", format!(" — {m}")),
            Verdict::Fail(m) => ("FAIL", format!(" — {m}")),
        };
        let line = format!(
            "{mark:4} {:32} {:>6.1}s{msg}",
            o.id,
            o.elapsed.as_secs_f64()
        );
        match o.verdict {
            Verdict::Fail(_) => warn!("{line}"),
            _ => info!("{line}"),
        }
    }
}
