//! W9-AJ / W9-AK: guard-domain isolation under a main-runtime stall
//! (the 054 incident shape, miniaturized) — `rio_controller::guard`.
//!
//! Red-first record: pre-fix, this file drove the SHIPPED topology
//! (`/healthz` served from the same runtime as the reconcilers) and
//! went red exactly as live_054 did — pre-stall 416µs, during-stall
//! the probe died on its read timeout. The transcript rides the
//! commit body; the topology under test here is the production
//! `guard::spawn` wiring from `main.rs`.
//!
//! Witness statements (R16):
//! - **W9-AJ** certifies D1 at the law's own quantifier — guard-domain
//!   responsiveness DURING component starvation: with every main
//!   worker pinned by blocking work (the production-shaped stall, not
//!   a paused clock), `/healthz` answers <100ms, a guard-hosted task
//!   at the lease cadence keeps ticking, the sentinel reports
//!   main-domain skew tracking the live stall while guard self-skew
//!   stays O(ms), and `/readyz` SHEDS (503) instead of wedging. The
//!   lease-renewal leg is certified by composition, disclosed: the
//!   renew loop is hosted on the guard (`main.rs` wiring +
//!   `GuardHandle::spawn_lease`), guard schedulability at the renew
//!   cadence is driven HERE, and loop behavior under a schedulable
//!   runtime is rio-lease's own cadence-test surface
//!   (`rio-lease/src/lib.rs` mod tests).
//! - **W9-AK** certifies the attribution tool: the stall counter and
//!   thread-table capture fire past threshold (edge-triggered), so
//!   the next freeze is attributable.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rio_controller::guard::{GuardConfig, spawn};

/// Plain-TCP HTTP/1.1 GET from the TEST thread (never a runtime under
/// test), so the probe cannot be descheduled by the stall it measures.
fn get_http(
    addr: std::net::SocketAddr,
    path: &str,
    budget: Duration,
) -> Result<(Duration, String), String> {
    let t0 = Instant::now();
    let mut s = TcpStream::connect_timeout(&addr, budget).map_err(|e| format!("connect: {e}"))?;
    s.set_read_timeout(Some(budget))
        .map_err(|e| e.to_string())?;
    s.write_all(format!("GET {path} HTTP/1.1\r\nhost: t\r\nconnection: close\r\n\r\n").as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut buf = String::new();
    s.read_to_string(&mut buf)
        .map_err(|e| format!("read: {e}"))?;
    Ok((t0.elapsed(), buf))
}

fn status_of(resp: &str) -> &str {
    resp.split_whitespace().nth(1).unwrap_or("?")
}

/// Build the production topology: a multi-thread "main" runtime (the
/// component domain) + the guard spawned exactly as `main.rs` does.
fn guard_on_stallable_main(
    cfg: GuardConfig,
    ready: Arc<AtomicBool>,
) -> (
    tokio::runtime::Runtime,
    rio_controller::guard::GuardHandle,
    std::net::SocketAddr,
    rio_common::signal::Token,
) {
    let main_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("main runtime");
    let shutdown = rio_common::signal::Token::new();
    let addr = cfg.health_addr;
    let guard = spawn(main_rt.handle().clone(), ready, cfg, shutdown.clone());
    // Wait for the guard's listener (bound on the guard thread).
    let t0 = Instant::now();
    loop {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "guard never bound {addr}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    (main_rt, guard, addr, shutdown)
}

fn free_local_addr() -> std::net::SocketAddr {
    // Bind-then-drop: the kernel-assigned port is free at guard bind
    // time with overwhelming likelihood (no parallel reuse in-process).
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("probe bind");
    l.local_addr().expect("addr")
}

/// Pin every main worker with blocking work for `len` (the
/// admitted-load compute/syscall shape).
fn stall_main(rt: &tokio::runtime::Runtime, workers: usize, len: Duration) {
    for _ in 0..workers {
        rt.spawn(async move {
            std::thread::sleep(len);
        });
    }
}

#[test]
// r[verify sys.guard.domain-isolation]
fn w9_aj_guard_serves_liveness_and_sheds_readiness_during_main_stall() {
    let ready = Arc::new(AtomicBool::new(false));
    let cfg = GuardConfig {
        health_addr: free_local_addr(),
        probe_interval: Duration::from_millis(50),
        stall_threshold: Duration::from_millis(400),
        ready_probe_budget: Duration::from_millis(300),
    };
    let (main_rt, guard, addr, _shutdown) = guard_on_stallable_main(cfg, ready.clone());

    // Pre-ready: /healthz 200, /readyz 503 (ready-gates-connect).
    let (_, h) = get_http(addr, "/healthz", Duration::from_secs(1)).expect("healthz pre-ready");
    assert_eq!(status_of(&h), "200", "{h}");
    let (_, r) = get_http(addr, "/readyz", Duration::from_secs(1)).expect("readyz pre-ready");
    assert_eq!(status_of(&r), "503", "{r}");

    // Ready + healthy main: both 200.
    ready.store(true, Ordering::Relaxed);
    let (_, r) = get_http(addr, "/readyz", Duration::from_secs(1)).expect("readyz healthy");
    assert_eq!(status_of(&r), "200", "{r}");

    // Guard-hosted canary at the 5s-lease cadence scaled down 50x:
    // ticks prove the guard schedules through the stall (the renew
    // loop is just another guard task — composition leg disclosed in
    // the module doc above).
    let ticks = Arc::new(AtomicU64::new(0));
    let t = ticks.clone();
    guard.spawn_on_guard(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            t.fetch_add(1, Ordering::Relaxed);
        }
    });

    // THE STALL: every main worker pinned for 3s.
    let stall = Duration::from_secs(3);
    stall_main(&main_rt, 2, stall);
    std::thread::sleep(Duration::from_millis(600));
    let ticks_at_stall_start = ticks.load(Ordering::Relaxed);

    // Liveness from the guard domain: <100ms DURING the stall.
    let (d, h) = get_http(addr, "/healthz", Duration::from_secs(1))
        .expect("healthz must answer during the stall (the 054 inverse)");
    assert_eq!(status_of(&h), "200", "{h}");
    assert!(
        d < Duration::from_millis(100),
        "healthz took {d:?} during stall"
    );

    // Readiness SHEDS: the working domain cannot schedule the probe.
    let (_, r) = get_http(addr, "/readyz", Duration::from_secs(2)).expect("readyz during stall");
    assert_eq!(status_of(&r), "503", "browned-out main must shed, got: {r}");

    // Sentinel: main skew tracks the live stall (running lower bound),
    // guard self-skew stays O(ms).
    std::thread::sleep(Duration::from_millis(700));
    let main_skew = guard.main_skew();
    let guard_skew = guard.guard_skew();
    assert!(
        main_skew >= Duration::from_millis(500),
        "main skew should track the live stall, got {main_skew:?}"
    );
    assert!(
        guard_skew < Duration::from_millis(50),
        "guard self-skew should stay O(ms) during a MAIN stall, got {guard_skew:?}"
    );
    let canary_ticks = ticks.load(Ordering::Relaxed) - ticks_at_stall_start;
    assert!(
        canary_ticks >= 8,
        "guard canary must keep its cadence through the stall (got {canary_ticks} ticks in ~1.3s)"
    );

    // Heal edge (both faces of the readiness law): stall ends, probe
    // round-trips again, readiness returns.
    std::thread::sleep(stall);
    let t0 = Instant::now();
    loop {
        let (_, r) = get_http(addr, "/readyz", Duration::from_secs(1)).expect("readyz post-stall");
        if status_of(&r) == "200" {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "readiness never healed post-stall: {r}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // Main skew settles back under the threshold once probes answer.
    let t0 = Instant::now();
    loop {
        if guard.main_skew() < Duration::from_millis(400) {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "main skew never settled post-stall: {:?}",
            guard.main_skew()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
// r[verify sys.guard.skew-sentinel]
fn w9_ak_stall_counter_and_capture_fire_past_threshold() {
    let ready = Arc::new(AtomicBool::new(true));
    let cfg = GuardConfig {
        health_addr: free_local_addr(),
        probe_interval: Duration::from_millis(50),
        stall_threshold: Duration::from_millis(300),
        ready_probe_budget: Duration::from_millis(200),
    };
    let (main_rt, guard, _addr, _shutdown) = guard_on_stallable_main(cfg, ready);
    assert_eq!(
        guard.main_stalls(),
        0,
        "no stall episodes on a healthy runtime"
    );

    // One stall episode well past the 300ms threshold.
    stall_main(&main_rt, 2, Duration::from_secs(2));
    let t0 = Instant::now();
    while guard.main_stalls() == 0 {
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "stall counter never fired past threshold (main_skew={:?})",
            guard.main_skew()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let after_first = guard.main_stalls();
    assert_eq!(after_first, 1, "edge-triggered: one increment per episode");

    // Still ONE while the same episode continues (edge, not level).
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        guard.main_stalls(),
        1,
        "level-triggered re-counting is the anti-shape"
    );

    // Episode ends; a SECOND stall is a second edge.
    std::thread::sleep(Duration::from_secs(2));
    let t0 = Instant::now();
    while guard.main_skew() >= Duration::from_millis(300) {
        assert!(
            t0.elapsed() < Duration::from_secs(3),
            "first episode never settled"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    stall_main(&main_rt, 2, Duration::from_secs(1));
    let t0 = Instant::now();
    while guard.main_stalls() < 2 {
        assert!(
            t0.elapsed() < Duration::from_secs(3),
            "second episode never counted (got {})",
            guard.main_stalls()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// W10-AT (wiring face): one sustained guard-domain starvation episode
/// counts ONCE — the same edge semantics as the main domain (one
/// `EdgeLatch`, two instances; merged_bug_003's two-semantics
/// divergence — guard counted LATE TICKS — was the red). Structural:
/// the storm's end is joined, never timed.
#[test]
// r[verify sys.guard.skew-sentinel]
fn w10_at_guard_domain_counts_episodes_not_late_ticks() {
    let ready = Arc::new(AtomicBool::new(true));
    let cfg = GuardConfig {
        health_addr: free_local_addr(),
        probe_interval: Duration::from_millis(50),
        stall_threshold: Duration::from_millis(300),
        ready_probe_budget: Duration::from_millis(200),
    };
    let (_main_rt, guard, _addr, _shutdown) = guard_on_stallable_main(cfg, ready);
    assert_eq!(guard.guard_stalls(), 0, "no episodes on a healthy guard");

    // ONE sustained partial-starvation episode on the GUARD runtime:
    // blocking chunks with yields between. Every sentinel fire inside
    // the storm is ~550ms late (>= the 300ms threshold) — the timer
    // deadline always lands inside a 600ms block — so the storm is one
    // continuous episode with no under-threshold fire until it ends.
    let storm = guard.spawn_on_guard(async {
        for _ in 0..3 {
            std::thread::sleep(Duration::from_millis(600));
            tokio::task::yield_now().await;
        }
    });

    // The episode is counted (bounded event poll)...
    let t0 = Instant::now();
    while guard.guard_stalls() == 0 {
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "guard-domain episode never counted (guard_skew={:?})",
            guard.guard_skew()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    // ...and counted ONCE for the whole storm: join the storm's end
    // (event, not duration), let a few clean ticks settle the latch,
    // then pin the count.
    let t0 = Instant::now();
    while !storm.is_finished() {
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "storm never finished"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(
        guard.guard_stalls(),
        1,
        "one sustained starvation episode = one increment (per-late-tick \
         counting is the merged_bug_003 anti-shape)"
    );
}
