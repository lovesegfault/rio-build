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
//!   ├─ go pipe ──────────► intermediate: blocks until the parent has
//!   │                      attached it to the cgroup; EOF = the
//!   │                      parent died first (abort, do not build)
//!   ├─ status pipe ◄────── intermediate + sandbox child: 8 bytes on
//!   │                      setup failure; EOF with 0 bytes when the
//!   │                      program exec'd (the write end is
//!   │                      close-on-exec)
//!   └─ pty master / pipes ◄ the program's stdout/stderr
//! ```
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
//! **Event channel:** the caller must keep draining the [`ExecEvent`]
//! receiver: log delivery awaits channel capacity, so a stalled (but
//! alive) receiver suspends log processing and with it **all** limit
//! enforcement — silence, log-volume, and the wall-clock timeout alike
//! (the supervision loop cannot reach its deadline arms while parked
//! on the send). A *dropped* receiver is fine — events are discarded
//! and execution continues.
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

use std::os::fd::{AsRawFd as _, OwnedFd};
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

/// Capacity of the internal raw-chunk channel between the blocking
/// pipe/pty readers and the async line splitter. Bounded so a build
/// that outputs faster than the caller consumes applies backpressure
/// to the reader thread (and ultimately the build's own writes) instead
/// of buffering without limit.
const LOG_CHUNK_CHANNEL_CAPACITY: usize = 64;

/// Why the executor killed the process tree before it finished on its
/// own. Takes precedence over the raw exit status when mapping the
/// final [`ExitOutcome`].
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
    // r[impl builder.exec.limits-isolated]
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
    /// Why the watchdog killed, if it did and the kill acted. Read by
    /// `execute()` after the reap to map the final exit outcome.
    reason: Arc<std::sync::Mutex<Option<KillReason>>>,
}

impl LimitWatchdog {
    /// Enforce until a limit fires (kill, then return) or nothing can
    /// ever fire again (return without killing).
    // r[impl builder.exec.limits-isolated]
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
                                self.kill(KillReason::LogLimit);
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
                    self.kill(KillReason::Timeout);
                    return;
                }
                // A second implementation site for the silence kill: the
                // deadline is reset by the raw captured bytes, upstream
                // of classification and delivery.
                // r[impl builder.silence.timeout-kill+3]
                () = sleep_until_opt(silent_at), if silent_at.is_some() => {
                    self.kill(KillReason::Silent);
                    return;
                }
            }
        }
    }

    /// Kill the tree and record why — but only when the kill actually
    /// acted on a live tree.
    ///
    /// Kill-under-the-reason-mutex: the kill happens while the reason
    /// slot is locked, so the exit it causes cannot be mapped before
    /// the slot is consistent; and a deadline that fires after the
    /// tree was reaped is a no-op that records nothing, so a late
    /// timer can never misattribute a natural exit as a limit kill.
    // r[impl builder.exec.limits-isolated]
    fn kill(&self, reason: KillReason) {
        let mut slot = self.reason.lock().unwrap_or_else(|e| e.into_inner());
        if self.tree.kill_tree() && slot.is_none() {
            *slot = Some(reason);
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
    let capture = CaptureFds::new(request, &plan).map_err(ExecError::Spawn)?;
    let (parent_capture, child_capture) = capture.split();

    // The child-side fd owners move INTO the fork closure below: a
    // dropped execute() future cannot close them (and recycle their
    // numbers into another execution's pipes) while a fork that
    // references those numbers is in flight.
    let owners = ChildFdOwners {
        status_w,
        go_r,
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
    tree.attach_reaper();
    let wait_task = {
        let tree = Arc::clone(&tree);
        tokio::task::spawn_blocking(move || {
            let status = wait_for(intermediate);
            // The pid is reaped: nothing may ever signal it again (it
            // could be recycled).
            tree.mark_reaped();
            status
        })
    };

    // ---- Cgroup attach, then the go byte ----------------------------------
    // The intermediate (and through fork inheritance the whole sandbox
    // process tree) must be inside the caller's cgroup before it starts
    // doing accountable work; it blocks on the go pipe until told
    // otherwise. The attach is a single small write to cgroupfs, done
    // inline rather than through spawn_blocking.
    if let Some(cgroup) = &request.limits.cgroup
        && let Err(e) = std::fs::write(
            cgroup.join("cgroup.procs"),
            format!("{}\n", intermediate.as_raw()),
        )
    {
        return abort_spawn(e, "attach the sandbox to the cgroup", tree_guard, wait_task).await;
    }
    if let Err(e) = nix::unistd::write(&go_w, &[1u8]) {
        return abort_spawn(
            std::io::Error::from(e),
            "signal the sandbox to proceed",
            tree_guard,
            wait_task,
        )
        .await;
    }
    drop(go_w);

    // ---- Readers -----------------------------------------------------------
    let status_task = tokio::task::spawn_blocking(move || read_status_pipe(status_r));
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
    let limit_reason: Arc<std::sync::Mutex<Option<KillReason>>> =
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
            reason: Arc::clone(&limit_reason),
        }
        .run(),
    );

    // ---- The supervision loop ----------------------------------------------
    let mut splitter = LineSplitter::default();
    let mut events_open = true;
    let mut started_sent = false;
    let mut status_report: Option<StatusReport> = None;
    let mut wait_status: Option<Result<i32, std::io::Error>> = None;
    let mut chunks_done = false;
    let mut status_task = status_task;
    let mut wait_task = wait_task;

    while wait_status.is_none() {
        tokio::select! {
            // Status pipe resolution: either the program exec'd or
            // setup failed somewhere.
            report = &mut status_task, if status_report.is_none() => {
                let report = report.unwrap_or(StatusReport::Corrupt);
                status_report = Some(report);
                match report {
                    StatusReport::ExecStarted => {
                        started_sent = true;
                        if events_open
                            && events
                                .send(ExecEvent::Started { pid: intermediate.as_raw() })
                                .await
                                .is_err()
                        {
                            events_open = false;
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
            // Captured output. Disabled once the readers hit EOF — the
            // wait arm below stays live regardless. Limit enforcement
            // lives in the watchdog, fed at raw-read time, so nothing
            // in this delivery path can delay a kill.
            chunk = chunk_rx.recv(), if !chunks_done => {
                match chunk {
                    Some(chunk) => {
                        for line in splitter.push(chunk.stream, &chunk.bytes) {
                            if events_open && events.send(line).await.is_err() {
                                events_open = false;
                            }
                        }
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
    // completes (kills are synchronous under the reason mutex) and
    // no-ops against the reaped tree without recording a reason.
    watchdog.abort();

    // Drain whatever output is still buffered, bounded.
    // One shared budget for the chunk drain AND the partial-line
    // flush: the tree is already gone, so a stalled (but alive) events
    // receiver must not be able to park execute() past the budget. If
    // the budget expires mid-drain the remaining lines are dropped —
    // best-effort by design once the receiver has stopped consuming.
    let _ = tokio::time::timeout(FINAL_DRAIN_TIMEOUT, async {
        while let Some(chunk) = chunk_rx.recv().await {
            for line in splitter.push(chunk.stream, &chunk.bytes) {
                if events_open && events.send(line).await.is_err() {
                    events_open = false;
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
    // The status report normally resolved long ago; give a straggler
    // (e.g. a setup failure racing the reap, or a program so fast that
    // the reap won the final select) a bounded window.
    if status_report.is_none() {
        status_report = Some(
            tokio::time::timeout(FINAL_DRAIN_TIMEOUT, &mut status_task)
                .await
                .map(|r| r.unwrap_or(StatusReport::Corrupt))
                .unwrap_or(StatusReport::Corrupt),
        );
    }
    // A very fast program can be reaped before the status arm ever ran;
    // the Started event is still owed to the caller. Best-effort and
    // bounded: it is the last event this execution emits, so a closed
    // channel needs no bookkeeping and a stalled receiver only costs
    // the drain budget, never an unbounded park.
    if !started_sent && status_report == Some(StatusReport::ExecStarted) && events_open {
        let _ = tokio::time::timeout(
            FINAL_DRAIN_TIMEOUT,
            events.send(ExecEvent::Started {
                pid: intermediate.as_raw(),
            }),
        )
        .await;
    }

    // ---- Interpret ----------------------------------------------------------
    match status_report {
        Some(StatusReport::SetupFailed(err)) => {
            // Resolve indexed phases back to the path they were
            // operating on so the operator-facing log names it.
            let entry = match err.phase {
                SetupPhase::Bind | SetupPhase::BindRemount => {
                    bind_targets.get(usize::from(err.detail))
                }
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
            return Err(ExecError::Setup(err));
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

    // Read the watchdog's verdict after the abort: the lock serializes
    // with a kill in flight, and a reason exists only when the kill
    // actually acted on the live tree.
    let kill_reason = *limit_reason.lock().unwrap_or_else(|e| e.into_inner());
    let exit = map_exit(raw_status, kill_reason);
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
    // r[impl builder.exec.fd-keep-set]
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
            // This process needs no fds beyond stdio any more: it only
            // waits for the sandbox child and forwards its exit status.
            // One full best-effort sweep (instead of hand-enumerated
            // closes that would silently miss any fd added later) so
            // EOF on the parent side tracks the sandbox child alone:
            // the status pipe must EOF the moment the child execs (its
            // copy is close-on-exec), and the pty/pipes must EOF the
            // moment the child exits, not when this process does.
            // r[impl builder.exec.fd-keep-set]
            // SAFETY: close_range over a numeric range; this process
            // touches no fd >= 3 after this point.
            unsafe {
                libc::syscall(libc::SYS_close_range, 3u32, libc::c_uint::MAX, 0u32);
            }
            let mut status: libc::c_int = 0;
            loop {
                // SAFETY: waitpid into a stack buffer for a direct child.
                let rc = unsafe { libc::waitpid(grandchild, &raw mut status, 0) };
                if rc == grandchild {
                    break;
                }
                if rc == -1 && Errno::last() != Errno::EINTR {
                    // Cannot learn the child's fate; 124 is this
                    // executor's "supervision failed" convention and is
                    // mapped to a plain exit code by the parent.
                    child::exit_immediately(124);
                }
            }
            let code = if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else if libc::WIFSIGNALED(status) {
                // Forward a fatal signal as the conventional 128+N.
                128 + libc::WTERMSIG(status)
            } else {
                124
            };
            child::exit_immediately(code);
        }
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
}

impl TreeState {
    fn new(cgroup: Option<PathBuf>) -> Arc<TreeState> {
        Arc::new(TreeState {
            phase: std::sync::Mutex::new(TreePhase::Armed),
            cgroup,
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

    /// Kill the adopted process tree, once, and never after it was
    /// reaped. Returns whether the kill *acted* — the tree was live
    /// (adopted, not yet killed, not yet reaped) and the signal was
    /// sent. Callers attributing an exit outcome to their kill (the
    /// [`LimitWatchdog`]) record their reason only on `true`: a kill
    /// that no-opped on an already-settled tree must not override the
    /// natural exit.
    ///
    /// Writes `cgroup.kill` when a cgroup was given (kills every
    /// process in the cgroup, including fork bombs the parent has
    /// never heard of) and then always SIGKILLs the intermediate
    /// directly: the tree may not be in the cgroup at all — the
    /// `cgroup.procs` attach is itself a step that can fail, and its
    /// abort path runs through here — and a cgroup the executor cannot
    /// write to must not leak the tree either. The intermediate's
    /// death SIGKILLs the sandbox child through `PR_SET_PDEATHSIG`
    /// (armed from the sandbox child's first setup instruction; see
    /// `child::setup`), and the sandbox child is pid 1 of the PID
    /// namespace, so the kernel then kills everything else in it.
    // r[impl builder.exec.tree-ownership]
    fn kill_tree(&self) -> bool {
        let mut phase = self.lock();
        if let TreePhase::Adopted { pid, killed, .. } = &mut *phase
            && !*killed
        {
            *killed = true;
            let pid = *pid;
            // The kill happens under the lock: cheap (a cgroupfs write
            // and a kill(2)), and it means mark_reaped can never
            // interleave between the killed=true flip and the signal.
            kill_pid_and_cgroup(self.cgroup.as_deref(), pid);
            true
        } else {
            false
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

/// Kill a process tree rooted at `pid`: `cgroup.kill` when a cgroup
/// exists, then always a direct SIGKILL of the root (see
/// [`TreeState::kill_tree`] for why both).
fn kill_pid_and_cgroup(cgroup: Option<&Path>, pid: nix::unistd::Pid) {
    if let Some(cgroup) = cgroup {
        let _ = std::fs::write(cgroup.join("cgroup.kill"), "1");
    }
    // SAFETY: SIGKILL to a pid the TreeState state machine guarantees
    // has not been reaped; a pid already dying from the cgroup kill
    // ignores the extra signal.
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

/// Map the intermediate's raw wait status (plus any kill reason the
/// executor recorded) to the caller-facing [`ExitOutcome`].
///
/// A recorded kill reason wins: the raw status of a tree the executor
/// itself killed is an implementation detail (`SIGKILL`, or whatever
/// exit code the shell turned it into). Otherwise the intermediate's
/// forwarding convention applies: exit codes `129..=192` are fatal
/// signals forwarded as `128 + signo` (a process that genuinely exits
/// with such a code is indistinguishable, which is the cost of the
/// convention), everything else is a plain exit code.
fn map_exit(raw_status: i32, kill: Option<KillReason>) -> ExitOutcome {
    if let Some(kill) = kill {
        return kill.into();
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

/// Early-error path between fork and the supervision loop: kill the
/// just-forked tree, reap it, and return a `Spawn` error.
async fn abort_spawn(
    error: std::io::Error,
    what: &'static str,
    tree_guard: ProcessTreeGuard,
    wait_task: tokio::task::JoinHandle<Result<i32, std::io::Error>>,
) -> Result<ExecutionOutcome, ExecError> {
    // Dropping the guard kills the adopted tree; the attached wait
    // task then reaps it (awaited so the tree is fully gone before the
    // error propagates).
    drop(tree_guard);
    let _ = wait_task.await;
    Err(ExecError::Spawn(std::io::Error::new(
        error.kind(),
        format!("failed to {what}: {error}"),
    )))
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
    capture: ChildCapture,
}

impl ChildFdOwners {
    /// The raw-fd view handed to the forked children. Valid for as
    /// long as `self` lives (which is the whole fork closure).
    fn child_fds(&self) -> ChildFds {
        ChildFds {
            status_pipe_w: self.status_w.as_raw_fd(),
            go_pipe_r: self.go_r.as_raw_fd(),
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
                        // r[impl builder.exec.limits-isolated]
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

    #[test]
    fn map_exit_kill_reason_wins() {
        assert_eq!(
            map_exit(exited(137), Some(KillReason::Timeout)),
            ExitOutcome::TimedOut
        );
        assert_eq!(
            map_exit(signaled(9), Some(KillReason::Silent)),
            ExitOutcome::Silent
        );
        assert_eq!(
            map_exit(exited(0), Some(KillReason::LogLimit)),
            ExitOutcome::LogLimitExceeded
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
        Arc<std::sync::Mutex<Option<KillReason>>>,
    ) {
        let (meter, bytes, activity) = ActivityMeter::new();
        let reason = Arc::new(std::sync::Mutex::new(None));
        let watchdog = LimitWatchdog {
            tree: Arc::clone(tree),
            timeout,
            max_silent,
            max_log_bytes,
            started: Instant::now(),
            activity,
            bytes,
            reason: Arc::clone(&reason),
        };
        (meter, watchdog, reason)
    }

    fn reason_of(slot: &std::sync::Mutex<Option<KillReason>>) -> Option<KillReason> {
        *slot.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The merged_bug_019 executor pin at the unit level: the timeout
    /// kill fires with ZERO event consumers anywhere — no chunk
    /// channel, no events receiver, nothing draining. Enforcement
    /// owns no send path to park on.
    // r[verify builder.exec.limits-isolated]
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
    // r[verify builder.exec.limits-isolated]
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
    // r[verify builder.exec.limits-isolated]
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
    // r[verify builder.exec.limits-isolated]
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
