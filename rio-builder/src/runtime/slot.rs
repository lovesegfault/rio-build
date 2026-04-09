//! Single-build occupancy tracking and cancellation.
//!
//! P0537: one build per pod, no concurrency knob. `BuildSlot` replaces
//! the old `Semaphore::new(1)` + `RwLock<HashSet<String>>` pair — both
//! "is a build running?" and "which drv_path?" live here.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::Notify;

/// Per-claim state. Created atomically by [`BuildSlot::try_claim`] and
/// torn down by [`BuildSlotGuard::drop`]. The `cancelled` flag exists
/// from the instant the slot is claimed, so a `Cancel` racing the
/// `try_claim` → `set_cgroup_path` window still has a flag to set
/// (Corr#3: previously `running` and the cancel target lived under
/// separate mutexes, and a cancel in that window returned `false` and
/// was lost).
struct SlotInner {
    drv_path: String,
    /// Same `Arc` handed to the spawned build task via
    /// [`BuildSlotGuard::cancelled`] and threaded into
    /// `ExecutorEnv.cancelled`.
    cancelled: Arc<AtomicBool>,
    /// Populated by [`BuildSlot::set_cgroup_path`] once
    /// `spawn_build_task` has computed it. `None` until then —
    /// `try_cancel_build` treats `None` like ENOENT (flag set, kill
    /// deferred to the executor's pre-cgroup poll).
    cgroup_path: Option<PathBuf>,
}

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
    /// `Some` while a build is in-flight. Single mutex over all
    /// per-build state (drv_path, cancel flag, cgroup path) so
    /// `try_cancel_build` reads a consistent snapshot — no TOCTOU
    /// between "is this drv running?" and "set its cancel flag".
    inner: std::sync::Mutex<Option<SlotInner>>,
    /// Notified on release. `notify_waiters` (not `_one`): the drain
    /// watcher and the build-done watcher may both be parked at once
    /// (SIGTERM during the build).
    idle: Notify,
}

impl BuildSlot {
    /// Claim the slot for `drv_path`. Returns `None` if already busy
    /// (caller logs and rejects the assignment — see struct doc).
    ///
    /// The returned guard owns the cancel flag for this build; callers
    /// reach it via [`BuildSlotGuard::cancelled`].
    pub fn try_claim(self: &Arc<Self>, drv_path: &str) -> Option<BuildSlotGuard> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.is_some() {
            return None;
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        *inner = Some(SlotInner {
            drv_path: drv_path.to_string(),
            cancelled: Arc::clone(&cancelled),
            cgroup_path: None,
        });
        Some(BuildSlotGuard {
            slot: Arc::clone(self),
            cancelled,
        })
    }

    /// Current in-flight drv_path, for heartbeat `running_build`.
    pub fn running(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|s| s.drv_path.clone())
    }

    pub fn is_busy(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Record the cgroup path for the running build. Called by
    /// `spawn_build_task` after computing it from `cgroup_parent` +
    /// `sanitize_build_id(drv_path)` — predictively, before the cgroup
    /// is actually created. See [`try_cancel_build`].
    pub fn set_cgroup_path(&self, cgroup_path: PathBuf) {
        if let Some(s) = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            s.cgroup_path = Some(cgroup_path);
        }
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
pub struct BuildSlotGuard {
    slot: Arc<BuildSlot>,
    cancelled: Arc<AtomicBool>,
}

impl BuildSlotGuard {
    /// The cancel flag for this build. Same `Arc` stored in the slot
    /// and set by [`try_cancel_build`]; thread it into `ExecutorEnv`
    /// and the post-build Err classifier.
    pub fn cancelled(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }
}

impl Drop for BuildSlotGuard {
    fn drop(&mut self) {
        *self.slot.inner.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.slot.idle.notify_waiters();
    }
}

/// Attempt to cancel the running build. Checks the slot's running
/// drv_path matches, sets the cancel flag, writes cgroup.kill.
///
/// Returns `true` if the build was found and the flag was set (kill
/// may still be deferred — see ENOENT handling). `false` if not found
/// (build already finished, or the cancel is for a different drv —
/// stale CancelSignal from a previous scheduler generation).
///
/// Called from main.rs's `Msg::Cancel` handler. Fire-and-forget:
/// the scheduler doesn't wait for confirmation (it's already
/// transitioned the derivation to Cancelled on its side — this
/// is just cleanup).
pub fn try_cancel_build(slot: &BuildSlot, drv_path: &str) -> bool {
    // Single lock for the whole operation: drv_path match + flag set +
    // cgroup path read happen atomically. With one build per pod the
    // slot holds 0-or-1 entry; the drv_path check guards against a
    // stale CancelSignal (scheduler restarted and re-sent for a build
    // this pod never had).
    let guard = slot.inner.lock().unwrap_or_else(|e| e.into_inner());
    let Some(inner) = guard.as_ref() else {
        tracing::debug!(
            drv_path,
            "cancel: slot idle (build finished or never started)"
        );
        return false;
    };
    if inner.drv_path != drv_path {
        tracing::debug!(
            drv_path,
            running = %inner.drv_path,
            "cancel: drv mismatch (stale CancelSignal)"
        );
        return false;
    }

    // Set flag BEFORE kill: if there's a race where execute_build
    // is reading the flag right now, we want "cancelled=true" to
    // be visible by the time it sees the Err from run_daemon_build.
    // The kill → stdout EOF → Err path has some latency (kernel
    // delivers SIGKILL, process dies, pipe closes, tokio wakes);
    // setting the flag first gives us a wider window.
    inner
        .cancelled
        .store(true, std::sync::atomic::Ordering::Release);

    let Some(cgroup_path) = inner.cgroup_path.as_deref() else {
        // Slot claimed but cgroup path not yet recorded —
        // spawn_build_task hasn't reached set_cgroup_path. The flag IS
        // set (lives in the slot from claim time), so execute_build's
        // pre-cgroup poll will abort. Same outcome as ENOENT below.
        //
        // r[impl builder.cancel.pre-cgroup-deferred]
        tracing::info!(
            drv_path,
            "cancel: cgroup path not yet recorded; flag set for pre-cgroup poll"
        );
        return true;
    };

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
