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
//!   │                      attached it to the cgroup
//!   ├─ status pipe ◄────── intermediate + sandbox child: 8 bytes on
//!   │                      setup failure; EOF with 0 bytes when the
//!   │                      program exec'd (the write end is
//!   │                      close-on-exec)
//!   └─ pty master / pipes ◄ the program's stdout/stderr
//! ```
//!
//! The *intermediate* exists because `unshare(CLONE_NEWPID)` only
//! affects subsequently-forked children: it unshares, forks the sandbox
//! child (which becomes pid 1 of the new PID namespace), closes its own
//! copies of every pipe so EOF tracks the sandbox child alone, waits,
//! and forwards the exit status (signals as `128 + signo`).
//!
//! # Lifecycle and cleanup
//!
//! The chroot directory ([`HostLayout::chroot_dir`]) is owned by the
//! caller: the caller creates it (or reuses the previous attempt's) and
//! the caller removes it. The executor owns everything else it creates
//! — pipes, the pty, the forked process tree — and cleans them up on
//! every path, including cancellation: a [`KillGuard`] kills the
//! process tree if the future returned by [`execute`] is dropped before
//! the tree has been reaped.
//!
//! # Caller contract for the event channel
//!
//! The caller must keep draining the [`ExecEvent`] receiver: log
//! delivery awaits channel capacity, so a stalled (but alive) receiver
//! suspends log processing and with it **all** limit enforcement —
//! silence, log-volume, and the wall-clock timeout alike (the
//! supervision loop cannot reach its deadline arms while parked on the
//! send). A *dropped* receiver is fine — events are discarded and
//! execution continues.

use std::os::fd::{AsRawFd as _, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use nix::errno::Errno;
use tokio::sync::mpsc;

use crate::child::{self, ChildFds, SETUP_ERROR_WIRE_LEN, SetupError, SetupPhase};
use crate::outcome::{
    ExecEvent, ExecutionOutcome, ExitOutcome, LogStream, OutputFileType, OutputMetadata,
    OutputReport,
};
use crate::plan::{HostLayout, SandboxPlan};
use crate::request::{ExecutionRequest, OutputCapture};
use crate::{ExecError, skeleton};

/// How long the parent keeps draining buffered log output after the
/// process tree has been reaped. Bounds the gap between "the build
/// exited" and "execute() returned" when the pty buffer still holds
/// data; EOF normally arrives well before this.
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

    let child_fds = ChildFds {
        status_pipe_w: status_w.as_raw_fd(),
        go_pipe_r: go_r.as_raw_fd(),
        stdout_fd: capture.child_stdout.as_raw_fd(),
        stderr_fd: capture
            .child_stderr
            .as_ref()
            .unwrap_or(&capture.child_stdout)
            .as_raw_fd(),
    };

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
    // The fork happens on a dedicated blocking thread: `tokio::process`
    // cannot be used (the child must unshare namespaces and run
    // arbitrary pre-exec code without exec'ing immediately), and
    // forking from an async worker thread would stall the runtime for
    // the duration of the pre-fork bookkeeping. Only the forking thread
    // exists in the child, which is why everything the child touches
    // was precomputed into the plan.
    let intermediate = {
        let fds = child_fds;
        tokio::task::spawn_blocking(move || spawn_intermediate(&plan, &fds))
            .await
            .map_err(|e| ExecError::Spawn(std::io::Error::other(e)))?
            .map_err(ExecError::Spawn)?
    };

    // From here on the process tree exists: arm the guard that kills it
    // if this future is dropped or an early error returns.
    let armed = Arc::new(AtomicBool::new(true));
    let kill_guard = KillGuard {
        cgroup: request.limits.cgroup.clone(),
        pid: intermediate,
        armed: Arc::clone(&armed),
    };

    // Close the child-side fd copies so EOF on the parent-side ends
    // tracks the child processes alone.
    drop(status_w);
    drop(go_r);
    let parent_capture = capture.into_parent_side();

    // Reap on a dedicated blocking thread. Started before anything can
    // fail so every error path below can `await` it after killing.
    let wait_task = {
        let armed = Arc::clone(&armed);
        tokio::task::spawn_blocking(move || {
            let status = wait_for(intermediate);
            // Disarm the kill guard the moment the pid is reaped: a
            // SIGKILL after reaping could hit a recycled pid.
            armed.store(false, Ordering::Release);
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
        return abort_spawn(e, "attach the sandbox to the cgroup", kill_guard, wait_task).await;
    }
    if let Err(e) = nix::unistd::write(&go_w, &[1u8]) {
        return abort_spawn(
            std::io::Error::from(e),
            "signal the sandbox to proceed",
            kill_guard,
            wait_task,
        )
        .await;
    }
    drop(go_w);

    // ---- Readers -----------------------------------------------------------
    let status_task = tokio::task::spawn_blocking(move || read_status_pipe(status_r));
    let (chunk_tx, mut chunk_rx) = mpsc::channel::<LogChunk>(LOG_CHUNK_CHANNEL_CAPACITY);
    spawn_log_readers(parent_capture, &chunk_tx);
    // The select loop below must observe channel closure when the
    // readers finish, so the parent's own sender clone goes away now.
    drop(chunk_tx);

    // ---- The supervision loop ----------------------------------------------
    let mut splitter = LineSplitter::default();
    let mut events_open = true;
    let mut started_sent = false;
    let mut status_report: Option<StatusReport> = None;
    let mut kill_reason: Option<KillReason> = None;
    let mut wait_status: Option<Result<i32, std::io::Error>> = None;
    let mut chunks_done = false;
    let mut log_bytes: u64 = 0;
    let mut last_output = Instant::now();
    let mut status_task = status_task;
    let mut wait_task = wait_task;

    while wait_status.is_none() {
        let timeout_at = request.limits.timeout.map(|t| started + t);
        let silent_at = request.limits.max_silent.map(|t| last_output + t);
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
                        // no-op safety net. Keep looping until reaped.
                        kill_tree(request.limits.cgroup.as_deref(), intermediate, &armed);
                    }
                }
            }
            // Captured output. Disabled once the readers hit EOF — the
            // wait/timeout/silence arms below stay live regardless, so
            // a program that closes its own stdout/stderr and keeps
            // running is still bounded by the limits.
            chunk = chunk_rx.recv(), if !chunks_done => {
                match chunk {
                    Some(chunk) => {
                        last_output = Instant::now();
                        log_bytes += chunk.bytes.len() as u64;
                        for line in splitter.push(chunk.stream, &chunk.bytes) {
                            if events_open && events.send(line).await.is_err() {
                                events_open = false;
                            }
                        }
                        if kill_reason.is_none()
                            && request.limits.max_log_bytes.is_some_and(|max| log_bytes > max)
                        {
                            kill_reason = Some(KillReason::LogLimit);
                            kill_tree(request.limits.cgroup.as_deref(), intermediate, &armed);
                        }
                    }
                    // Both readers finished (EOF / EIO). Nothing more to
                    // enforce on the log side; the reap (and the
                    // deadlines) finish the loop.
                    None => chunks_done = true,
                }
            }
            // The process tree was reaped.
            status = &mut wait_task, if wait_status.is_none() => {
                wait_status = Some(status.unwrap_or_else(|e| Err(std::io::Error::other(e))));
            }
            // Wall-clock deadline.
            () = sleep_until_opt(timeout_at), if timeout_at.is_some() && kill_reason.is_none() => {
                kill_reason = Some(KillReason::Timeout);
                kill_tree(request.limits.cgroup.as_deref(), intermediate, &armed);
            }
            // Silence deadline.
            () = sleep_until_opt(silent_at), if silent_at.is_some() && kill_reason.is_none() => {
                kill_reason = Some(KillReason::Silent);
                kill_tree(request.limits.cgroup.as_deref(), intermediate, &armed);
            }
        }
    }
    let stop = SystemTime::now();
    // The tree is reaped; nothing left to kill.
    kill_guard.defuse();

    // Drain whatever output is still buffered, bounded.
    let _ = tokio::time::timeout(FINAL_DRAIN_TIMEOUT, async {
        while let Some(chunk) = chunk_rx.recv().await {
            log_bytes += chunk.bytes.len() as u64;
            for line in splitter.push(chunk.stream, &chunk.bytes) {
                if events_open && events.send(line).await.is_err() {
                    events_open = false;
                }
            }
        }
    })
    .await;
    if let Some(line) = splitter.flush()
        && events_open
    {
        // The channel may already be gone; the line is best-effort.
        let _ = events.send(line).await;
    }
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
    // the Started event is still owed to the caller. Best-effort: it is
    // the last event this execution emits, so a closed channel needs no
    // bookkeeping.
    if !started_sent && status_report == Some(StatusReport::ExecStarted) && events_open {
        let _ = events
            .send(ExecEvent::Started {
                pid: intermediate.as_raw(),
            })
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
            // Close this process's copies of every parent-facing fd so
            // EOF on the parent side tracks the sandbox child alone:
            // the status pipe must EOF the moment the child execs (its
            // copy is close-on-exec), and the pty/pipes must EOF the
            // moment the child exits, not when this process does.
            // SAFETY: closing fds this process owns and never touches
            // again.
            unsafe {
                libc::close(fds.status_pipe_w);
                libc::close(fds.stdout_fd);
                if fds.stderr_fd != fds.stdout_fd {
                    libc::close(fds.stderr_fd);
                }
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
// Parent-side helpers.
// ---------------------------------------------------------------------------

/// Kills the sandbox process tree when dropped, unless defused.
///
/// Exists so that dropping the future returned by [`execute`] (caller
/// cancellation) cannot leak a running build. With a cgroup the kill is
/// `cgroup.kill` (atomic over every descendant); without one it is a
/// SIGKILL of the intermediate process, whose death cascades to the
/// sandbox child via `PR_SET_PDEATHSIG` and from pid 1 to the rest of
/// the PID namespace. The `armed` flag is shared with the waitpid task,
/// which disarms it at reap time so a late kill cannot hit a recycled
/// pid.
struct KillGuard {
    cgroup: Option<PathBuf>,
    pid: nix::unistd::Pid,
    armed: Arc<AtomicBool>,
}

impl KillGuard {
    fn defuse(&self) {
        self.armed.store(false, Ordering::Release);
    }
}

impl Drop for KillGuard {
    fn drop(&mut self) {
        // Deliberately blocking inside Drop: there is no async Drop,
        // and the kill is a single tiny cgroupfs write (or a kill(2)),
        // not something worth a runtime handle.
        kill_tree(self.cgroup.as_deref(), self.pid, &self.armed);
    }
}

/// Kill the sandbox process tree, once.
///
/// Writes `cgroup.kill` when a cgroup was given (kills every process in
/// the cgroup, including fork bombs the parent has never heard of) and
/// then always SIGKILLs the intermediate directly: the tree may not be
/// in the cgroup at all — the `cgroup.procs` attach is itself a step
/// that can fail, and its abort path runs through here — and a cgroup
/// the executor cannot write to must not leak the tree either. The
/// intermediate's death SIGKILLs the sandbox child through
/// `PR_SET_PDEATHSIG`, and the sandbox child is pid 1 of the PID
/// namespace, so the kernel then kills everything else in it.
fn kill_tree(cgroup: Option<&Path>, pid: nix::unistd::Pid, armed: &AtomicBool) {
    // swap(false): only the first caller acts, and never after the
    // waitpid task has reaped the pid (it clears the flag).
    if !armed.swap(false, Ordering::AcqRel) {
        return;
    }
    if let Some(cgroup) = cgroup {
        let _ = std::fs::write(cgroup.join("cgroup.kill"), "1");
    }
    // SAFETY: SIGKILL to a pid this executor forked and has not reaped
    // (the armed flag above guards the reaped/recycled-pid case); a pid
    // already dying from the cgroup kill ignores the extra signal.
    unsafe {
        libc::kill(pid.as_raw(), libc::SIGKILL);
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
/// Lets the select arms above stay declarative.
async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
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
    kill_guard: KillGuard,
    wait_task: tokio::task::JoinHandle<Result<i32, std::io::Error>>,
) -> Result<ExecutionOutcome, ExecError> {
    drop(kill_guard); // kills the tree (still armed)
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

/// The parent-side read ends after the child-side ends were dropped.
struct ParentCapture(Vec<(OwnedFd, LogStream)>);

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

    /// Drop the child-side ends (post-fork) and keep the read side.
    fn into_parent_side(self) -> ParentCapture {
        ParentCapture(self.parent)
    }
}

/// Spawn one blocking reader per capture fd, feeding raw chunks into
/// `chunk_tx`. The readers exit on EOF, on EIO (how a pty master
/// reports that every slave fd is gone), or when the channel closes.
fn spawn_log_readers(capture: ParentCapture, chunk_tx: &mpsc::Sender<LogChunk>) {
    for (fd, stream) in capture.0 {
        let tx = chunk_tx.clone();
        tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 8192];
            loop {
                match nix::unistd::read(&fd, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
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
/// cap is reached the accumulated bytes are emitted as a (partial) line
/// so they flow through the normal event pipeline and whatever
/// log-volume limits the caller enforces there.
const MAX_PENDING_LINE_BYTES: usize = 1 << 20;

/// Accumulates raw chunks per stream and emits complete lines as
/// [`ExecEvent::Log`]s. Lines are split on `\n`; a trailing `\r` (pty
/// raw mode passes through the `\r\n` the line discipline would have
/// produced) is stripped. A line that reaches [`MAX_PENDING_LINE_BYTES`]
/// without a terminator is emitted in chunks of that size; a single
/// control frame longer than that (e.g. a >1 MiB `@nix` line) is
/// therefore split and dropped as malformed by the downstream filter —
/// pathological input, accepted trade-off.
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
                });
            } else {
                buf.push(*byte);
                if buf.len() >= MAX_PENDING_LINE_BYTES {
                    events.push(ExecEvent::Log {
                        stream,
                        line: std::mem::take(buf),
                    });
                }
            }
        }
        events
    }

    /// Emit the trailing unterminated line, if any. Called once after
    /// EOF so a final `printf` without a newline is not lost.
    fn flush(&mut self) -> Option<ExecEvent> {
        self.pending
            .iter_mut()
            .find(|(_, buf)| !buf.is_empty())
            .map(|(stream, buf)| ExecEvent::Log {
                stream: *stream,
                line: std::mem::take(buf),
            })
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;
    use crate::request::{Isolation, Limits, Mount, Personality};

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

    // -- kill_tree -----------------------------------------------------------

    #[test]
    fn kill_tree_kills_the_pid_even_when_the_cgroup_write_succeeds() {
        // A plain tempdir stands in for a cgroup directory whose
        // `cgroup.kill` write succeeds without killing anything — the
        // exact shape of the failed-attach abort path, where the tree
        // was never attached to the cgroup. The pid kill must still
        // happen or the caller deadlocks awaiting the reap.
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .expect("spawn sleeper");
        let pid = nix::unistd::Pid::from_raw(child.id() as i32);
        let armed = AtomicBool::new(true);
        kill_tree(Some(tmp.path()), pid, &armed);
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
    }

    // -- LineSplitter --------------------------------------------------------

    fn lines(events: Vec<ExecEvent>) -> Vec<(LogStream, Vec<u8>)> {
        events
            .into_iter()
            .map(|e| match e {
                ExecEvent::Log { stream, line } => (stream, line),
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
        // The remainder still flushes at EOF.
        assert!(s.flush().is_some());
    }

    #[test]
    fn line_splitter_splits_and_strips_crlf() {
        let mut s = LineSplitter::default();
        let events = s.push(LogStream::Merged, b"hello\r\nwor");
        assert_eq!(lines(events), vec![(LogStream::Merged, b"hello".to_vec())]);
        let events = s.push(LogStream::Merged, b"ld\n");
        assert_eq!(lines(events), vec![(LogStream::Merged, b"world".to_vec())]);
        assert!(s.flush().is_none());
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
        match s.flush() {
            Some(ExecEvent::Log { stream, line }) => {
                assert_eq!(stream, LogStream::Stdout);
                assert_eq!(line, b"out-partial".to_vec());
            }
            other => panic!("expected the stdout partial, got {other:?}"),
        }
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
