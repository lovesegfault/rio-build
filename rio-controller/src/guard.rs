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
//!
//! The lease loop's kube client is NOT shared: `run_lease_loop`
//! constructs its own client, so apiserver I/O for renewal never rides
//! a main-domain connection pool.
//!
//! Residual (priced, signed via the dossier): a dedicated thread still
//! dies with the process (kubelet covers that) and still lags under
//! cgroup-level CPU starvation — if a future freeze is cgroup-class,
//! the sentinel's capture will say so and a CPU-request raise joins
//! the fix.

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
/// field is an `Arc<Atomic*>` or a runtime handle (see the module-doc
/// census).
#[derive(Clone)]
pub struct GuardHandle {
    main_skew_us: Arc<AtomicU64>,
    guard_skew_us: Arc<AtomicU64>,
    main_stalls: Arc<AtomicU64>,
    guard_rt: tokio::runtime::Handle,
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

    /// Host the lease renew loop on the guard domain. THE point of
    /// the split for the lease: renewal keeps its 5s cadence while
    /// the main domain is stalled, so the fence-check premise
    /// (`rio-lease/src/lib.rs` — fence-check latency ≤ one tick)
    /// survives admitted-load starvation instead of being violated
    /// 2.5–3× (the 054 measurement). `run_lease_loop` builds its own
    /// kube client on THIS runtime — no main-domain pool sharing.
    pub fn spawn_lease<H: rio_lease::LeaseHooks>(
        &self,
        cfg: rio_lease::LeaseConfig,
        state: rio_lease::LeaderState,
        hooks: H,
        shutdown: rio_common::signal::Token,
    ) {
        self.guard_rt
            .spawn(rio_lease::run_lease_loop(cfg, state, hooks, shutdown));
    }

    /// Test/diagnostic seam: spawn an arbitrary task onto the guard
    /// runtime. Production wiring uses the named methods above so the
    /// guard's task census stays readable in `main.rs`.
    pub fn spawn_on_guard<F>(&self, fut: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
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

/// Spawn the guard domain: one OS thread (`rio-guard`) driving a
/// `current_thread` runtime that hosts the health server and the skew
/// sentinel. The lease loop joins via [`GuardHandle::spawn_lease`].
///
/// `main` is the WORKING domain's handle (probe target); `ready` is
/// the readiness flag main.rs flips after `connect_forever`.
// r[impl sys.guard.domain-isolation]
pub fn spawn(
    main: tokio::runtime::Handle,
    ready: Arc<AtomicBool>,
    cfg: GuardConfig,
    shutdown: rio_common::signal::Token,
) -> GuardHandle {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .thread_name("rio-guard")
        .build()
        .expect("guard runtime build cannot fail with enable_all on a fresh builder");
    let handle = GuardHandle {
        main_skew_us: Arc::new(AtomicU64::new(0)),
        guard_skew_us: Arc::new(AtomicU64::new(0)),
        main_stalls: Arc::new(AtomicU64::new(0)),
        guard_rt: rt.handle().clone(),
    };
    let h = handle.clone();
    let thread_shutdown = shutdown.clone();
    std::thread::Builder::new()
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
                tokio::spawn(sentinel(
                    main,
                    h.main_skew_us.clone(),
                    h.guard_skew_us.clone(),
                    h.main_stalls.clone(),
                    cfg.probe_interval,
                    cfg.stall_threshold,
                ));
                thread_shutdown.cancelled().await;
            });
        })
        .expect("spawning the rio-guard thread cannot fail at boot");
    handle
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
/// Threshold crossing is edge-triggered: one thread-table capture +
/// one `..._stalls_total` increment per stall episode, re-armed when a
/// probe answers under the threshold.
async fn sentinel(
    main: tokio::runtime::Handle,
    main_skew_us: Arc<AtomicU64>,
    guard_skew_us: Arc<AtomicU64>,
    main_stalls: Arc<AtomicU64>,
    interval: Duration,
    threshold: Duration,
) {
    let mut outstanding: Option<(Instant, tokio::sync::oneshot::Receiver<Duration>)> = None;
    let mut stalled = false;
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
        if guard_over >= threshold {
            metrics::counter!("rio_controller_runtime_skew_stalls_total", "domain" => "guard")
                .increment(1);
            warn!(
                skew_secs = guard_over.as_secs_f64(),
                "guard-domain timer overshoot past threshold (guard itself starved \
                 — cgroup-class pressure?)"
            );
        }

        // Main-domain skew: settle or extend the outstanding probe.
        let observed = match outstanding.take() {
            None => None,
            Some((sent, mut rx)) => match rx.try_recv() {
                Ok(delay) => Some(delay),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    // Still unanswered: export the running lower bound
                    // and keep waiting — never stack a second probe.
                    let bound = sent.elapsed();
                    main_skew_us.store(
                        bound.as_micros().min(u128::from(u64::MAX)) as u64,
                        Ordering::Relaxed,
                    );
                    metrics::gauge!("rio_controller_runtime_skew_seconds", "domain" => "main")
                        .set(bound.as_secs_f64());
                    if bound >= threshold && !stalled {
                        stalled = true;
                        main_stalls.fetch_add(1, Ordering::Relaxed);
                        metrics::counter!(
                            "rio_controller_runtime_skew_stalls_total",
                            "domain" => "main"
                        )
                        .increment(1);
                        warn!(
                            skew_secs = bound.as_secs_f64(),
                            threads = %capture_thread_table(),
                            "main-runtime scheduling delay past threshold; thread \
                             table captured (the 054 attribution record)"
                        );
                    }
                    outstanding = Some((sent, rx));
                    continue;
                }
                // Sender dropped without sending: the main runtime is
                // shutting down — stop probing, keep serving health.
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    continue;
                }
            },
        };
        if let Some(delay) = observed {
            main_skew_us.store(
                delay.as_micros().min(u128::from(u64::MAX)) as u64,
                Ordering::Relaxed,
            );
            metrics::gauge!("rio_controller_runtime_skew_seconds", "domain" => "main")
                .set(delay.as_secs_f64());
            if delay < threshold {
                stalled = false;
            }
        }
        // Launch the next probe. The closure body runs at first poll:
        // `sent.elapsed()` AT THAT INSTANT is the scheduling delay.
        let (tx, rx) = tokio::sync::oneshot::channel();
        let sent = Instant::now();
        main.spawn(async move {
            let _ = tx.send(sent.elapsed());
        });
        outstanding = Some((sent, rx));
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
        // Every row is `tid comm state` — tid numeric, state one char+.
        for row in table.split("; ").filter(|r| !r.starts_with('<')) {
            let mut parts = row.splitn(3, ' ');
            let tid = parts.next().expect("tid");
            assert!(tid.chars().all(|c| c.is_ascii_digit()), "tid: {row}");
            assert!(parts.next().is_some(), "comm: {row}");
            assert!(parts.next().is_some(), "state: {row}");
        }
    }
}
