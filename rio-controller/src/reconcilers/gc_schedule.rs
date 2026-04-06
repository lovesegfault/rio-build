//! GC cron reconciler. Calls StoreAdminService.TriggerGC on an
//! interval (default 24h, `gc_interval_hours` config; 0 = disabled).
//!
//! Unlike the WorkerPool reconciler (event-driven, watches CRDs),
//! this is a plain interval loop — no K8s resource to watch. More
//! like [`crate::scaling::Autoscaler::run`] than like a kube-rs
//! `Controller::run`.
//!
//! # Connect-per-tick (R-CONN constraint)
//!
//! tonic has NO default connect timeout. A held channel whose
//! backing pod died (stale endpoint IP) hangs on the NEXT RPC's
//! lazy-reconnect SYN forever. `connect_store_admin` sets a 10s
//! `connect_timeout` on the Channel builder — but that only guards
//! the INITIAL eager connect, not reconnects.
//!
//! So: connect FRESH every tick, inside `tokio::time::timeout(30s,
//! ...)`. On timeout or Err → `warn!` + increment
//! `rio_controller_gc_runs_total{result="connect_failure"}` +
//! `continue`. NEVER `?`-propagate out of the loop — one bad tick
//! doesn't kill the cron.
//!
//! 30s is generous (cross-AZ connect is ~100ms). It's a safety
//! backstop for the "stale IP" case, not a tuned latency bound.
//!
//! # Leader gating
//!
//! Per controller.md, rio-controller is single-replica by design
//! (no leader election). `replicas > 1` is an operator misconfig.
//! No `is_leader` gate here — there's no leader to check. If
//! someone DOES run two replicas, both will trigger GC; the
//! store's `GC_LOCK_ID` advisory lock serializes them (second
//! caller gets "already running"), so the blast radius is wasted
//! RPCs, not double-sweep.

// r[impl ctrl.gc.cron-schedule]

use std::future::Future;
use std::time::Duration;

use tracing::{info, warn};

use rio_proto::types::{GcProgress, GcRequest};

/// Connect timeout wrapping [`rio_proto::client::connect_store_admin`].
/// See module doc for why this is separate from the channel builder's
/// own `connect_timeout`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// One tick's outcome. Maps 1:1 to the `result` label on
/// `rio_controller_gc_runs_total`. Three-way split so a dashboard
/// can distinguish "store unreachable" (infra) from "store reached
/// but RPC failed" (store bug / bad request).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TickResult {
    /// Connected, TriggerGC accepted, progress stream drained to
    /// `is_complete=true`. Includes the store's "already running"
    /// response — from the cron's POV that's success (GC is
    /// happening, just not via us).
    Success,
    /// Connect timed out or errored. Store unreachable (pod down,
    /// Service has no endpoints, stale IP hangs SYN).
    ConnectFailure,
    /// Connected, but TriggerGC returned an error status OR the
    /// progress stream terminated with `Err(Status)`. Store-side
    /// problem: gate refusal, mark/sweep SQL failure, etc.
    RpcFailure,
}

impl TickResult {
    fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::ConnectFailure => "connect_failure",
            Self::RpcFailure => "rpc_failure",
        }
    }
}

/// Main loop. `main.rs` spawns this via `spawn_monitored("gc-cron", ...)`
/// when `gc_interval_hours > 0`. Returns on shutdown cancellation.
///
/// `store_addr` is the same address as `Config::store_addr` (shared
/// gRPC port — StoreAdminService is hosted alongside StoreService/
/// ChunkService).
pub async fn run(store_addr: String, tick: Duration, shutdown: rio_common::signal::Token) {
    info!(store_addr = %store_addr, tick_secs = tick.as_secs(), "GC cron starting");
    run_loop(tick, shutdown, move || {
        let addr = store_addr.clone();
        async move { tick_once(&addr).await }
    })
    .await;
    info!("GC cron stopped");
}

/// The loop shell, generic over the tick body so tests can inject
/// a counting mock. `F` is called once per interval; its result
/// feeds the `rio_controller_gc_runs_total{result=...}` counter.
///
/// `interval()` fires IMMEDIATELY on first poll, so the first GC
/// runs at t≈0 (shortly after startup), then every `tick` after.
/// That's the desired behavior: a controller restart shouldn't
/// delay GC by 24h.
///
/// `MissedTickBehavior::Skip`: a tick body that takes >24h (store
/// hung, stream drains slowly) doesn't fire twice immediately
/// after. Catch up on the next normal interval boundary.
pub(crate) async fn run_loop<F, Fut>(
    tick: Duration,
    shutdown: rio_common::signal::Token,
    mut tick_fn: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = TickResult>,
{
    let mut interval = tokio::time::interval(tick);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = interval.tick() => {}
        }
        let result = tick_fn().await;
        metrics::counter!("rio_controller_gc_runs_total", "result" => result.label()).increment(1);
    }
}

/// One tick: connect (with timeout), TriggerGC, drain the progress
/// stream. Never panics; every failure path maps to a
/// [`TickResult`] variant.
///
/// The GcRequest is intentionally minimal: `dry_run=false`,
/// `force=false`, `grace_period_hours=None` (store's 2h default),
/// `extra_roots=vec![]`. The cron has no visibility into live-
/// build output paths (that's the scheduler's `GcRoots` actor,
/// and it proxies via `AdminService.TriggerGC`, not us). For
/// background GC, the default roots (gc_roots table + uploading
/// manifests + grace window + scheduler_live_pins + tenant
/// retention) are sufficient.
async fn tick_once(store_addr: &str) -> TickResult {
    // Connect with timeout. The inner connect_store_admin has a
    // 10s connect_timeout on the Channel builder, but wrap again:
    // belt-and-suspenders against any code path that bypasses
    // the builder timeout (e.g. DNS resolution stalls before the
    // socket connect even starts).
    let connect = rio_proto::client::connect_store_admin(store_addr);
    let mut client = match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            warn!(error = %e, addr = %store_addr, "GC cron: store-admin connect failed");
            return TickResult::ConnectFailure;
        }
        Err(_elapsed) => {
            warn!(addr = %store_addr, timeout_secs = CONNECT_TIMEOUT.as_secs(),
                  "GC cron: store-admin connect timed out");
            return TickResult::ConnectFailure;
        }
    };

    let req = GcRequest {
        dry_run: false,
        grace_period_hours: None, // store default (2h)
        extra_roots: Vec::new(),
        force: false,
    };
    let mut stream = match client.trigger_gc(req).await {
        Ok(resp) => resp.into_inner(),
        Err(e) => {
            warn!(error = %e, "GC cron: TriggerGC rpc failed");
            return TickResult::RpcFailure;
        }
    };

    // Drain the progress stream. The store sends one message after
    // mark, one after sweep (is_complete=true). An in-stream
    // Err(Status) means the store's run_gc returned an error
    // (empty-refs gate refusal, mark/sweep SQL failure). Log each
    // progress msg at info — 24h GC is rare enough that per-phase
    // visibility is worth the log volume.
    loop {
        match stream.message().await {
            Ok(Some(GcProgress {
                paths_scanned,
                paths_collected,
                bytes_freed,
                is_complete,
                current_path,
            })) => {
                info!(
                    paths_scanned,
                    paths_collected,
                    bytes_freed,
                    is_complete,
                    msg = %current_path,
                    "GC cron: progress"
                );
                if is_complete {
                    return TickResult::Success;
                }
            }
            Ok(None) => {
                // Stream closed without is_complete=true. The
                // store's tokio::spawn ended (panic? cancellation?)
                // before sending the final message. Treat as RPC
                // failure — the GC may or may not have committed.
                warn!("GC cron: progress stream ended without is_complete");
                return TickResult::RpcFailure;
            }
            Err(e) => {
                warn!(error = %e, "GC cron: progress stream error");
                return TickResult::RpcFailure;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rio_test_support::metrics::CountingRecorder;

    /// Drive the loop's next tick. `start_paused` means all futures
    /// auto-advance when idle, but the SPAWNED loop task won't
    /// observe `advance()` until we yield back to it. A handful of
    /// yields is enough — the loop body is yield-select-yield-
    /// counter-yield (three await points at most per tick).
    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    // r[verify ctrl.gc.cron-schedule]
    /// Exactly one tick-fn invocation per interval. `interval()`
    /// fires immediately on first poll (t=0), then at t=24h, 48h.
    /// Proves the loop doesn't double-fire or skip.
    #[tokio::test(start_paused = true)]
    async fn one_trigger_per_24h_tick() {
        let recorder = CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_c = calls.clone();
        let shutdown = rio_common::signal::Token::new();
        let sd = shutdown.clone();

        let h = tokio::spawn(async move {
            run_loop(Duration::from_secs(24 * 3600), sd, move || {
                calls_c.fetch_add(1, Ordering::SeqCst);
                async { TickResult::Success }
            })
            .await;
        });

        // t≈0: interval fires immediately on first poll.
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "first tick at t≈0");
        assert_eq!(
            recorder.get("rio_controller_gc_runs_total{result=success}"),
            1,
            "success counter = 1 after first tick; keys={:?}",
            recorder.all_keys()
        );

        // Advance to t=24h. Second tick.
        tokio::time::advance(Duration::from_secs(24 * 3600)).await;
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2, "second tick at t=24h");

        // Advance to t=48h. Third tick.
        tokio::time::advance(Duration::from_secs(24 * 3600)).await;
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 3, "third tick at t=48h");
        assert_eq!(
            recorder.get("rio_controller_gc_runs_total{result=success}"),
            3
        );

        // Advance by LESS than the interval. NO new tick.
        tokio::time::advance(Duration::from_secs(12 * 3600)).await;
        settle().await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "no tick at t=60h (halfway through interval)"
        );

        shutdown.cancel();
        h.await.unwrap();
    }

    // r[verify ctrl.gc.cron-schedule]
    /// Connect-fail path: tick returns ConnectFailure → counter
    /// incremented with `result="connect_failure"` → loop CONTINUES
    /// → next tick fires normally. Proves the loop never `?`-propagates
    /// out on a bad tick.
    #[tokio::test(start_paused = true)]
    async fn connect_failure_increments_counter_and_continues() {
        let recorder = CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        // Scripted: tick 0 = connect_failure, ticks 1+ = success.
        // If the loop died on tick 0, tick 1 would never fire.
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_c = calls.clone();
        let shutdown = rio_common::signal::Token::new();
        let sd = shutdown.clone();

        let h = tokio::spawn(async move {
            run_loop(Duration::from_secs(24 * 3600), sd, move || {
                let n = calls_c.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n == 0 {
                        TickResult::ConnectFailure
                    } else {
                        TickResult::Success
                    }
                }
            })
            .await;
        });

        // t≈0: first tick → ConnectFailure.
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "first tick fired");
        assert_eq!(
            recorder.get("rio_controller_gc_runs_total{result=connect_failure}"),
            1,
            "connect_failure counter = 1; keys={:?}",
            recorder.all_keys()
        );
        assert_eq!(
            recorder.get("rio_controller_gc_runs_total{result=success}"),
            0,
            "success not yet incremented"
        );

        // t=24h: loop MUST still be alive → second tick fires.
        tokio::time::advance(Duration::from_secs(24 * 3600)).await;
        settle().await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "loop survived connect_failure; second tick fired"
        );
        assert_eq!(
            recorder.get("rio_controller_gc_runs_total{result=success}"),
            1,
            "second tick → success counter = 1"
        );
        // connect_failure counter unchanged (only tick 0 failed).
        assert_eq!(
            recorder.get("rio_controller_gc_runs_total{result=connect_failure}"),
            1
        );

        shutdown.cancel();
        h.await.unwrap();
    }

    /// Shutdown token cancels the loop promptly — `biased;` in
    /// the select means cancellation is checked before the tick
    /// arm, so cancelling at a tick boundary doesn't fire one
    /// more tick.
    #[tokio::test(start_paused = true)]
    async fn shutdown_cancels_promptly() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_c = calls.clone();
        let shutdown = rio_common::signal::Token::new();
        let sd = shutdown.clone();

        let h = tokio::spawn(async move {
            run_loop(Duration::from_secs(24 * 3600), sd, move || {
                calls_c.fetch_add(1, Ordering::SeqCst);
                async { TickResult::Success }
            })
            .await;
        });

        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "t=0 tick");

        // Cancel, then advance past the next tick boundary. The
        // loop should exit BEFORE observing the t=24h tick.
        shutdown.cancel();
        tokio::time::advance(Duration::from_secs(24 * 3600)).await;
        settle().await;

        h.await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "no post-cancel tick (biased select)"
        );
    }
}
