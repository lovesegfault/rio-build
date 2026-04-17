//! Per-build cgroup monitor tasks: CPU peak poller, OOM watcher, drain.
//!
//! Spawned alongside `run_daemon_build` and stopped/read after the build
//! completes. Separated from `mod.rs` so the cgroup-polling mechanics
//! (atomic-f64-max compare-exchange, kill+drain poll loop) live next to
//! each other instead of interleaved with daemon-lifecycle orchestration.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Read `cpu.stat` `usage_usec` from a cgroup path. Free fn (not a
/// method on BuildCgroup) so the CPU poll task can clone the PATH
/// and call this without holding a `&BuildCgroup` across the
/// `run_daemon_build` await.
///
/// Thin wrapper over the pure parser in cgroup.rs. `None` on read
/// fail (cgroup directory removed mid-poll — shouldn't happen, the
/// executor drops BuildCgroup AFTER the poll task is aborted).
fn read_cpu_stat(cgroup_path: &Path) -> Option<u64> {
    let content = std::fs::read_to_string(cgroup_path.join("cpu.stat")).ok()?;
    crate::cgroup::parse_cpu_stat_usage_usec(&content)
}

/// Handles to the per-build cgroup monitor tasks. `stop()` aborts both
/// and reads their accumulated state; `Drop` aborts as a safety net so
/// an early `?` in the caller doesn't leak 1Hz pollers.
pub(super) struct CgroupMonitors {
    cpu_poll: tokio::task::JoinHandle<()>,
    oom_watch: tokio::task::JoinHandle<()>,
    peak_cpu: Arc<AtomicU64>,
    oom_detected: Arc<AtomicBool>,
}

impl CgroupMonitors {
    /// Abort both monitor tasks and return `(peak_cpu_cores, oom_detected)`.
    /// `abort()` doesn't wait — both tasks are pure read, no cleanup needed.
    pub(super) fn stop(self) -> (f64, bool) {
        self.cpu_poll.abort();
        self.oom_watch.abort();
        (
            f64::from_bits(self.peak_cpu.load(Ordering::Acquire)),
            self.oom_detected.load(Ordering::SeqCst),
        )
    }
}

impl Drop for CgroupMonitors {
    fn drop(&mut self) {
        // Abort guard: if run_daemon_build panics (or any `?` between
        // spawn and the explicit `stop()` early-returns), the pollers
        // would leak as 1Hz tasks reading a dead cgroup path forever.
        // `.abort()` on a completed/already-aborted handle is a no-op,
        // so the explicit `stop()` above is harmless redundancy.
        self.cpu_poll.abort();
        self.oom_watch.abort();
    }
}

/// Spawn the per-build cgroup CPU poller and OOM watcher.
///
/// Both run concurrently with `run_daemon_build` (which awaits). The
/// returned [`CgroupMonitors`] aborts them on `Drop`; the caller should
/// call `.stop()` after the build completes to read peak CPU + OOM flag.
///
/// Clones the cgroup PATH (not the `BuildCgroup` — moving it would put
/// `Drop` in the task, which we don't want; `Drop` must run after
/// `daemon.wait()` in the caller).
pub(super) fn spawn_cgroup_monitors(
    build_cgroup: &crate::cgroup::BuildCgroup,
    cgroup_parent: &Path,
) -> CgroupMonitors {
    // CPU polling task: samples `cpu.stat usage_usec` every second,
    // computes instantaneous cores = `delta_usec/elapsed_usec`, tracks
    // max. The cgroup's `usage_usec` is tree-cumulative, so this captures
    // the builder's CPU too.
    //
    // Stores max as f64 bits in an `AtomicU64` — there's no `AtomicF64`.
    // compare_exchange loop for max (`fetch_max` on u64 bits would compare
    // BIT PATTERNS, not float values — `2.0_f64.to_bits() > 8.0_f64.to_bits()`
    // is NOT guaranteed). Standard f64-atomic pattern.
    let peak_cpu = Arc::new(AtomicU64::new(0));
    let cpu_poll_path = build_cgroup.path().to_path_buf();
    let cpu_poll_peak = Arc::clone(&peak_cpu);
    let cpu_poll = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        // First tick fires immediately — skip it, we want a 1s baseline.
        interval.tick().await;
        let mut prev_usec = read_cpu_stat(&cpu_poll_path);
        let mut prev_instant = std::time::Instant::now();
        loop {
            interval.tick().await;
            let now_usec = read_cpu_stat(&cpu_poll_path);
            let now_instant = std::time::Instant::now();
            // Both samples must be Some. If the first read failed
            // (cgroup not populated yet — daemon hasn't forked),
            // prev is None and we just advance. If THIS read fails
            // (cgroup removed? shouldn't happen until Drop), skip.
            if let (Some(prev), Some(now)) = (prev_usec, now_usec) {
                let delta_usec = now.saturating_sub(prev);
                let elapsed_usec = now_instant.duration_since(prev_instant).as_micros() as u64;
                // elapsed_usec is ~1_000_000 (1s interval) but
                // jitters. Guard /0 for the impossible case where
                // two ticks fire at the same instant.
                if elapsed_usec > 0 {
                    let cores = delta_usec as f64 / elapsed_usec as f64;
                    // Compare-exchange max: load, if cores > current,
                    // try to swap. Loop until success or current >= cores.
                    let mut current_bits = cpu_poll_peak.load(Ordering::Relaxed);
                    loop {
                        if f64::from_bits(current_bits) >= cores {
                            break; // already higher, done
                        }
                        match cpu_poll_peak.compare_exchange_weak(
                            current_bits,
                            cores.to_bits(),
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => break,                       // we set it
                            Err(actual) => current_bits = actual, // raced, retry
                        }
                    }
                }
            }
            prev_usec = now_usec;
            prev_instant = now_instant;
        }
    });

    // r[impl builder.oom.cgroup-watch]
    // OOM watcher (I-196 defense-in-depth). Polls the POD-level
    // `memory.events` (delegated root — where k8s set memory.max; the
    // per-build sub-cgroup has no limit of its own) for `oom_kill`
    // increments. When the kernel OOM-kills a build process, make
    // typically respawns it → loop that burns the silence timeout.
    // Detect the first kill, cgroup.kill the build to break the loop,
    // and flag it so the result becomes CgroupOom (→ Infrastructure-
    // Failure → scheduler bumps resource_floor) instead of a confusing
    // Wire(UnexpectedEof) or silence-timeout BuildFailed.
    //
    // Baseline captured at spawn: a prior build's OOM (or the FUSE
    // warm getting killed) shouldn't count. `None` baseline (file
    // unreadable — memory controller off, or non-k8s test env) → the
    // task idles harmlessly; the build_cores clamp is the primary fix
    // anyway.
    let oom_detected = Arc::new(AtomicBool::new(false));
    let oom_watch = {
        let parent = cgroup_parent.to_path_buf();
        let kill_path = build_cgroup.path().to_path_buf();
        let flag = Arc::clone(&oom_detected);
        let baseline = crate::cgroup::read_oom_kill(&parent);
        tokio::spawn(async move {
            let Some(baseline) = baseline else {
                return; // can't watch — no memory.events
            };
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.tick().await; // skip immediate fire
            loop {
                interval.tick().await;
                let Some(n) = crate::cgroup::read_oom_kill(&parent) else {
                    continue; // transient read fail; keep polling
                };
                if n > baseline {
                    tracing::warn!(
                        baseline,
                        current = n,
                        "cgroup oom_kill incremented during build; killing build cgroup"
                    );
                    flag.store(true, Ordering::SeqCst);
                    // Break the make-respawn loop. run_daemon_build
                    // sees daemon EOF; the caller's flag check converts
                    // that into CgroupOom.
                    let _ = std::fs::write(kill_path.join("cgroup.kill"), "1");
                    return;
                }
            }
        })
    };

    CgroupMonitors {
        cpu_poll,
        oom_watch,
        peak_cpu,
        oom_detected,
    }
}

/// Kill the per-build cgroup tree, wait for it to drain, then drop.
///
/// `daemon.kill()` in the caller SIGKILLs the nix-daemon process only.
/// The builder is a GRANDCHILD (forked by the daemon during
/// `wopBuildDerivation`) and is not in the daemon's process group — it
/// lives on in the cgroup. On the success path the builder has already
/// exited (build finished → daemon sent `STDERR_LAST`); on the timeout/
/// error path it's still running a `sleep 3600` or a stuck compiler.
///
/// `cgroup.kill` walks the tree: SIGKILLs everything, including sub-
/// cgroups the daemon may have created. Idempotent — writing "1" to an
/// empty cgroup is a no-op — so we call it unconditionally rather than
/// branching on `build_result.is_err()`.
// r[impl builder.cgroup.kill-on-teardown]
pub(super) async fn drain_build_cgroup(build_cgroup: crate::cgroup::BuildCgroup) {
    if let Err(e) = build_cgroup.kill() {
        // ENOENT shouldn't happen (we hold the BuildCgroup, Drop hasn't
        // run); EACCES would mean delegation is broken. Log and fall
        // through — rmdir will fail EBUSY and warn again, but we don't
        // want to upgrade a successful build to an error here.
        tracing::warn!(error = %e, "build_cgroup.kill() failed");
    }
    // cgroup.kill is async: write returns before procs are gone. Poll
    // cgroup.procs until empty or 2s elapsed (same budget as daemon.wait;
    // SIGKILL → exit is ~ms, 2s is vast headroom for a zombie-reparented
    // tree). Sync read on blocking pool — 200 iterations of a single-line
    // procfs read, negligible.
    let cgroup_path_for_poll = build_cgroup.path().to_path_buf();
    let drained = tokio::task::spawn_blocking(move || {
        for _ in 0..200 {
            match std::fs::read_to_string(cgroup_path_for_poll.join("cgroup.procs")) {
                Ok(s) if s.trim().is_empty() => return true,
                Ok(_) => std::thread::sleep(Duration::from_millis(10)),
                // ENOENT: cgroup already gone (shouldn't happen — we
                // hold the BuildCgroup — but treat as drained).
                Err(_) => return true,
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    if !drained {
        tracing::warn!(
            cgroup = %build_cgroup.path().display(),
            "cgroup still has processes 2s after cgroup.kill; rmdir will EBUSY"
        );
    }
    // build_cgroup drops here. rmdir succeeds if the drain above emptied
    // it; otherwise Drop warns EBUSY + leaks (cleared on pod restart).
}
