//! The guard domain: a dedicated `current_thread` tokio runtime on its
//! own OS thread hosting the kill-wired liveness surface, the lease
//! renew loop, and the runtime-skew sentinel.
//!
//! Round-9 Banner B (the 054 incident close):
//!
//! - **D1 — failure-domain isolation.** A guard's timer/sensor/decision
//!   path must remain schedulable when the component it guards is not.
//!   Pre-split, `/healthz` was an axum task on the SHARED runtime: a
//!   13–21s reconciler stall under admitted load read as sensor
//!   silence and kubelet killed a healthy singleton 5× (live_054).
//!   Measured economics (192-cpu bench): shared-runtime guard skew =
//!   the stall length (4.9–9.9s); a dedicated OS-thread watchdog:
//!   69–97µs — five orders of magnitude for +1 thread / +24–80KB.
//! - **D3 — kill-wired vs shed-wired.** Kill-wired surfaces (liveness,
//!   lease renew) are served from the guard domain, which stays
//!   responsive under worst-case admitted load. Readiness stays wired
//!   to the WORKING domain: `/readyz` forwards a probe to the main
//!   runtime, so a browned-out controller sheds (Endpoints removal)
//!   instead of being killed.
//! - **D5 — the skew sentinel.** D1 is falsifiable in production: the
//!   sentinel exports each domain's executor-scheduling delay as
//!   `rio_controller_runtime_skew_seconds{domain}` and captures a
//!   thread table past [`GuardConfig::stall_threshold`]
//!   (`rio_controller_runtime_skew_stalls_total{domain}`), so the next
//!   freeze is attributable instead of a silent kill-loop.
//!
//! ## Clock choice (divergence record)
//!
//! Skew is measured with [`std::time::Instant`] (CLOCK_MONOTONIC):
//! executor-scheduling delay is an in-process quantity. Host-suspend
//! attribution deliberately stays in rio-lease's CLOCK_BOOTTIME domain
//! (`rio-lease/src/clock.rs` `suspend_aware_now` — the in-tree
//! precedent the dossier names); a suspended host is the lease's
//! self-fence story, not a runtime stall.
//!
//! ## Cross-domain census (the honest cost of the split)
//!
//! Isolation is real only if no lock a stalled main-domain task can
//! HOLD sits on the guard's paths. Everything shared across the two
//! domains, exhaustively:
//!
//! 1. `ready: Arc<AtomicBool>` — readiness flag (atomic).
//! 2. `main: tokio::runtime::Handle` — `/readyz` + sentinel probe
//!    spawns. `Handle::spawn` contends only with other SPAWNERS on the
//!    injection queue; a worker stalled in user code (compute/syscall)
//!    does not hold it.
//! 3. The skew/stall `Arc<AtomicU64>` mirrors (atomics).
//! 4. `shutdown: CancellationToken` — waker registration takes a
//!    short internal lock, never held across user code or `.await`.
//! 5. `rio_lease::LeaderState` — all `Arc<Atomic*>` fields (verified
//!    at the struct definition), shared with the nodeclaim_pool
//!    reconciler on the main domain.
//! 6. The global `metrics` recorder registry — emit-side operations
//!    take bounded registry locks with no `.await` inside; a task
//!    stalled MID-EMIT is the one disclosed residual, priced as
//!    pathological (emit bodies are lock-short and allocation-free).
//! 7. `epilogue_tx: mpsc::UnboundedSender<DrainHandle>` — adoption
//!    sends take a short queue lock, never held across user code or
//!    `.await`; the receiver lives on the guard root only.
//!
//! ## Task-lifetime census (the join-obligation axis; bug_118)
//!
//! Locks are not the only cross-domain obligation: a task hosted on
//! the guard runtime either dies with the runtime BY DESIGN or owes a
//! post-cancellation EPILOGUE the root must drain. Every guard-runtime
//! spawn site carries its disposition, exhaustively (enforced by the
//! W10-AU census in `tests/guard_epilogue.rs`, enumerated from the
//! spawn sites themselves):
//!
//! - health server (`tokio::spawn` in the root): runtime-lifetime —
//!   graceful-shutdown server, no epilogue.
//! - skew sentinel (`tokio::spawn` in the root): runtime-lifetime —
//!   pure measurement loop, no epilogue.
//! - lease loop ([`GuardHandle::spawn_lease`]): EPILOGUE-BEARING — the
//!   graceful-release `step_down()` PATCH runs after cancellation;
//!   spawning returns a [`DrainHandle`] the root drains bounded
//!   (`sys.epilogue.drain`).
//! - [`GuardHandle::spawn_on_guard`] (test/diagnostic seam): the
//!   caller owns the returned `JoinHandle`; production callsites must
//!   declare a `drain-census:` disposition.
//! - the `rio-guard` OS thread itself (`std::thread::Builder::spawn`
//!   in [`spawn`]): PROCESS-JOINED — its `JoinHandle` is captured in
//!   [`GuardJoin`] and the process owner `.join()`s it after the
//!   working domain drains (bug_023; the §Verifier-one-step-removed(b)
//!   recurrence of bug_118 one lifecycle level up).
//!
//! The lease loop's kube client is NOT shared: `run_lease_loop`
//! constructs its own client, so apiserver I/O for renewal never rides
//! a main-domain connection pool.
//!
//! Residual (priced, signed via the dossier): on the GRACEFUL path the
//! process owner [`GuardJoin::join`]s this thread after the working
//! domain drains, so the epilogue lands before exit (bug_023; the
//! process-joined class above). The thread still dies with the process
//! on SIGKILL/crash (kubelet covers that — the steal threshold is the
//! designed fallback) and still lags under cgroup-level CPU starvation
//! — if a future freeze is cgroup-class, the sentinel's capture will
//! say so and a CPU-request raise joins the fix.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use tracing::{info, warn};

/// Tuning for the guard domain. All axes typed and violable (R17):
/// the defaults are derivations, not magic.
#[derive(Debug, Clone)]
pub struct GuardConfig {
    /// `/healthz` + `/readyz` bind address (the chart's `health` port).
    pub health_addr: std::net::SocketAddr,
    /// Sentinel cadence. 1s: an order of magnitude under the chart's
    /// 10s liveness period, so the gauge moves well before kubelet
    /// votes; cost is one no-op task spawn per tick.
    pub probe_interval: Duration,
    /// Main-domain skew at which the sentinel captures a thread table
    /// and increments the stall counter. 1s: above any healthy
    /// scheduling delay by ~3 orders of magnitude (bench: µs), below
    /// the 4.9–9.9s measured incident stalls by the same margin.
    pub stall_threshold: Duration,
    /// `/readyz` main-domain probe budget. 1s: well inside the
    /// chart's readiness `timeoutSeconds: 5`, so the shed verdict is
    /// the controller's own, not kubelet's timeout.
    pub ready_probe_budget: Duration,
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            health_addr: ([0, 0, 0, 0], 9194).into(),
            probe_interval: Duration::from_secs(1),
            stall_threshold: Duration::from_secs(1),
            ready_probe_budget: Duration::from_secs(1),
        }
    }
}

/// Lock-free cross-domain view of the guard. Cheap to clone; every
/// field is an `Arc<Atomic*>`, a runtime handle, or a lock-free
/// channel sender (see the module-doc census).
#[derive(Clone)]
pub struct GuardHandle {
    main_skew_us: Arc<AtomicU64>,
    guard_skew_us: Arc<AtomicU64>,
    main_stalls: Arc<AtomicU64>,
    guard_stalls: Arc<AtomicU64>,
    guard_rt: tokio::runtime::Handle,
    epilogue_tx: tokio::sync::mpsc::UnboundedSender<DrainHandle>,
}

/// A guard-runtime task that owes a post-cancellation EPILOGUE (the
/// lease loop's graceful-release `step_down()` PATCH). The R24
/// construction type for the drain law (`sys.epilogue.drain`): spawning
/// an epilogue-bearing task RETURNS this handle, and the guard root is
/// the only drain site — the caller's sole discharge is
/// [`GuardHandle::adopt_epilogue`], which hands the handle to the root;
/// on cancellation the root awaits it BOUNDED (the budget the epilogue
/// crate derives) before the runtime drops. A discarded handle is a
/// compile-line error under `--deny warnings` and a census red
/// (W10-AU).
#[must_use = "the guard root must drain this epilogue before the runtime drops: \
              pass it to GuardHandle::adopt_epilogue"]
pub struct DrainHandle {
    /// Task name for the drain-timeout log line.
    name: &'static str,
    /// The epilogue-bearing task.
    task: tokio::task::JoinHandle<()>,
    /// Bounded drain window: how long the root keeps the runtime alive
    /// after cancellation for THIS epilogue to land.
    budget: Duration,
}

impl DrainHandle {
    /// Await the epilogue bounded. Private BY DESIGN: the guard root's
    /// shutdown path is the only drop site (`sys.epilogue.drain`).
    async fn drain(self) {
        // On expiry the epilogue is abandoned (logged) and the runtime
        // drop proceeds — the lease steal threshold is the fallback.
        // timeout-census: delay — shutdown delayed ≤ budget while the
        // epilogue lands. census[gen: rio-controller/tests/timeout_census.txt]
        match tokio::time::timeout(self.budget, self.task).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!(task = self.name, error = %e, "guard epilogue task panicked"),
            Err(_) => warn!(
                task = self.name,
                budget_secs = self.budget.as_secs_f64(),
                "guard epilogue did not land within its drain budget; proceeding with \
                 runtime drop (the steal threshold is the fallback)"
            ),
        }
    }
}

impl GuardHandle {
    /// Latest observed MAIN-domain executor-scheduling delay. While a
    /// probe is outstanding this is a RUNNING LOWER BOUND (it grows
    /// with the stall), so the value is truthful DURING a freeze, not
    /// only after it resolves. The W9-AU oracle (S4 consumes this).
    // r[impl sys.guard.skew-sentinel]
    pub fn main_skew(&self) -> Duration {
        Duration::from_micros(self.main_skew_us.load(Ordering::Relaxed))
    }

    /// Latest observed GUARD-domain self-skew (timer overshoot on the
    /// dedicated runtime). O(ms) even while the main domain is stalled
    /// — the D1 witness reads this.
    pub fn guard_skew(&self) -> Duration {
        Duration::from_micros(self.guard_skew_us.load(Ordering::Relaxed))
    }

    /// Main-domain stall episodes observed (edge-triggered at
    /// [`GuardConfig::stall_threshold`]). Mirror of
    /// `rio_controller_runtime_skew_stalls_total{domain="main"}`.
    pub fn main_stalls(&self) -> u64 {
        self.main_stalls.load(Ordering::Relaxed)
    }

    /// Guard-domain stall episodes observed (the same edge semantics
    /// as [`Self::main_stalls`] — one `EdgeLatch` type, two
    /// instances; the latch type is module-private).
    /// Mirror of
    /// `rio_controller_runtime_skew_stalls_total{domain="guard"}` —
    /// the accessor merged_bug_003's witness gap named (W9-AK could
    /// only pin the main domain).
    pub fn guard_stalls(&self) -> u64 {
        self.guard_stalls.load(Ordering::Relaxed)
    }

    /// Host the lease renew loop on the guard domain. THE point of
    /// the split for the lease: renewal keeps its 5s cadence while
    /// the main domain is stalled, so the fence-check premise
    /// (`rio-lease/src/lib.rs` — fence-check latency ≤ one tick)
    /// survives admitted-load starvation instead of being violated
    /// 2.5–3× (the 054 measurement). `run_lease_loop` builds its own
    /// kube client on THIS runtime — no main-domain pool sharing.
    /// Returns the lease loop's [`DrainHandle`] — the epilogue
    /// obligation (`sys.epilogue.drain`): the loop owes a
    /// post-cancellation graceful-release PATCH, so the caller MUST
    /// hand the handle to the guard root via
    /// [`Self::adopt_epilogue`]; the root drains it bounded
    /// ([`rio_lease::SHUTDOWN_EPILOGUE_BUDGET`]) before the runtime
    /// drops.
    // r[impl sys.epilogue.drain+2]
    pub fn spawn_lease<H: rio_lease::LeaseHooks>(
        &self,
        cfg: rio_lease::LeaseConfig,
        state: rio_lease::LeaderState,
        hooks: H,
        shutdown: rio_common::signal::Token,
    ) -> DrainHandle {
        DrainHandle {
            name: "lease-loop",
            task: self
                .guard_rt
                .spawn(rio_lease::run_lease_loop(cfg, state, hooks, shutdown)),
            budget: rio_lease::SHUTDOWN_EPILOGUE_BUDGET,
        }
    }

    /// The injected-client form of [`Self::spawn_lease`]: the SAME
    /// hosting wiring (and the same [`DrainHandle`] obligation) with
    /// the kube client supplied by the caller — the harness seam for
    /// driving the production lease loop against an in-process mock
    /// apiserver (the guard drain-protocol test is the consumer).
    /// Production wiring uses [`Self::spawn_lease`].
    pub fn spawn_lease_with_client<H: rio_lease::LeaseHooks>(
        &self,
        client: kube::Client,
        cfg: rio_lease::LeaseConfig,
        state: rio_lease::LeaderState,
        hooks: H,
        shutdown: rio_common::signal::Token,
    ) -> DrainHandle {
        DrainHandle {
            name: "lease-loop",
            task: self.guard_rt.spawn(rio_lease::run_lease_loop_with(
                client, cfg, state, hooks, shutdown,
            )),
            budget: rio_lease::SHUTDOWN_EPILOGUE_BUDGET,
        }
    }

    /// Hand an epilogue-bearing task's [`DrainHandle`] to the guard
    /// root — the ONLY discharge for the handle. The root drains every
    /// adopted handle bounded after cancellation, BEFORE the guard
    /// runtime drops (`sys.epilogue.drain`). If the root has already
    /// exited (shutdown raced the spawn), the obligation is
    /// undischargeable: log it — the lease steal threshold is the
    /// designed fallback.
    pub fn adopt_epilogue(&self, handle: DrainHandle) {
        if let Err(e) = self.epilogue_tx.send(handle) {
            warn!(
                task = e.0.name,
                "guard root already exited; epilogue cannot be drained \
                 (the steal threshold is the fallback)"
            );
        }
    }

    /// Test/diagnostic seam: spawn an arbitrary task onto the guard
    /// runtime. Production wiring uses the named methods above so the
    /// guard's task census stays readable in `main.rs`.
    pub fn spawn_on_guard<F>(&self, fut: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        // drain-census: caller-joined — the JoinHandle is returned;
        // the caller owns the task's lifetime (test/diagnostic seam).
        self.guard_rt.spawn(fut)
    }
}

/// `/readyz` handler state (lives on the guard runtime).
#[derive(Clone)]
struct ReadyState {
    ready: Arc<AtomicBool>,
    main: tokio::runtime::Handle,
    budget: Duration,
}

/// The guard thread's LINEAR join token (the process↔thread axis;
/// bug_023). [`spawn`] returns `(GuardHandle, GuardJoin)`: the
/// clonable atomics handle is split from the move-only join token so
/// the process owner CANNOT exit without explicitly disposing of the
/// thread. `#[must_use]` alone does NOT catch `let (g, _) = …`
/// (verified on stable; `_` suppresses the lint), so [`Self::join`] is
/// the only discharge and the [`Drop`] impl panics on any other path —
/// the runtime enforcement; the W10-AU `PROCESS_JOIN_FORMS` census in
/// `tests/guard_epilogue.rs` is the static one.
#[must_use = "the process owner MUST GuardJoin::join() after the working \
              domain drains; a dropped token is the bug_023 shape"]
pub struct GuardJoin {
    thread: Option<std::thread::JoinHandle<()>>,
}

impl GuardJoin {
    /// Block until the guard thread exits — bounded: the root drains
    /// every adopted epilogue under each handle's own budget before it
    /// returns (the lease loop's is
    /// [`rio_lease::SHUTDOWN_EPILOGUE_BUDGET`], whose derivation from
    /// the renew constants is the `const_assert!` at
    /// `rio-lease/src/lib.rs:160`). Call AFTER the cancellation token
    /// fires and the working domain's drains return; calling before
    /// cancellation deadlocks (the root never reaches the drain
    /// state).
    // r[impl sys.epilogue.drain+2]
    pub fn join(mut self) {
        if let Err(panic) = self
            .thread
            .take()
            .expect("GuardJoin::join consumes the token exactly once")
            .join()
        {
            std::panic::resume_unwind(panic);
        }
    }
}

impl Drop for GuardJoin {
    fn drop(&mut self) {
        if self.thread.is_some() && !std::thread::panicking() {
            // The `take()` in `join()` is the only path that empties
            // `thread`; reaching here with it populated means the
            // process owner returned without joining — the guard
            // thread is mid-`epilogue.drain()` and process exit will
            // abort the in-flight `step_down()` PATCH, leaving the
            // dead pod holding the lease the full ~19s STEAL_AFTER.
            // Panic is bounded-safe: we are already on the
            // process-exit path; an unwind here turns a silent lease
            // hold into a loud crash with a stack the next reviewer
            // can read. The bomb is for the NON-UNWINDING leak only:
            // during unwind a second panic is an abort that buries
            // the real failure (r26 irony-check on bug_023), so the
            // panicking() guard yields to the original panic — the
            // steal threshold is the fallback either way.
            panic!(
                "GuardJoin dropped without .join() — bug_023: the process owner \
                 returned without joining the rio-guard thread; the in-flight \
                 lease step_down() PATCH would be aborted by process exit"
            );
        }
    }
}

/// Spawn the guard domain: one OS thread (`rio-guard`) driving a
/// `current_thread` runtime that hosts the health server and the skew
/// sentinel. The lease loop joins via [`GuardHandle::spawn_lease`].
///
/// `main` is the WORKING domain's handle (probe target); `ready` is
/// the readiness flag main.rs flips after `connect_forever`. Returns
/// the clonable [`GuardHandle`] and the linear [`GuardJoin`] token —
/// the process owner MUST `.join()` the token after the working
/// domain drains (`sys.epilogue.drain`).
// r[impl sys.guard.domain-isolation]
pub fn spawn(
    main: tokio::runtime::Handle,
    ready: Arc<AtomicBool>,
    cfg: GuardConfig,
    shutdown: rio_common::signal::Token,
) -> (GuardHandle, GuardJoin) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .thread_name("rio-guard")
        .build()
        .expect("guard runtime build cannot fail with enable_all on a fresh builder");
    let (epilogue_tx, mut epilogue_rx) = tokio::sync::mpsc::unbounded_channel::<DrainHandle>();
    let handle = GuardHandle {
        main_skew_us: Arc::new(AtomicU64::new(0)),
        guard_skew_us: Arc::new(AtomicU64::new(0)),
        main_stalls: Arc::new(AtomicU64::new(0)),
        guard_stalls: Arc::new(AtomicU64::new(0)),
        guard_rt: rt.handle().clone(),
        epilogue_tx,
    };
    let h = handle.clone();
    let thread_shutdown = shutdown.clone();
    let thread = std::thread::Builder::new()
        .name("rio-guard".into())
        .spawn(move || {
            rt.block_on(async move {
                // Health server: kill-wired liveness answered locally;
                // shed-wired readiness forwarded to the working domain.
                let router = Router::new()
                    .route("/healthz", get(async || StatusCode::OK))
                    .route("/readyz", get(readyz))
                    .with_state(ReadyState {
                        ready,
                        main: main.clone(),
                        budget: cfg.ready_probe_budget,
                    });
                let serve_shutdown = thread_shutdown.clone();
                let health_addr = cfg.health_addr;
                // drain-census: runtime-lifetime — graceful-shutdown
                // server, no post-cancellation epilogue; dies with the
                // runtime BY DESIGN.
                tokio::spawn(async move {
                    info!(addr = %health_addr, "guard: starting health server");
                    match tokio::net::TcpListener::bind(health_addr).await {
                        Ok(listener) => {
                            if let Err(e) = axum::serve(listener, router)
                                .with_graceful_shutdown(serve_shutdown.cancelled_owned())
                                .await
                            {
                                tracing::error!(error = %e, "guard: health server failed");
                            }
                        }
                        Err(e) => {
                            // Same posture as rio-common::spawn_axum: log and
                            // end the task; kubelet liveness fails → restart.
                            tracing::error!(error = %e, addr = %health_addr, "guard: health bind failed");
                        }
                    }
                });
                // drain-census: runtime-lifetime — pure measurement
                // loop, no post-cancellation epilogue; dies with the
                // runtime BY DESIGN.
                tokio::spawn(sentinel(
                    main,
                    h.main_skew_us.clone(),
                    h.guard_skew_us.clone(),
                    h.main_stalls.clone(),
                    h.guard_stalls.clone(),
                    cfg.probe_interval,
                    cfg.stall_threshold,
                ));
                thread_shutdown.cancelled().await;
                // sys.epilogue.drain — the root's shutdown path, the
                // protocol's drain state: this runtime HOSTS epilogue
                // work (the lease loop's graceful-release PATCH), so
                // its lifetime must not be the bare cancellation token
                // its tasks key on. On cancellation the root drains
                // every adopted epilogue BOUNDED (each handle carries
                // its own budget), THEN returns — the runtime (and its
                // in-flight epilogues) drops only after. bug_118: the
                // discarded-handle form aborted the step_down PATCH
                // and left the dead pod holding the lease the full
                // ~19s STEAL_AFTER.
                epilogue_rx.close();
                while let Some(epilogue) = epilogue_rx.recv().await {
                    epilogue.drain().await;
                }
            });
        })
        .expect("spawning the rio-guard thread cannot fail at boot");
    (
        handle,
        GuardJoin {
            thread: Some(thread),
        },
    )
}

/// Readiness: 503 until the dependency flag flips, then 200 iff the
/// WORKING domain schedules a no-op probe within the budget. A
/// browned-out main runtime sheds (Endpoints removal) — never a kill
/// verdict (D3).
async fn readyz(State(s): State<ReadyState>) -> (StatusCode, &'static str) {
    if !s.ready.load(Ordering::Relaxed) {
        return (StatusCode::SERVICE_UNAVAILABLE, "starting");
    }
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    s.main.spawn(async move {
        let _ = tx.send(());
    });
    // timeout-census: refusal — readiness sheds 503 (Endpoints removal);
    // nothing committed. census[gen: rio-controller/tests/timeout_census.txt]
    match tokio::time::timeout(s.budget, rx).await {
        Ok(Ok(())) => (StatusCode::OK, "ok"),
        // Elapsed, or the probe task was dropped (runtime shutting
        // down) — either way the working domain is not serving.
        _ => (StatusCode::SERVICE_UNAVAILABLE, "main runtime unresponsive"),
    }
}

/// The D5 sentinel: one loop on the guard runtime measuring both
/// domains' executor-scheduling delay.
///
/// - GUARD self-skew: timer overshoot of its own tick (a late wake on
///   a near-idle `current_thread` runtime means the GUARD is starved
///   — the self-check face).
/// - MAIN skew: a no-op probe task is spawned onto the main runtime;
///   its time-to-first-poll IS the scheduling delay. At most ONE probe
///   is outstanding (no pile-up on a stalled runtime); while it is
///   unanswered the exported skew is the running lower bound
///   `now - probe_sent`, so the gauge tracks a live stall in real
///   time.
///
/// Threshold crossing is edge-triggered in BOTH domains through one
/// [`EdgeLatch`] instance each: one `..._stalls_total` increment per
/// stall episode, re-armed at resolution. A main-domain episode that
/// starts and resolves between ticks still counts once at its settle;
/// the thread-table capture fires only on LIVE rising edges (a settled
/// episode has nothing live left to attribute). When the main runtime
/// closes, the probe lane retires (typed [`ProbeRespawn`]) and the
/// guard self-skew keeps serving.
async fn sentinel(
    main: tokio::runtime::Handle,
    main_skew_us: Arc<AtomicU64>,
    guard_skew_us: Arc<AtomicU64>,
    main_stalls: Arc<AtomicU64>,
    guard_stalls: Arc<AtomicU64>,
    interval: Duration,
    threshold: Duration,
) {
    let mut watch = MainDomainWatch::new(main_stalls, main_skew_us, threshold);
    let mut guard_latch = EdgeLatch::new("guard", guard_stalls);
    loop {
        let expected = Instant::now() + interval;
        tokio::time::sleep(interval).await;
        // Guard self-skew: how late did our own timer fire?
        let guard_over = Instant::now().saturating_duration_since(expected);
        guard_skew_us.store(
            guard_over.as_micros().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
        metrics::gauge!("rio_controller_runtime_skew_seconds", "domain" => "guard")
            .set(guard_over.as_secs_f64());
        // The SAME edge semantics as the main domain: one latch type,
        // the increment is the rising edge (merged_bug_003 — this arm
        // used to count every late tick).
        if guard_latch.observe(guard_over >= threshold) {
            warn!(
                skew_secs = guard_over.as_secs_f64(),
                "guard-domain timer overshoot past threshold (guard itself starved \
                 — cgroup-class pressure?)"
            );
        }

        // Main-domain skew: observe the outstanding probe, then
        // launch per the typed respawn policy.
        match watch.on_tick() {
            MainTick::Quiet => {}
            MainTick::Stall { bound } => warn!(
                skew_secs = bound.as_secs_f64(),
                threads = %capture_thread_table(),
                "main-runtime scheduling delay past threshold; thread \
                 table captured (the 054 attribution record)"
            ),
            MainTick::Retired => info!(
                "main runtime closed; sentinel retires main-domain probing \
                 (guard self-skew keeps serving)"
            ),
        }
        if watch.wants_probe() {
            // The closure body runs at first poll: `sent.elapsed()` AT
            // THAT INSTANT is the scheduling delay.
            let (tx, rx) = tokio::sync::oneshot::channel();
            let sent = Instant::now();
            main.spawn(async move {
                let _ = tx.send(sent.elapsed());
            });
            watch.launched(sent, rx);
        }
    }
}

/// Edge-triggered stall accounting shared by BOTH skew domains. The
/// episode counter mirror and the
/// `rio_controller_runtime_skew_stalls_total` emission live INSIDE
/// [`Self::observe`], so a counting arm without the latch is
/// unrepresentable — one type, one HELP, one semantics
/// (merged_bug_003).
struct EdgeLatch {
    domain: &'static str,
    in_episode: bool,
    episodes: Arc<AtomicU64>,
}

impl EdgeLatch {
    fn new(domain: &'static str, episodes: Arc<AtomicU64>) -> Self {
        Self {
            domain,
            in_episode: false,
            episodes,
        }
    }

    /// Feed one observation ("the domain is past threshold right
    /// now"). Returns true exactly on the rising edge; the increment
    /// IS the edge. Callers hang their edge actions (warn, thread
    /// table) on the return value, never on private bookkeeping.
    fn observe(&mut self, over_threshold: bool) -> bool {
        let edge = over_threshold && !self.in_episode;
        self.in_episode = over_threshold;
        if edge {
            self.episodes.fetch_add(1, Ordering::Relaxed);
            metrics::counter!(
                "rio_controller_runtime_skew_stalls_total",
                "domain" => self.domain
            )
            .increment(1);
        }
        edge
    }

    /// Settle at an episode's RESOLUTION: `was_over` says whether the
    /// resolved observation crossed the threshold. An episode that
    /// started and resolved between observations (no live edge ever
    /// seen) still counts exactly once; either way the latch re-arms
    /// (merged_bug_003: the old Ok-arm only re-armed, so a
    /// between-ticks episode was systematically uncounted whenever
    /// stall_threshold < probe_interval).
    fn settle(&mut self, was_over: bool) {
        if was_over && !self.in_episode {
            self.observe(true);
        }
        self.observe(false);
    }
}

/// Typed respawn policy for the main-domain probe lane
/// (merged_bug_003: the Closed arm used to fall through to the
/// unconditional loop-bottom respawn, probing a shut-down runtime
/// every other tick forever against its own "stop probing" comment).
#[derive(Debug)]
enum ProbeRespawn {
    Continue,
    Retired,
}

/// What a main-domain tick observed. The caller's edge actions hang
/// off this; the watch owns ALL counting via its [`EdgeLatch`].
enum MainTick {
    /// Nothing actionable.
    Quiet,
    /// LIVE rising edge: the outstanding probe's running lower bound
    /// crossed the threshold this tick — capture and warn now, while
    /// there is something live to attribute.
    Stall { bound: Duration },
    /// The main runtime shut down (probe sender dropped): bookkeeping
    /// settled, probe lane retired. Returned exactly once.
    Retired,
}

/// The main-domain half of the sentinel, factored so its tick logic
/// is unit-testable with scripted probes (the settle and retirement
/// faces are deterministic here; the integration tests keep the
/// wiring face).
struct MainDomainWatch {
    latch: EdgeLatch,
    skew_us: Arc<AtomicU64>,
    threshold: Duration,
    outstanding: Option<(Instant, tokio::sync::oneshot::Receiver<Duration>)>,
    respawn: ProbeRespawn,
}

impl MainDomainWatch {
    fn new(episodes: Arc<AtomicU64>, skew_us: Arc<AtomicU64>, threshold: Duration) -> Self {
        Self {
            latch: EdgeLatch::new("main", episodes),
            skew_us,
            threshold,
            outstanding: None,
            respawn: ProbeRespawn::Continue,
        }
    }

    fn export(&self, skew: Duration) {
        self.skew_us.store(
            skew.as_micros().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
        metrics::gauge!("rio_controller_runtime_skew_seconds", "domain" => "main")
            .set(skew.as_secs_f64());
    }

    /// Observe the outstanding probe (if any). Never spawns — the
    /// sentinel loop pairs this with [`Self::wants_probe`] /
    /// [`Self::launched`].
    fn on_tick(&mut self) -> MainTick {
        let Some((sent, mut rx)) = self.outstanding.take() else {
            return MainTick::Quiet;
        };
        match rx.try_recv() {
            Ok(delay) => {
                self.export(delay);
                self.latch.settle(delay >= self.threshold);
                MainTick::Quiet
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                // Still unanswered: export the running lower bound and
                // keep waiting — never stack a second probe.
                let bound = sent.elapsed();
                self.export(bound);
                let edge = self.latch.observe(bound >= self.threshold);
                self.outstanding = Some((sent, rx));
                if edge {
                    MainTick::Stall { bound }
                } else {
                    MainTick::Quiet
                }
            }
            // Sender dropped without sending: the main runtime is
            // shutting down — settle the bookkeeping the old bare
            // continue skipped, retire the probe lane (typed respawn
            // policy), keep serving guard self-skew.
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                self.latch.settle(false);
                self.respawn = ProbeRespawn::Retired;
                MainTick::Retired
            }
        }
    }

    /// Launch policy: at most one outstanding probe, none after
    /// retirement.
    fn wants_probe(&self) -> bool {
        self.outstanding.is_none() && matches!(self.respawn, ProbeRespawn::Continue)
    }

    fn launched(&mut self, sent: Instant, rx: tokio::sync::oneshot::Receiver<Duration>) {
        self.outstanding = Some((sent, rx));
    }
}

/// One line per thread of this process: `tid comm state`, from
/// `/proc/self/task` (Linux; the only supported deploy target). The
/// 054 attribution record: at capture time the table says WHICH
/// threads exist and whether they are running (R), sleeping (S), or
/// in uninterruptible I/O (D — the I-165 class).
pub(crate) fn capture_thread_table() -> String {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return "<thread table unavailable: /proc/self/task unreadable>".into();
    };
    let mut rows = Vec::new();
    for entry in tasks.flatten() {
        // tids are ASCII digits; a non-UTF-8 name is unreachable on
        // procfs — skip rather than lossy-replace (P0290).
        let Some(tid) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let stat = std::fs::read_to_string(entry.path().join("stat")).unwrap_or_default();
        // stat: `pid (comm) state ...` — comm may contain spaces; the
        // closing paren is the parse anchor.
        let (comm, state) = match (stat.find('('), stat.rfind(')')) {
            (Some(o), Some(c)) if c > o => {
                let comm = &stat[o + 1..c];
                let state = stat[c + 1..].split_whitespace().next().unwrap_or("?");
                (comm.to_owned(), state.to_owned())
            }
            _ => ("?".into(), "?".into()),
        };
        rows.push(format!("{tid} {comm} {state}"));
        if rows.len() >= 64 {
            rows.push("<truncated at 64 threads>".into());
            break;
        }
    }
    rows.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capture parses /proc and names this very thread group's
    /// threads (W9-AK's attribution half, unit face).
    #[test]
    fn thread_table_capture_is_nonempty_and_parses() {
        let table = capture_thread_table();
        assert!(
            !table.is_empty() && !table.starts_with("<thread table unavailable"),
            "capture failed: {table}"
        );
        // Every row is `tid comm state` — tid numeric, comm nonempty,
        // state exactly one char. Anchored from the RIGHT (bug_011):
        // comm may contain spaces (`prctl(PR_SET_NAME, "my thread")`),
        // so `splitn(3, ' ')` cannot isolate the state token; the
        // production parse already anchors on `rfind(')')` and the
        // smoke test mirrors that with `rsplit_once(' ')`.
        for row in table.split("; ").filter(|r| !r.starts_with('<')) {
            let (head, state) = row
                .rsplit_once(' ')
                .unwrap_or_else(|| panic!("state: {row}"));
            let (tid, comm) = head.split_once(' ').unwrap_or((head, ""));
            assert!(tid.chars().all(|c| c.is_ascii_digit()), "tid: {row}");
            assert!(!comm.is_empty(), "comm: {row}");
            assert_eq!(state.len(), 1, "state one char: {row}");
        }
    }

    /// W10-AT (primitive): ONE EdgeLatch semantics — observations
    /// within an episode count once at the rising edge; resolution
    /// re-arms.
    #[test]
    fn w10_at_edge_latch_counts_episodes_not_observations() {
        let episodes = Arc::new(AtomicU64::new(0));
        let mut latch = EdgeLatch::new("guard", episodes.clone());
        assert!(latch.observe(true), "first over-threshold tick is the edge");
        for _ in 0..4 {
            assert!(!latch.observe(true), "same episode never re-counts");
        }
        assert_eq!(episodes.load(Ordering::Relaxed), 1);
        latch.observe(false);
        assert!(
            latch.observe(true),
            "a new episode after resolution is a new edge"
        );
        assert_eq!(episodes.load(Ordering::Relaxed), 2);
    }

    /// W10-AT (settle face): an episode that starts AND resolves
    /// between ticks — the probe answered late before any live
    /// observation — still counts exactly once (the Ok-settle arm used
    /// to only re-arm; systematic undercount when stall_threshold <
    /// probe_interval).
    #[test]
    fn w10_at_settled_between_ticks_episode_counts_once() {
        let episodes = Arc::new(AtomicU64::new(0));
        let mut watch = MainDomainWatch::new(
            episodes.clone(),
            Arc::new(AtomicU64::new(0)),
            Duration::from_millis(50),
        );
        let (tx, rx) = tokio::sync::oneshot::channel();
        watch.launched(Instant::now(), rx);
        tx.send(Duration::from_millis(300)).expect("receiver alive");
        assert!(matches!(watch.on_tick(), MainTick::Quiet));
        assert_eq!(
            episodes.load(Ordering::Relaxed),
            1,
            "a between-ticks episode must count at its settle"
        );
        // And the latch re-armed: a following LIVE episode is a new edge.
        let (_keep, rx2) = tokio::sync::oneshot::channel();
        watch.launched(
            Instant::now()
                .checked_sub(Duration::from_millis(200))
                .expect("backdate"),
            rx2,
        );
        assert!(matches!(watch.on_tick(), MainTick::Stall { .. }));
        assert_eq!(episodes.load(Ordering::Relaxed), 2);
    }

    /// W10-AT (live face): a live episode spanning many observations
    /// counts once at its rising edge, and the capture action
    /// (`MainTick::Stall`) fires exactly there.
    #[test]
    fn w10_at_live_episode_counts_once_and_captures_at_edge() {
        let episodes = Arc::new(AtomicU64::new(0));
        let mut watch = MainDomainWatch::new(
            episodes.clone(),
            Arc::new(AtomicU64::new(0)),
            Duration::from_millis(50),
        );
        let (tx, rx) = tokio::sync::oneshot::channel();
        watch.launched(
            Instant::now()
                .checked_sub(Duration::from_millis(200))
                .expect("backdate"),
            rx,
        );
        assert!(
            matches!(watch.on_tick(), MainTick::Stall { .. }),
            "the rising edge carries the capture action"
        );
        assert!(
            matches!(watch.on_tick(), MainTick::Quiet),
            "the same live episode never re-captures or re-counts"
        );
        assert_eq!(episodes.load(Ordering::Relaxed), 1);
        // The probe finally answers late: same episode, still one count.
        tx.send(Duration::from_millis(400)).expect("receiver alive");
        assert!(matches!(watch.on_tick(), MainTick::Quiet));
        assert_eq!(
            episodes.load(Ordering::Relaxed),
            1,
            "settling the episode a live edge already counted must not double-count"
        );
        // Re-armed after resolution: the next late settle is a new episode.
        let (tx3, rx3) = tokio::sync::oneshot::channel();
        watch.launched(Instant::now(), rx3);
        tx3.send(Duration::from_millis(300))
            .expect("receiver alive");
        assert!(matches!(watch.on_tick(), MainTick::Quiet));
        assert_eq!(episodes.load(Ordering::Relaxed), 2);
    }

    /// W10-AT (retirement face): the probe sender dropping — the main
    /// runtime shut down — retires the lane: typed, reported once, no
    /// respawn (the bare-continue form probed a dead runtime every
    /// other tick forever).
    #[test]
    fn w10_at_closed_probe_retires_lane() {
        let episodes = Arc::new(AtomicU64::new(0));
        let mut watch = MainDomainWatch::new(
            episodes.clone(),
            Arc::new(AtomicU64::new(0)),
            Duration::from_millis(50),
        );
        let (tx, rx) = tokio::sync::oneshot::channel::<Duration>();
        drop(tx);
        watch.launched(Instant::now(), rx);
        assert!(
            matches!(watch.on_tick(), MainTick::Retired),
            "sender-dropped must retire the probe lane (typed)"
        );
        assert!(
            !watch.wants_probe(),
            "a retired lane never respawns against the dead runtime"
        );
        assert!(
            matches!(watch.on_tick(), MainTick::Quiet),
            "retirement reports exactly once"
        );
    }
}
