//! Per-build cgroup monitor tasks: CPU peak poller, OOM watcher, drain.
//!
//! Spawned alongside `run_daemon_build` and stopped/read after the build
//! completes. Separated from `mod.rs` so the cgroup-polling mechanics
//! (atomic-f64-max compare-exchange, kill+drain poll loop) live next to
//! each other instead of interleaved with daemon-lifecycle orchestration.

use std::path::{Path, PathBuf};
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

/// Handles to the per-build cgroup monitor tasks. `stop()` aborts them
/// and reads their accumulated state; `Drop` aborts as a safety net so
/// an early `?` in the caller doesn't leak 1Hz pollers.
pub(super) struct CgroupMonitors {
    cpu_poll: tokio::task::JoinHandle<()>,
    oom_watch: tokio::task::JoinHandle<()>,
    quota_poll: tokio::task::JoinHandle<()>,
    peak_cpu: Arc<AtomicU64>,
    oom_detected: Arc<AtomicBool>,
    /// Max-tracked `dqb_curspace` over the build (merged_bug_074: the
    /// DURING-BUILD peak — `keep-failed` is unset, so the daemon
    /// deletes a failed build's scratch before any post-build sample;
    /// `dqb_curspace` is current bytes, not a kernel HWM, hence the
    /// poll). 0 = never sampled (no prjquota / build exited <1s).
    peak_quota: Arc<AtomicU64>,
    /// Pod-level cgroup (where `memory.events` lives). Held so
    /// `stop()` can do a final synchronous `read_oom_kill`.
    parent: PathBuf,
    /// `oom_kill` count at spawn. `None` ⇒ memory controller off /
    /// non-k8s test env — no OOM watching possible.
    baseline: Option<u64>,
}

impl CgroupMonitors {
    /// Abort the monitor tasks and return
    /// `(peak_cpu_cores, oom_detected, peak_quota_bytes)`.
    /// `abort()` doesn't wait — the tasks are pure read, no cleanup
    /// needed.
    ///
    /// Performs one final synchronous `read_oom_kill` against the stored
    /// baseline: an OOM that lands in the <1s gap between the watcher's
    /// last tick and build-exit would otherwise be missed. Fast-exit
    /// toolchains (cargo / single `cc` / `python setup.py`) exit ~100ms
    /// after a child OOM, so the 1Hz watcher misses ~90% of those —
    /// `MiscFailure → PermanentFailure` poisons the drv instead of
    /// `CgroupOom → InfrastructureFailure → bump resource_floor`.
    /// `memory.events oom_kill` is cumulative and outlives the killed
    /// process; the build cgroup is destroyed *later* by
    /// `drain_build_cgroup`.
    ///
    /// Unlike the watcher tick, this final read does NOT write
    /// `cgroup.kill`, so `oom_detected` can be `true` while the daemon
    /// reported `Built` (script tolerated the killed child via
    /// `|| true` / `make -k` / retry-runner). The caller
    /// (`apply_oom_override`) gates the `CgroupOom` reclassification on
    /// `build_result.is_err()` for that reason.
    ///
    /// `peak_quota_bytes` is `None` when the poller never landed a
    /// sample (no prjquota on the node, or the build exited before
    /// the first 1s tick) — the caller falls back to its own one-shot.
    pub(super) fn stop(self) -> (f64, bool, Option<u64>) {
        self.cpu_poll.abort();
        self.oom_watch.abort();
        self.quota_poll.abort();
        let final_oom = self
            .baseline
            .is_some_and(|b| crate::cgroup::read_oom_kill(&self.parent).is_some_and(|n| n > b));
        let peak_quota = self.peak_quota.load(Ordering::Acquire);
        (
            f64::from_bits(self.peak_cpu.load(Ordering::Acquire)),
            self.oom_detected.load(Ordering::SeqCst) || final_oom,
            (peak_quota > 0).then_some(peak_quota),
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
        self.quota_poll.abort();
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
    overlay_base_dir: &Path,
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

    // r[impl builder.oom.cgroup-watch+3]
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
    let parent = cgroup_parent.to_path_buf();
    let baseline = crate::cgroup::read_oom_kill(&parent);
    let oom_watch = {
        let parent = parent.clone();
        let kill_path = build_cgroup.path().to_path_buf();
        let flag = Arc::clone(&oom_detected);
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

    // merged_bug_074: the per-build prjquota peak poller. Samples
    // `dqb_curspace` on the overlay base at 1Hz and max-tracks — the
    // causally relevant usage window for the disk-exhaustion
    // classification is DURING the build (the daemon deletes a failed
    // build's scratch before returning when keep-failed is unset, so
    // the post-daemon one-shot under-reads exactly the dominant
    // exhaustion shape). Same blocking-read posture as the CPU poller
    // (one ioctl + one syscall per tick — negligible); a node without
    // prjquota degrades to zero samples and the caller's one-shot.
    let peak_quota = Arc::new(AtomicU64::new(0));
    let quota_poll = {
        let dir = overlay_base_dir.to_path_buf();
        let peak = Arc::clone(&peak_quota);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.tick().await; // skip the immediate fire
            loop {
                interval.tick().await;
                if let Ok(Some(used)) = crate::quota::current_bytes(&dir) {
                    peak.fetch_max(used, Ordering::Relaxed);
                }
            }
        })
    };

    CgroupMonitors {
        cpu_poll,
        oom_watch,
        quota_poll,
        peak_cpu,
        oom_detected,
        peak_quota,
        parent,
        baseline,
    }
}

/// Budget for the post-kill `cgroup.procs` drain poll.
///
/// SIGKILL → exit is normally ~ms, but a kill-evading process can
/// linger far longer: uninterruptible D-state (stuck I/O, writeback
/// flush) or pre-submitted io_uring SQEs keep a task alive past
/// `cgroup.kill`. The old 2s budget made the quiesce best-effort
/// (warn + proceed); output collection now REFUSES to run when the
/// budget expires non-empty (`refuse_outputs_unless_quiesced` in
/// mod.rs), so the budget trades teardown latency in the pathological
/// case against false-positive build aborts. 30s: the loop exits on
/// the first empty read (builds are already dead in the normal case),
/// and 30s covers writeback/D-state stalls that 2s demonstrably did
/// not.
pub(super) const DRAIN_BUDGET: Duration = Duration::from_secs(30);

/// Result of the post-kill drain poll. [`DrainOutcome::Quiesced`] is
/// the ONLY value that permits output collection — deny-on-failure:
/// every error path (read failure, ENOENT teardown race, poll-task
/// panic) maps to `NotQuiesced`, never to "assume empty".
#[derive(Debug)]
pub(super) enum DrainOutcome {
    /// `cgroup.procs` read as empty — no live writers in the tree.
    Quiesced,
    /// Processes survived the budget, or the cgroup state could not be
    /// verified. `reason` is human-readable for the resulting error.
    NotQuiesced { reason: String },
}

/// Kill the per-build cgroup tree, wait for it to drain, then drop.
/// Returns whether the tree provably quiesced; the caller MUST NOT
/// collect outputs on [`DrainOutcome::NotQuiesced`].
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
pub(super) async fn drain_build_cgroup(build_cgroup: crate::cgroup::BuildCgroup) -> DrainOutcome {
    if let Err(e) = build_cgroup.kill() {
        // ENOENT shouldn't happen (we hold the BuildCgroup, Drop hasn't
        // run); EACCES would mean delegation is broken. Log and fall
        // through — the poll below decides whether the tree quiesced
        // anyway (it may already be empty on the success path).
        tracing::warn!(error = %e, "build_cgroup.kill() failed");
    }
    // cgroup.kill is async: write returns before procs are gone. Poll
    // cgroup.procs until empty or DRAIN_BUDGET elapsed. Sync read on
    // blocking pool — 10ms-interval single-line procfs reads, negligible.
    let cgroup_path_for_poll = build_cgroup.path().to_path_buf();
    let outcome = tokio::task::spawn_blocking(move || {
        poll_procs_until_empty(&cgroup_path_for_poll, DRAIN_BUDGET)
    })
    .await
    // Deny-on-failure: a panicked poll task leaves the cgroup
    // state unverified.
    .unwrap_or_else(|e| DrainOutcome::NotQuiesced {
        reason: format!("drain poll task panicked: {e}"),
    });
    if let DrainOutcome::NotQuiesced { reason } = &outcome {
        tracing::warn!(
            cgroup = %build_cgroup.path().display(),
            reason,
            "build cgroup not quiesced after cgroup.kill; output collection will be refused; rmdir will EBUSY"
        );
    }
    outcome
    // build_cgroup drops here. rmdir succeeds if the drain above emptied
    // it; otherwise Drop warns EBUSY + leaks (cleared on pod restart).
}

/// Poll `<cgroup>/cgroup.procs` until it reads empty or `budget`
/// elapses. Exits early on the first empty read.
///
/// Deny-on-failure: any read error returns `NotQuiesced` immediately —
/// ENOENT (someone else tore the cgroup down — teardown race), EMFILE,
/// EACCES all mean the tree's state cannot be verified, and an
/// unverified cgroup must be treated as a cgroup with live writers.
fn poll_procs_until_empty(cgroup_path: &Path, budget: Duration) -> DrainOutcome {
    let deadline = std::time::Instant::now() + budget;
    let procs = cgroup_path.join("cgroup.procs");
    loop {
        let remaining = match std::fs::read_to_string(&procs) {
            Ok(s) => s.lines().filter(|l| !l.trim().is_empty()).count(),
            Err(e) => {
                return DrainOutcome::NotQuiesced {
                    reason: format!("cgroup.procs unreadable: {e}"),
                };
            }
        };
        if remaining == 0 {
            return DrainOutcome::Quiesced;
        }
        if std::time::Instant::now() >= deadline {
            return DrainOutcome::NotQuiesced {
                reason: format!(
                    "{remaining} process(es) survived cgroup.kill past the {}s drain budget",
                    budget.as_secs()
                ),
            };
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `CgroupMonitors` directly (no real cgroup, no spawned
    /// pollers — both handles are no-op tasks). `stop()`'s final-sample
    /// read only needs `parent`/`baseline` populated.
    fn mk_monitors(parent: PathBuf, baseline: Option<u64>) -> CgroupMonitors {
        CgroupMonitors {
            cpu_poll: tokio::spawn(async {}),
            oom_watch: tokio::spawn(async {}),
            quota_poll: tokio::spawn(async {}),
            peak_cpu: Arc::new(AtomicU64::new(0)),
            oom_detected: Arc::new(AtomicBool::new(false)),
            peak_quota: Arc::new(AtomicU64::new(0)),
            parent,
            baseline,
        }
    }

    fn write_oom_kill(dir: &Path, n: u64) {
        std::fs::write(dir.join("memory.events"), format!("oom 0\noom_kill {n}\n")).unwrap();
    }

    // r[verify builder.oom.cgroup-watch+3]
    /// `stop()` MUST do a final synchronous `read_oom_kill`: an OOM that
    /// lands between the watcher's last 1Hz tick and build-exit (fast-exit
    /// toolchains: cargo / single cc exit ~100ms after a child OOM) would
    /// otherwise read `oom_detected=false` and the `CgroupOom` override
    /// would be skipped → `MiscFailure → PermanentFailure` instead of
    /// `InfrastructureFailure → bump resource_floor`.
    #[tokio::test]
    async fn test_stop_final_sample_catches_oom_between_ticks() {
        let parent = tempfile::tempdir().unwrap();
        write_oom_kill(parent.path(), 0);
        let monitors = mk_monitors(parent.path().to_path_buf(), Some(0));
        // OOM lands AFTER the (no-op) watcher was spawned but BEFORE stop().
        // The watcher never observed it (no tick fired); stop() must.
        write_oom_kill(parent.path(), 1);
        let (_, oom, _) = monitors.stop();
        assert!(oom, "final-sample read must see oom_kill 0→1");
    }

    /// Sensitivity: no increment → `stop()` reports `false`.
    #[tokio::test]
    async fn test_stop_final_sample_no_oom() {
        let parent = tempfile::tempdir().unwrap();
        write_oom_kill(parent.path(), 0);
        let monitors = mk_monitors(parent.path().to_path_buf(), Some(0));
        let (_, oom, _) = monitors.stop();
        assert!(!oom);
    }

    /// `baseline=None` (memory controller off / non-k8s test env) →
    /// graceful degradation: final-sample read is skipped, returns `false`
    /// even if `memory.events` later appears with `oom_kill > 0`.
    #[tokio::test]
    async fn test_stop_final_sample_baseline_none() {
        let parent = tempfile::tempdir().unwrap();
        write_oom_kill(parent.path(), 5);
        let monitors = mk_monitors(parent.path().to_path_buf(), None);
        let (_, oom, _) = monitors.stop();
        assert!(!oom, "baseline=None means no OOM watching");
    }

    // r[verify builder.cgroup.quiesce-before-collect]
    /// A kill-evading process (D-state, pre-submitted io_uring SQEs)
    /// keeps `cgroup.procs` non-empty past `cgroup.kill`: the poll MUST
    /// report `NotQuiesced` (with the surviving count in the reason)
    /// instead of claiming the tree is quiesced — the caller refuses
    /// output collection on anything but `Quiesced`.
    #[test]
    fn test_poll_procs_not_quiesced_when_procs_remain() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cgroup.procs"), "1234\n5678\n").unwrap();
        match poll_procs_until_empty(dir.path(), Duration::from_millis(50)) {
            DrainOutcome::NotQuiesced { reason } => {
                assert!(
                    reason.contains("2 process(es) survived cgroup.kill"),
                    "reason must name the surviving count: {reason}"
                );
            }
            DrainOutcome::Quiesced => panic!("non-empty cgroup.procs must not be Quiesced"),
        }
    }

    /// Happy path: empty `cgroup.procs` → `Quiesced`, exits on the
    /// first read without burning the budget.
    #[test]
    fn test_poll_procs_empty_is_quiesced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cgroup.procs"), "").unwrap();
        let start = std::time::Instant::now();
        let outcome = poll_procs_until_empty(dir.path(), Duration::from_secs(30));
        assert!(
            matches!(outcome, DrainOutcome::Quiesced),
            "got: {outcome:?}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "empty cgroup must exit early, not burn the budget"
        );
    }

    // r[verify builder.cgroup.quiesce-before-collect]
    /// Deny-on-failure: an unreadable `cgroup.procs` (ENOENT teardown
    /// race, fd exhaustion, …) is an UNVERIFIED cgroup — the poll MUST
    /// report `NotQuiesced`, never map a read error to "assume empty".
    #[test]
    fn test_poll_procs_read_error_is_not_quiesced() {
        let dir = tempfile::tempdir().unwrap();
        // No cgroup.procs file at all → read errors with ENOENT.
        match poll_procs_until_empty(dir.path(), Duration::from_millis(50)) {
            DrainOutcome::NotQuiesced { reason } => {
                assert!(
                    reason.contains("cgroup.procs unreadable"),
                    "reason must say the read failed: {reason}"
                );
            }
            DrainOutcome::Quiesced => panic!("unreadable cgroup.procs must not be Quiesced"),
        }
    }

    /// A tree that empties mid-poll is observed as `Quiesced` — the
    /// poll keeps re-reading until the budget, not just once.
    #[test]
    fn test_poll_procs_observes_late_drain() {
        let dir = tempfile::tempdir().unwrap();
        let procs = dir.path().join("cgroup.procs");
        std::fs::write(&procs, "1234\n").unwrap();
        let writer = std::thread::spawn({
            let procs = procs.clone();
            move || {
                std::thread::sleep(Duration::from_millis(50));
                std::fs::write(&procs, "").unwrap();
            }
        });
        let outcome = poll_procs_until_empty(dir.path(), Duration::from_secs(30));
        writer.join().unwrap();
        assert!(
            matches!(outcome, DrainOutcome::Quiesced),
            "late drain within budget must be observed, got: {outcome:?}"
        );
    }
}
