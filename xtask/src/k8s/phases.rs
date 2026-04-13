//! `up` phase DAG: declarative dependency table + concurrent ready-set
//! executor. Split out of `mod.rs` so the clap surface (UpOpts/K8sArgs)
//! and the dispatch logic (run/run_up) are separate from the DAG core.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;

use anyhow::{Result, bail};
use tokio::task::JoinSet;

use super::provider::{self, Provider};
use crate::config::XtaskConfig;
use crate::ui;

/// Ordered bring-up phases for `k8s up`. `up` with no phase flags runs
/// the full sequence; flags select a subset (still in this order).
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum Phase {
    Bootstrap,
    Provision,
    Kubeconfig,
    Ami,
    Push,
    Deploy,
}

impl Phase {
    /// Canonical order for selection/validation. Execution order is the
    /// DAG induced by [`Phase::deps`] — [`run_up_phases`] runs every
    /// phase whose dependencies are satisfied concurrently (bootstrap →
    /// provision; ami ∥ kubeconfig ∥ push after provision; deploy
    /// waits on the join of all three).
    pub const ALL: [Phase; 6] = [
        Phase::Bootstrap,
        Phase::Provision,
        Phase::Kubeconfig,
        Phase::Ami,
        Phase::Push,
        Phase::Deploy,
    ];

    pub(super) fn name(self) -> &'static str {
        match self {
            Phase::Bootstrap => "bootstrap",
            Phase::Provision => "provision",
            Phase::Kubeconfig => "kubeconfig",
            Phase::Ami => "ami",
            Phase::Push => "push",
            Phase::Deploy => "deploy",
        }
    }

    /// Declarative dependency table for the [`run_up_phases`] DAG
    /// executor. A phase is ready when every dep listed here is done
    /// (or wasn't selected — unselected deps are treated as
    /// pre-satisfied so `up --push --deploy` works without
    /// `--provision`).
    ///
    /// Adding an edge: extend the match arm. The `phase_deps_acyclic`
    /// test catches cycles.
    pub const fn deps(self) -> &'static [Phase] {
        match self {
            Phase::Bootstrap => &[],
            Phase::Provision => &[Phase::Bootstrap],
            Phase::Kubeconfig => &[Phase::Provision],
            // region/cluster_name (ami) and ECR repo URL (push) are
            // tofu outputs → neither can start before provision. On a
            // fresh account the state bucket doesn't even exist until
            // bootstrap, so the previous `Ami => &[]` failed `tofu
            // output` outright. build (the nix-build half of push) is
            // folded in rather than a separate output — it's fast and
            // local, not worth a typed-output channel.
            Phase::Ami | Phase::Push => &[Phase::Provision],
            // deploy reads the AMI tag from EC2 (ami phase registers
            // + tags it) + image tag (push) + talks to the cluster
            // (kubeconfig).
            Phase::Deploy => &[Phase::Kubeconfig, Phase::Push, Phase::Ami],
        }
    }
}

/// Per-phase knobs threaded through [`run_up_phases`]. Bundled so the
/// concurrent-dispatch core has a stable test surface independent of
/// `UpOpts` (which is clap-coupled). Owned + `Clone` so each spawned
/// phase task can hold its own copy (`'static` for `tokio::spawn`).
#[derive(Clone)]
pub(super) struct PhaseParams {
    pub(super) yes: bool,
    pub(super) deploy: provider::DeployOpts,
}

/// Concurrent core of `up`: a ready-set DAG executor over
/// [`Phase::deps`]. A phase spawns the moment all its (selected)
/// dependencies have completed; independent chains run concurrently
/// (bootstrap→provision, then ami ∥ kubeconfig ∥ push).
///
/// **No cancellation on error.** ami is a ~4 GB coldsnap upload per
/// arch; push is a multi-arch image push. A sibling failure (push
/// errors mid-ami-upload, or vice versa) MUST NOT drop the in-flight
/// work — that abandons a half-uploaded snapshot the operator then
/// has to clean up by hand. In-flight phases always run
/// to completion. A failed phase's *dependents* are never spawned
/// (their dep is never marked done); *siblings* on independent dep
/// chains continue. Errors are collected and surfaced together after
/// the graph drains.
///
/// `ami_branch` is injected so tests can mock it
/// without an EKS account or a live cluster. They are only awaited if
/// their phase is in `selected`.
///
/// **I-198 — per-phase `tokio::spawn`.** Each phase runs on its own
/// tokio task (`JoinSet`), so a phase that blocks the calling thread
/// (sync `sh::read`, `std::thread::sleep`, an SDK call that turns out
/// to be blocking) parks ONE worker thread — siblings on other workers
/// keep running. The earlier `FuturesUnordered` design polled every
/// phase from a single task and depended on every phase being purely
/// cooperative; one stray sync call serialized the whole DAG.
/// `Provider: Send + Sync` and `Arc`-threaded `cfg`/`pp` give the
/// `'static` bound `tokio::spawn` needs. Phases should still prefer
/// `sh::run`/`run_read` (yield) over `sh::read`/`run_sync` (block) —
/// each blocked phase ties up a runtime worker — but a slip is now a
/// throughput cost, not a wall-clock-doubling stall.
///
/// `ui::step` wraps each phase in a `tracing::info_span!` so the
/// per-phase close event (and any error) carries the phase name.
pub(super) async fn run_up_phases<A>(
    p: Arc<dyn Provider>,
    cfg: Arc<XtaskConfig>,
    selected: &[Phase],
    pp: PhaseParams,
    ami_branch: A,
) -> Result<()>
where
    A: Future<Output = Result<()>> + Send + 'static,
{
    // Per-phase unsatisfied-dep set, restricted to `selected`: a dep
    // the user didn't ask for is treated as already satisfied (so
    // `up --push --deploy` works without `--provision`).
    let mut pending: HashMap<Phase, HashSet<Phase>> = selected
        .iter()
        .map(|&ph| {
            let deps = ph
                .deps()
                .iter()
                .copied()
                .filter(|d| selected.contains(d))
                .collect();
            (ph, deps)
        })
        .collect();
    let mut started: HashSet<Phase> = HashSet::new();
    let mut failed: Vec<(Phase, anyhow::Error)> = Vec::new();
    let mut running: JoinSet<(Phase, Result<()>)> = JoinSet::new();
    // Injected futures, taken exactly once when their phase dispatches.
    let mut ami = Some(ami_branch);

    loop {
        // Spawn every selected phase whose pending-dep set is empty.
        // Each task owns clones of the Arcs/params it needs (`'static`
        // for `tokio::spawn`) — nothing borrows from this stack frame.
        for &ph in selected {
            if started.contains(&ph) || !pending[&ph].is_empty() {
                continue;
            }
            started.insert(ph);
            match ph {
                Phase::Bootstrap => {
                    let (p, cfg) = (p.clone(), cfg.clone());
                    running.spawn(async move {
                        (ph, ui::step("bootstrap", || p.bootstrap(&cfg)).await)
                    });
                }
                Phase::Provision => {
                    let (p, cfg) = (p.clone(), cfg.clone());
                    let yes = pp.yes;
                    running.spawn(async move {
                        (ph, ui::step("provision", || p.provision(&cfg, yes)).await)
                    });
                }
                Phase::Kubeconfig => {
                    let (p, cfg) = (p.clone(), cfg.clone());
                    running.spawn(async move {
                        (ph, ui::step("kubeconfig", || p.kubeconfig(&cfg)).await)
                    });
                }
                Phase::Ami => {
                    let a = ami.take().expect("ami dispatched once");
                    // Caller already wraps in ui::step("ami", ..).
                    running.spawn(async move { (ph, a.await) });
                }
                Phase::Push => {
                    let (p, cfg) = (p.clone(), cfg.clone());
                    running.spawn(async move {
                        // build+push kept as one phase: build is fast
                        // and local; not worth a typed-output channel
                        // between DAG nodes for one edge.
                        let r = async {
                            let images = ui::step("build", || p.build(&cfg)).await?;
                            ui::step("push", || p.push(&images, &cfg)).await
                        }
                        .await;
                        (ph, r)
                    });
                }
                Phase::Deploy => {
                    let (p, cfg, pp) = (p.clone(), cfg.clone(), pp.clone());
                    running.spawn(async move {
                        (ph, ui::step("deploy", || p.deploy(&cfg, &pp.deploy)).await)
                    });
                }
            }
        }

        // Drain one. `join_next()` never aborts the rest — in-flight
        // phases run to completion regardless of what we do with the
        // result. JoinSet's Drop WOULD abort, but we only return after
        // the loop exits (set empty), so every spawned task has joined.
        let Some(joined) = running.join_next().await else {
            break; // nothing running, nothing ready ⇒ graph drained
        };
        // JoinError = panic or cancel. We never cancel; surface a panic
        // verbatim — the phase task is gone, no in-flight work to save.
        let (ph, r) = joined.expect("phase task panicked");
        match r {
            // Success: clear this phase from every dependent's pending
            // set; next spawn pass picks up newly-ready phases.
            Ok(()) => {
                for deps in pending.values_mut() {
                    deps.remove(&ph);
                }
            }
            // Failure: record but DON'T remove from dependents' pending
            // sets — they stay non-empty forever, so dependents never
            // spawn. Siblings (independent pending sets) are unaffected.
            Err(e) => failed.push((ph, e)),
        }
    }

    if failed.is_empty() {
        return Ok(());
    }
    let skipped: Vec<_> = selected
        .iter()
        .filter(|ph| !started.contains(ph))
        .map(|ph| ph.name())
        .collect();
    for ph in &skipped {
        ui::step_skip(ph, "dependency failed");
    }
    let skipped_ctx = if skipped.is_empty() {
        String::new()
    } else {
        format!(" (skipped: {})", skipped.join(", "))
    };
    match failed.len() {
        // Single failure: hoist the real anyhow::Error so the
        // backtrace/context chain survives.
        1 => {
            let (ph, e) = failed.pop().unwrap();
            Err(e.context(format!("{} failed{skipped_ctx}", ph.name())))
        }
        n => {
            let mut msg = format!("{n} phases failed{skipped_ctx}:");
            for (ph, e) in &failed {
                msg.push_str(&format!("\n  {}: {e:#}", ph.name()));
            }
            bail!(msg)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;

    use super::super::shared;
    use super::*;
    use provider::BuiltImages;

    // -- mock provider for run_up_phases concurrency tests ------------

    type Log = Arc<Mutex<Vec<&'static str>>>;

    /// Records call order; per-method delays make ready-set
    /// interleaving observable. `bootstrap_at` timestamps bootstrap
    /// completion relative to construction (for the I-198 raw-blocking
    /// test).
    struct MockProvider {
        log: Log,
        provision_delay: Duration,
        kubeconfig_delay: Duration,
        push_delay: Duration,
        push_err: bool,
        t0: std::time::Instant,
        bootstrap_at: Arc<Mutex<Option<Duration>>>,
    }

    impl MockProvider {
        fn new(log: Log) -> Self {
            Self {
                log,
                provision_delay: Duration::ZERO,
                kubeconfig_delay: Duration::ZERO,
                push_delay: Duration::ZERO,
                push_err: false,
                t0: std::time::Instant::now(),
                bootstrap_at: Arc::default(),
            }
        }
        fn record(&self, what: &'static str) {
            self.log.lock().unwrap().push(what);
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        fn context_matches(&self, _: &str) -> bool {
            unimplemented!()
        }
        async fn bootstrap(&self, _: &XtaskConfig) -> Result<()> {
            self.record("bootstrap");
            *self.bootstrap_at.lock().unwrap() = Some(self.t0.elapsed());
            Ok(())
        }
        async fn provision(&self, _: &XtaskConfig, _: bool) -> Result<()> {
            tokio::time::sleep(self.provision_delay).await;
            self.record("provision");
            Ok(())
        }
        async fn kubeconfig(&self, _: &XtaskConfig) -> Result<()> {
            self.record("kubeconfig:start");
            tokio::time::sleep(self.kubeconfig_delay).await;
            self.record("kubeconfig");
            Ok(())
        }
        async fn build(&self, _: &XtaskConfig) -> Result<BuiltImages> {
            self.record("build");
            Ok(BuiltImages {
                dir: tempfile::TempDir::new()?,
                tag: "test".into(),
            })
        }
        async fn push(&self, _: &BuiltImages, _: &XtaskConfig) -> Result<()> {
            tokio::time::sleep(self.push_delay).await;
            if self.push_err {
                bail!("mock push failed");
            }
            self.record("push");
            Ok(())
        }
        async fn deploy(&self, _: &XtaskConfig, _: &provider::DeployOpts) -> Result<()> {
            self.record("deploy");
            Ok(())
        }
        async fn smoke(&self, _: &XtaskConfig) -> Result<()> {
            unimplemented!()
        }
        async fn tunnel(&self, _: u16) -> Result<shared::ProcessGuard> {
            unimplemented!()
        }
        async fn tunnel_grpc(
            &self,
            _: u16,
            _: u16,
        ) -> Result<((u16, shared::ProcessGuard), (u16, shared::ProcessGuard))> {
            unimplemented!()
        }
        async fn destroy(&self, _: &XtaskConfig) -> Result<()> {
            unimplemented!()
        }
    }

    fn pp() -> PhaseParams {
        PhaseParams {
            yes: true,
            deploy: provider::DeployOpts {
                log_level: "info".into(),
                ..Default::default()
            },
        }
    }

    fn cfg() -> Arc<XtaskConfig> {
        Arc::new(XtaskConfig::default())
    }

    fn pos(log: &[&str], what: &str) -> usize {
        log.iter().position(|&x| x == what).unwrap()
    }

    /// All phases selected: ami, kubeconfig and push spawn together
    /// once provision completes; deploy only starts after all three.
    /// ami sleeps longer than push so "push before ami, deploy after
    /// ami" proves the post-provision fan-out actually overlapped.
    #[tokio::test]
    async fn up_phases_concurrent_ordering() {
        let log: Log = Arc::default();
        let p: Arc<dyn Provider> = Arc::new(MockProvider::new(log.clone()));

        let ami_log = log.clone();
        let ami = async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            ami_log.lock().unwrap().push("ami");
            Ok(())
        };

        run_up_phases(p, cfg(), &Phase::ALL, pp(), ami)
            .await
            .unwrap();

        let log = log.lock().unwrap();
        // infra chain stays internally ordered
        assert!(pos(&log, "bootstrap") < pos(&log, "provision"));
        assert!(pos(&log, "provision") < pos(&log, "push"));
        // ami overlapped its siblings: push (instant) finished while
        // ami (30ms) was still sleeping — proves the post-provision
        // fan-out ran concurrently, not sequentially
        assert!(pos(&log, "push") < pos(&log, "ami"), "log: {log:?}");
        // deploy waited for the join: it's after BOTH ami and push
        assert!(pos(&log, "ami") < pos(&log, "deploy"), "log: {log:?}");
        assert!(pos(&log, "push") < pos(&log, "deploy"), "log: {log:?}");
    }

    /// The critical safety property: a push failure does NOT cancel
    /// an in-flight ami upload (sibling, same dep). `try_join!` would
    /// drop the ami future the moment push errors; the JoinSet drain
    /// lets ami run to completion and surfaces both results after.
    #[tokio::test]
    async fn up_push_error_does_not_cancel_ami() {
        let log: Log = Arc::default();
        let mut p = MockProvider::new(log.clone());
        p.push_err = true;
        let p: Arc<dyn Provider> = Arc::new(p);

        let ami_log = log.clone();
        let ami = async move {
            // push fails immediately; ami is still mid-upload here.
            tokio::time::sleep(Duration::from_millis(50)).await;
            ami_log.lock().unwrap().push("ami");
            Ok(())
        };

        let r = run_up_phases(
            p,
            cfg(),
            &[Phase::Provision, Phase::Push, Phase::Ami],
            pp(),
            ami,
        )
        .await;

        // ami ran to completion despite push erroring first
        let log = log.lock().unwrap();
        assert!(
            log.contains(&"ami"),
            "ami was cancelled — JoinSet drain semantics broken: {log:?}"
        );
        // overall result is still Err, with the push context
        let e = r.unwrap_err().to_string();
        assert!(e.contains("push failed"), "err: {e}");
    }

    /// Phase selection still gates: `up --push --deploy` runs only
    /// push+deploy; ami_branch is the caller's responsibility (here a
    /// no-op) and bootstrap/provision/kubeconfig are skipped.
    #[tokio::test]
    async fn up_phases_selection_subset() {
        let log: Log = Arc::default();
        let p: Arc<dyn Provider> = Arc::new(MockProvider::new(log.clone()));

        run_up_phases(p, cfg(), &[Phase::Push, Phase::Deploy], pp(), async {
            Ok(())
        })
        .await
        .unwrap();

        assert_eq!(&*log.lock().unwrap(), &["build", "push", "deploy"]);
    }

    /// True ready-set (not layer-barrier, not serial chain): kubeconfig
    /// and push both depend ONLY on provision, so once provision
    /// finishes both must spawn in the same tick. Each one's start
    /// marker must land before the OTHER's finish marker — proves
    /// overlap regardless of which finishes first.
    #[tokio::test]
    async fn up_dag_kubeconfig_push_concurrent() {
        let log: Log = Arc::default();
        let mut p = MockProvider::new(log.clone());
        p.provision_delay = Duration::from_millis(5);
        p.kubeconfig_delay = Duration::from_millis(30);
        p.push_delay = Duration::from_millis(30);
        let p: Arc<dyn Provider> = Arc::new(p);

        run_up_phases(
            p,
            cfg(),
            &[
                Phase::Bootstrap,
                Phase::Provision,
                Phase::Kubeconfig,
                Phase::Push,
            ],
            pp(),
            async { Ok(()) },
        )
        .await
        .unwrap();

        let log = log.lock().unwrap();
        // kubeconfig started before push finished
        assert!(
            pos(&log, "kubeconfig:start") < pos(&log, "push"),
            "log: {log:?}"
        );
        // push started (build is its first sub-step) before kubeconfig
        // finished
        assert!(pos(&log, "build") < pos(&log, "kubeconfig"), "log: {log:?}");
        // and both honored the provision dep
        assert!(pos(&log, "provision") < pos(&log, "kubeconfig:start"));
        assert!(pos(&log, "provision") < pos(&log, "build"));
    }

    /// I-198 regression. Each phase runs on its own tokio task
    /// (`JoinSet::spawn`), so a phase that BLOCKS THE THREAD — raw
    /// `std::thread::sleep`, sync `sh::read`, an SDK call that turns
    /// out to be blocking — parks one runtime worker; the sibling on
    /// another worker keeps running. ami parks for 200ms with NO
    /// `spawn_blocking` / no yield; bootstrap (sibling, no deps,
    /// instant) must still complete in <100ms.
    ///
    /// Under the old single-task `FuturesUnordered` executor this
    /// would serialize: cooperative concurrency only — one parked
    /// thread = no sibling polled. Option A (a6a1de7a) papered over it
    /// by mandating `spawn_blocking` everywhere; this test only passes
    /// with the structural fix (per-phase spawn).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn up_dag_raw_blocking_does_not_stall_sibling() {
        let log: Log = Arc::default();

        let ami_log = log.clone();
        let ami = async move {
            ami_log.lock().unwrap().push("ami:start");
            // Raw thread park — NO spawn_blocking, NO .await. Under
            // FuturesUnordered this stalls every sibling.
            std::thread::sleep(Duration::from_millis(200));
            ami_log.lock().unwrap().push("ami");
            Ok(())
        };

        // Ami is first in `selected` so it dispatches (and parks its
        // worker) before bootstrap is spawned — if both ran on one
        // task, bootstrap_at would be ≥200ms.
        let mp = MockProvider::new(log.clone());
        let bootstrap_at = mp.bootstrap_at.clone();
        let p: Arc<dyn Provider> = Arc::new(mp);
        run_up_phases(p, cfg(), &[Phase::Ami, Phase::Bootstrap], pp(), ami)
            .await
            .unwrap();

        let log = log.lock().unwrap();
        let b_at = bootstrap_at.lock().unwrap().expect("bootstrap ran");
        // Bootstrap completed while ami's worker was parked. 100ms
        // slack for CI jitter; failure mode is ≥200ms.
        assert!(
            b_at < Duration::from_millis(100),
            "bootstrap serialized behind ami's blocking work: {b_at:?} (log: {log:?})"
        );
        // bootstrap finished before ami's 200ms sleep returned —
        // holds regardless of which task the runtime schedules first.
        assert!(pos(&log, "bootstrap") < pos(&log, "ami"), "log: {log:?}");
        assert!(log.contains(&"ami:start"), "log: {log:?}");
    }

    /// A failed phase poisons its dependents but NOT its siblings.
    /// push errors immediately; kubeconfig (sibling — same dep,
    /// independent of push) is mid-flight and runs to completion;
    /// deploy (depends on push) is never spawned.
    #[tokio::test]
    async fn up_dag_failed_dep_skips_dependents_not_siblings() {
        let log: Log = Arc::default();
        let mut p = MockProvider::new(log.clone());
        p.kubeconfig_delay = Duration::from_millis(20);
        p.push_err = true;
        let p: Arc<dyn Provider> = Arc::new(p);

        let r = run_up_phases(
            p,
            cfg(),
            // Ami unselected → pre-satisfied, so deploy's only blockers
            // are kubeconfig+push.
            &[
                Phase::Provision,
                Phase::Kubeconfig,
                Phase::Push,
                Phase::Deploy,
            ],
            pp(),
            async { Ok(()) },
        )
        .await;

        let log = log.lock().unwrap();
        // sibling completed despite push having already failed
        assert!(log.contains(&"kubeconfig"), "log: {log:?}");
        // dependent never spawned
        assert!(!log.contains(&"deploy"), "log: {log:?}");
        // error names the failed phase and the skipped dependent
        let e = format!("{:#}", r.unwrap_err());
        assert!(e.contains("push failed"), "err: {e}");
        assert!(e.contains("skipped: deploy"), "err: {e}");
    }

    /// Kahn's-algorithm cycle check over [`Phase::deps`]. Runs as a
    /// test (not `debug_assert!` in `deps()`) because `deps()` is
    /// `const fn` and the table is static — one check at test time is
    /// enough to catch a bad edge added in review.
    #[test]
    fn phase_deps_acyclic() {
        let mut indeg: HashMap<Phase, usize> =
            Phase::ALL.iter().map(|&p| (p, p.deps().len())).collect();
        // self-edges would otherwise pass Kahn (indeg never hits 0 →
        // caught as "cycle"), but flag them explicitly for the message.
        for &p in &Phase::ALL {
            assert!(!p.deps().contains(&p), "{p:?} depends on itself");
        }
        let mut ready: Vec<Phase> = indeg
            .iter()
            .filter(|&(_, &d)| d == 0)
            .map(|(&p, _)| p)
            .collect();
        let mut visited = 0;
        while let Some(p) = ready.pop() {
            visited += 1;
            for &succ in &Phase::ALL {
                if succ.deps().contains(&p) {
                    let d = indeg.get_mut(&succ).unwrap();
                    *d -= 1;
                    if *d == 0 {
                        ready.push(succ);
                    }
                }
            }
        }
        assert_eq!(
            visited,
            Phase::ALL.len(),
            "Phase::deps() has a cycle: unresolved = {:?}",
            indeg
                .iter()
                .filter(|&(_, &d)| d > 0)
                .map(|(p, _)| p)
                .collect::<Vec<_>>()
        );
    }
}
