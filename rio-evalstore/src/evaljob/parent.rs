//! The eval-parent orchestration loop (ADR-024 P3b): fork-no-exec
//! workers over per-worker socketpairs, relay worker frames to the
//! coordinator channel, route IFD completions, re-queue crashed
//! workers' attrs, and recycle workers between attrs.
//!
//! Single-threaded by construction: one `poll(2)` loop owns every fd.
//! Workers are forked from this loop, so the no-live-threads rule
//! holds at every fork (`r[bc.evalparent.fork-safety]`); the eval
//! callback runs ONLY in the child.
//!
//! Frame topology (the ADR's complete cross-process inventory): the
//! coordinator↔parent edge speaks `CoordinatorFrame`/`WorkerFrame`;
//! each parent↔worker edge speaks the SAME message types, so worker
//! frames relay upstream as raw bytes without re-encoding. The parent
//! peeks (decodes) each worker frame only to track attr completion and
//! IFD routing.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

use prost::Message;
use rio_proto::evaljob::{
    CoordinatorFrame, RecycleNotice, Shutdown, WorkItem, WorkerError, WorkerFrame,
    coordinator_frame, worker_frame,
};

use super::framing;
use super::framing::FdIo as ChanIo;
use crate::store::EvalStore;

/// Eval callback: runs in the FORKED WORKER for one attr. It must
/// evaluate the attr and emit the final `ResultFrame` (via
/// [`crate::ffi::rio_emit_result`] / [`EvalStore::assemble_subgraph`])
/// on `worker_fd` before returning `Ok`. `Err(message)` makes the
/// worker report a non-fatal `WorkerError` for the attr.
pub type EvalFn<'a> = dyn FnMut(&str, RawFd) -> Result<(), String> + 'a;

#[derive(Debug, Clone)]
pub struct EvalParentOpts {
    /// Maximum concurrent fork workers (N ≈ cores).
    pub max_workers: usize,
    /// Recycle a worker after this many attrs (0 = disabled).
    pub recycle_attrs: u32,
    /// Recycle a worker whose RSS exceeds this between attrs
    /// (0 = disabled).
    pub recycle_rss_mb: u64,
    /// How many times a crashed worker's in-flight attr is re-queued
    /// before it is reported lost.
    pub attr_retries: u32,
}

impl Default for EvalParentOpts {
    fn default() -> Self {
        EvalParentOpts {
            max_workers: std::thread::available_parallelism().map_or(4, |n| n.get()),
            recycle_attrs: 64,
            recycle_rss_mb: 4096,
            attr_retries: 1,
        }
    }
}

struct Worker {
    pid: libc::pid_t,
    stream: UnixStream,
    /// Attr assigned and not yet completed (final ResultFrame or
    /// WorkerError for it).
    current: Option<String>,
    attrs_done: u32,
    /// Shutdown sent (recycle or global drain) — no new assignments;
    /// EOF + exit are expected.
    draining: bool,
}

/// Drive the eval parent until the coordinator's `Shutdown` drains
/// cleanly (Ok) or the channel/setup fails (Err). `eval` runs in each
/// forked worker; the parent itself never evaluates.
pub fn run_eval_parent(
    store: &EvalStore,
    chan_fd: RawFd,
    opts: &EvalParentOpts,
    eval: &mut EvalFn<'_>,
) -> io::Result<()> {
    // The claim table must exist before the first fork so every
    // worker generation shares one mapping.
    store
        .enable_claim_table()
        .map_err(|e| io::Error::other(format!("claim table: {e}")))?;

    let mut chan = ChanIo(chan_fd);
    let mut workers: Vec<Worker> = Vec::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut retries: HashMap<String, u32> = HashMap::new();
    // IFD drv_path → worker pids awaiting a completion, in request
    // order. A Vec, not a single pid: two workers can block on the
    // SAME drv (shared import), and the coordinator answers each
    // request with its own completion — overwriting would strand the
    // first requester forever.
    let mut ifd_routes: HashMap<String, Vec<libc::pid_t>> = HashMap::new();
    let mut shutdown = false;
    let mut chan_eof = false;
    let mut generation: u32 = 0;

    loop {
        // Reap exited workers; crashed ones re-queue their attr
        // (bounded) — the parent NEVER dies from a worker death.
        // r[impl bc.evalparent.crash-requeue]
        reap(
            &mut workers,
            &mut queue,
            &mut retries,
            &mut ifd_routes,
            &mut chan,
            opts,
        )?;

        // Assign queued attrs; fork new workers up to the cap.
        if !shutdown || !queue.is_empty() {
            schedule(store, &mut workers, &mut queue, opts, eval, chan_fd)?;
        }

        // Drain: after Shutdown, once nothing is queued or in flight,
        // shut idle workers down and finish when all are gone.
        if shutdown && queue.is_empty() && workers.iter().all(|w| w.current.is_none()) {
            for w in workers.iter_mut().filter(|w| !w.draining) {
                let _ = framing::write_frame(
                    &mut &w.stream,
                    &CoordinatorFrame {
                        msg: Some(coordinator_frame::Msg::Shutdown(Shutdown {})),
                    },
                );
                w.draining = true;
            }
            if workers.is_empty() {
                return Ok(());
            }
        }
        if chan_eof && !shutdown {
            // Coordinator died without Shutdown: kill workers, bail.
            for w in &workers {
                // SAFETY: signalling our own children.
                unsafe { libc::kill(w.pid, libc::SIGKILL) };
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "coordinator channel closed before Shutdown",
            ));
        }

        // Wait for readability (or a child exit closing its fd —
        // POLLHUP wakes us; the timeout is only a reap fallback).
        let mut fds: Vec<libc::pollfd> = Vec::with_capacity(1 + workers.len());
        if !chan_eof {
            fds.push(libc::pollfd {
                fd: chan_fd,
                events: libc::POLLIN,
                revents: 0,
            });
        }
        for w in &workers {
            fds.push(libc::pollfd {
                fd: w.stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        if fds.is_empty() {
            continue; // chan closed, workers gone → loop resolves above
        }
        // SAFETY: fds points at a valid pollfd array of its length.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 200) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }

        let readable: Vec<RawFd> = fds
            .iter()
            .filter(|p| p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0)
            .map(|p| p.fd)
            .collect();
        for fd in readable {
            if fd == chan_fd {
                match handle_chan_frame(&mut chan, &mut workers, &mut queue, &mut ifd_routes)? {
                    ChanEvent::Frame => {}
                    ChanEvent::Shutdown => shutdown = true,
                    ChanEvent::Eof => chan_eof = true,
                }
            } else if let Some(idx) = workers.iter().position(|w| w.stream.as_raw_fd() == fd) {
                handle_worker_frame(
                    &mut chan,
                    &mut workers,
                    idx,
                    &mut ifd_routes,
                    &mut generation,
                    opts,
                )?;
            }
        }
    }
}

enum ChanEvent {
    Frame,
    Shutdown,
    Eof,
}

fn handle_chan_frame(
    chan: &mut ChanIo,
    workers: &mut [Worker],
    queue: &mut VecDeque<String>,
    ifd_routes: &mut HashMap<String, Vec<libc::pid_t>>,
) -> io::Result<ChanEvent> {
    let frame: Option<CoordinatorFrame> = framing::read_frame(chan)?;
    let Some(frame) = frame else {
        return Ok(ChanEvent::Eof);
    };
    match frame.msg {
        Some(coordinator_frame::Msg::Work(WorkItem { attr })) => {
            queue.push_back(attr);
            Ok(ChanEvent::Frame)
        }
        Some(coordinator_frame::Msg::IfdCompletion(c)) => {
            // Route to the oldest blocked requester (the coordinator
            // sends one completion per relayed request, so each pop
            // resumes exactly one worker). A dead worker's route was
            // dropped at reap time; a late completion with no route is
            // discarded (its attr was re-queued and will re-request).
            // r[impl bc.evalparent.ifd-relay]
            if let Some(pids) = ifd_routes.get_mut(&c.drv_path) {
                let pid = pids.remove(0);
                if pids.is_empty() {
                    ifd_routes.remove(&c.drv_path);
                }
                if let Some(w) = workers.iter_mut().find(|w| w.pid == pid) {
                    let _ = framing::write_frame(
                        &mut &w.stream,
                        &CoordinatorFrame {
                            msg: Some(coordinator_frame::Msg::IfdCompletion(c)),
                        },
                    );
                }
            }
            Ok(ChanEvent::Frame)
        }
        Some(coordinator_frame::Msg::AckFeedback(_)) => {
            // Cluster-ack digests. The coordinator is the retention
            // authority (it drops drv bodies on ack); workers don't
            // retain bodies past their frames, so there is nothing to
            // drop parent-side today. Kept as an explicit no-op so the
            // frame is consumed, not an error.
            Ok(ChanEvent::Frame)
        }
        Some(coordinator_frame::Msg::Shutdown(_)) => Ok(ChanEvent::Shutdown),
        None => Ok(ChanEvent::Frame), // unknown future frame kind
    }
}

fn handle_worker_frame(
    chan: &mut ChanIo,
    workers: &mut [Worker],
    idx: usize,
    ifd_routes: &mut HashMap<String, Vec<libc::pid_t>>,
    generation: &mut u32,
    opts: &EvalParentOpts,
) -> io::Result<()> {
    let raw = match framing::read_raw_frame(&mut &workers[idx].stream) {
        Ok(Some(raw)) => raw,
        // EOF or torn frame: the worker is gone (or dying) — reap()
        // owns the bookkeeping; just stop reading this fd by marking
        // it draining so schedule() skips it.
        Ok(None) | Err(_) => {
            workers[idx].draining = true;
            return Ok(());
        }
    };
    let decoded = WorkerFrame::decode(raw.as_slice())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    // Relay first (raw bytes — byte-stable by construction), then
    // update bookkeeping.
    framing::write_raw_frame(chan, &raw)?;

    let pid = workers[idx].pid;
    match decoded.msg {
        Some(worker_frame::Msg::Result(ref f)) if !f.root_drv_digest.is_empty() => {
            let w = &mut workers[idx];
            w.current = None;
            w.attrs_done += 1;
            maybe_recycle(chan, w, generation, opts)?;
        }
        Some(worker_frame::Msg::IfdRequest(ref req)) => {
            if let Some(node) = &req.node {
                ifd_routes
                    .entry(node.drv_path.clone())
                    .or_default()
                    .push(pid);
            }
        }
        Some(worker_frame::Msg::Error(ref e)) => {
            let w = &mut workers[idx];
            if w.current.as_deref() == Some(e.attr.as_str()) {
                w.current = None;
                w.attrs_done += 1;
                maybe_recycle(chan, w, generation, opts)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Recycle decision, checked between attrs (never mid-eval): attr
/// quota or RSS threshold → send Shutdown (process exit IS the GC)
/// and tell the coordinator via RecycleNotice.
// r[impl bc.evalparent.recycle]
fn maybe_recycle(
    chan: &mut ChanIo,
    w: &mut Worker,
    generation: &mut u32,
    opts: &EvalParentOpts,
) -> io::Result<()> {
    let quota = opts.recycle_attrs > 0 && w.attrs_done >= opts.recycle_attrs;
    let rss = opts.recycle_rss_mb > 0 && rss_mb(w.pid).is_some_and(|m| m > opts.recycle_rss_mb);
    if !(quota || rss) || w.draining {
        return Ok(());
    }
    let _ = framing::write_frame(
        &mut &w.stream,
        &CoordinatorFrame {
            msg: Some(coordinator_frame::Msg::Shutdown(Shutdown {})),
        },
    );
    w.draining = true;
    *generation += 1;
    framing::write_frame(
        chan,
        &WorkerFrame {
            msg: Some(worker_frame::Msg::Recycle(RecycleNotice {
                generation: *generation,
            })),
        },
    )
}

/// Resident set size of a child, from /proc/<pid>/statm (field 2 ×
/// page size). `None` when the proc entry is gone (already exited).
fn rss_mb(pid: libc::pid_t) -> Option<u64> {
    let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // SAFETY: sysconf is a pure query.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    Some(pages * page / (1024 * 1024))
}

fn reap(
    workers: &mut Vec<Worker>,
    queue: &mut VecDeque<String>,
    retries: &mut HashMap<String, u32>,
    ifd_routes: &mut HashMap<String, Vec<libc::pid_t>>,
    chan: &mut ChanIo,
    opts: &EvalParentOpts,
) -> io::Result<()> {
    loop {
        let mut status: libc::c_int = 0;
        // SAFETY: waitpid with WNOHANG never blocks.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid <= 0 {
            return Ok(());
        }
        let Some(idx) = workers.iter().position(|w| w.pid == pid) else {
            continue; // not ours (e.g. a grandchild reparented away)
        };
        let w = workers.swap_remove(idx);
        for pids in ifd_routes.values_mut() {
            pids.retain(|p| *p != pid);
        }
        ifd_routes.retain(|_, pids| !pids.is_empty());
        let crashed = !w.draining;
        let Some(attr) = w.current else {
            if crashed {
                // Idle worker died (e.g. OOM-killed between attrs):
                // nothing lost; a replacement forks on demand.
                report_crash(chan, "", &describe(pid, status), false)?;
            }
            continue;
        };
        // In-flight attr on a non-draining worker = crash. (Draining
        // workers are only ever shut down idle, so `current` set +
        // draining means the Shutdown raced a crash — same handling.)
        let n = retries.entry(attr.clone()).or_insert(0);
        *n += 1;
        if *n <= opts.attr_retries {
            // Re-queue to the FRONT: a fresh fork picks it up next.
            // Visibility rides a non-fatal, attr-less WorkerError —
            // the coordinator logs it without failing the attr.
            // r[impl bc.evalparent.crash-requeue]
            let msg = format!(
                "{}; re-queueing attr '{attr}' (attempt {})",
                describe(pid, status),
                *n + 1
            );
            queue.push_front(attr);
            report_crash(chan, "", &msg, false)?;
        } else {
            // Retries exhausted: the attr is lost (named WorkerError),
            // other attrs proceed.
            let msg = format!(
                "{}; attr '{attr}' crashed its worker {} times — giving up",
                describe(pid, status),
                *n
            );
            report_crash(chan, &attr, &msg, false)?;
        }
    }
}

fn describe(pid: libc::pid_t, status: libc::c_int) -> String {
    if libc::WIFSIGNALED(status) {
        format!(
            "eval worker (pid {pid}) killed by signal {}",
            libc::WTERMSIG(status)
        )
    } else {
        format!(
            "eval worker (pid {pid}) exited with status {}",
            libc::WEXITSTATUS(status)
        )
    }
}

fn report_crash(chan: &mut ChanIo, attr: &str, message: &str, fatal: bool) -> io::Result<()> {
    framing::write_frame(
        chan,
        &WorkerFrame {
            msg: Some(worker_frame::Msg::Error(WorkerError {
                attr: attr.to_string(),
                message: message.to_string(),
                fatal,
            })),
        },
    )
}

fn schedule(
    store: &EvalStore,
    workers: &mut Vec<Worker>,
    queue: &mut VecDeque<String>,
    opts: &EvalParentOpts,
    eval: &mut EvalFn<'_>,
    chan_fd: RawFd,
) -> io::Result<()> {
    while !queue.is_empty() {
        // Prefer an idle live worker; otherwise fork (up to the cap).
        let idle = workers
            .iter_mut()
            .find(|w| !w.draining && w.current.is_none());
        let w: &mut Worker = match idle {
            Some(w) => w,
            None => {
                let active = workers.iter().filter(|w| !w.draining).count();
                if active >= opts.max_workers.max(1) {
                    return Ok(()); // saturated; assign on next completion
                }
                let w = fork_worker(store, workers, eval, chan_fd)?;
                workers.push(w);
                workers.last_mut().expect("just pushed")
            }
        };
        let attr = queue.pop_front().expect("checked non-empty");
        match framing::write_frame(
            &mut &w.stream,
            &CoordinatorFrame {
                msg: Some(coordinator_frame::Msg::Work(WorkItem {
                    attr: attr.clone(),
                })),
            },
        ) {
            Ok(()) => w.current = Some(attr),
            Err(_) => {
                // Worker vanished between fork and assignment (or its
                // socket died): put the attr back; reap() handles the
                // corpse next iteration.
                w.draining = true;
                queue.push_front(attr);
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Fork one worker. The child closes the coordinator channel and every
/// sibling's socket, runs the worker loop, and NEVER returns.
fn fork_worker(
    store: &EvalStore,
    workers: &[Worker],
    eval: &mut EvalFn<'_>,
    chan_fd: RawFd,
) -> io::Result<Worker> {
    let (parent_end, child_end) = UnixStream::pair()?;
    // SAFETY: fork in a single-threaded process (the loop's invariant
    // — r[impl bc.evalparent.fork-safety]); the child only uses
    // inherited state plus the eval callback.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // CHILD. Close the parent-side fds so coordinator/worker EOF
        // semantics aren't held open by siblings.
        drop(parent_end);
        // SAFETY: closing inherited fds the child must not hold.
        unsafe {
            libc::close(chan_fd);
            for w in workers {
                libc::close(w.stream.as_raw_fd());
            }
        }
        // Drop the fork-inherited pack segment: it shares the parent's
        // O_APPEND file description with every sibling, so writing
        // through it would interleave records and corrupt this
        // process's offset bookkeeping. The first child write
        // allocates a fresh per-pid segment (single-writer restored).
        store.post_fork_child();
        run_worker_loop(store, child_end, eval);
    }
    drop(child_end);
    Ok(Worker {
        pid,
        stream: parent_end,
        current: None,
        attrs_done: 0,
        draining: false,
    })
}

/// The worker's frame loop: evaluate assigned attrs until Shutdown or
/// channel EOF, then flush the CAS and exit. Recycling is the
/// PARENT's decision (it sends Shutdown between attrs); the worker is
/// deliberately dumb so assignment vs. exit cannot race.
fn run_worker_loop(store: &EvalStore, stream: UnixStream, eval: &mut EvalFn<'_>) -> ! {
    let mut io = &stream;
    loop {
        match framing::read_frame::<_, CoordinatorFrame>(&mut io) {
            Ok(Some(CoordinatorFrame {
                msg: Some(coordinator_frame::Msg::Work(WorkItem { attr })),
            })) => {
                if let Err(message) = eval(&attr, stream.as_raw_fd()) {
                    let _ = framing::write_frame(
                        &mut io,
                        &WorkerFrame {
                            msg: Some(worker_frame::Msg::Error(WorkerError {
                                attr,
                                message,
                                fatal: false,
                            })),
                        },
                    );
                }
            }
            Ok(Some(CoordinatorFrame {
                msg: Some(coordinator_frame::Msg::Shutdown(_)),
            }))
            | Ok(None)
            | Err(_) => break,
            // Stray IfdCompletion/AckFeedback outside an IFD wait —
            // nothing is blocked on them; skip.
            Ok(Some(_)) => continue,
        }
    }
    // Persist this worker's pack segment + LRU touches; the process
    // exit IS the eval-state GC (boehmgc never collects under
    // GC_DONT_GC), so this flush is the only teardown that matters.
    let _ = store.flush();
    std::process::exit(0);
}
