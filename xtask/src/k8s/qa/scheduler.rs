//! Two-phase scenario scheduler.
//!
//! Phase 1: `Shared` + `Tenant` concurrently. Shared is unbounded;
//! Tenant is bounded by the tenant-pool semaphore (`acquire(count)`).
//!
//! Phase 2: `Exclusive`, greedy-scheduled with reader/writer semantics
//! over `Component`s. Each scenario declares a write set
//! (`Isolation::Exclusive { mutates }`) and a read set
//! (`Scenario::reads`, default `[Scheduler]`). A scenario is runnable
//! when its writes don't overlap any in-flight read or write, and its
//! reads don't overlap any in-flight write. Read-read overlap is
//! allowed. Re-scan on every completion.
//!
//! The read set exists because `mutates` only captures destruction:
//! i024 kills the scheduler leader (`mutates: [Scheduler, FetcherPool]`)
//! while i039 (`mutates: [Store]`) and i040 (`mutates: [S3, Postgres]`)
//! had disjoint write sets and ran concurrently — but both *submit*
//! builds, so they hit the leader transition and saw a false-positive
//! "scheduler actor is unavailable (panicked or exited)". A read
//! dependency on `Scheduler` serializes them with i024 without
//! over-claiming a write.

use std::collections::{HashMap, HashSet, VecDeque};
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

    // Report BEFORE cleanup — a cleanup failure (e.g. cli-tunnel
    // port-forward died after a scheduler-kill scenario) must not
    // swallow the verdicts.
    report(&outcomes);
    let fails = outcomes
        .iter()
        .filter(|o| matches!(o.verdict, Verdict::Fail(_)))
        .count();

    // Best-effort cleanup. Phase-2 scenarios may have killed the
    // scheduler-leader the original cli-tunnel was forwarded to —
    // re-open a fresh CliCtx so DeleteTenant reaches the new leader.
    // Cleanup failures warn rather than override the run's verdict.
    if let Err(e) = (async {
        let cli2 = CliCtx::open(&kube, 0, 0).await?;
        Arc::into_inner(pool)
            .expect("all leases released")
            .cleanup(&kube, &cli2)
            .await
    })
    .await
    {
        tracing::warn!("tenant cleanup failed (ephemeral tenants left behind): {e:#}");
    }
    drop(pg);

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

/// Reader/writer locks over `Component`s held by in-flight phase-2
/// scenarios. Standard rwlock semantics: many readers OR one writer per
/// component. A scenario `mutates: X` is the writer; a scenario
/// `reads: X` (without also mutating X) is a reader.
///
/// Reads are reference-counted because two readers of the same
/// component may overlap (e.g. i039 and i040 both `reads: [Scheduler]`,
/// disjoint writes); a `HashSet` would drop the read hold as soon as
/// the *first* of them finished and let a writer in under the second.
#[derive(Default)]
struct ComponentLocks {
    /// Write-held. At most one in-flight writer per component, so a
    /// set suffices.
    mutated: HashSet<Component>,
    /// Read-held → reader count.
    read: HashMap<Component, usize>,
}

impl ComponentLocks {
    /// Acquire if no conflict; on success record the holds and return
    /// `true`. On conflict no state changes and returns `false`.
    fn try_acquire(&mut self, mutates: &[Component], reads: &[Component]) -> bool {
        // W-W: another in-flight scenario is writing the same component.
        if mutates.iter().any(|c| self.mutated.contains(c)) {
            return false;
        }
        // R-W: this scenario reads a component another is writing.
        if reads.iter().any(|c| self.mutated.contains(c)) {
            return false;
        }
        // W-R: this scenario writes a component another is reading.
        if mutates.iter().any(|c| self.read.contains_key(c)) {
            return false;
        }
        self.mutated.extend(mutates.iter().copied());
        for c in reads {
            *self.read.entry(*c).or_default() += 1;
        }
        true
    }

    fn release(&mut self, mutates: &[Component], reads: &[Component]) {
        for c in mutates {
            self.mutated.remove(c);
        }
        for c in reads {
            if let Some(n) = self.read.get_mut(c) {
                *n -= 1;
                if *n == 0 {
                    self.read.remove(c);
                }
            }
        }
    }
}

/// Greedy reader/writer scheduler. Each Exclusive gets ONE tenant
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
    let mut locks = ComponentLocks::default();
    type Held = (&'static [Component], &'static [Component]);
    let mut set: JoinSet<(Outcome, Held)> = JoinSet::new();
    let mut out = Vec::new();

    loop {
        // Launch everything currently runnable.
        let mut i = 0;
        while i < pending.len() {
            let meta = pending[i].meta();
            let Isolation::Exclusive { mutates } = meta.isolation else {
                unreachable!("phase 2 is exclusive-only")
            };
            let reads = pending[i].reads();
            if locks.try_acquire(mutates, reads) {
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
                    (o, (mutates, reads))
                });
            } else {
                i += 1;
            }
        }
        // Drain one completion, release its locks, re-scan.
        match set.join_next().await {
            Some(res) => {
                let (o, (mutates, reads)) = res.expect("scenario task panicked");
                locks.release(mutates, reads);
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
    // Catch panics so one scenario's debug_assert / unwrap doesn't
    // take down the whole run (and lose all other verdicts + leave
    // phase-2 components held forever).
    use futures_util::FutureExt;
    let fut = std::panic::AssertUnwindSafe(tokio::time::timeout(meta.timeout, s.run(&mut ctx)))
        .catch_unwind();
    let verdict = match fut.await {
        Ok(Ok(Ok(v))) => v,
        Ok(Ok(Err(e))) => Verdict::Fail(format!("error: {e:#}")),
        Ok(Err(_)) => Verdict::Fail(format!("timeout after {:?}", meta.timeout)),
        Err(panic) => {
            let msg = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "non-string panic payload".into());
            Verdict::Fail(format!("PANIC: {msg}"))
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::qa::Component as C;
    use crate::k8s::qa::scenarios::ALL;

    fn find(id: &str) -> &'static dyn Scenario {
        *ALL.iter()
            .find(|s| s.meta().id == id)
            .unwrap_or_else(|| panic!("scenario {id} not in ALL"))
    }

    fn mutates(s: &'static dyn Scenario) -> &'static [Component] {
        match s.meta().isolation {
            Isolation::Exclusive { mutates } => mutates,
            other => panic!("{} is {other:?}, expected Exclusive", s.meta().id),
        }
    }

    #[test]
    fn write_write_conflict_blocks() {
        let mut l = ComponentLocks::default();
        assert!(l.try_acquire(&[C::Scheduler], &[]));
        assert!(!l.try_acquire(&[C::Scheduler], &[]));
    }

    #[test]
    fn read_write_conflict_blocks_in_both_directions() {
        // Writer in flight, reader must wait.
        let mut l = ComponentLocks::default();
        assert!(l.try_acquire(&[C::Scheduler], &[]));
        assert!(!l.try_acquire(&[C::Store], &[C::Scheduler]));

        // Reader in flight, writer must wait.
        let mut l = ComponentLocks::default();
        assert!(l.try_acquire(&[C::Store], &[C::Scheduler]));
        assert!(!l.try_acquire(&[C::Scheduler], &[]));
    }

    #[test]
    fn read_read_overlap_allowed() {
        // Two scenarios with disjoint writes both reading Scheduler can
        // run concurrently — read-read is not a conflict.
        let mut l = ComponentLocks::default();
        assert!(l.try_acquire(&[C::Store], &[C::Scheduler]));
        assert!(l.try_acquire(&[C::S3, C::Postgres], &[C::Scheduler]));
    }

    #[test]
    fn read_count_releases_correctly() {
        // Two readers; releasing one must NOT unblock a writer.
        let mut l = ComponentLocks::default();
        assert!(l.try_acquire(&[C::Store], &[C::Scheduler]));
        assert!(l.try_acquire(&[C::S3], &[C::Scheduler]));
        assert!(!l.try_acquire(&[C::Scheduler], &[]));
        l.release(&[C::Store], &[C::Scheduler]);
        assert!(!l.try_acquire(&[C::Scheduler], &[]));
        l.release(&[C::S3], &[C::Scheduler]);
        assert!(l.try_acquire(&[C::Scheduler], &[]));
    }

    #[test]
    fn try_acquire_failure_has_no_side_effects() {
        // A failed acquire must leave the locks untouched — a partial
        // acquire (e.g. recording the writes but bailing on the read
        // check) would leak a hold and deadlock the greedy loop.
        let mut l = ComponentLocks::default();
        assert!(l.try_acquire(&[C::Store], &[C::Scheduler]));
        // Conflicts on Scheduler (W-R: this writes, held read); also
        // names Postgres, which must stay free afterwards.
        assert!(!l.try_acquire(&[C::Scheduler, C::Postgres], &[]));
        assert!(l.try_acquire(&[C::Postgres], &[]));
    }

    /// The cluster-A regression this module fixes: i024 kills the
    /// scheduler leader (`mutates: [Scheduler, FetcherPool]`); i039
    /// (`mutates: [Store]`) and i040 (`mutates: [S3, Postgres]`) have
    /// disjoint write sets but both submit builds, so they read
    /// `Scheduler`. Pre-fix the greedy scheduler ran all three at
    /// once → false-positive "scheduler actor is unavailable" from a
    /// graceful drain, not a panic.
    #[test]
    fn i024_serializes_with_build_submitting_scenarios() {
        let i024 = find("i024-restart-drains-fods");
        let i039 = find("i039-store-kill-survives");
        let i040 = find("i040-chunk-verify");

        let mut l = ComponentLocks::default();
        assert!(l.try_acquire(mutates(i024), i024.reads()));
        assert!(
            !l.try_acquire(mutates(i039), i039.reads()),
            "i039 submits a build → must not run while i024 is killing the leader"
        );
        assert!(
            !l.try_acquire(mutates(i040), i040.reads()),
            "i040 submits a build → must not run while i024 is killing the leader"
        );

        // After i024 releases, both become runnable (and CAN run with
        // each other — disjoint writes, read-read on Scheduler).
        l.release(mutates(i024), i024.reads());
        assert!(l.try_acquire(mutates(i039), i039.reads()));
        assert!(l.try_acquire(mutates(i040), i040.reads()));
    }

    /// Concurrency must not regress: i024 should still be able to run
    /// alongside scenarios that genuinely never touch the scheduler.
    #[test]
    fn i024_concurrent_with_no_scheduler_dependency() {
        let i024 = find("i024-restart-drains-fods");

        for other_id in [
            "i048a-stale-realisation",         // PG-only LEFT JOIN probe
            "i109-authorized-keys-hot-reload", // gateway secret reload
            "i201-stranded-chunks",            // PG↔S3 sample
            "i207-stale-uploading",            // PG-only invariant
            "i086-reconcile-error-loud",       // controller CR probe
        ] {
            let other = find(other_id);
            assert_eq!(
                other.reads(),
                &[] as &[Component],
                "{other_id} has no scheduler dependency and should override reads() to []"
            );
            let mut l = ComponentLocks::default();
            assert!(l.try_acquire(mutates(i024), i024.reads()));
            assert!(
                l.try_acquire(mutates(other), other.reads()),
                "{other_id} must remain runnable while i024 holds Scheduler"
            );
        }
    }

    /// Every Exclusive scenario that submits a build (calls
    /// `nix_build*via_gateway*`) must keep the default
    /// `reads() = [Scheduler]`. This is a coarse structural check, not
    /// a substitute for review, but it catches "added a build to an
    /// existing scenario without revisiting its reads() override".
    #[test]
    fn build_submitting_exclusives_read_scheduler() {
        // Source-grep would be brittle; instead enumerate the known
        // build-submitting Exclusives and assert on each. Keep this
        // list in sync with `nix_build*via_gateway*` callsites in
        // `scenarios/`.
        let build_submitters = [
            "i024-restart-drains-fods",
            "i033-zombie-executors",
            "i039-store-kill-survives",
            "i040-chunk-verify",
            "i046-drain-reaches-leader",
            "i047-missing-input-redispatch",
            "i048c-blackhole-self-test",
            "i058-recovery-transitions",
            "i064-gateway-drain",
            "i183-pending-reaped",
            "i208-floor-hydrated",
            "i209-assignment-terminal",
        ];
        for id in build_submitters {
            let s = find(id);
            assert!(
                s.reads().contains(&C::Scheduler),
                "{id} submits a build but reads() doesn't include Scheduler — \
                 it would race a concurrent leader kill"
            );
        }
    }
}
