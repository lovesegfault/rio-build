//! The `execute()` entry point: parent-side orchestration of one
//! sandboxed execution.
//!
//! The pipeline is: validate → [`SandboxPlan::compile`] →
//! [`skeleton::build`] → fork the *intermediate* process → (intermediate)
//! [`child::enter_namespaces`] → fork the *sandbox child* → (sandbox
//! child) [`child::setup_and_exec`] → `execve`. While the sandboxed
//! program runs, the parent streams captured output as
//! [`ExecEvent`]s, enforces the request's [`Limits`], and finally reaps
//! the process tree and reports per-output metadata.
//!
//! # Process topology and fd protocol
//!
//! ```text
//! parent (tokio)
//!   ├─ go pipe ──────────► two-byte gate. Byte 1 → intermediate: the
//!   │                      parent has attached it to `<cg>`; byte 2 →
//!   │                      sandbox child: the parent has placed it in
//!   │                      `<cg>/build`. EOF at either gate = the
//!   │                      parent died first (abort, do not build)
//!   ├─ placement sock ◄─── intermediate: the sandbox child's pid plus
//!   │                      a pidfd (`SCM_RIGHTS`), sent right after
//!   │                      the fork — the parent's recycle-proof
//!   │                      handle to the build principal
//!   ├─ status pipe ◄────── intermediate + sandbox child: 8 bytes on
//!   │                      setup failure; EOF with 0 bytes when the
//!   │                      program exec'd (the write end is
//!   │                      close-on-exec)
//!   └─ pty master / pipes ◄ the program's stdout/stderr
//! ```
//!
//! The *build principal* is the sandbox child: the root of the tree
//! that runs tenant code. It lives in the `<cg>/build` sub-cgroup
//! (created and populated by the parent before the release byte), so
//! limit kills can target exactly the build — never the intermediate,
//! which is the executor's own relay and the sole carrier of the
//! principal's true exit status.
//!
//! Each forked process starts by closing every inherited fd outside its
//! keep set ([`ChildFds`]): the intermediate immediately after fork —
//! which is what makes go-pipe EOF a genuine parent-death signal, since
//! it then holds no copy of the write end it reads — and again down to
//! stdio after forking the sandbox child, so the status pipe and the
//! capture fds EOF on the sandbox child's progress alone.
//!
//! The *intermediate* exists because `unshare(CLONE_NEWPID)` only
//! affects subsequently-forked children: it unshares, forks the sandbox
//! child (which becomes pid 1 of the new PID namespace), sheds every
//! remaining fd so EOF tracks the sandbox child alone, waits, and
//! forwards the exit status (signals as `128 + signo`).
//!
//! # Lifecycle and cleanup
//!
//! The chroot directory ([`HostLayout::chroot_dir`]) is owned by the
//! caller: the caller creates it (or reuses the previous attempt's) and
//! the caller removes it. The executor owns everything else it creates
//! — pipes, the pty, the forked process tree — and cleans them up on
//! every path, including cancellation at *any* await point: a
//! [`ProcessTreeGuard`] is armed before the fork is even submitted to
//! the blocking pool, the fork closure publishes the new pid only by
//! adopting it into the guard (and destroys the process itself if the
//! guard is already gone), and dropping the guard kills — and, when no
//! reaper task exists yet, reaps — whatever was adopted. There is no
//! instant at which a forked process exists outside an armed kill
//! path, including the [spawn-blocking submission, await resumption]
//! window.
//!
//! # Caller contract
//!
//! **Event channel:** limit enforcement is unconditional — it runs in
//! a dedicated [`LimitWatchdog`] task fed by the capture readers at
//! raw-read time, and no receiver behavior can delay a kill. A stalled
//! (but alive) receiver affects only data delivery: events queue in a
//! bounded pending buffer ([`PENDING_BYTES_CAP`]), and once it fills,
//! backpressure cascades to the build itself (chunk channel → blocking
//! readers → pipe), bounding the executor's buffering at ≈ 3 MiB. A
//! *dropped* receiver discards events and execution continues
//! unthrottled. After the tree is reaped, delivery of whatever remains
//! is bounded by [`FINAL_DRAIN_TIMEOUT`].
//!
//! **Serialization:** the executor performs no execution-level
//! locking; the caller MUST NOT overlap executions. Two reasons: the
//! sandbox identity ([`Isolation::uid`](crate::Isolation::uid)) is a
//! singleton host uid without a user namespace, so overlapping builds
//! could observe and signal each other's processes; and `fork(2)`
//! copies the whole fd table, so a second execution's pipes would be
//! inherited by the first's forked children (the keep-set sweep bounds
//! that exposure but does not license overlap). rio-builder satisfies
//! this contract with `BuildSlot`: one build per pod, busy assignments
//! rejected, never queued.

use std::os::fd::{AsRawFd as _, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use nix::errno::Errno;
use tokio::sync::{mpsc, watch};

use crate::child::{self, ChildFds, SETUP_ERROR_WIRE_LEN, SetupError, SetupPhase};
use crate::outcome::{
    ExecEvent, ExecutionOutcome, ExitOutcome, LogStream, OutputFileType, OutputMetadata,
    OutputReport,
};
use crate::plan::{HostLayout, SandboxPlan};
use crate::request::{ExecutionRequest, OutputCapture};
use crate::{ExecError, skeleton};

/// The budget for every post-reap event operation: draining buffered
/// log output, the partial-line flush, the straggler status read, and
/// the late `Started` send. Bounds the gap between "the build exited"
/// and "execute() returned" even when the events receiver is alive but
/// stalled; EOF and channel capacity normally arrive well before this.
const FINAL_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a principal-targeted kill waits for the relay to forward
/// the killed build's status before escalating to a whole-tree kill.
/// The relay's remaining work at that point is one `waitpid` wake-up
/// plus an `_exit` — microseconds; the grace absorbs scheduler
/// pathology on starved nodes. Generous is fine: the escalation is a
/// backstop against a stuck relay (a supervision failure), not part
/// of any normal path, and the wait runs concurrently with the
/// supervision loop's own reap of the same exit.
const RELAY_ESCALATION_GRACE: Duration = Duration::from_secs(5);

/// Capacity of the internal raw-chunk channel between the blocking
/// pipe/pty readers and the async line splitter. Bounded so a build
/// that outputs faster than the caller consumes applies backpressure
/// to the reader thread (and ultimately the build's own writes) instead
/// of buffering without limit.
const LOG_CHUNK_CHANNEL_CAPACITY: usize = 64;

/// Budget of the pending-event queue between the supervision loop and
/// the caller's receiver, charged in *retained-footprint* units. Once
/// the charged total reaches the cap the loop stops consuming chunks,
/// which fills the chunk channel, parks the blocking readers, fills the
/// pipe, and finally write-blocks the build itself — the same
/// backpressure cascade as before, one queue deeper.
///
/// Every queued event is charged its full retained footprint —
/// `size_of::<ExecEvent>()` (the queue slot) plus the line buffer's
/// heap capacity — so the cascade engages for *every* input shape.
/// Charging payload bytes alone left the zero/short-line class
/// unbounded: a flood of empty lines retained a queue slot each while
/// charging nothing, so the gate never tripped (round-15
/// merged_bug_001). With the footprint charge the queue length is
/// bounded at `PENDING_BYTES_CAP / size_of::<ExecEvent>()` = 65,536
/// events for zero-payload floods.
///
/// The hard memory bound: ≤ the cap in charged footprint, plus one
/// chunk's worth of overshoot (the gate is checked before each chunk —
/// up to one cap-forced fragment of [`MAX_PENDING_LINE_BYTES`] plus the
/// chunk remainder ≤ 8 KiB), plus the `VecDeque`'s amortized ×2 slot
/// over-allocation: ≈ 2–5 MiB per execution across all input classes.
///
/// Dominance over both prior bounds, per input class:
/// - max-size lines: ≤ cap + 1 chunk ≈ 3 MiB (pre-FU1 256-slot channel
///   allowed 256 × 1 MiB = 256 MiB);
/// - zero-payload lines: 65,536 slots ≈ 2 MiB charged (the FU1
///   payload-only charge allowed unbounded growth; pre-FU1 bounded at
///   256 slots). Strictly tighter than each predecessor's worst class.
const PENDING_BYTES_CAP: usize = 2 << 20;

/// FIFO of events awaiting a receiver permit, with retained-footprint
/// accounting.
///
/// The queue is the *only* path to the caller: the supervision loop
/// never awaits `events.send()` directly, so a stalled receiver can
/// park delivery — never supervision, and never the [`LimitWatchdog`].
struct PendingEvents {
    queue: std::collections::VecDeque<ExecEvent>,
    bytes: usize,
}

impl PendingEvents {
    fn new() -> PendingEvents {
        PendingEvents {
            queue: std::collections::VecDeque::new(),
            bytes: 0,
        }
    }

    /// Charged cost of one queued event: its full retained footprint.
    /// The fixed struct size covers the `VecDeque` slot every event
    /// occupies (what bounds zero-payload floods); the payload term is
    /// the heap allocation actually retained — `capacity`, not `len`,
    /// because the buffer's allocation is what the queue keeps alive.
    // r[impl builder.exec.event-budget]
    fn cost(event: &ExecEvent) -> usize {
        std::mem::size_of::<ExecEvent>()
            + match event {
                ExecEvent::Log { line, .. } => line.capacity(),
                ExecEvent::Started { .. } => 0,
            }
    }

    fn push(&mut self, event: ExecEvent) {
        self.bytes += Self::cost(&event);
        self.queue.push_back(event);
    }

    fn pop(&mut self) -> Option<ExecEvent> {
        let event = self.queue.pop_front()?;
        self.bytes -= Self::cost(&event);
        Some(event)
    }

    fn clear(&mut self) {
        self.queue.clear();
        self.bytes = 0;
    }

    fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Stop consuming chunks once the queued bytes reach the cap.
    fn at_cap(&self) -> bool {
        self.bytes >= PENDING_BYTES_CAP
    }
}

/// Why the executor killed the process tree before it finished on its
/// own. Takes precedence over the raw exit status when mapping the
/// final [`ExitOutcome`] — if the wait status corroborates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KillReason {
    Timeout,
    Silent,
    LogLimit,
}

impl From<KillReason> for ExitOutcome {
    fn from(k: KillReason) -> ExitOutcome {
        match k {
            KillReason::Timeout => ExitOutcome::TimedOut,
            KillReason::Silent => ExitOutcome::Silent,
            KillReason::LogLimit => ExitOutcome::LogLimitExceeded,
        }
    }
}

/// What a kill that *acted* was aimed at. Decides which wait statuses
/// can corroborate the recorded claim — the two targets produce
/// disjoint relay statuses by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KillTarget {
    /// The build principal only: `<cg>/build/cgroup.kill` plus the
    /// placed pidfd. The relay is never signaled, so it always
    /// survives to forward the principal's true status — the only
    /// status such a kill can produce is the forwarded `128+9`.
    Principal,
    /// The whole tree, relay included (pre-placement: no principal
    /// handle exists yet, and no tenant instruction has run — the
    /// release gate is still closed).
    Tree,
}

/// A recorded limit kill: the *claim* layer of the verdict contract.
/// [`map_exit`] honors it only when the wait status is one the kill,
/// as targeted, could actually have produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KillClaim {
    reason: KillReason,
    target: KillTarget,
}

/// What the setup-status pipe reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusReport {
    /// EOF with no bytes: every close-on-exec copy of the write end is
    /// gone, i.e. the program exec'd.
    ExecStarted,
    /// A full 8-byte setup failure report.
    SetupFailed(SetupError),
    /// Bytes arrived but did not decode (truncated write or unknown
    /// phase discriminant). Treated as an unattributable setup failure.
    Corrupt,
}

/// One raw read from a capture fd, before line splitting.
struct LogChunk {
    stream: LogStream,
    bytes: Vec<u8>,
}

/// Activity accounting fed by the blocking capture readers, at read
/// time — before the bytes enter any channel. The recorded volume and
/// the change notification reflect what the program actually wrote,
/// independent of every downstream consumer (the chunk channel, the
/// line splitter, the caller's event receiver). Mirrors the position
/// where the upstream daemon resets its own activity clock: when the
/// supervisor *reads* program output, not when it forwards it.
///
/// Held only by the readers; the parent drops its handle right after
/// spawning them so the watch channel closes when the last reader
/// exits.
struct ActivityMeter {
    /// Cumulative captured bytes, shared with the [`LimitWatchdog`].
    bytes: Arc<AtomicU64>,
    /// Change notification carrying the cumulative total.
    tick: watch::Sender<u64>,
}

impl ActivityMeter {
    /// Returns the meter (for the readers), the shared byte counter,
    /// and the watch receiver (both for the watchdog).
    fn new() -> (Arc<ActivityMeter>, Arc<AtomicU64>, watch::Receiver<u64>) {
        let (tick, rx) = watch::channel(0u64);
        let bytes = Arc::new(AtomicU64::new(0));
        (
            Arc::new(ActivityMeter {
                bytes: Arc::clone(&bytes),
                tick,
            }),
            bytes,
            rx,
        )
    }

    /// Record `n` freshly read bytes and wake the watchdog.
    // r[impl builder.exec.limits-isolated+2]
    fn record(&self, n: usize) {
        let total = self.bytes.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
        self.tick.send_replace(total);
    }
}

/// The limit-enforcement task: wall-clock timeout, max-silent deadline,
/// and log-volume cap, isolated from event delivery.
///
/// Owns only the kill handle, its deadline timers, and the
/// receive-only activity watch — no channel `Sender` of any kind, so
/// "enforcement parked on a channel send" is unwritable here, not just
/// unwritten. No event-consumer behavior can delay a kill.
struct LimitWatchdog {
    tree: Arc<TreeState>,
    timeout: Option<Duration>,
    max_silent: Option<Duration>,
    max_log_bytes: Option<u64>,
    /// The pre-fork instant: the timeout base is unchanged from the
    /// previous in-loop enforcement.
    started: Instant,
    activity: watch::Receiver<u64>,
    bytes: Arc<AtomicU64>,
    /// Why (and at what target) the watchdog killed, if it did and
    /// the kill acted on a tree the phase machine still believed
    /// live. A *claim*, not a verdict: `execute()` reads it after the
    /// reap and [`map_exit`] honors it only when the wait status is
    /// one the kill, as targeted, could actually have produced.
    claim: Arc<std::sync::Mutex<Option<KillClaim>>>,
}

impl LimitWatchdog {
    /// Enforce until a limit fires (kill, then return) or nothing can
    /// ever fire again (return without killing).
    // r[impl builder.exec.limits-isolated+2]
    async fn run(mut self) {
        // Tokio instants throughout: identical to the monotonic clock
        // in production, virtualizable under `start_paused` tests.
        let started = tokio::time::Instant::from_std(self.started);
        // Silence base: the watchdog starts right after the go byte,
        // the same instant the old in-loop clock was initialized.
        let mut last_activity = tokio::time::Instant::now();
        let mut activity_open = true;
        loop {
            let timeout_at = self.timeout.map(|t| started + t);
            let silent_at = self.max_silent.map(|t| last_activity + t);
            // Totality: return once no arm can ever fire again — no
            // deadlines configured and the activity arm is either
            // closed or pointless (no log cap to enforce and no
            // silence clock to reset).
            if timeout_at.is_none()
                && silent_at.is_none()
                && (!activity_open || self.max_log_bytes.is_none())
            {
                return;
            }
            tokio::select! {
                changed = self.activity.changed(), if activity_open => {
                    match changed {
                        Ok(()) => {
                            last_activity = tokio::time::Instant::now();
                            if self
                                .max_log_bytes
                                .is_some_and(|max| self.bytes.load(Ordering::Relaxed) > max)
                            {
                                self.kill_and_escalate(KillReason::LogLimit).await;
                                return;
                            }
                        }
                        Err(_) => {
                            // Every reader exited (EOF). If the tree is
                            // already settled there is nothing left to
                            // bound; otherwise keep the deadline arms
                            // live with the silence clock frozen at the
                            // last real output — a program that closed
                            // its own stdout/stderr and keeps running
                            // is still bounded.
                            if self.tree.is_settled() {
                                return;
                            }
                            activity_open = false;
                        }
                    }
                }
                () = sleep_until_opt(timeout_at), if timeout_at.is_some() => {
                    self.kill_and_escalate(KillReason::Timeout).await;
                    return;
                }
                // A second implementation site for the silence kill: the
                // deadline is reset by the raw captured bytes, upstream
                // of classification and delivery.
                // r[impl builder.silence.timeout-kill+3]
                () = sleep_until_opt(silent_at), if silent_at.is_some() => {
                    self.kill_and_escalate(KillReason::Silent).await;
                    return;
                }
            }
        }
    }

    /// Kill the build and record why — but only when the kill
    /// actually acted on a tree the state machine still believed
    /// live — then, for a principal-targeted kill, backstop the
    /// relay's forward with a claim-free escalation.
    ///
    /// This is the *narrowing* layer of the two-layer verdict contract
    /// (see [`map_exit`] for the deciding layer). Kill-under-the-
    /// claim-mutex: the kill happens while the claim slot is locked,
    /// so the exit it causes cannot be mapped before the slot is
    /// consistent; a deadline that fires after the tree settled is a
    /// no-op that records nothing. The state machine necessarily lags
    /// the kernel by the [exit observed, phase flipped] window, so a
    /// recorded claim alone is not a verdict — `map_exit` honors it
    /// only when the wait status is one the kill, as targeted, could
    /// actually have produced.
    ///
    /// A principal kill leaves the relay alive on purpose (it is the
    /// carrier of the corroborating status). Its one job is bounded:
    /// if the relay has not been reaped within
    /// [`RELAY_ESCALATION_GRACE`] of the kill, [`TreeState::
    /// escalate_relay`] takes the whole tree down with **no claim
    /// mutation** — the resulting `signaled(SIGKILL)` relay status
    /// does not corroborate a `Principal` claim, so the verdict
    /// degrades to the honest `Signaled(9)` rather than a
    /// manufactured limit verdict.
    // r[impl builder.exec.limits-isolated+2]
    // r[impl builder.exec.kill-targets-principal]
    async fn kill_and_escalate(&self, reason: KillReason) {
        let target = {
            let mut slot = self.claim.lock().unwrap_or_else(|e| e.into_inner());
            let target = self.tree.kill_tree();
            if let Some(target) = target
                && slot.is_none()
            {
                *slot = Some(KillClaim { reason, target });
            }
            target
        };
        if target == Some(KillTarget::Principal) {
            tokio::time::sleep(RELAY_ESCALATION_GRACE).await;
            if !self.tree.is_settled() {
                self.tree.escalate_relay();
            }
        }
    }
}

/// Run one sandboxed execution to completion.
///
/// Returns an [`ExecutionOutcome`] whenever the sandbox was constructed
/// and the program ran — however it exited. Returns an [`ExecError`]
/// only for infrastructure failures: an invalid request, a failed
/// skeleton build, a failed fork/pipe/cgroup setup, or a sandbox-setup
/// failure reported by the child (with the failing phase and errno).
///
/// See the module docs for the caller contract on `events` and for
/// chroot-directory ownership.
pub async fn execute(
    request: &ExecutionRequest,
    host: &HostLayout,
    events: mpsc::Sender<ExecEvent>,
) -> Result<ExecutionOutcome, ExecError> {
    // ---- Plan and skeleton (no processes yet) ---------------------------
    let plan = SandboxPlan::compile(request, host)?;
    let plan = {
        let mut plan = plan;
        // Filesystem I/O: off the async thread.
        tokio::task::spawn_blocking(move || skeleton::build(&mut plan).map(|()| plan))
            .await
            .map_err(|e| ExecError::Skeleton(std::io::Error::other(e)))?
            .map_err(ExecError::Skeleton)?
    };

    // ---- Pipes and capture fds ------------------------------------------
    let (status_r, status_w) = make_pipe().map_err(spawn_err("create the status pipe"))?;
    let (go_r, go_w) = make_pipe().map_err(spawn_err("create the go pipe"))?;
    let (placement_r, placement_w) =
        make_placement_socketpair().map_err(spawn_err("create the placement socket"))?;
    let capture = CaptureFds::new(request, &plan).map_err(ExecError::Spawn)?;
    let (parent_capture, child_capture) = capture.split();

    // The child-side fd owners move INTO the fork closure below: a
    // dropped execute() future cannot close them (and recycle their
    // numbers into another execution's pipes) while a fork that
    // references those numbers is in flight.
    let owners = ChildFdOwners {
        status_w,
        go_r,
        placement_w,
        capture: child_capture,
    };
    let child_fds = owners.child_fds();

    // ---- Fork the intermediate -------------------------------------------
    // Kept out of the plan move below so an indexed setup failure
    // (a bind or special mount) can be reported with the path it was
    // operating on, not just its index.
    let bind_targets: Vec<PathBuf> = plan.binds.iter().map(|b| b.target.clone()).collect();
    let special_targets: Vec<PathBuf> = plan
        .special_mounts
        .iter()
        .map(|m| m.target.clone())
        .collect();
    let start = SystemTime::now();
    let started = Instant::now();

    // The kill guard exists BEFORE the fork is submitted: there is no
    // instant — including the [spawn_blocking submission, await
    // resumption] window — at which a forked process can exist without
    // an armed guard. Dropping this future from here on kills (and, if
    // no reaper task is attached yet, reaps) whatever was forked.
    // r[impl builder.exec.tree-ownership]
    let tree = TreeState::new(request.limits.cgroup.clone());
    let tree_guard = ProcessTreeGuard {
        state: Arc::clone(&tree),
    };

    // The fork happens on a dedicated blocking thread: `tokio::process`
    // cannot be used (the child must unshare namespaces and run
    // arbitrary pre-exec code without exec'ing immediately), and
    // forking from an async worker thread would stall the runtime for
    // the duration of the pre-fork bookkeeping. Only the forking thread
    // exists in the child, which is why everything the child touches
    // was precomputed into the plan. Blocking tasks are never cancelled
    // by tokio once running (and queued ones still run) — the guard
    // handshake below relies on exactly that documented behavior.
    let intermediate = {
        let tree = Arc::clone(&tree);
        tokio::task::spawn_blocking(move || {
            // `owners` lives (and drops) in this closure.
            let fds = child_fds;
            // The caller may already be gone — its drop marked the
            // tree Dead while this task sat in the blocking-pool
            // queue. Don't create a process nobody supervises.
            if tree.is_dead() {
                return Err(std::io::Error::other(
                    "caller cancelled before the sandbox was forked",
                ));
            }
            let pid = spawn_intermediate(&plan, &fds)?;
            // Adoption is the only pid-publication path. If the caller
            // disappeared between the pre-check and here, adopt() has
            // already killed and reaped the fresh process.
            // r[impl builder.exec.tree-ownership]
            tree.adopt(pid).map_err(|CallerGone| {
                std::io::Error::other("caller cancelled while the sandbox was being forked")
            })?;
            // Close the parent's copies of the child-side ends so EOF
            // on the parent-side ends tracks the child processes
            // alone. Same protocol position as before the restructure,
            // but no longer droppable early by a cancelled caller.
            drop(owners);
            Ok(pid)
        })
        .await
        .map_err(|e| ExecError::Spawn(std::io::Error::other(e)))?
        .map_err(ExecError::Spawn)?
    };

    // Reap on a dedicated blocking thread. attach_reaper() and the
    // spawn happen with no await between them, so there is no state
    // where the guard believes a reaper exists but none was spawned.
    // observe_then_reap flips the tree phase BEFORE the status is
    // consumed, so a kill can never target a recycled pid.
    tree.attach_reaper();
    let wait_task = {
        let tree = Arc::clone(&tree);
        tokio::task::spawn_blocking(move || observe_then_reap(&tree, intermediate))
    };
    // The status reader exists from the moment a forked child can
    // write a setup report — BEFORE the first abortable step — so the
    // teardown path below can surface a typed failure instead of
    // discarding it with the dropped read end.
    // r[impl builder.exec.setup-error-surfaced]
    let status_task = tokio::task::spawn_blocking(move || read_status_pipe(status_r));
    let sandbox = SpawnedSandbox {
        tree_guard,
        wait_task,
        status_task,
    };

    // ---- Cgroup attach, then the go byte ----------------------------------
    // The intermediate (and through fork inheritance the whole sandbox
    // process tree) must be inside the caller's cgroup before it starts
    // doing accountable work; it blocks on the go pipe until told
    // otherwise. The attach is a single small write to cgroupfs, done
    // inline rather than through spawn_blocking. The build sub-cgroup
    // is created first so the placement step below cannot find it
    // missing: by the time any process exists in `<cg>`, `<cg>/build`
    // exists too. (No controllers are delegated into it — `<cg>`'s
    // limits cover both levels hierarchically — so populating `<cg>`
    // itself stays legal under the no-internal-process rule.)
    if let Some(cgroup) = &request.limits.cgroup {
        if let Err(e) = create_build_subcgroup(cgroup) {
            return Err(sandbox
                .abort(
                    e,
                    "create the build sub-cgroup",
                    &bind_targets,
                    &special_targets,
                )
                .await);
        }
        if let Err(e) = std::fs::write(
            cgroup.join("cgroup.procs"),
            format!("{}\n", intermediate.as_raw()),
        ) {
            return Err(sandbox
                .abort(
                    e,
                    "attach the sandbox to the cgroup",
                    &bind_targets,
                    &special_targets,
                )
                .await);
        }
    }
    if let Err(e) = nix::unistd::write(&go_w, &[1u8]) {
        return Err(sandbox
            .abort(
                std::io::Error::from(e),
                "signal the sandbox to proceed",
                &bind_targets,
                &special_targets,
            )
            .await);
    }

    // ---- Principal placement, then the release byte ------------------------
    // The intermediate forks the sandbox child and hands back its pid
    // plus a pidfd; the parent moves the child into `<cg>/build` and
    // stores the pidfd as the kill target. Only then does the release
    // byte let the child proceed — no tenant instruction ever runs
    // outside the principal kill scope. The recv is blocking (the
    // message arrives microseconds after the go byte unless the
    // intermediate died, which closes the socket and EOFs the recv).
    // r[impl builder.exec.kill-targets-principal]
    let (principal_pid, principal_pidfd) =
        match tokio::task::spawn_blocking(move || recv_placement(placement_r))
            .await
            .map_err(|e| ExecError::Spawn(std::io::Error::other(e)))?
        {
            Ok(placement) => placement,
            Err(e) => {
                return Err(sandbox
                    .abort(
                        e,
                        "receive the build principal placement",
                        &bind_targets,
                        &special_targets,
                    )
                    .await);
            }
        };
    if let Some(cgroup) = &request.limits.cgroup
        && let Err(e) = std::fs::write(
            cgroup.join(BUILD_SUBCGROUP).join("cgroup.procs"),
            format!("{principal_pid}\n"),
        )
        && !placement_attach_tolerable(&e)
    {
        // A tolerable failure means the principal already exited (its
        // status is en route through the relay); anything else means
        // the kill scope could not be established — do not run.
        return Err(sandbox
            .abort(
                e,
                "place the build principal in the build sub-cgroup",
                &bind_targets,
                &special_targets,
            )
            .await);
    }
    tree.place_principal(principal_pidfd);
    if let Err(e) = nix::unistd::write(&go_w, &[1u8])
        && !(e == Errno::EPIPE && tree.was_killed())
    {
        // EPIPE with a kill already recorded is the benign race — a
        // deadline fired inside [placement, release] and the kill took
        // the gated child (the pipe's only remaining read end) with
        // it. The relay is about to forward the verdict; fall through
        // to supervision instead of aborting it away.
        return Err(sandbox
            .abort(
                std::io::Error::from(e),
                "release the placed build principal",
                &bind_targets,
                &special_targets,
            )
            .await);
    }
    drop(go_w);

    // ---- Readers -----------------------------------------------------------
    // Past the last abortable step: the supervision loop owns the
    // parts from here (the guard stays alive to the end of execute()).
    let SpawnedSandbox {
        tree_guard: _tree_guard,
        wait_task,
        status_task,
    } = sandbox;
    let (chunk_tx, mut chunk_rx) = mpsc::channel::<LogChunk>(LOG_CHUNK_CHANNEL_CAPACITY);
    let (meter, meter_bytes, activity_rx) = ActivityMeter::new();
    spawn_log_readers(parent_capture, &chunk_tx, &meter);
    // The select loop below must observe channel closure when the
    // readers finish, so the parent's own sender clone goes away now —
    // and the watchdog must observe the activity watch closing, so the
    // parent's meter handle goes with it.
    drop(chunk_tx);
    drop(meter);

    // ---- Limit enforcement -------------------------------------------------
    // Isolated in its own task: the watchdog owns the kill handle, its
    // deadline timers, and the receive-only activity watch — and
    // nothing else. It performs no channel sends, so no consumer
    // behavior (a stalled events receiver, a full chunk channel) can
    // delay a limit kill.
    let limit_claim: Arc<std::sync::Mutex<Option<KillClaim>>> =
        Arc::new(std::sync::Mutex::new(None));
    let watchdog = tokio::spawn(
        LimitWatchdog {
            tree: Arc::clone(&tree),
            timeout: request.limits.timeout,
            max_silent: request.limits.max_silent,
            max_log_bytes: request.limits.max_log_bytes,
            started,
            activity: activity_rx,
            bytes: meter_bytes,
            claim: Arc::clone(&limit_claim),
        }
        .run(),
    );

    // ---- The supervision loop ----------------------------------------------
    let mut splitter = LineSplitter::default();
    let mut pending = PendingEvents::new();
    let mut events_open = true;
    let mut started_queued = false;
    let mut status_report: Option<StatusReport> = None;
    let mut wait_status: Option<Result<i32, std::io::Error>> = None;
    let mut chunks_done = false;
    let mut status_task = status_task;
    let mut wait_task = wait_task;

    while wait_status.is_none() {
        tokio::select! {
            // Queued delivery — the ONLY path events reach the caller.
            // `reserve()` is its own arm, so the loop never parks on
            // the receiver: no capacity simply means this arm stays
            // pending while the others keep running.
            // r[impl builder.exec.limits-isolated+2]
            permit = events.reserve(), if events_open && !pending.is_empty() => {
                match permit {
                    Ok(permit) => {
                        permit.send(pending.pop().expect("arm guarded on non-empty queue"));
                    }
                    Err(_) => {
                        // Receiver dropped: per the caller contract,
                        // events are discarded and execution continues
                        // unthrottled. From here on splitter output is
                        // dropped at push time — re-queueing it would
                        // fill the queue, disable the chunk arm, and
                        // write-block a build whose receiver is gone
                        // forever.
                        events_open = false;
                        pending.clear();
                    }
                }
            }
            // Status pipe resolution: either the program exec'd or
            // setup failed somewhere.
            report = &mut status_task, if status_report.is_none() => {
                let report = report.unwrap_or(StatusReport::Corrupt);
                status_report = Some(report);
                match report {
                    StatusReport::ExecStarted => {
                        if events_open {
                            started_queued = true;
                            pending.push(ExecEvent::Started { pid: intermediate.as_raw() });
                        }
                    }
                    StatusReport::SetupFailed(_) | StatusReport::Corrupt => {
                        // The tree is exiting on its own; the kill is a
                        // no-op safety net (lifecycle hygiene, not limit
                        // enforcement). Keep looping until reaped.
                        tree.kill_tree();
                    }
                }
            }
            // Captured output. Disabled once the readers hit EOF, and
            // paused while the pending queue is at its byte cap (the
            // backpressure cascade: queue → chunk channel → readers →
            // pipe → the build's own writes). The wait arm below stays
            // live regardless. Limit enforcement lives in the watchdog,
            // fed at raw-read time, so nothing in this delivery path
            // can delay a kill.
            chunk = chunk_rx.recv(), if !chunks_done && !pending.at_cap() => {
                match chunk {
                    Some(chunk) => {
                        if events_open {
                            for line in splitter.push(chunk.stream, &chunk.bytes) {
                                pending.push(line);
                            }
                        }
                        // A closed receiver consumes chunks without
                        // splitting: the readers must never block on a
                        // consumer that no longer exists.
                    }
                    // Both readers finished (EOF / EIO). The reap (and
                    // the watchdog's deadlines) finish the loop.
                    None => chunks_done = true,
                }
            }
            // The process tree was reaped.
            status = &mut wait_task, if wait_status.is_none() => {
                wait_status = Some(status.unwrap_or_else(|e| Err(std::io::Error::other(e))));
            }
        }
    }
    let stop = SystemTime::now();
    // The tree is reaped (the wait task marked it so); the guard is
    // inert from here on and simply drops when execute() returns.

    // Enforcement is over: stop the watchdog before any drain work so
    // a deadline cannot fire mid-drain. A kill already in flight
    // completes (kills are synchronous under the claim mutex) and
    // no-ops against the reaped tree without recording a claim. An
    // escalation grace pending for a principal kill is cancelled with
    // the task — the tree settled, which is exactly the condition the
    // escalation exists to force.
    watchdog.abort();

    // The status report normally resolved long ago; give a straggler
    // (e.g. a setup failure racing the reap, or a program so fast that
    // the reap won the final select) a bounded window — before the
    // drain, so an owed `Started` joins the queued delivery below
    // instead of needing its own post-drain send.
    if status_report.is_none() {
        status_report = Some(
            tokio::time::timeout(FINAL_DRAIN_TIMEOUT, &mut status_task)
                .await
                .map(|r| r.unwrap_or(StatusReport::Corrupt))
                .unwrap_or(StatusReport::Corrupt),
        );
    }
    if !started_queued && status_report == Some(StatusReport::ExecStarted) && events_open {
        pending.push(ExecEvent::Started {
            pid: intermediate.as_raw(),
        });
    }

    // Drain: queued events first (FIFO with the lines below), then
    // whatever output is still buffered, then the partial-line flush —
    // one shared budget for all of it. The tree is already gone, so a
    // stalled (but alive) events receiver must not be able to park
    // execute() past the budget. If the budget expires mid-drain the
    // remaining events are dropped — best-effort by design once the
    // receiver has stopped consuming.
    let _ = tokio::time::timeout(FINAL_DRAIN_TIMEOUT, async {
        while let Some(event) = pending.pop() {
            if !events_open || events.send(event).await.is_err() {
                events_open = false;
                break;
            }
        }
        while let Some(chunk) = chunk_rx.recv().await {
            if !events_open {
                continue; // keep consuming so the readers can exit
            }
            for line in splitter.push(chunk.stream, &chunk.bytes) {
                if events.send(line).await.is_err() {
                    events_open = false;
                    break;
                }
            }
        }
        for line in splitter.flush() {
            // The channel may already be gone; the lines are
            // best-effort.
            if events_open && events.send(line).await.is_err() {
                events_open = false;
            }
        }
    })
    .await;

    // ---- Interpret ----------------------------------------------------------
    match status_report {
        Some(StatusReport::SetupFailed(err)) => {
            return Err(setup_failure_to_error(err, &bind_targets, &special_targets));
        }
        Some(StatusReport::Corrupt) => {
            return Err(ExecError::Spawn(std::io::Error::other(
                "the sandbox process reported a setup failure that could not be decoded",
            )));
        }
        Some(StatusReport::ExecStarted) | None => {}
    }
    let raw_status = match wait_status.expect("loop exits only once wait_status is set") {
        Ok(s) => s,
        Err(e) => return Err(ExecError::Spawn(e)),
    };

    // Read the watchdog's claim after the abort: the lock serializes
    // with a kill in flight, and a claim exists only when the kill
    // actually acted on the live tree.
    let kill_claim = *limit_claim.lock().unwrap_or_else(|e| e.into_inner());
    let exit = map_exit(raw_status, kill_claim);
    let outputs = {
        let request = request.clone();
        tokio::task::spawn_blocking(move || collect_outputs(&request))
            .await
            .map_err(|e| ExecError::Spawn(std::io::Error::other(e)))?
    };

    Ok(ExecutionOutcome {
        exit,
        outputs,
        start,
        stop,
    })
}

// ---------------------------------------------------------------------------
// Forked-process side.
// ---------------------------------------------------------------------------

/// Fork the intermediate process. Returns its pid on the parent side;
/// never returns on the child side.
///
/// Runs on a `spawn_blocking` thread. Everything the child needs is
/// either in the plan (pre-built C strings, the seccomp program) or
/// built here before the fork (the execve pointer arrays).
fn spawn_intermediate(plan: &SandboxPlan, fds: &ChildFds) -> std::io::Result<nix::unistd::Pid> {
    let ptrs = plan.exec.ptr_arrays();
    // SAFETY: the child branch only calls async-signal-safe code (raw
    // syscalls over pre-built buffers; see child.rs) before `execve` or
    // `_exit`, which is the contract for forking a multi-threaded
    // process.
    match unsafe { libc::fork() } {
        -1 => Err(std::io::Error::last_os_error()),
        0 => intermediate_main(plan, fds, ptrs.argv.as_slice(), ptrs.envp.as_slice()),
        pid => Ok(nix::unistd::Pid::from_raw(pid)),
    }
}

/// The intermediate process: enter the namespaces, fork the sandbox
/// child (pid 1 of the new PID namespace), close the pipe copies, wait,
/// forward the exit status. Never returns.
///
/// Async-signal-safe: no allocation, no panicking paths — every error
/// is reported through the status pipe and `_exit`.
fn intermediate_main(
    plan: &SandboxPlan,
    fds: &ChildFds,
    argv: &[*const libc::c_char],
    envp: &[*const libc::c_char],
) -> ! {
    // Shed every inherited fd outside the keep set before anything
    // else: the parent's ends of every pipe (including the go pipe's
    // write end), the pty master, the async runtime's internals. This
    // is what makes go-pipe EOF a real parent-death signal — the read
    // below would otherwise wait on a write end this very process
    // holds — and it caps what the sandbox child can inherit at the
    // keep set.
    // r[impl builder.exec.fd-keep-set+1]
    if let Err(err) = child::shed_inherited_fds(fds) {
        child::report_failure_and_exit(fds.status_pipe_w, &err);
    }
    if let Err(err) = child::enter_namespaces(plan, fds) {
        child::report_failure_and_exit(fds.status_pipe_w, &err);
    }
    // SAFETY: this process is single-threaded (it is itself a fresh
    // fork); the grandchild branch only runs async-signal-safe code.
    match unsafe { libc::fork() } {
        -1 => {
            let err = SetupError {
                phase: child::SetupPhase::ForkSandboxChild,
                errno: Errno::last() as i32,
                detail: 0,
            };
            child::report_failure_and_exit(fds.status_pipe_w, &err);
        }
        0 => {
            // The sandbox child. Only returns on failure.
            let err = child::setup_and_exec(plan, fds, argv, envp);
            child::report_failure_and_exit(fds.status_pipe_w, &err);
        }
        grandchild => {
            // Hand the parent its recycle-proof handle to the build
            // principal — the freshly forked sandbox child — before
            // shedding anything: pid (host namespace; unshare affects
            // only our children) plus a pidfd over SCM_RIGHTS. The
            // child is gated on the release byte, which the parent
            // sends only after using this message to place it in the
            // build sub-cgroup.
            // r[impl builder.exec.kill-targets-principal]
            if let Err(err) = send_placement(fds.placement_sock_w, grandchild) {
                child::report_failure_and_exit(fds.status_pipe_w, &err);
            }
            // This process needs no fds beyond stdio any more: it only
            // waits for the sandbox child and forwards its exit status.
            // One full best-effort sweep (instead of hand-enumerated
            // closes that would silently miss any fd added later) so
            // EOF on the parent side tracks the sandbox child alone:
            // the status pipe must EOF the moment the child execs (its
            // copy is close-on-exec), and the pty/pipes must EOF the
            // moment the child exits, not when this process does —
            // and the sweep takes this process's copies of the go pipe
            // and the placement socket with it, which is what keeps
            // the gated child's EOF semantics parent-only.
            // r[impl builder.exec.fd-keep-set+1]
            // SAFETY: close_range over a numeric range; this process
            // touches no fd >= 3 after this point.
            unsafe {
                libc::syscall(libc::SYS_close_range, 3u32, libc::c_uint::MAX, 0u32);
            }
            relay_wait_and_forward(grandchild);
        }
    }
}

/// THE relay loop: wait for the principal, forward its status, never
/// return. Async-signal-safe (raw syscalls over a stack buffer).
///
/// This exact function is the production relay
/// ([`intermediate_main`]'s tail) *and* the relay of the two-level
/// test harness — factored so the harness cannot drift from
/// production. The kill-corroboration race fixed by the principal
/// split lived precisely in this loop's [principal exit, forward]
/// window, and its three prior fixes were each validated against
/// single-process harnesses that could not express that window; the
/// shared loop makes the production topology the topology under test.
// r[impl builder.exec.kill-targets-principal]
fn relay_wait_and_forward(principal: libc::pid_t) -> ! {
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: waitpid into a stack buffer for a direct child.
        let rc = unsafe { libc::waitpid(principal, &raw mut status, 0) };
        if rc == principal {
            break;
        }
        if rc == -1 && Errno::last() != Errno::EINTR {
            // Cannot learn the principal's fate; 124 is this
            // executor's "supervision failed" convention and is
            // mapped to a plain exit code by the parent.
            child::exit_immediately(124);
        }
    }
    child::exit_immediately(relay_forward_code(status));
}

/// The relay's forwarding convention, as a pure function of the
/// principal's wait status: plain exit codes pass through, fatal
/// signals become `128 + signo`, anything else is the supervision-
/// failure 124.
fn relay_forward_code(status: libc::c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        // Forward a fatal signal as the conventional 128+N.
        128 + libc::WTERMSIG(status)
    } else {
        124
    }
}

// ---------------------------------------------------------------------------
// Parent-side helpers: process-tree ownership.
// ---------------------------------------------------------------------------

/// Where the forked process tree is in its lifecycle.
///
/// The state machine that makes the tree's ownership structural: a pid
/// can only become known by being adopted into an `Armed` state, kills
/// can only target an `Adopted` (never `Reaped`, i.e. possibly
/// recycled) pid, and a guard dropped before adoption (`Dead`) forces
/// the in-flight fork to destroy its own process.
#[derive(Debug)]
enum TreePhase {
    /// Guard armed; no process forked yet.
    Armed,
    /// A process tree exists, rooted at `pid`.
    Adopted {
        pid: nix::unistd::Pid,
        /// The tree has been killed (kills act once).
        killed: bool,
        /// A dedicated waitpid task owns reaping (the guard's drop
        /// must not blocking-reap).
        reaper_attached: bool,
        /// The build principal's pidfd, present from the placement
        /// handshake on: the recycle-proof handle that lets kills
        /// target the sandbox child directly instead of the relay.
        /// `None` before placement (no tenant instruction has run
        /// yet — the child is gated on the release byte).
        principal: Option<OwnedFd>,
    },
    /// The tree has been reaped. The pid may be recycled: nothing may
    /// ever signal it again.
    Reaped,
    /// The guard was dropped before any process was adopted. A fork
    /// still in flight must destroy the process it creates instead of
    /// publishing it.
    Dead,
}

/// The fork closure tried to adopt a pid after the caller was gone.
/// The process has already been killed and reaped by `adopt` itself.
#[derive(Debug)]
struct CallerGone;

/// Shared ownership state of the forked process tree.
///
/// Held by the [`ProcessTreeGuard`] (RAII), the fork closure (to adopt
/// the pid), the waitpid task (to mark the reap), and the supervision
/// loop (to kill on limits).
struct TreeState {
    phase: std::sync::Mutex<TreePhase>,
    cgroup: Option<PathBuf>,
    /// `<cgroup>/build` — the principal's kill scope, derived once so
    /// every kill path scopes identically.
    build_cgroup: Option<PathBuf>,
}

impl TreeState {
    fn new(cgroup: Option<PathBuf>) -> Arc<TreeState> {
        let build_cgroup = cgroup.as_ref().map(|c| c.join(BUILD_SUBCGROUP));
        Arc::new(TreeState {
            phase: std::sync::Mutex::new(TreePhase::Armed),
            cgroup,
            build_cgroup,
        })
    }

    /// Lock the phase, surviving poison: the state is a plain enum
    /// whose invariants cannot be torn by a panicking holder.
    fn lock(&self) -> std::sync::MutexGuard<'_, TreePhase> {
        self.phase.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Has the owning guard already been dropped?
    fn is_dead(&self) -> bool {
        matches!(*self.lock(), TreePhase::Dead)
    }

    /// Is the tree past the point where a kill could ever act —
    /// reaped, or never adopted because the caller vanished first?
    fn is_settled(&self) -> bool {
        matches!(*self.lock(), TreePhase::Reaped | TreePhase::Dead)
    }

    /// Has a kill already acted on the adopted tree? (Used to
    /// recognize the benign release-EPIPE race: the kill destroyed
    /// the gated child holding the pipe's only read end.)
    fn was_killed(&self) -> bool {
        matches!(*self.lock(), TreePhase::Adopted { killed: true, .. })
    }

    /// Publish a freshly forked pid. **The only path by which a pid
    /// becomes known to the rest of the executor.**
    ///
    /// If the guard was dropped while the fork was in flight, the
    /// process must not exist: it is killed and (blocking) reaped
    /// right here — we are on the fork's blocking thread — and the
    /// caller gets [`CallerGone`].
    // r[impl builder.exec.tree-ownership]
    fn adopt(&self, pid: nix::unistd::Pid) -> Result<(), CallerGone> {
        let mut phase = self.lock();
        match *phase {
            TreePhase::Armed => {
                *phase = TreePhase::Adopted {
                    pid,
                    killed: false,
                    reaper_attached: false,
                    principal: None,
                };
                Ok(())
            }
            TreePhase::Dead => {
                drop(phase);
                // Caller gone: this process must not outlive the fork
                // closure. Kill (cgroup first — the fork inherits the
                // caller's cgroup assignment only after the attach, so
                // the direct SIGKILL is what matters here) and reap so
                // no zombie outlives us either.
                kill_pid_and_cgroup(self.cgroup.as_deref(), pid);
                blocking_reap(pid);
                Err(CallerGone)
            }
            TreePhase::Adopted { .. } | TreePhase::Reaped => {
                // A second adoption is a programming error in this
                // module; there is exactly one fork per TreeState.
                debug_assert!(false, "double adoption of a process tree");
                Ok(())
            }
        }
    }

    /// A dedicated waitpid task now owns reaping: the guard's drop only
    /// needs to kill, never to reap.
    fn attach_reaper(&self) {
        if let TreePhase::Adopted {
            reaper_attached, ..
        } = &mut *self.lock()
        {
            *reaper_attached = true;
        }
    }

    /// The waitpid task reaped the tree: the pid may be recycled, and
    /// nothing may ever signal it again.
    fn mark_reaped(&self) {
        *self.lock() = TreePhase::Reaped;
    }

    /// Store the build principal's pidfd received from the placement
    /// handshake. From here on, kills can target the principal
    /// directly.
    ///
    /// If a kill (or the guard's drop) already acted by the time
    /// placement completes, the tree-level kill covered the principal
    /// — it was still cgroup-confined under `<cg>` and the relay's
    /// death cascades via `PR_SET_PDEATHSIG` — but send the pidfd
    /// SIGKILL anyway as belt-and-braces (a no-op on a dead process;
    /// pidfds never recycle) and discard the handle.
    // r[impl builder.exec.kill-targets-principal]
    fn place_principal(&self, pidfd: OwnedFd) {
        let mut phase = self.lock();
        match &mut *phase {
            TreePhase::Adopted {
                killed: false,
                principal,
                ..
            } => {
                *principal = Some(pidfd);
            }
            _ => {
                drop(phase);
                pidfd_kill(&pidfd);
            }
        }
    }

    /// Kill the build, once, and never after the tree was reaped.
    /// Returns the target the kill *acted* on — `None` when the tree
    /// was already settled or killed. Callers attributing an exit
    /// outcome to their kill (the [`LimitWatchdog`]) record their
    /// claim only on `Some`, tagged with the returned target: a kill
    /// that no-opped must not override the natural exit, and the
    /// target decides which wait statuses can corroborate the claim.
    ///
    /// **Post-placement** ([`KillTarget::Principal`]): write
    /// `<cg>/build/cgroup.kill` (the principal's whole subtree,
    /// including fork bombs) and `pidfd_send_signal` the principal —
    /// and *never* signal the relay. The relay survives by
    /// construction, reaps the principal, and forwards its true
    /// status: `128+9` if our kill won, the natural status if the
    /// principal had already exited. The kill machinery can no longer
    /// destroy the evidence it is judged by (merged_bug_046's window
    /// is unexpressible, not narrowed). A relay that then fails to
    /// forward within the escalation grace is taken down by
    /// [`TreeState::escalate_relay`], claim-free.
    ///
    /// **Pre-placement** ([`KillTarget::Tree`]): the legacy whole-tree
    /// kill — `<cg>/cgroup.kill` (recursive, so it covers a
    /// just-populated `<cg>/build` too) plus a direct SIGKILL of the
    /// relay, whose death cascades to a mid-setup sandbox child via
    /// `PR_SET_PDEATHSIG`. Safe to aim at the relay here because the
    /// release gate is still closed: no tenant instruction has run,
    /// so no completed build exists to misattribute.
    // r[impl builder.exec.tree-ownership]
    // r[impl builder.exec.kill-targets-principal]
    fn kill_tree(&self) -> Option<KillTarget> {
        let mut phase = self.lock();
        if let TreePhase::Adopted {
            pid,
            killed,
            principal,
            ..
        } = &mut *phase
            && !*killed
        {
            *killed = true;
            let pid = *pid;
            // The kill happens under the lock: cheap (cgroupfs writes
            // and a signal), and it means mark_reaped can never
            // interleave between the killed=true flip and the signal.
            if let Some(pidfd) = principal {
                if let Some(build_cgroup) = self.build_cgroup.as_deref() {
                    let _ = std::fs::write(build_cgroup.join("cgroup.kill"), "1");
                }
                pidfd_kill(pidfd);
                Some(KillTarget::Principal)
            } else {
                kill_pid_and_cgroup(self.cgroup.as_deref(), pid);
                Some(KillTarget::Tree)
            }
        } else {
            None
        }
    }

    /// Escalation backstop for a placed kill: the relay had one job —
    /// reap the killed principal and forward `137` — and has not done
    /// it within the grace period. Take the whole tree down (relay
    /// included) so the execution terminates.
    ///
    /// Deliberately **claim-free**: this path records nothing, and the
    /// `signaled(SIGKILL)` relay status it produces does not
    /// corroborate a `Principal` claim — the verdict honestly degrades
    /// to `Signaled(9)` instead of a manufactured `TimedOut`/`Silent`.
    /// (A relay stuck past the grace is a supervision failure, not
    /// evidence about the build.)
    // r[impl builder.exec.kill-targets-principal]
    fn escalate_relay(&self) {
        let phase = self.lock();
        if let TreePhase::Adopted { pid, .. } = &*phase {
            kill_pid_and_cgroup(self.cgroup.as_deref(), *pid);
        }
    }
}

/// RAII owner of the forked process tree.
///
/// Created — and therefore armed — *before* the fork is submitted to
/// the blocking pool, so there is no instant at which a forked process
/// exists without a guard. Dropping it (caller cancellation at any
/// await point, or an early error return) kills whatever was adopted
/// and, when no waitpid task is attached yet, reaps it too.
// r[impl builder.exec.tree-ownership]
struct ProcessTreeGuard {
    state: Arc<TreeState>,
}

impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        // Deliberately blocking inside Drop: there is no async Drop,
        // and both the kill (a cgroupfs write + kill(2)) and the
        // no-reaper reap (a waitpid on a freshly SIGKILLed tree) are
        // bounded.
        let mut phase = self.state.lock();
        match &mut *phase {
            TreePhase::Armed => {
                // Nothing adopted yet. Mark Dead so an in-flight fork
                // closure destroys its own process instead of
                // publishing it.
                *phase = TreePhase::Dead;
            }
            TreePhase::Adopted {
                pid,
                killed,
                reaper_attached,
                ..
            } => {
                let pid = *pid;
                let attached = *reaper_attached;
                if !*killed {
                    *killed = true;
                    kill_pid_and_cgroup(self.state.cgroup.as_deref(), pid);
                }
                if !attached {
                    // Nobody else will reap it; do it here so the
                    // caller's drop leaves neither a process nor a
                    // zombie behind. Mark Reaped first so nothing else
                    // can signal the (soon recyclable) pid.
                    *phase = TreePhase::Reaped;
                    drop(phase);
                    blocking_reap(pid);
                }
                // With a reaper attached the waitpid task finishes the
                // job (it survives this drop: blocking tasks detach).
            }
            TreePhase::Reaped | TreePhase::Dead => {}
        }
    }
}

/// Name of the principal's sub-cgroup under the caller's cgroup. The
/// parent creates `<cg>/build` before the fork and moves the sandbox
/// child into it at placement; the intermediate stays a direct member
/// of `<cg>`. No controllers are delegated into it (`<cg>` keeps its
/// `subtree_control` empty), so `<cg>`'s limits and accounting cover
/// both levels hierarchically while `<cg>/build/cgroup.kill` scopes a
/// kill to exactly the build.
/// Exported so the builder's own enforcement writers (the log-cap
/// arms in its stderr loop) can aim at the SAME principal kill scope
/// instead of the build root: a cap kill that hits `<cg>` kills the
/// relay with the principal and destroys the forwarded wait status —
/// the exact evidence-destruction `builder.exec.kill-targets-principal`
/// exists to forbid. Writers outside rio-exec stay UNCLAIMED (the
/// verdict authority for a builder cap kill is the builder's own cap
/// flag, not a kill claim); routing them through the claim machinery
/// is the named follow-up tail (`KillReason::LogLimit`).
pub const BUILD_SUBCGROUP: &str = "build";

/// SIGKILL via pidfd: recycle-proof by construction (a pidfd names the
/// process instance, never a reusable number) and namespace-correct
/// (works on a pid-namespace init from outside). Best-effort like
/// every other kill primitive here; a dead target is a no-op.
fn pidfd_kill(pidfd: &OwnedFd) {
    // SAFETY: pidfd_send_signal(2) on an owned pidfd; no memory
    // preconditions (null siginfo = same semantics as kill(2)).
    unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0u32,
        );
    }
}

/// Kill a process tree rooted at `pid`: `cgroup.kill` when a cgroup
/// exists, then always a direct SIGKILL of the root (see
/// [`TreeState::kill_tree`] for why both).
fn kill_pid_and_cgroup(cgroup: Option<&Path>, pid: nix::unistd::Pid) {
    if let Some(cgroup) = cgroup {
        let _ = std::fs::write(cgroup.join("cgroup.kill"), "1");
    }
    // SAFETY: SIGKILL to a pid that is either live or an un-reaped
    // zombie. The phase machine flips to `Reaped` BEFORE the wait
    // status is consumed (`observe_then_reap`; the guard's no-reaper
    // drop marks before its blocking reap too), and every kill path
    // consults the phase under the same lock — so a pid is never
    // signaled after it became recyclable. Signaling a zombie is a
    // no-op; a pid already dying from the cgroup kill ignores the
    // extra signal.
    unsafe {
        libc::kill(pid.as_raw(), libc::SIGKILL);
    }
}

/// `waitpid` until the process is gone, ignoring errors (ECHILD means
/// someone else already reaped it, which is equally final).
fn blocking_reap(pid: nix::unistd::Pid) {
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: waitpid into a stack buffer for a direct child.
        let rc = unsafe { libc::waitpid(pid.as_raw(), &raw mut status, 0) };
        if rc == pid.as_raw() || (rc == -1 && Errno::last() != Errno::EINTR) {
            return;
        }
    }
}

/// `waitpid` the intermediate, retrying on EINTR. Returns the raw wait
/// status.
fn wait_for(pid: nix::unistd::Pid) -> Result<i32, std::io::Error> {
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: waitpid into a stack buffer for a direct child.
        let rc = unsafe { libc::waitpid(pid.as_raw(), &raw mut status, 0) };
        if rc == pid.as_raw() {
            return Ok(status);
        }
        if rc == -1 {
            let errno = Errno::last();
            if errno == Errno::EINTR {
                continue;
            }
            return Err(std::io::Error::from(errno));
        }
    }
}

/// Observe the intermediate's exit *without* consuming it
/// (`waitid(WEXITED | WNOWAIT)`), flip the tree phase to `Reaped` while
/// the zombie still owns the pid, and only then perform the real reap.
///
/// The ordering is the point: by the instant the wait status is
/// consumed — the only instant after which the kernel may recycle the
/// pid — the phase machine already says `Reaped`, so
/// [`TreeState::kill_tree`] (which consults the phase under the same
/// lock) can never signal a recycled pid. A kill that races the
/// [exit, observe] window signals a zombie, which is harmless: the
/// zombie holds the pid until we consume the status. The recorded
/// reason such a racing kill leaves behind is discarded by
/// [`map_exit`]'s corroboration layer.
///
/// If `waitid` itself fails for any reason other than `EINTR`, fall
/// back to the legacy reap-then-mark order: a tree must never be
/// marked `Reaped` while the process may still be alive (the mark is
/// what disarms every kill path).
// r[impl builder.exec.limits-isolated+2]
fn observe_then_reap(tree: &TreeState, pid: nix::unistd::Pid) -> Result<i32, std::io::Error> {
    loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waitid with WEXITED|WNOWAIT into a zeroed stack
        // siginfo for a direct child; WNOWAIT leaves the child
        // reapable (the status is not consumed).
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid.as_raw() as libc::id_t,
                &raw mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if rc == 0 {
            // Exit observed; the zombie still owns the pid. Disarm the
            // kill paths first, then consume the status.
            tree.mark_reaped();
            return wait_for(pid);
        }
        let errno = Errno::last();
        if errno == Errno::EINTR {
            continue;
        }
        // waitid failed (it should not, for a direct child): legacy
        // order — reap, then mark.
        let status = wait_for(pid);
        tree.mark_reaped();
        return status;
    }
}

/// Can `raw_status` — the *relay's* wait status — have been produced
/// by the executor's own kill, given what that kill targeted?
///
/// [`KillTarget::Principal`]: the kill signaled only the build
/// principal and its sub-cgroup; the relay survived by construction
/// and forwarded what the principal died of. Our SIGKILL therefore
/// produces exactly the forwarded `128 + 9` — and nothing else. A
/// relay that itself died of `SIGKILL` was killed by something that
/// was NOT this kill (external interference, the OOM killer, or our
/// own claim-free escalation), so the claim is not corroborated and
/// the natural mapping applies. This is the closure of
/// merged_bug_046: the kill machinery cannot manufacture its own
/// corroborating evidence, because the one status that corroborates
/// can only be produced *through* the surviving relay.
///
/// [`KillTarget::Tree`]: the legacy pre-placement kill signals the
/// relay (and everything below it via cgroup/pdeathsig cascade), so
/// both shapes corroborate — direct `SIGKILL` death or the forwarded
/// `128 + 9`. Safe precisely because the release gate was still
/// closed when such a kill acted: no tenant instruction had run, so
/// there is no completed build to misattribute.
// r[impl builder.exec.limits-isolated+2]
// r[impl builder.exec.kill-targets-principal]
fn corroborates(target: KillTarget, raw_status: i32) -> bool {
    let forwarded_sigkill =
        libc::WIFEXITED(raw_status) && libc::WEXITSTATUS(raw_status) == 128 + libc::SIGKILL;
    match target {
        KillTarget::Principal => forwarded_sigkill,
        KillTarget::Tree => {
            forwarded_sigkill
                || (libc::WIFSIGNALED(raw_status) && libc::WTERMSIG(raw_status) == libc::SIGKILL)
        }
    }
}

/// Map the relay's raw wait status (plus any kill claim the executor
/// recorded) to the caller-facing [`ExitOutcome`].
///
/// Two-layer verdict contract, deciding layer. A recorded kill claim
/// exists only because the watchdog issued a kill against a tree the
/// phase machine still believed live — but the phase machine
/// necessarily lags the kernel's exit event, so the claim is honored
/// only when the wait status is one the kill, as targeted, could
/// actually have produced ([`corroborates`]). Any other status means
/// the kill lost the race to a natural exit, and the natural exit
/// wins — a clean `exit(0)` can never be relabeled `TimedOut`/
/// `Silent`/`LogLimitExceeded` by a deadline that fired into the
/// [exit observed, phase flipped] window, **including** when the
/// deadline fired inside the [principal exit, relay forward] window:
/// a post-placement kill never signals the relay, so the relay always
/// survives to deliver the natural status that defeats the stale
/// claim (merged_bug_046's third window, closed structurally).
///
/// Otherwise the relay's forwarding convention applies: exit codes
/// `129..=192` are fatal signals forwarded as `128 + signo`,
/// everything else is a plain exit code.
///
/// # Residual states (final enumeration)
///
/// 1. **Natural 137.** A build that genuinely exits with code `137`
///    (or is SIGKILLed by something else inside the sandbox, e.g. its
///    own watchdog) while an executor kill raced it is attributed to
///    the executor's kill — the forwarded status is bit-identical.
///    Cost of the forwarding convention; bounded by the canary metric
///    `rio_builder_kill_verdict_outputs_present_total` (a kill verdict
///    whose declared outputs all materialized is the coincidence
///    signature). Irreducible at the wait level: distinguishing "our
///    pidfd SIGKILL" from "any other SIGKILL of the principal in the
///    same instant" requires kernel-level exit-reason attribution
///    (who sent the signal), which `waitpid`/`waitid` do not expose.
/// 2. **Relay stuck past grace.** A relay that fails to forward
///    within [`RELAY_ESCALATION_GRACE`] is taken down claim-free; its
///    `signaled(SIGKILL)` status does not corroborate the `Principal`
///    claim, so the verdict degrades to `Signaled(9)` — the claim is
///    DROPPED, never honored on manufactured evidence. A stuck relay
///    is a supervision failure and is reported as one, not as a
///    limit verdict.
/// 3. **External SIGKILL of the relay.** Same shape as (2) from
///    outside (node OOM killer, operator): the forwarded status is
///    lost with the relay, and the outcome is the honest
///    `Signaled(9)`. Irreducible without exit-reason attribution —
///    and biased the safe way: uncertainty degrades toward "relay
///    died", never toward relabeling a build.
// r[impl builder.exec.limits-isolated+2]
fn map_exit(raw_status: i32, kill: Option<KillClaim>) -> ExitOutcome {
    if let Some(kill) = kill
        && corroborates(kill.target, raw_status)
    {
        return kill.reason.into();
    }
    if libc::WIFEXITED(raw_status) {
        let code = libc::WEXITSTATUS(raw_status);
        if (129..=192).contains(&code) {
            ExitOutcome::Signaled(code - 128)
        } else {
            ExitOutcome::Exited(code)
        }
    } else if libc::WIFSIGNALED(raw_status) {
        ExitOutcome::Signaled(libc::WTERMSIG(raw_status))
    } else {
        // Stopped/continued statuses cannot reach here (waitpid is
        // called without WUNTRACED/WCONTINUED); treat defensively as a
        // failure rather than panicking in the supervision path.
        ExitOutcome::Exited(-1)
    }
}

/// Translate a declared (sandbox-absolute) output path to the host path
/// it materializes at, through the writable mount that contains it.
fn output_host_path(request: &ExecutionRequest, output: &Path) -> Option<PathBuf> {
    let mount = request.writable_mount_for(output)?;
    let rel = output.strip_prefix(&mount.target).ok()?;
    Some(mount.source.join(rel))
}

/// `lstat` every declared output through its host-side path.
fn collect_outputs(request: &ExecutionRequest) -> Vec<OutputReport> {
    use std::os::unix::fs::MetadataExt as _;
    request
        .declared_outputs
        .iter()
        .map(|path| {
            let host_path = output_host_path(request, path)
                // validate() guarantees a writable mount exists for
                // every declared output.
                .expect("validated request had an unmappable declared output");
            match std::fs::symlink_metadata(&host_path) {
                Ok(meta) => OutputReport {
                    path: path.clone(),
                    host_path,
                    exists: true,
                    metadata: Some(OutputMetadata {
                        mode: meta.mode(),
                        uid: meta.uid(),
                        size: meta.size(),
                        file_type: if meta.file_type().is_symlink() {
                            OutputFileType::Symlink
                        } else if meta.file_type().is_dir() {
                            OutputFileType::Directory
                        } else if meta.file_type().is_file() {
                            OutputFileType::Regular
                        } else {
                            OutputFileType::Other
                        },
                    }),
                },
                Err(_) => OutputReport {
                    path: path.clone(),
                    host_path,
                    exists: false,
                    metadata: None,
                },
            }
        })
        .collect()
}

/// A `sleep_until` that is pending forever when there is no deadline.
/// Lets the watchdog's select arms stay declarative.
async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Create one O_CLOEXEC pipe pair as `(read, write)`.
fn make_pipe() -> nix::Result<(OwnedFd, OwnedFd)> {
    nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
}

// ---------------------------------------------------------------------------
// Principal placement: the relay-handed pidfd.
// ---------------------------------------------------------------------------

/// Create the placement socketpair as `(parent_recv, child_send)`.
///
/// `SOCK_SEQPACKET` so the single placement message keeps its boundary;
/// `SOCK_CLOEXEC` for hygiene (the sandbox child's own sweep is the
/// real guarantee — fork inheritance ignores CLOEXEC).
fn make_placement_socketpair() -> std::io::Result<(OwnedFd, OwnedFd)> {
    use std::os::fd::FromRawFd as _;
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: socketpair(2) into a stack array; on success both fds
    // are fresh and owned here, immediately wrapped.
    let rc = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fresh fds from a successful socketpair, owned exactly once.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// The placement message's data payload: the sandbox child's pid in
/// the *host* PID namespace (the intermediate unshares only for its
/// children, so its `fork` return value is host-namespace), as native
/// bytes. The pidfd rides alongside as `SCM_RIGHTS`.
const PLACEMENT_MSG_LEN: usize = std::mem::size_of::<libc::pid_t>();

/// Intermediate side: open a pidfd for the freshly forked sandbox
/// child and send `(pid, pidfd)` to the parent over the placement
/// socket.
///
/// The pid datum alone would be a recycle hazard everywhere EXCEPT
/// here: this process is the child's parent and has not reaped it, so
/// the kernel cannot recycle the pid while the message is in flight —
/// dead or alive, the child holds it (zombies pin their pid). The
/// pidfd is what the *parent* keeps long-term; the raw pid is only
/// used for the immediate sub-cgroup attach.
///
/// Async-signal-safe: raw syscalls over stack buffers only.
fn send_placement(sock: RawFd, pid: libc::pid_t) -> Result<(), SetupError> {
    // SAFETY: pidfd_open(2) for a direct, un-reaped child.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
    if pidfd < 0 {
        return Err(SetupError::new(SetupPhase::PlacementSend, Errno::last()));
    }
    let pidfd = pidfd as libc::c_int;

    let payload = pid.to_ne_bytes();
    let mut iov = libc::iovec {
        iov_base: payload.as_ptr().cast_mut().cast(),
        iov_len: payload.len(),
    };
    // One fd's worth of control message, in a zeroed, aligned buffer.
    // 64 bytes, u64-aligned: comfortably ≥ CMSG_SPACE(4) (24 on LP64).
    let mut cmsg_buf = [0u64; 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr().cast();
    // SAFETY: CMSG_* macros over the zeroed header/buffer just built.
    let rc = unsafe {
        msg.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32) as _;
        let cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::c_int>(), pidfd);
        libc::sendmsg(sock, &raw const msg, 0)
    };
    let send_errno = Errno::last();
    // SAFETY: closing the fd this function opened; the kernel holds
    // its own reference inside the queued message.
    unsafe { libc::close(pidfd) };
    if rc != payload.len() as isize {
        return Err(SetupError::new(
            SetupPhase::PlacementSend,
            if rc < 0 { send_errno } else { Errno::EPROTO },
        ));
    }
    Ok(())
}

/// Parent side: receive the `(pid, pidfd)` placement message. Blocking
/// — call from `spawn_blocking`. EOF (the intermediate died before
/// sending — its setup failed) and malformed messages surface as
/// errors for the abort path; the status pipe carries the real
/// diagnosis.
fn recv_placement(sock: OwnedFd) -> std::io::Result<(libc::pid_t, OwnedFd)> {
    use std::os::fd::FromRawFd as _;
    let mut payload = [0u8; PLACEMENT_MSG_LEN];
    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    };
    let mut cmsg_buf = [0u64; 4]; // ≥ CMSG_SPACE(4), aligned
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&cmsg_buf) as _;
    let n = loop {
        // SAFETY: recvmsg into the stack buffers wired above;
        // MSG_CMSG_CLOEXEC so the received pidfd cannot leak into a
        // concurrent fork+exec elsewhere in the process.
        let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &raw mut msg, libc::MSG_CMSG_CLOEXEC) };
        if n >= 0 {
            break n as usize;
        }
        if Errno::last() != Errno::EINTR {
            return Err(std::io::Error::last_os_error());
        }
    };
    // Walk the control messages for the SCM_RIGHTS fd FIRST: even a
    // malformed message may have transferred an fd, and an unclaimed
    // fd is a leak.
    let mut received_fd: Option<OwnedFd> = None;
    // SAFETY: CMSG_* walk over the msghdr recvmsg just filled.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let fd = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::c_int>());
                let fd = OwnedFd::from_raw_fd(fd);
                if received_fd.replace(fd).is_some() {
                    return Err(std::io::Error::other(
                        "placement message carried more than one fd",
                    ));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&raw mut msg, cmsg);
        }
    }
    if n == 0 {
        // EOF: every write end is gone — the intermediate died before
        // placing the principal (its setup failed; the status pipe has
        // the typed error).
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "the sandbox exited before the build principal was placed",
        ));
    }
    if n != PLACEMENT_MSG_LEN || (msg.msg_flags & libc::MSG_CTRUNC) != 0 {
        return Err(std::io::Error::other("malformed placement message"));
    }
    let Some(pidfd) = received_fd else {
        return Err(std::io::Error::other("placement message carried no pidfd"));
    };
    Ok((libc::pid_t::from_ne_bytes(payload), pidfd))
}

/// Create `<cg>/build`, tolerating a leftover from a previous attempt
/// against the same caller-owned cgroup (`EEXIST` — the directory is
/// just a kill scope; an empty pre-existing one is as good as fresh).
fn create_build_subcgroup(cgroup: &Path) -> std::io::Result<()> {
    match std::fs::create_dir(cgroup.join(BUILD_SUBCGROUP)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// Is a `cgroup.procs` write failure tolerable at placement time?
///
/// `ESRCH`/`EINVAL` mean the principal already exited (its zombie pins
/// the pid but is no longer attachable) — the build is over and the
/// relay is about to forward its true status, so placement has nothing
/// left to scope. Anything else (`EACCES`, `ENOENT`, `EBUSY`, …) means
/// the sub-cgroup itself is broken and limit kills could not target
/// the principal: fail the execution rather than run unsupervised.
fn placement_attach_tolerable(err: &std::io::Error) -> bool {
    matches!(err.raw_os_error(), Some(libc::ESRCH) | Some(libc::EINVAL))
}

/// Annotate an io::Error from early sandbox spawning with what was
/// being attempted.
fn spawn_err<E: Into<std::io::Error>>(what: &'static str) -> impl FnOnce(E) -> ExecError {
    move |e| {
        let e = e.into();
        ExecError::Spawn(std::io::Error::new(
            e.kind(),
            format!("failed to {what}: {e}"),
        ))
    }
}

/// Resolve an indexed setup phase to the path it was operating on and
/// emit the structured warn plus the typed error. Shared by the
/// post-go interpret arm and the pre-go abort path, so a setup failure
/// is reported identically wherever it surfaces.
// r[impl builder.exec.setup-error-surfaced]
fn setup_failure_to_error(
    err: SetupError,
    bind_targets: &[PathBuf],
    special_targets: &[PathBuf],
) -> ExecError {
    let entry = match err.phase {
        SetupPhase::Bind | SetupPhase::BindRemount => bind_targets.get(usize::from(err.detail)),
        SetupPhase::MountSpecial => special_targets.get(usize::from(err.detail)),
        _ => None,
    };
    tracing::warn!(
        phase = err.phase.describe(),
        errno = err.errno,
        detail = err.detail,
        path = entry.map(|p| p.display().to_string()),
        "sandbox setup failed"
    );
    ExecError::Setup(err)
}

/// The triplet that exists from the moment the sandbox is forked: the
/// RAII kill guard, the reaper task, and the status-pipe reader. Built
/// atomically at fork time, so every teardown path owns all three —
/// there is no pre-go window in which a setup failure can be reported
/// but nothing is positioned to read it.
// r[impl builder.exec.setup-error-surfaced]
// TODO: F4 (round-15 C7 follow-up) — promote this handle to a full
// typestate (Forked -> Going -> Supervised) so pre-go teardown, the go
// release, and supervision-loop adoption are distinct types and a
// handle cannot be aborted twice or supervised before release.
struct SpawnedSandbox {
    tree_guard: ProcessTreeGuard,
    wait_task: tokio::task::JoinHandle<Result<i32, std::io::Error>>,
    status_task: tokio::task::JoinHandle<StatusReport>,
}

impl SpawnedSandbox {
    /// Early-error teardown between fork and the supervision loop:
    /// kill the tree, reap it, then drain the status pipe and prefer a
    /// decoded setup failure over the generic abort cause — the child
    /// usually died of a reportable setup error first, and that error
    /// (not the cgroup/go-pipe symptom it caused) is the diagnosis.
    /// The error classification is unchanged either way (both arms are
    /// worker-local infrastructure failures); this is diagnosis
    /// fidelity, not routing.
    ///
    /// The drain is bounded: by the time the reap completes, every
    /// write end of the status pipe is closed (the parent's copy
    /// dropped inside the fork closure; the children's copies died
    /// with the tree), so the reader sees a full report or EOF — it
    /// cannot block.
    // r[impl builder.exec.setup-error-surfaced]
    async fn abort(
        self,
        error: std::io::Error,
        what: &'static str,
        bind_targets: &[PathBuf],
        special_targets: &[PathBuf],
    ) -> ExecError {
        let SpawnedSandbox {
            tree_guard,
            wait_task,
            status_task,
        } = self;
        // Dropping the guard kills the adopted tree; the attached wait
        // task then reaps it (awaited so the tree is fully gone before
        // the error propagates).
        drop(tree_guard);
        let _ = wait_task.await;
        match status_task.await.unwrap_or(StatusReport::Corrupt) {
            StatusReport::SetupFailed(err) => {
                setup_failure_to_error(err, bind_targets, special_targets)
            }
            // EOF is the "exec'd" convention on the happy path; here it
            // only means the child died without reporting (usually from
            // our own kill). Keep the generic abort cause for it and
            // for undecodable reports.
            StatusReport::ExecStarted | StatusReport::Corrupt => ExecError::Spawn(
                std::io::Error::new(error.kind(), format!("failed to {what}: {error}")),
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Capture plumbing.
// ---------------------------------------------------------------------------

/// The capture fds, child side + parent side, before the fork.
struct CaptureFds {
    /// Becomes the child's fd 1 (and 2 for merged capture).
    child_stdout: OwnedFd,
    /// Becomes the child's fd 2 for separate-pipes capture.
    child_stderr: Option<OwnedFd>,
    /// Parent-side read ends, tagged with the stream they carry.
    parent: Vec<(OwnedFd, LogStream)>,
}

/// The parent-side read ends after the split.
struct ParentCapture(Vec<(OwnedFd, LogStream)>);

/// The child-side write ends after the split.
struct ChildCapture {
    stdout: OwnedFd,
    stderr: Option<OwnedFd>,
}

/// Owners of every child-side fd, moved INTO the fork closure.
///
/// Because these `OwnedFd`s live inside the `spawn_blocking` closure,
/// dropping the [`execute`] future cannot close them — and recycle
/// their numbers into another execution's pipes — while a fork that
/// references those numbers is in flight. They drop (closing the
/// parent's copies of the child-side ends) at the end of the closure,
/// the same protocol position as the old post-await drops, but no
/// longer droppable early.
// r[impl builder.exec.tree-ownership]
struct ChildFdOwners {
    status_w: OwnedFd,
    go_r: OwnedFd,
    placement_w: OwnedFd,
    capture: ChildCapture,
}

impl ChildFdOwners {
    /// The raw-fd view handed to the forked children. Valid for as
    /// long as `self` lives (which is the whole fork closure).
    fn child_fds(&self) -> ChildFds {
        ChildFds {
            status_pipe_w: self.status_w.as_raw_fd(),
            go_pipe_r: self.go_r.as_raw_fd(),
            placement_sock_w: self.placement_w.as_raw_fd(),
            stdout_fd: self.capture.stdout.as_raw_fd(),
            stderr_fd: self
                .capture
                .stderr
                .as_ref()
                .unwrap_or(&self.capture.stdout)
                .as_raw_fd(),
        }
    }
}

impl CaptureFds {
    fn new(request: &ExecutionRequest, plan: &SandboxPlan) -> std::io::Result<CaptureFds> {
        match request.capture {
            OutputCapture::MergedPty => {
                let pty = nix::pty::openpty(None, None).map_err(std::io::Error::from)?;
                // Raw mode, set from the parent: no echo, no canonical
                // line editing, no NL→CRNL translation — the captured
                // bytes are exactly what the program wrote. The child
                // does not call tcsetattr.
                let mut termios =
                    nix::sys::termios::tcgetattr(&pty.slave).map_err(std::io::Error::from)?;
                nix::sys::termios::cfmakeraw(&mut termios);
                nix::sys::termios::tcsetattr(
                    &pty.slave,
                    nix::sys::termios::SetArg::TCSANOW,
                    &termios,
                )
                .map_err(std::io::Error::from)?;
                // Hygiene for the inherited fd: the slave belongs to the
                // sandbox uid and nobody else. Ownership changes need
                // privilege; skip them when running unprivileged (the
                // executor and the sandbox are the same user there).
                let _ = nix::sys::stat::fchmod(
                    &pty.slave,
                    nix::sys::stat::Mode::from_bits_truncate(0o600),
                );
                let _ = nix::unistd::fchown(
                    &pty.slave,
                    Some(nix::unistd::Uid::from_raw(plan.child.uid)),
                    Some(nix::unistd::Gid::from_raw(plan.child.gid)),
                );
                Ok(CaptureFds {
                    child_stdout: pty.slave,
                    child_stderr: None,
                    parent: vec![(pty.master, LogStream::Merged)],
                })
            }
            OutputCapture::SeparatePipes => {
                let (out_r, out_w) = make_pipe().map_err(std::io::Error::from)?;
                let (err_r, err_w) = make_pipe().map_err(std::io::Error::from)?;
                Ok(CaptureFds {
                    child_stdout: out_w,
                    child_stderr: Some(err_w),
                    parent: vec![(out_r, LogStream::Stdout), (err_r, LogStream::Stderr)],
                })
            }
        }
    }

    /// Split into the parent-side read ends and the child-side write
    /// ends. The child side moves into the fork closure (see
    /// [`ChildFdOwners`]).
    fn split(self) -> (ParentCapture, ChildCapture) {
        (
            ParentCapture(self.parent),
            ChildCapture {
                stdout: self.child_stdout,
                stderr: self.child_stderr,
            },
        )
    }
}

/// Spawn one blocking reader per capture fd, feeding raw chunks into
/// `chunk_tx`. The readers exit on EOF, on EIO (how a pty master
/// reports that every slave fd is gone), or when the channel closes.
///
/// Activity is recorded on the meter at *read* time, before the chunk
/// enters the channel: the silence clock and the log-volume cap track
/// what the program wrote even when every downstream consumer is
/// parked.
fn spawn_log_readers(
    capture: ParentCapture,
    chunk_tx: &mpsc::Sender<LogChunk>,
    meter: &Arc<ActivityMeter>,
) {
    for (fd, stream) in capture.0 {
        let tx = chunk_tx.clone();
        let meter = Arc::clone(meter);
        tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 8192];
            loop {
                match nix::unistd::read(&fd, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        // r[impl builder.exec.limits-isolated+2]
                        meter.record(n);
                        let chunk = LogChunk {
                            stream,
                            bytes: buf[..n].to_vec(),
                        };
                        if tx.blocking_send(chunk).is_err() {
                            break;
                        }
                    }
                    Err(Errno::EINTR) => continue,
                    // EIO from a pty master == all slave fds closed ==
                    // the process tree is gone. Anything else is also
                    // terminal for this reader.
                    Err(_) => break,
                }
            }
        });
    }
}

/// Read the status pipe to resolution: 8 bytes (setup failed) or EOF
/// (the program exec'd and the close-on-exec write end vanished).
fn read_status_pipe(fd: OwnedFd) -> StatusReport {
    let mut buf = [0u8; SETUP_ERROR_WIRE_LEN];
    let mut filled = 0;
    while filled < buf.len() {
        match nix::unistd::read(&fd, &mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(Errno::EINTR) => continue,
            Err(_) => break,
        }
    }
    match filled {
        0 => StatusReport::ExecStarted,
        SETUP_ERROR_WIRE_LEN => SetupError::from_bytes(&buf)
            .map(StatusReport::SetupFailed)
            .unwrap_or(StatusReport::Corrupt),
        _ => StatusReport::Corrupt,
    }
}

/// Cap on a single un-terminated line accumulating in [`LineSplitter`].
///
/// A program that streams bytes without ever emitting `\n` (`base64
/// -w0`, a binary dumped to stdout, a deliberate flood) would otherwise
/// grow the pending buffer without bound — memory charged to the
/// executor process, outside the build cgroup's accounting. When the
/// cap is reached the accumulated bytes are emitted as a (partial,
/// `terminated: false`) fragment so they flow through the normal event
/// pipeline and whatever log-volume limits the caller enforces there;
/// the fragment boundary travels on the event, so callers can treat
/// the split logical line as one unit.
const MAX_PENDING_LINE_BYTES: usize = 1 << 20;

/// Accumulates raw chunks per stream and emits complete lines as
/// [`ExecEvent::Log`]s. Lines are split on `\n`; a trailing `\r` (pty
/// raw mode passes through the `\r\n` the line discipline would have
/// produced) is stripped. A line that reaches [`MAX_PENDING_LINE_BYTES`]
/// without a terminator is emitted in fragments of that size, each
/// marked `terminated: false`; the event at the eventual `\n` (or the
/// EOF flush) closes the logical line. Callers that classify lines by
/// their head bytes use the flag to extend the head's classification
/// over its continuations.
#[derive(Default)]
struct LineSplitter {
    pending: Vec<(LogStream, Vec<u8>)>,
}

impl LineSplitter {
    fn push(&mut self, stream: LogStream, bytes: &[u8]) -> Vec<ExecEvent> {
        let buf = match self.pending.iter_mut().find(|(s, _)| *s == stream) {
            Some((_, buf)) => buf,
            None => {
                self.pending.push((stream, Vec::new()));
                &mut self.pending.last_mut().expect("entry was just pushed").1
            }
        };
        let mut events = Vec::new();
        for byte in bytes {
            if *byte == b'\n' {
                if buf.last() == Some(&b'\r') {
                    buf.pop();
                }
                events.push(ExecEvent::Log {
                    stream,
                    line: std::mem::take(buf),
                    terminated: true,
                });
            } else {
                buf.push(*byte);
                if buf.len() >= MAX_PENDING_LINE_BYTES {
                    // Cap-forced fragment: the logical line continues in
                    // the next event, so it is not terminated.
                    events.push(ExecEvent::Log {
                        stream,
                        line: std::mem::take(buf),
                        terminated: false,
                    });
                }
            }
        }
        events
    }

    /// Emit every stream's trailing unterminated line. Called after EOF
    /// so a final `printf` without a newline is not lost — under
    /// [`OutputCapture::SeparatePipes`] both stdout and stderr can end
    /// with a partial line, so all pending buffers are drained, not
    /// just the first.
    fn flush(&mut self) -> Vec<ExecEvent> {
        self.pending
            .iter_mut()
            .filter(|(_, buf)| !buf.is_empty())
            .map(|(stream, buf)| ExecEvent::Log {
                stream: *stream,
                line: std::mem::take(buf),
                // EOF flush of an unterminated trailing line: the
                // logical line never saw its `\n`.
                terminated: false,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;
    use crate::request::{Isolation, Limits, Mount, Personality, SandboxIdentity};

    // -- map_exit ----------------------------------------------------------

    /// Build a raw wait status the way the kernel encodes a normal exit.
    fn exited(code: i32) -> i32 {
        (code & 0xff) << 8
    }

    /// Build a raw wait status for death-by-signal.
    fn signaled(sig: i32) -> i32 {
        sig & 0x7f
    }

    #[test]
    fn map_exit_plain_codes() {
        assert_eq!(map_exit(exited(0), None), ExitOutcome::Exited(0));
        assert_eq!(map_exit(exited(7), None), ExitOutcome::Exited(7));
        assert_eq!(map_exit(exited(127), None), ExitOutcome::Exited(127));
    }

    #[test]
    fn map_exit_forwarded_signals() {
        // The intermediate forwards a SIGKILLed sandbox child as 137.
        assert_eq!(map_exit(exited(137), None), ExitOutcome::Signaled(9));
        assert_eq!(map_exit(exited(143), None), ExitOutcome::Signaled(15));
    }

    #[test]
    fn map_exit_direct_signal() {
        // The intermediate itself dying of a signal (the executor's own
        // no-cgroup kill path) is also a signal outcome.
        assert_eq!(map_exit(signaled(9), None), ExitOutcome::Signaled(9));
    }

    /// Shorthand claims for the corroboration matrix.
    fn principal(reason: KillReason) -> Option<KillClaim> {
        Some(KillClaim {
            reason,
            target: KillTarget::Principal,
        })
    }

    fn tree_claim(reason: KillReason) -> Option<KillClaim> {
        Some(KillClaim {
            reason,
            target: KillTarget::Tree,
        })
    }

    /// A recorded kill claim decides the outcome when — and only when
    /// — the wait status is one the kill, as targeted, could have
    /// produced. A principal kill produces only the forwarded 137
    /// (the relay survives by construction); a pre-placement tree
    /// kill produces either shape.
    // r[verify builder.exec.limits-isolated+2]
    // r[verify builder.exec.kill-targets-principal]
    #[test]
    fn map_exit_kill_claim_wins_when_corroborated() {
        assert_eq!(
            map_exit(exited(137), principal(KillReason::Timeout)),
            ExitOutcome::TimedOut
        );
        assert_eq!(
            map_exit(exited(137), principal(KillReason::LogLimit)),
            ExitOutcome::LogLimitExceeded
        );
        assert_eq!(
            map_exit(exited(137), tree_claim(KillReason::Timeout)),
            ExitOutcome::TimedOut
        );
        assert_eq!(
            map_exit(signaled(9), tree_claim(KillReason::Silent)),
            ExitOutcome::Silent,
            "a tree kill signals the relay directly; pre-placement no \
             tenant code ran, so the direct shape stays corroborating"
        );
    }

    /// THE merged_bug_046/074 pin: a `Principal` claim over a relay
    /// that died of SIGKILL is NOT corroborated — a principal kill
    /// never signals the relay, so that status was manufactured by
    /// something else (external interference or our own claim-free
    /// escalation) and must not relabel the build.
    // r[verify builder.exec.limits-isolated+2]
    // r[verify builder.exec.kill-targets-principal]
    #[test]
    fn map_exit_principal_claim_rejects_relay_sigkill() {
        assert_eq!(
            map_exit(signaled(9), principal(KillReason::Timeout)),
            ExitOutcome::Signaled(9),
            "a SIGKILLed relay cannot corroborate a principal kill — \
             the verdict degrades honestly instead of manufacturing \
             TimedOut"
        );
        assert_eq!(
            map_exit(signaled(9), principal(KillReason::Silent)),
            ExitOutcome::Signaled(9)
        );
    }

    /// THE merged_bug_024 pin (flipped from the parent fix's racy
    /// pinning): a kill claim over a wait status our SIGKILL cannot
    /// have produced is discarded — the natural exit wins. The clean
    /// `exit(0)` case is exactly the [exit observed, phase flipped]
    /// window the shadow state machine cannot see — and for a
    /// `Principal` claim it is also the [principal exit, relay
    /// forward] window of merged_bug_046, which the surviving relay
    /// now reports truthfully.
    // r[verify builder.exec.limits-isolated+2]
    // r[verify builder.exec.kill-targets-principal]
    #[test]
    fn map_exit_uncorroborated_kill_yields_to_natural_exit() {
        assert_eq!(
            map_exit(exited(0), principal(KillReason::Silent)),
            ExitOutcome::Exited(0),
            "a completed build whose exit was relayed after the kill \
             keeps its clean outcome — the bug_046 window"
        );
        assert_eq!(
            map_exit(exited(0), tree_claim(KillReason::LogLimit)),
            ExitOutcome::Exited(0),
            "a clean exit can never be relabeled as a limit kill"
        );
        assert_eq!(
            map_exit(exited(7), principal(KillReason::Timeout)),
            ExitOutcome::Exited(7)
        );
        assert_eq!(
            map_exit(signaled(15), tree_claim(KillReason::Timeout)),
            ExitOutcome::Signaled(15),
            "a SIGTERM death is not ours; the kill claim is discarded"
        );
        assert_eq!(
            map_exit(exited(143), principal(KillReason::Silent)),
            ExitOutcome::Signaled(15),
            "forwarded SIGTERM corroborates neither target"
        );
    }

    // -- output_host_path --------------------------------------------------

    fn request_with_mounts() -> ExecutionRequest {
        ExecutionRequest {
            program: PathBuf::from("/bin/sh"),
            args: vec![OsString::from("sh")],
            env: vec![],
            cwd: PathBuf::from("/work"),
            mounts: vec![
                Mount {
                    source: PathBuf::from("/host/work"),
                    target: PathBuf::from("/work"),
                    writable: true,
                    optional: false,
                },
                Mount {
                    source: PathBuf::from("/host/work/out"),
                    target: PathBuf::from("/work/out"),
                    writable: true,
                    optional: false,
                },
                Mount {
                    source: PathBuf::from("/host/inputs"),
                    target: PathBuf::from("/inputs"),
                    writable: false,
                    optional: false,
                },
            ],
            extra_devices: vec![],
            inline_files: vec![],
            declared_outputs: vec![PathBuf::from("/work/out/result")],
            capture: OutputCapture::MergedPty,
            isolation: Isolation {
                network: false,
                uid: 1000,
                gid: 100,
                identity: SandboxIdentity {
                    user: "buildbot".into(),
                    group: "buildgrp".into(),
                    gecos: "Test build user".into(),
                },
                personality: Personality::Native,
                hostname: "sandbox".to_string(),
                deny_setuid_and_xattrs: true,
            },
            limits: Limits {
                timeout: None,
                max_silent: None,
                max_log_bytes: None,
                cgroup: None,
            },
        }
    }

    #[test]
    fn output_host_path_uses_most_specific_writable_mount() {
        let req = request_with_mounts();
        assert_eq!(
            output_host_path(&req, Path::new("/work/out/result")),
            Some(PathBuf::from("/host/work/out/result"))
        );
        // A path under the broader mount only.
        assert_eq!(
            output_host_path(&req, Path::new("/work/log.txt")),
            Some(PathBuf::from("/host/work/log.txt"))
        );
    }

    #[test]
    fn output_host_path_rejects_read_only_and_unmounted_paths() {
        let req = request_with_mounts();
        assert_eq!(output_host_path(&req, Path::new("/inputs/file")), None);
        assert_eq!(output_host_path(&req, Path::new("/elsewhere")), None);
    }

    // -- process-tree ownership (TreeState / ProcessTreeGuard) ---------------

    /// Spawn a `sleep 30` and return its pid (a real, killable child).
    fn spawn_sleeper() -> (std::process::Child, nix::unistd::Pid) {
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .expect("spawn sleeper");
        let pid = nix::unistd::Pid::from_raw(child.id() as i32);
        (child, pid)
    }

    /// waitpid(WNOHANG) classification for assertions.
    fn pid_state(pid: nix::unistd::Pid) -> &'static str {
        let mut status: libc::c_int = 0;
        // SAFETY: waitpid into a stack buffer.
        let rc = unsafe { libc::waitpid(pid.as_raw(), &raw mut status, libc::WNOHANG) };
        if rc == pid.as_raw() {
            "was-zombie-now-reaped"
        } else if rc == 0 {
            "running-or-zombie"
        } else if Errno::last() == Errno::ECHILD {
            "already-reaped"
        } else {
            "waitpid-error"
        }
    }

    /// The fork-in-flight-during-cancellation scenario, exercised
    /// directly on the state machine: the guard drops (caller gone)
    /// BEFORE the fork closure adopts the pid it just created. adopt()
    /// must refuse, kill the process, and reap it — no process, no
    /// zombie.
    // r[verify builder.exec.tree-ownership]
    #[test]
    fn tree_state_adopt_after_guard_drop_kills_and_reaps() {
        let tree = TreeState::new(None);
        let guard = ProcessTreeGuard {
            state: Arc::clone(&tree),
        };
        drop(guard); // caller gone before any fork completed

        let (_child, pid) = spawn_sleeper();
        assert!(
            tree.adopt(pid).is_err(),
            "adopt after guard drop must report CallerGone"
        );
        // adopt() killed AND reaped: the pid is no longer ours at all.
        assert_eq!(
            pid_state(pid),
            "already-reaped",
            "the self-destructed fork must leave neither a process nor a zombie"
        );
    }

    /// Cancellation after adoption but before the reaper task is
    /// attached: the guard's own Drop must kill AND reap.
    // r[verify builder.exec.tree-ownership]
    #[test]
    fn tree_guard_drop_after_adopt_without_reaper_reaps() {
        let tree = TreeState::new(None);
        let guard = ProcessTreeGuard {
            state: Arc::clone(&tree),
        };
        let (_child, pid) = spawn_sleeper();
        tree.adopt(pid).expect("adopt with a live guard");

        drop(guard); // kills + reaps (no reaper attached)

        assert_eq!(
            pid_state(pid),
            "already-reaped",
            "guard drop without an attached reaper must kill and reap"
        );
        assert!(
            matches!(*tree.lock(), TreePhase::Reaped),
            "the state machine must record the reap"
        );
    }

    /// Structural state-machine assertions (no wall-clock): kills act
    /// once, and never after the reap (the pid may be recycled).
    // r[verify builder.exec.tree-ownership]
    #[test]
    fn tree_state_kill_acts_once_and_never_after_reap() {
        let tree = TreeState::new(None);
        let guard = ProcessTreeGuard {
            state: Arc::clone(&tree),
        };
        let (_child, pid) = spawn_sleeper();
        tree.adopt(pid).expect("adopt");
        tree.attach_reaper(); // the test plays the reaper below

        tree.kill_tree();
        assert!(
            matches!(*tree.lock(), TreePhase::Adopted { killed: true, .. }),
            "first kill must mark the tree killed"
        );
        // Second kill: the state shows it took the already-killed branch.
        tree.kill_tree();
        assert!(matches!(
            *tree.lock(),
            TreePhase::Adopted { killed: true, .. }
        ));

        // The test reaps (it attached itself as the reaper).
        blocking_reap(pid);
        tree.mark_reaped();
        assert!(matches!(*tree.lock(), TreePhase::Reaped));

        // Kill after reap: must not act (recycled-pid hazard).
        tree.kill_tree();
        assert!(
            matches!(*tree.lock(), TreePhase::Reaped),
            "a kill after the reap must be a no-op"
        );

        // Guard drop after reap: also a no-op.
        drop(guard);
        assert!(matches!(*tree.lock(), TreePhase::Reaped));
    }

    /// 3cfe38c36 regression, ported to the state machine: the pid kill
    /// must happen even when the cgroup.kill write succeeds (the tree
    /// may never have been attached to the cgroup at all).
    // r[verify builder.exec.tree-ownership]
    #[test]
    fn kill_tree_kills_the_pid_even_when_the_cgroup_write_succeeds() {
        // A plain tempdir stands in for a cgroup directory whose
        // `cgroup.kill` write succeeds without killing anything — the
        // exact shape of the failed-attach abort path, where the tree
        // was never attached to the cgroup. The pid kill must still
        // happen or the caller deadlocks awaiting the reap.
        let tmp = tempfile::tempdir().expect("tempdir");
        let (mut child, pid) = spawn_sleeper();
        let tree = TreeState::new(Some(tmp.path().to_path_buf()));
        let _guard = ProcessTreeGuard {
            state: Arc::clone(&tree),
        };
        tree.adopt(pid).expect("adopt");
        tree.attach_reaper(); // the test reaps via child.try_wait below

        tree.kill_tree();
        assert_eq!(
            std::fs::read(tmp.path().join("cgroup.kill")).expect("cgroup.kill written"),
            b"1",
            "the cgroup write itself happened (and succeeded)"
        );
        let start = Instant::now();
        loop {
            match child.try_wait().expect("try_wait") {
                Some(_) => break,
                None => {
                    assert!(
                        start.elapsed() < Duration::from_secs(5),
                        "child survived kill_tree despite the successful cgroup write"
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        tree.mark_reaped();
    }

    // -- PendingEvents -------------------------------------------------------

    fn log_event(text: &[u8]) -> ExecEvent {
        ExecEvent::Log {
            stream: LogStream::Merged,
            line: text.to_vec(),
            terminated: true,
        }
    }

    /// FIFO order and byte accounting across push/pop, including the
    /// metadata-only `Started` event.
    /// FIFO order and footprint accounting across push/pop: every
    /// event is charged the fixed struct size plus its payload's
    /// retained capacity, and pops return exactly what pushes charged.
    // r[verify builder.exec.event-budget]
    #[test]
    fn pending_events_fifo_and_byte_accounting() {
        let slot = std::mem::size_of::<ExecEvent>();
        let mut q = PendingEvents::new();
        assert!(q.is_empty());
        let abc = log_event(b"abc");
        let abc_cost = slot
            + match &abc {
                ExecEvent::Log { line, .. } => line.capacity(),
                ExecEvent::Started { .. } => unreachable!(),
            };
        q.push(abc);
        q.push(ExecEvent::Started { pid: 7 });
        let defgh = log_event(b"defgh");
        let defgh_cost = slot
            + match &defgh {
                ExecEvent::Log { line, .. } => line.capacity(),
                ExecEvent::Started { .. } => unreachable!(),
            };
        q.push(defgh);
        assert_eq!(
            q.bytes,
            abc_cost + slot + defgh_cost,
            "every event charges its slot; payloads charge their capacity"
        );
        assert!(!q.at_cap());

        assert!(matches!(q.pop(), Some(ExecEvent::Log { line, .. }) if line == b"abc"));
        assert_eq!(q.bytes, slot + defgh_cost);
        assert!(matches!(q.pop(), Some(ExecEvent::Started { pid: 7 })));
        assert!(matches!(q.pop(), Some(ExecEvent::Log { line, .. }) if line == b"defgh"));
        assert_eq!(q.bytes, 0, "accounting returns to zero when drained");
        assert!(q.pop().is_none());
    }

    /// The cap gate trips at the charged budget and clear() resets both
    /// the queue and the accounting (the dropped-receiver path).
    // r[verify builder.exec.event-budget]
    #[test]
    fn pending_events_cap_and_clear() {
        let slot = std::mem::size_of::<ExecEvent>();
        let mut q = PendingEvents::new();
        // One event whose charged cost lands exactly one byte short of
        // the cap (slot + payload capacity = cap - 1).
        q.push(log_event(&vec![b'x'; PENDING_BYTES_CAP - slot - 1]));
        assert!(!q.at_cap());
        // Any further event charges at least its slot, crossing the cap.
        q.push(log_event(b""));
        assert!(q.at_cap(), "the gate trips at the charged cap");

        q.clear();
        assert!(q.is_empty());
        assert_eq!(q.bytes, 0);
        assert!(!q.at_cap(), "a cleared queue accepts chunks again");
    }

    /// THE merged_bug_001 pin: a flood of zero-payload lines — which
    /// charge nothing under payload-only accounting — trips the cap
    /// after at most `PENDING_BYTES_CAP / size_of::<ExecEvent>()`
    /// events (65,536 at the current sizes), bounding both the queue
    /// length and the retained memory for the adversarial class.
    // r[verify builder.exec.event-budget]
    #[test]
    fn pending_events_zero_payload_flood_is_bounded() {
        let slot = std::mem::size_of::<ExecEvent>();
        let bound = PENDING_BYTES_CAP.div_ceil(slot);
        assert!(
            bound <= 65_536,
            "size_of::<ExecEvent>() shrank below 32 bytes; update the \
             documented event bound (cap / slot = {bound})"
        );
        let mut q = PendingEvents::new();
        let mut pushed = 0usize;
        while !q.at_cap() {
            q.push(log_event(b""));
            pushed += 1;
            assert!(
                pushed <= bound,
                "zero-payload events must charge their slot: the cap \
                 never tripped within the computed bound"
            );
        }
        assert_eq!(
            pushed, bound,
            "the flood trips exactly at cap / slot events"
        );
    }

    // -- LimitWatchdog -------------------------------------------------------

    /// Build a watchdog over `tree` plus the meter that feeds it and
    /// the reason slot it reports through. Tests drive activity by
    /// calling `meter.record()` directly — the watchdog cannot tell a
    /// test body from a blocking capture reader, which is the point:
    /// enforcement consumes only the raw-read signal.
    fn watchdog_parts(
        tree: &Arc<TreeState>,
        timeout: Option<Duration>,
        max_silent: Option<Duration>,
        max_log_bytes: Option<u64>,
    ) -> (
        Arc<ActivityMeter>,
        LimitWatchdog,
        Arc<std::sync::Mutex<Option<KillClaim>>>,
    ) {
        let (meter, bytes, activity) = ActivityMeter::new();
        let claim = Arc::new(std::sync::Mutex::new(None));
        let watchdog = LimitWatchdog {
            tree: Arc::clone(tree),
            timeout,
            max_silent,
            max_log_bytes,
            started: Instant::now(),
            activity,
            bytes,
            claim: Arc::clone(&claim),
        };
        (meter, watchdog, claim)
    }

    fn reason_of(slot: &std::sync::Mutex<Option<KillClaim>>) -> Option<KillReason> {
        slot.lock()
            .unwrap_or_else(|e| e.into_inner())
            .map(|c| c.reason)
    }

    /// The merged_bug_019 executor pin at the unit level: the timeout
    /// kill fires with ZERO event consumers anywhere — no chunk
    /// channel, no events receiver, nothing draining. Enforcement
    /// owns no send path to park on.
    // r[verify builder.exec.limits-isolated+2]
    #[tokio::test(start_paused = true)]
    async fn watchdog_timeout_kills_with_zero_consumers() {
        let tree = TreeState::new(None);
        let _guard = ProcessTreeGuard {
            state: Arc::clone(&tree),
        };
        let (_child, pid) = spawn_sleeper();
        tree.adopt(pid).expect("adopt");
        tree.attach_reaper(); // the test reaps below

        let (_meter, watchdog, reason) =
            watchdog_parts(&tree, Some(Duration::from_secs(10)), None, None);
        tokio::spawn(watchdog.run())
            .await
            .expect("watchdog task panicked");

        assert_eq!(reason_of(&reason), Some(KillReason::Timeout));
        assert!(
            matches!(*tree.lock(), TreePhase::Adopted { killed: true, .. }),
            "the timeout must have killed the tree"
        );
        blocking_reap(pid);
        tree.mark_reaped();
    }

    /// Activity recorded by the meter resets the silence clock; its
    /// absence fires the silence kill.
    // r[verify builder.exec.limits-isolated+2]
    // r[verify builder.silence.timeout-kill+3]
    #[tokio::test(start_paused = true)]
    async fn watchdog_silence_resets_on_activity() {
        let tree = TreeState::new(None);
        let _guard = ProcessTreeGuard {
            state: Arc::clone(&tree),
        };
        let (_child, pid) = spawn_sleeper();
        tree.adopt(pid).expect("adopt");
        tree.attach_reaper();

        let (meter, watchdog, reason) =
            watchdog_parts(&tree, None, Some(Duration::from_secs(5)), None);
        let task = tokio::spawn(watchdog.run());

        // Feed activity at +3s: the silence deadline moves to +8s.
        tokio::time::sleep(Duration::from_secs(3)).await;
        meter.record(1);
        // At +7s (4s after the last activity) the tree must be alive.
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert_eq!(reason_of(&reason), None, "activity must reset the clock");
        assert!(matches!(
            *tree.lock(),
            TreePhase::Adopted { killed: false, .. }
        ));
        // No further activity: the kill fires at +8s.
        task.await.expect("watchdog task panicked");
        assert_eq!(reason_of(&reason), Some(KillReason::Silent));
        assert!(matches!(
            *tree.lock(),
            TreePhase::Adopted { killed: true, .. }
        ));
        blocking_reap(pid);
        tree.mark_reaped();
    }

    /// The byte cap is checked against the meter's raw-read total.
    // r[verify builder.exec.limits-isolated+2]
    #[tokio::test(start_paused = true)]
    async fn watchdog_log_cap_kills_at_threshold() {
        let tree = TreeState::new(None);
        let _guard = ProcessTreeGuard {
            state: Arc::clone(&tree),
        };
        let (_child, pid) = spawn_sleeper();
        tree.adopt(pid).expect("adopt");
        tree.attach_reaper();

        let (meter, watchdog, reason) = watchdog_parts(&tree, None, None, Some(100));
        let task = tokio::spawn(watchdog.run());

        meter.record(60);
        tokio::task::yield_now().await;
        assert_eq!(reason_of(&reason), None, "under the cap: no kill");

        meter.record(60); // total 120 > 100
        task.await.expect("watchdog task panicked");
        assert_eq!(reason_of(&reason), Some(KillReason::LogLimit));
        blocking_reap(pid);
        tree.mark_reaped();
    }

    /// THE misattribution pin: a deadline that fires after the tree
    /// was reaped records no reason — the natural exit status wins
    /// structurally, not by luck of timer ordering.
    // r[verify builder.exec.limits-isolated+2]
    #[tokio::test(start_paused = true)]
    async fn watchdog_records_reason_only_when_kill_acted() {
        let tree = TreeState::new(None);
        let (mut child, pid) = spawn_sleeper();
        tree.adopt(pid).expect("adopt");
        tree.attach_reaper();
        // The build finishes (and is reaped) before the deadline.
        child.kill().expect("kill sleeper");
        blocking_reap(pid);
        tree.mark_reaped();

        let (_meter, watchdog, reason) =
            watchdog_parts(&tree, Some(Duration::from_secs(1)), None, None);
        tokio::spawn(watchdog.run())
            .await
            .expect("watchdog task panicked");

        assert_eq!(
            reason_of(&reason),
            None,
            "a kill that never acted must not claim the exit"
        );
        assert!(matches!(*tree.lock(), TreePhase::Reaped));
    }

    /// Spawn a shell that exits with `code` (a real child whose natural
    /// exit the tests observe).
    fn spawn_exiting(code: i32) -> (std::process::Child, nix::unistd::Pid) {
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", &format!("exit {code}")])
            .spawn()
            .expect("spawn exiting child");
        let pid = nix::unistd::Pid::from_raw(child.id() as i32);
        (child, pid)
    }

    /// Block until `pid`'s exit is observable, WITHOUT consuming it
    /// (`waitid(WEXITED | WNOWAIT)`): on return the child is a zombie
    /// that still owns its pid.
    fn observe_exit_nowait(pid: nix::unistd::Pid) {
        loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: waitid with WEXITED|WNOWAIT into a zeroed stack
            // siginfo for a direct child.
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid.as_raw() as libc::id_t,
                    &raw mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if rc == 0 {
                return;
            }
            assert_eq!(Errno::last(), Errno::EINTR, "waitid failed");
        }
    }

    /// THE merged_bug_024 near-miss window, constructed
    /// deterministically (no /proc polling, no sleeps): the child has
    /// exited 0 and is a zombie, but the phase machine still says
    /// `Adopted` — a deadline kill ACTS (signals the zombie, harmless;
    /// records its claim) and the corroboration layer then discards
    /// the claim because the wait status is a clean exit our SIGKILL
    /// cannot have produced.
    // r[verify builder.exec.limits-isolated+2]
    #[test]
    fn zombie_window_kill_claim_is_discarded() {
        let tree = TreeState::new(None);
        let (_child, pid) = spawn_exiting(0);
        tree.adopt(pid).expect("adopt");
        tree.attach_reaper();

        // Deterministically reach the window: exit observed by the
        // kernel, status not yet consumed, phase not yet flipped.
        observe_exit_nowait(pid);
        assert_eq!(
            tree.kill_tree(),
            Some(KillTarget::Tree),
            "the kill acts on the shadow-live zombie (the racy window)"
        );

        // What observe_then_reap's tail does: flip, then consume.
        tree.mark_reaped();
        let status = wait_for(pid).expect("zombie stays reapable");
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
        assert_eq!(
            map_exit(status, tree_claim(KillReason::Timeout)),
            ExitOutcome::Exited(0),
            "the uncorroborated claim must not relabel the clean exit"
        );
    }

    /// The placement handshake end to end against a real two-level
    /// fork: the relay-side `send_placement` delivers the grandchild's
    /// pid plus a pidfd over `SCM_RIGHTS`, and the received pidfd is a
    /// usable kill handle for a process that is NOT our child — alive
    /// until `pidfd_kill`, observably dead after (pidfd polls
    /// readable on exit).
    // r[verify builder.exec.kill-targets-principal]
    #[test]
    fn placement_handshake_delivers_pid_and_live_pidfd() {
        let (sock_r, sock_w) = make_placement_socketpair().expect("socketpair");

        // SAFETY: the child branch only calls async-signal-safe code
        // (fork, the function under test, _exit).
        let relay = match unsafe { libc::fork() } {
            -1 => panic!("fork failed: {}", std::io::Error::last_os_error()),
            0 => {
                // The relay: fork a parked grandchild, place it, exit.
                match unsafe { libc::fork() } {
                    -1 => unsafe { libc::_exit(10) },
                    0 => loop {
                        // The grandchild parks until SIGKILLed.
                        unsafe { libc::pause() };
                    },
                    grandchild => {
                        if send_placement(sock_w.as_raw_fd(), grandchild).is_err() {
                            unsafe { libc::_exit(11) };
                        }
                        unsafe { libc::_exit(0) };
                    }
                }
            }
            pid => nix::unistd::Pid::from_raw(pid),
        };
        drop(sock_w); // parent's copy: EOF must track the relay alone

        let (pid, pidfd) = recv_placement(sock_r).expect("placement message");
        assert!(pid > 0, "placement pid must be a real pid");
        // The relay exits cleanly after sending.
        let status = wait_for(relay).expect("reap relay");
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "relay failed: status {status} (10=fork, 11=send_placement)"
        );

        let poll_pidfd = |timeout_ms: libc::c_int| {
            let mut pfd = libc::pollfd {
                fd: pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll over one stack pollfd.
            unsafe { libc::poll(&raw mut pfd, 1, timeout_ms) }
        };
        assert_eq!(
            poll_pidfd(0),
            0,
            "the parked grandchild must still be alive (pidfd not readable)"
        );
        pidfd_kill(&pidfd);
        assert_eq!(
            poll_pidfd(10_000),
            1,
            "the pidfd kill must terminate the grandchild (pidfd readable)"
        );
        // The grandchild reparented when the relay exited; init reaps it.
    }

    /// A relay that dies before sending the placement message must
    /// surface as EOF to the parent's recv, not park it.
    // r[verify builder.exec.kill-targets-principal]
    #[test]
    fn placement_recv_eofs_when_the_relay_dies_before_sending() {
        let (sock_r, sock_w) = make_placement_socketpair().expect("socketpair");
        // SAFETY: the child branch only calls _exit.
        let relay = match unsafe { libc::fork() } {
            -1 => panic!("fork failed: {}", std::io::Error::last_os_error()),
            0 => unsafe { libc::_exit(7) },
            pid => nix::unistd::Pid::from_raw(pid),
        };
        drop(sock_w);
        let err = recv_placement(sock_r).expect_err("EOF must error");
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        let _ = wait_for(relay);
    }

    /// `place_principal` stores the pidfd only on a live, unkilled
    /// tree; on a tree that was already killed the handle is consumed
    /// without resurrecting kill eligibility.
    // r[verify builder.exec.kill-targets-principal]
    #[test]
    fn place_principal_stores_only_on_a_live_tree() {
        let dummy_fd = || {
            let (r, _w) = make_pipe().expect("pipe");
            r
        };

        let tree = TreeState::new(None);
        let (_child, pid) = spawn_exiting(0);
        tree.adopt(pid).expect("adopt");
        tree.place_principal(dummy_fd());
        assert!(
            matches!(
                &*tree.lock(),
                TreePhase::Adopted {
                    principal: Some(_),
                    ..
                }
            ),
            "placement on a live tree must store the principal handle"
        );

        let tree = TreeState::new(None);
        let (_child2, pid2) = spawn_exiting(0);
        tree.adopt(pid2).expect("adopt");
        assert_eq!(
            tree.kill_tree(),
            Some(KillTarget::Tree),
            "kill acts on the live (unplaced) tree"
        );
        tree.place_principal(dummy_fd());
        assert!(
            matches!(
                &*tree.lock(),
                TreePhase::Adopted {
                    killed: true,
                    principal: None,
                    ..
                }
            ),
            "placement after a kill must not store the handle"
        );
    }

    // -- the two-level production-topology harness ---------------------------

    /// A real relay + principal pair in the production shape: the
    /// relay runs [`relay_wait_and_forward`] — the *same function* as
    /// [`intermediate_main`]'s tail — and hands the principal over
    /// with the production [`send_placement`]. The principal parks
    /// until the test releases it (one byte → `_exit(0)`) or kills
    /// it. Every kill-corroboration property is pinned against this
    /// topology; a single-process harness cannot express the
    /// [principal exit, relay forward] window this class of bug lives
    /// in.
    struct TwoLevelHarness {
        relay: nix::unistd::Pid,
        /// The test's own duplicate of the principal pidfd (the tree
        /// holds the placed original).
        principal_pidfd: OwnedFd,
        /// Write a byte to make the principal `_exit(0)`.
        release_w: OwnedFd,
    }

    fn spawn_two_level(tree: &TreeState) -> TwoLevelHarness {
        let (sock_r, sock_w) = make_placement_socketpair().expect("socketpair");
        let (release_r, release_w) = make_pipe().expect("release pipe");
        // SAFETY: both forked branches only call async-signal-safe
        // code (fork, read, the functions under test, _exit).
        let relay = match unsafe { libc::fork() } {
            -1 => panic!("fork failed: {}", std::io::Error::last_os_error()),
            0 => {
                match unsafe { libc::fork() } {
                    -1 => unsafe { libc::_exit(120) },
                    0 => {
                        // The principal: park until released, then
                        // complete cleanly.
                        let mut b = [0u8; 1];
                        loop {
                            let n = unsafe {
                                libc::read(release_r.as_raw_fd(), b.as_mut_ptr().cast(), 1)
                            };
                            match n {
                                1 => unsafe { libc::_exit(0) },
                                0 => unsafe { libc::_exit(121) },
                                _ if Errno::last() == Errno::EINTR => {}
                                _ => unsafe { libc::_exit(122) },
                            }
                        }
                    }
                    principal => {
                        if send_placement(sock_w.as_raw_fd(), principal).is_err() {
                            unsafe { libc::_exit(123) };
                        }
                        // THE production relay loop.
                        relay_wait_and_forward(principal);
                    }
                }
            }
            pid => nix::unistd::Pid::from_raw(pid),
        };
        drop(sock_w);
        let (_pid, pidfd) = recv_placement(sock_r).expect("placement message");
        let principal_pidfd = pidfd.try_clone().expect("dup pidfd");
        tree.adopt(relay).expect("adopt");
        tree.attach_reaper();
        tree.place_principal(pidfd);
        TwoLevelHarness {
            relay,
            principal_pidfd,
            release_w,
        }
    }

    /// Block until `pid` is stopped (`T`/`t` in `/proc/<pid>/stat`) so
    /// SIGSTOP-staged windows are deterministic, with a deadline.
    fn wait_until_stopped(pid: nix::unistd::Pid) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .expect("read /proc/<pid>/stat");
            // State is the field after the parenthesized comm.
            let state = stat
                .rsplit(") ")
                .next()
                .and_then(|rest| rest.chars().next())
                .expect("parse stat state");
            if state == 'T' || state == 't' {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "process {pid} never stopped (state {state})"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Block until the principal behind `pidfd` has exited (pidfd
    /// polls readable), with a deadline.
    fn wait_until_principal_dead(pidfd: &OwnedFd) {
        let mut pfd = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll over one stack pollfd.
        let rc = unsafe { libc::poll(&raw mut pfd, 1, 10_000) };
        assert_eq!(rc, 1, "principal did not exit within the deadline");
    }

    /// THE merged_bug_046/074 closure, staged deterministically on the
    /// production topology: the principal completes cleanly while the
    /// relay is descheduled (SIGSTOP stands in for scheduler delay),
    /// a limit kill fires inside that [principal exit, relay forward]
    /// window — and the completed build keeps its clean outcome,
    /// because the kill targeted the principal and the surviving
    /// relay forwarded the truth. Under the pre-split design this
    /// exact staging SIGKILLed the stopped relay and manufactured a
    /// corroborated `TimedOut`.
    // r[verify builder.exec.kill-targets-principal]
    // r[verify builder.exec.limits-isolated+2]
    #[test]
    fn principal_kill_loses_to_a_completed_build_across_a_stopped_relay() {
        let tree = TreeState::new(None);
        let h = spawn_two_level(&tree);

        // Park the relay BEFORE the principal exits: its waitpid
        // wake-up cannot run, so the forward is provably pending.
        nix::sys::signal::kill(h.relay, nix::sys::signal::Signal::SIGSTOP).expect("SIGSTOP");
        wait_until_stopped(h.relay);

        // The principal completes its build and exits 0; the relay,
        // stopped, holds it as an unforwarded zombie.
        nix::unistd::write(&h.release_w, &[1u8]).expect("release the principal");
        wait_until_principal_dead(&h.principal_pidfd);

        // The deadline fires into the window. The kill targets the
        // principal (already a zombie — no-op) and NEVER the relay.
        assert_eq!(
            tree.kill_tree(),
            Some(KillTarget::Principal),
            "a placed tree must be killed at the principal"
        );
        let claim = Some(KillClaim {
            reason: KillReason::Silent,
            target: KillTarget::Principal,
        });

        // The relay resumes, reaps the principal, forwards 0.
        nix::sys::signal::kill(h.relay, nix::sys::signal::Signal::SIGCONT).expect("SIGCONT");
        let status = observe_then_reap(&tree, h.relay).expect("relay status");
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "the surviving relay must forward the natural exit (status {status})"
        );
        assert_eq!(
            map_exit(status, claim),
            ExitOutcome::Exited(0),
            "a completed build must never be relabeled by a kill that \
             raced its relay forward — merged_bug_046's window"
        );
    }

    /// The inverse staging: the principal is still running when the
    /// kill fires. The pidfd SIGKILL terminates it, the (live) relay
    /// reaps and forwards 137, and the claim is corroborated — the
    /// limit verdict stands.
    // r[verify builder.exec.kill-targets-principal]
    // r[verify builder.exec.limits-isolated+2]
    #[test]
    fn principal_kill_terminates_a_live_build_and_corroborates() {
        let tree = TreeState::new(None);
        let h = spawn_two_level(&tree);

        assert_eq!(
            tree.kill_tree(),
            Some(KillTarget::Principal),
            "a placed tree must be killed at the principal"
        );
        let claim = Some(KillClaim {
            reason: KillReason::Timeout,
            target: KillTarget::Principal,
        });

        let status = observe_then_reap(&tree, h.relay).expect("relay status");
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 137,
            "the relay must forward our SIGKILL as 137 (status {status})"
        );
        assert_eq!(map_exit(status, claim), ExitOutcome::TimedOut);
    }

    /// Residual 2, pinned end to end through the watchdog: a relay
    /// stuck past the escalation grace is taken down claim-free — the
    /// recorded `Principal` claim survives but its corroboration
    /// fails on the escalated relay's `signaled(SIGKILL)`, so the
    /// verdict degrades to the honest `Signaled(9)` instead of a
    /// manufactured limit verdict.
    // r[verify builder.exec.kill-targets-principal]
    // r[verify builder.exec.limits-isolated+2]
    #[tokio::test(start_paused = true)]
    async fn relay_stuck_past_grace_is_escalated_claim_free() {
        let tree = TreeState::new(None);
        let h = spawn_two_level(&tree);
        let (_meter, watchdog, claim_slot) = watchdog_parts(&tree, None, None, None);

        // Wedge the relay past any plausible forward.
        nix::sys::signal::kill(h.relay, nix::sys::signal::Signal::SIGSTOP).expect("SIGSTOP");
        wait_until_stopped(h.relay);

        // The full watchdog path: principal kill, grace (virtual
        // time), claim-free escalation of the still-unsettled tree.
        watchdog.kill_and_escalate(KillReason::Timeout).await;
        assert_eq!(
            *claim_slot.lock().unwrap_or_else(|e| e.into_inner()),
            Some(KillClaim {
                reason: KillReason::Timeout,
                target: KillTarget::Principal,
            }),
            "the principal kill must record its claim"
        );

        let status = tokio::task::spawn_blocking({
            let tree = Arc::clone(&tree);
            let relay = h.relay;
            move || observe_then_reap(&tree, relay)
        })
        .await
        .expect("join")
        .expect("relay status");
        assert!(
            libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGKILL,
            "the escalation must have taken the relay down (status {status})"
        );
        assert_eq!(
            map_exit(
                status,
                *claim_slot.lock().unwrap_or_else(|e| e.into_inner())
            ),
            ExitOutcome::Signaled(9),
            "the dropped claim must degrade honestly, never to a \
             manufactured limit verdict"
        );
    }

    /// `observe_then_reap`: the WNOWAIT observation leaves the child
    /// reapable (the real reap still returns the full status) and the
    /// phase is `Reaped` by return.
    // r[verify builder.exec.limits-isolated+2]
    #[test]
    fn observe_then_reap_preserves_status_and_flips_phase() {
        let tree = TreeState::new(None);
        let (_child, pid) = spawn_exiting(7);
        tree.adopt(pid).expect("adopt");
        tree.attach_reaper();

        let status = observe_then_reap(&tree, pid).expect("status");
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 7,
            "WNOWAIT must not consume the exit status"
        );
        assert!(matches!(*tree.lock(), TreePhase::Reaped));
    }

    /// Once `observe_then_reap` has run, every kill path is disarmed:
    /// a late deadline's `kill_tree()` no-ops (returns false, signals
    /// nothing) — the structural guarantee that a recycled pid can
    /// never be targeted.
    // r[verify builder.exec.limits-isolated+2]
    #[test]
    fn late_kill_after_observe_then_reap_is_inert() {
        let tree = TreeState::new(None);
        let (_child, pid) = spawn_exiting(0);
        tree.adopt(pid).expect("adopt");
        tree.attach_reaper();

        observe_then_reap(&tree, pid).expect("status");
        assert_eq!(
            tree.kill_tree(),
            None,
            "a reaped tree is unkillable; no signal may chase the pid"
        );
    }

    /// Build a [`SpawnedSandbox`] over a sleeper child plus a
    /// hand-fed status pipe (the write end stays with the test).
    fn sandbox_parts() -> (SpawnedSandbox, OwnedFd) {
        let tree = TreeState::new(None);
        let (_child, pid) = spawn_sleeper();
        tree.adopt(pid).expect("adopt");
        tree.attach_reaper();
        let tree_guard = ProcessTreeGuard {
            state: Arc::clone(&tree),
        };
        let wait_task = {
            let tree = Arc::clone(&tree);
            tokio::task::spawn_blocking(move || observe_then_reap(&tree, pid))
        };
        let (status_r, status_w) = make_pipe().expect("status pipe");
        let status_task = tokio::task::spawn_blocking(move || read_status_pipe(status_r));
        (
            SpawnedSandbox {
                tree_guard,
                wait_task,
                status_task,
            },
            status_w,
        )
    }

    /// THE bug_101 pin: a typed setup failure written before the abort
    /// (the child died of a reportable error; the cgroup attach then
    /// failed as a symptom) surfaces as `ExecError::Setup` with the
    /// failing phase and errno — not as the generic abort cause.
    // r[verify builder.exec.setup-error-surfaced]
    #[tokio::test]
    async fn abort_surfaces_typed_setup_error() {
        let (sandbox, status_w) = sandbox_parts();
        let report = SetupError {
            phase: SetupPhase::FdSweep,
            errno: libc::EPERM,
            detail: 0,
        };
        nix::unistd::write(&status_w, &report.to_bytes()).expect("write report");
        drop(status_w);

        let got = sandbox
            .abort(
                std::io::Error::from_raw_os_error(libc::ESRCH),
                "attach the sandbox to the cgroup",
                &[],
                &[],
            )
            .await;
        match got {
            ExecError::Setup(err) => {
                assert_eq!(err.phase, SetupPhase::FdSweep);
                assert_eq!(err.errno, libc::EPERM);
            }
            other => panic!("expected the typed setup error, got {other:?}"),
        }
    }

    /// EOF on the status pipe (the child died without reporting —
    /// normally from the abort's own kill) keeps the generic abort
    /// cause naming the step that failed.
    // r[verify builder.exec.setup-error-surfaced]
    #[tokio::test]
    async fn abort_eof_keeps_generic_spawn_error() {
        let (sandbox, status_w) = sandbox_parts();
        drop(status_w); // EOF, no report

        let got = sandbox
            .abort(
                std::io::Error::from_raw_os_error(libc::ESRCH),
                "attach the sandbox to the cgroup",
                &[],
                &[],
            )
            .await;
        match got {
            ExecError::Spawn(err) => {
                let msg = err.to_string();
                assert!(
                    msg.contains("attach the sandbox to the cgroup"),
                    "the abort cause must name the failed step: {msg}"
                );
            }
            other => panic!("expected the generic spawn error, got {other:?}"),
        }
    }

    /// Readers gone + tree settled = nothing left to enforce: the
    /// watchdog returns instead of idling on dead timers.
    #[tokio::test(start_paused = true)]
    async fn watchdog_activity_closed_with_settled_tree_exits() {
        let tree = TreeState::new(None);
        let (mut child, pid) = spawn_sleeper();
        tree.adopt(pid).expect("adopt");
        tree.attach_reaper();
        child.kill().expect("kill sleeper");
        blocking_reap(pid);
        tree.mark_reaped();

        // Only the log cap is configured: the activity arm is the one
        // live arm, and closing it against a settled tree must end the
        // task.
        let (meter, watchdog, reason) = watchdog_parts(&tree, None, None, Some(1_000_000));
        let task = tokio::spawn(watchdog.run());
        drop(meter);
        task.await.expect("watchdog task panicked");
        assert_eq!(reason_of(&reason), None);
    }

    /// Totality: with no limits configured there is nothing to watch.
    #[tokio::test(start_paused = true)]
    async fn watchdog_no_limits_no_activity_returns() {
        let tree = TreeState::new(None);
        let (_meter, watchdog, reason) = watchdog_parts(&tree, None, None, None);
        tokio::spawn(watchdog.run())
            .await
            .expect("watchdog task panicked");
        assert_eq!(reason_of(&reason), None);
        assert!(
            matches!(*tree.lock(), TreePhase::Armed),
            "no kill may be issued when no limit exists"
        );
    }

    // -- LineSplitter --------------------------------------------------------

    fn lines(events: Vec<ExecEvent>) -> Vec<(LogStream, Vec<u8>)> {
        events
            .into_iter()
            .map(|e| match e {
                ExecEvent::Log { stream, line, .. } => (stream, line),
                ExecEvent::Started { .. } => panic!("unexpected Started"),
            })
            .collect()
    }

    #[test]
    fn line_splitter_bounds_unterminated_lines() {
        let mut s = LineSplitter::default();
        // 2.5 caps worth of newline-free output: the splitter must emit
        // two full chunks and keep only the remainder pending.
        let blob = vec![b'a'; MAX_PENDING_LINE_BYTES * 2 + MAX_PENDING_LINE_BYTES / 2];
        let events = s.push(LogStream::Merged, &blob);
        let emitted = lines(events);
        assert_eq!(emitted.len(), 2, "two forced flushes at the cap");
        assert!(
            emitted
                .iter()
                .all(|(_, l)| l.len() == MAX_PENDING_LINE_BYTES)
        );
        let (_, pending) = s
            .pending
            .iter()
            .find(|(stream, _)| *stream == LogStream::Merged)
            .expect("pending entry");
        assert_eq!(pending.len(), MAX_PENDING_LINE_BYTES / 2);
        // The remainder still flushes at EOF — also unterminated.
        let flushed = s.flush();
        assert!(!flushed.is_empty());
        assert!(
            flushed.iter().all(|e| matches!(
                e,
                ExecEvent::Log {
                    terminated: false,
                    ..
                }
            )),
            "EOF flush of a capped line's remainder is not a logical line end"
        );
    }

    /// The framing flag: newline-emitted lines are terminated, both
    /// fragment producers (cap force-emit, EOF flush) are not, and a
    /// CRLF terminator counts as a normal line end.
    #[test]
    fn line_splitter_marks_fragments_unterminated() {
        let mut s = LineSplitter::default();
        // Cap-forced fragment: false; the eventual newline closes the
        // logical line with true.
        let mut blob = vec![b'x'; MAX_PENDING_LINE_BYTES];
        blob.extend_from_slice(b"tail\n");
        let events = s.push(LogStream::Merged, &blob);
        let flags: Vec<bool> = events
            .iter()
            .map(|e| match e {
                ExecEvent::Log { terminated, .. } => *terminated,
                ExecEvent::Started { .. } => panic!("unexpected Started"),
            })
            .collect();
        assert_eq!(flags, vec![false, true], "cap fragment then newline end");

        // Plain newline and CRLF: terminated.
        let events = s.push(LogStream::Merged, b"a\nb\r\n");
        assert!(events.iter().all(|e| matches!(
            e,
            ExecEvent::Log {
                terminated: true,
                ..
            }
        )));

        // EOF flush of a trailing partial: unterminated.
        assert!(s.push(LogStream::Merged, b"partial").is_empty());
        let flushed = s.flush();
        assert_eq!(flushed.len(), 1);
        assert!(matches!(
            flushed[0],
            ExecEvent::Log {
                terminated: false,
                ..
            }
        ));
    }

    #[test]
    fn line_splitter_splits_and_strips_crlf() {
        let mut s = LineSplitter::default();
        let events = s.push(LogStream::Merged, b"hello\r\nwor");
        assert_eq!(lines(events), vec![(LogStream::Merged, b"hello".to_vec())]);
        let events = s.push(LogStream::Merged, b"ld\n");
        assert_eq!(lines(events), vec![(LogStream::Merged, b"world".to_vec())]);
        assert!(s.flush().is_empty());
    }

    #[test]
    fn line_splitter_keeps_streams_separate_and_flushes_partials() {
        let mut s = LineSplitter::default();
        assert!(s.push(LogStream::Stdout, b"out-partial").is_empty());
        let events = s.push(LogStream::Stderr, b"err-line\n");
        assert_eq!(
            lines(events),
            vec![(LogStream::Stderr, b"err-line".to_vec())]
        );
        assert_eq!(
            lines(s.flush()),
            vec![(LogStream::Stdout, b"out-partial".to_vec())]
        );
    }

    #[test]
    fn line_splitter_flush_drains_every_streams_trailing_partial() {
        // Under SeparatePipes both stdout and stderr can end with an
        // unterminated line; flush must emit both, not just the first
        // non-empty buffer it finds.
        let mut s = LineSplitter::default();
        assert!(s.push(LogStream::Stdout, b"out-tail").is_empty());
        assert!(s.push(LogStream::Stderr, b"err-tail").is_empty());
        let flushed = lines(s.flush());
        assert_eq!(flushed.len(), 2);
        assert!(flushed.contains(&(LogStream::Stdout, b"out-tail".to_vec())));
        assert!(flushed.contains(&(LogStream::Stderr, b"err-tail".to_vec())));
        // Everything was drained; a second flush has nothing left.
        assert!(s.flush().is_empty());
    }

    #[test]
    fn line_splitter_handles_empty_lines() {
        let mut s = LineSplitter::default();
        let events = s.push(LogStream::Merged, b"\n\n");
        assert_eq!(
            lines(events),
            vec![
                (LogStream::Merged, Vec::new()),
                (LogStream::Merged, Vec::new()),
            ]
        );
    }

    // -- StatusReport decoding ----------------------------------------------

    #[test]
    fn status_report_decodes_through_pipe() {
        // Write a real SetupError through a real pipe and read it back
        // with the production reader.
        let (r, w) = make_pipe().expect("pipe");
        let err = SetupError {
            phase: child::SetupPhase::PivotRoot,
            errno: libc::EPERM,
            detail: 0,
        };
        nix::unistd::write(&w, &err.to_bytes()).expect("write");
        drop(w);
        assert_eq!(read_status_pipe(r), StatusReport::SetupFailed(err));
    }

    #[test]
    fn status_report_eof_means_exec_started() {
        let (r, w) = make_pipe().expect("pipe");
        drop(w);
        assert_eq!(read_status_pipe(r), StatusReport::ExecStarted);
    }

    #[test]
    fn status_report_truncated_is_corrupt() {
        let (r, w) = make_pipe().expect("pipe");
        nix::unistd::write(&w, &[1, 2, 3]).expect("write");
        drop(w);
        assert_eq!(read_status_pipe(r), StatusReport::Corrupt);
    }
}
