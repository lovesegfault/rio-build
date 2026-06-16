//! sh-002C: guard-domain isolation for the scheduler's lease loop —
//! the §lifecycle strike-3 close (bug_118 → bug_023 → sh-002C).
//!
//! Red-first record: at base this file drove the SHIPPED topology
//! (`main.rs:24,333` — `spawn_monitored("lease-loop", run_lease_loop)`
//! on the SAME `#[tokio::main]` runtime as the dag-actor) under a
//! single-thread runtime to deterministically reproduce. The 16.35s
//! `Tick` (sh-002 advisory Stage B) starved the renew tick →
//! `SELF_FENCE_AFTER=11s` tripped → `rio_scheduler_lease_lost_total`
//! incremented and the leader ran a phantom 1.84s recovery against a
//! lease the standby never stole. The transcript rides the commit
//! body; the topology under test in the GREEN form is the production
//! `guard::spawn` wiring from `main.rs`.
//!
//! The lease-renewal leg is certified by composition, disclosed: the
//! renew loop is hosted on the guard (`main.rs` wiring +
//! `GuardHandle::spawn_lease`), guard schedulability at the renew
//! cadence is driven HERE, and loop behavior under a schedulable
//! runtime is rio-lease's own cadence-test surface
//! (`rio-lease/src/lib.rs` mod tests).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use rio_scheduler::guard::{GuardConfig, spawn};

/// sh-002C red-first: a long actor turn (the 16.35s Tick) MUST NOT
/// starve the lease renew loop. Scaled 50× so the witness lands in
/// nextest's budget: `RENEW_INTERVAL` 5s → 100ms; `SELF_FENCE_AFTER`
/// 11s → 220ms; the 16.35s Tick → 2s.
///
/// RED at base (the SHIPPED topology below): the lease-cadence canary
/// shares the dag-actor's runtime and a single blocking turn starves
/// it for the whole stall — 0 ticks in 2s, ≥9× the scaled
/// `SELF_FENCE_AFTER`, so `maybe_self_fence` trips. GREEN after the
/// guard split: the canary lives on the dedicated `current_thread`
/// runtime and keeps its cadence through the stall (≥15 ticks).
#[test]
// r[verify sched.lease.guard-isolated]
// r[verify sys.guard.domain-isolation]
fn sh002c_long_actor_turn_does_not_self_fence() {
    // PRODUCTION topology (main.rs `guard::spawn` wiring): a
    // multi-thread main runtime (the dag-actor's domain) + the guard
    // spawned exactly as `main.rs` does. 2 workers so the stall is
    // deterministic (every worker pinned).
    let main_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("main runtime");
    let shutdown = rio_common::signal::Token::new();
    let (guard, guard_join) = spawn(
        main_rt.handle().clone(),
        Arc::new(AtomicBool::new(true)),
        GuardConfig {
            health_addr: ([127, 0, 0, 1], 0).into(),
            probe_interval: Duration::from_millis(50),
            stall_threshold: Duration::from_millis(400),
            ready_probe_budget: Duration::from_millis(300),
        },
        shutdown.clone(),
    );
    // The lease-cadence canary at the scaled `RENEW_INTERVAL`, hosted
    // on the GUARD runtime via the production seam — the production
    // lease loop (`rio_lease::run_lease_loop`) is exactly a task at
    // this shape: `interval.tick().await` → renew PATCH; one missed
    // tick past `SELF_FENCE_AFTER` is `maybe_self_fence`.
    let ticks = Arc::new(AtomicU64::new(0));
    let t = ticks.clone();
    guard.spawn_on_guard(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            t.fetch_add(1, Ordering::Relaxed);
        }
    });
    // The 16.35s Tick (sh-002 Stage B; `17-ready-cache-sweep` /
    // `complete_ready_from_store_batch`), scaled. Every main worker
    // pinned by blocking work — the admitted-load shape, not a paused
    // clock.
    for _ in 0..2 {
        main_rt.spawn(async {
            std::thread::sleep(Duration::from_secs(2));
        });
    }
    std::thread::sleep(Duration::from_secs(2));
    let after = ticks.load(Ordering::Relaxed);
    // 2s at a 100ms cadence = 20 ticks budget; the assertion floor is
    // 15 (25% slack for builder contention). RED at base: 0 (the
    // canary shared the dag-actor's runtime).
    assert!(
        after >= 15,
        "lease-cadence canary starved during a long actor turn: {after} ticks \
         in 2s (scaled SELF_FENCE_AFTER = 220ms — `maybe_self_fence` would \
         have tripped {n}× over). The lease renew loop MUST be hosted on a \
         runtime the dag-actor cannot starve.",
        n = 2000 / 220,
    );
    // D5 sentinel: main skew tracks the live stall (running lower
    // bound), guard self-skew stays O(ms) — the D1 witness.
    assert!(
        guard.main_skew() >= Duration::from_millis(500),
        "main skew should track the live stall, got {:?}",
        guard.main_skew()
    );
    assert!(
        guard.guard_skew() < Duration::from_millis(50),
        "guard self-skew should stay O(ms) during a MAIN stall, got {:?}",
        guard.guard_skew()
    );
    assert!(
        guard.main_stalls() >= 1,
        "the stall episode must be edge-counted (sh-002C attribution)"
    );
    shutdown.cancel();
    guard_join.join();
}

/// F2 (bug_023, the runtime enforcement face): a `GuardJoin` dropped
/// without `.join()` panics. `#[must_use]` cannot catch
/// `let (g, _) = …` (verified on stable; `_` suppresses the lint), so
/// the linear token's Drop is the runtime gate.
#[test]
#[should_panic(expected = "GuardJoin dropped without .join()")]
fn dropped_guard_join_panics() {
    let shutdown = rio_common::signal::Token::new();
    let main = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("main runtime");
    // The `_` suppression is THE evasion the Drop-panic exists to
    // close — driven against the real spawn.
    #[allow(clippy::no_effect_underscore_binding)]
    let (_g, _) = spawn(
        main.handle().clone(),
        Arc::new(AtomicBool::new(true)),
        GuardConfig {
            health_addr: ([127, 0, 0, 1], 0).into(),
            ..Default::default()
        },
        shutdown,
    );
    // `_` drops at end-of-statement (the line above), so the Drop-panic
    // already fired; the guard thread leaks for this nextest process's
    // lifetime — bounded.
}

/// r26 irony-check on bug_023 (the §one-step-removed inverse): a
/// `GuardJoin` live across an UNWINDING panic must yield to the
/// original failure. Without the `thread::panicking()` guard the Drop
/// fires a SECOND panic → panic-while-panicking → SIGABRT, burying
/// the real message.
#[test]
#[should_panic(expected = "the actual failure")]
fn guard_join_drop_during_unwind_yields_original_panic() {
    let shutdown = rio_common::signal::Token::new();
    let main = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("main runtime");
    let (_g, _gj) = spawn(
        main.handle().clone(),
        Arc::new(AtomicBool::new(true)),
        GuardConfig {
            health_addr: ([127, 0, 0, 1], 0).into(),
            ..Default::default()
        },
        shutdown.clone(),
    );
    shutdown.cancel();
    // _gj is live across this panic; its Drop sees thread::panicking()
    // and yields, so #[should_panic(expected=…)] matches.
    panic!("the actual failure");
}
