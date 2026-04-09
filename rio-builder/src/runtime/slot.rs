//! Single-build occupancy tracking and cancellation.
//!
//! P0537: one build per pod, no concurrency knob. `BuildSlot` replaces
//! the old `Semaphore::new(1)` + `RwLock<HashSet<String>>` pair — both
//! "is a build running?" and "which drv_path?" live here.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::Notify;

/// Single-build occupancy. P0537: one build per pod, no concurrency
/// knob. Replaces the old `Semaphore::new(1)` + `RwLock<HashSet<String>>`
/// pair — both "is a build running?" and "which drv_path?" live here.
///
/// `try_claim` is non-blocking by design: with one build per pod, the
/// scheduler shouldn't dispatch while busy (heartbeat reports
/// `running_build`). An assignment arriving while busy is a scheduler
/// bug; the old `acquire_owned().await` would have queued it locally,
/// silently defeating capacity reporting.
#[derive(Default)]
pub struct BuildSlot {
    /// `Some(drv_path)` while a build is in-flight. `Mutex` not
    /// `RwLock`: with one build, contention is impossible (claim/release
    /// are serial; heartbeat reads every 10s).
    running: std::sync::Mutex<Option<String>>,
    /// Notified on release. `notify_waiters` (not `_one`): the drain
    /// watcher and the build-done watcher may both be parked at once
    /// (SIGTERM during the build).
    idle: Notify,
    /// Cancel target for the running build: (cgroup path for
    /// `cgroup.kill`, cancelled flag). Populated by `spawn_build_task`
    /// PREDICTIVELY (before the cgroup is created); read by
    /// [`try_cancel_build`]. The flag is the same `Arc` threaded into
    /// `ExecutorEnv.cancelled` and the spawned task's Err-classifier.
    ///
    /// `Mutex` like `running`: the Cancel handler reads once;
    /// `spawn_build_task` writes once. Cleared by `BuildSlotGuard::drop`
    /// (same lifetime as `running`).
    cancel: std::sync::Mutex<Option<(PathBuf, Arc<AtomicBool>)>>,
}

impl BuildSlot {
    /// Claim the slot for `drv_path`. Returns `None` if already busy
    /// (caller logs and rejects the assignment — see struct doc).
    pub fn try_claim(self: &Arc<Self>, drv_path: &str) -> Option<BuildSlotGuard> {
        let mut slot = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return None;
        }
        *slot = Some(drv_path.to_string());
        Some(BuildSlotGuard(Arc::clone(self)))
    }

    /// Current in-flight drv_path, for heartbeat `running_build`.
    pub fn running(&self) -> Option<String> {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn is_busy(&self) -> bool {
        self.running().is_some()
    }

    /// Record the cancel target for the running build. See field doc.
    pub fn set_cancel_target(&self, cgroup_path: PathBuf, cancelled: Arc<AtomicBool>) {
        *self.cancel.lock().unwrap_or_else(|e| e.into_inner()) = Some((cgroup_path, cancelled));
    }

    /// Park until the slot is idle. Missed-notification-safe: the
    /// `notified()` future is registered BEFORE checking `is_busy()`
    /// (Notify's documented `enable()` pattern), so a release between
    /// the check and the await still wakes us.
    pub async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            // Register interest before the load — see tokio::sync::Notify
            // docs §"Avoiding missed notifications".
            notified.as_mut().enable();
            if !self.is_busy() {
                return;
            }
            notified.await;
        }
    }
}

/// RAII release: `Drop` clears the slot and wakes any `wait_idle()`
/// callers. Held inside `spawn_build_task`'s spawned future (same
/// lifetime the old `OwnedSemaphorePermit` had).
pub struct BuildSlotGuard(Arc<BuildSlot>);

impl Drop for BuildSlotGuard {
    fn drop(&mut self) {
        *self.0.running.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self.0.cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.0.idle.notify_waiters();
    }
}

/// Attempt to cancel the running build. Checks the slot's running
/// drv_path matches, sets the cancel flag, writes cgroup.kill.
///
/// Returns `true` if the build was found and kill was attempted
/// (kill may still fail if the cgroup was already removed — we
/// log and consider it "cancelled anyway"). `false` if not found
/// (build already finished, or the cancel is for a different drv —
/// stale CancelSignal from a previous scheduler generation).
///
/// Called from main.rs's `Msg::Cancel` handler. Fire-and-forget:
/// the scheduler doesn't wait for confirmation (it's already
/// transitioned the derivation to Cancelled on its side — this
/// is just cleanup).
pub fn try_cancel_build(slot: &BuildSlot, drv_path: &str) -> bool {
    // drv_path match: with one build per pod the slot holds 0-or-1
    // entry; the drv_path check guards against a stale CancelSignal
    // (scheduler restarted and re-sent for a build this pod never had).
    if slot.running().as_deref() != Some(drv_path) {
        tracing::debug!(
            drv_path,
            "cancel: build not in slot (finished or never started)"
        );
        return false;
    }
    let guard = slot.cancel.lock().unwrap_or_else(|e| e.into_inner());
    let Some((cgroup_path, cancelled)) = guard.as_ref() else {
        // Slot claimed but cancel target not yet set — spawn_build_task
        // hasn't reached set_cancel_target. Same as ENOENT below: return
        // false; the scheduler will re-send.
        tracing::debug!(
            drv_path,
            "cancel: slot claimed but cancel target not yet set"
        );
        return false;
    };

    // Set flag BEFORE kill: if there's a race where execute_build
    // is reading the flag right now, we want "cancelled=true" to
    // be visible by the time it sees the Err from run_daemon_build.
    // The kill → stdout EOF → Err path has some latency (kernel
    // delivers SIGKILL, process dies, pipe closes, tokio wakes);
    // setting the flag first gives us a wider window.
    cancelled.store(true, std::sync::atomic::Ordering::Release);

    match crate::cgroup::kill_cgroup(cgroup_path) {
        Ok(()) => {
            tracing::info!(drv_path, cgroup = %cgroup_path.display(), "build cancelled via cgroup.kill");
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Cgroup doesn't exist — cancel arrived before execute_build
            // reached BuildCgroup::create. Under JIT fetch this window
            // is overlay → resolve → prepare_sandbox → register +
            // prefetch (sub-second; the I-165 47-min warm stall is
            // gone), so the cancel-poll select was removed.
            //
            // r[impl builder.cancel.pre-cgroup-deferred]
            // LEAVE THE FLAG SET. execute_build checks it before the
            // register+prefetch phase and aborts with
            // `ExecutorError::Cancelled` without spawning the daemon.
            // The misclassification risk (an unrelated Err later
            // reported as Cancelled) is real but is the lesser evil:
            // a build that the scheduler already transitioned to
            // Cancelled has no client waiting on its real outcome,
            // and an unkillable builder burns activeDeadlineSeconds
            // (1h) of compute × N pods (I-166: ×86).
            tracing::info!(
                drv_path,
                cgroup = %cgroup_path.display(),
                "cancel: cgroup not yet created; flag left set for pre-cgroup poll"
            );
            true
        }
        Err(e) => {
            // EACCES (delegation broken?) / EINVAL (kernel < 5.14?).
            // We don't know if the kill landed. Leave the flag set —
            // if the build IS still running and later errs, we'll
            // misclassify as Cancelled, but that's less bad than the
            // reverse (kill DID land, we clear flag, build errs from
            // the kill, we report InfrastructureFailure → reassign).
            tracing::warn!(drv_path, error = %e, "cgroup.kill failed (non-ENOENT); flag left set");
            true
        }
    }
}
