//! fuse-over-io_uring transport for the castore-FUSE (Linux 6.14+).
//!
//! This is the castore-FUSE's only request transport: the session
//! registers one set of ring entries per possible CPU against the
//! mountd-handed `/dev/fuse` fd, and once every queue has its entries
//! the kernel routes requests over the rings — one `COMMIT_AND_FETCH`
//! submission both answers a request and re-arms the entry, replacing
//! the classic `read(2)`+`write(2)` pair per upcall.
//!
//! # Threading: one thread per queue, each owning its own ring
//!
//! This is the libfuse ≥ 3.17 shape. Per FUSE queue (= per possible
//! CPU) there is one `castore-uring-q{N}` thread with its **own**
//! io_uring instance, pinned to that queue's CPU. The thread registers
//! its queue's entries and then lives inside
//! `io_uring_enter(GETEVENTS)`: each wakeup delivers a batch of
//! requests, fast ops are answered inline, and the resulting
//! `COMMIT_AND_FETCH`es are staged and flushed by the *same*
//! `io_uring_enter` that waits for the next batch — the steady state
//! is ~1 syscall per request, zero wakeup hops, zero cross-CPU
//! handoff. (A single shared ring with a central CQE loop and a
//! channel handoff to a worker pool measures ~3.5 `io_uring_enter` +
//! ~8 context switches per op on a metadata storm — wakeup latency,
//! not work, dominates — which is why the engine is shaped this way.)
//!
//! # Two-tier dispatch
//!
//! Fast ops — lookup, getattr, readdir(+plus), readlink, statfs,
//! xattrs, opendir/releasedir, and opens/reads served from local state
//! (a promoted cache entry, an already-open fallback handle) — are
//! answered inline on the queue thread; they are pure memory reads
//! against the prefetched DAG plus at most a local file op, and they
//! are >99% of a metadata storm. Ops that can wait on the network
//! (cold `open` → store gRPC → S3, reads inside a streaming fill
//! window, `release`'s mountd round-trip) are handed to a small slow
//! pool (`castore-uring-w{N}`).
//!
//! The slow pool **never submits to any ring**. Request delivery runs
//! as task work on the task that submitted the entry's command
//! (`io_uring_cmd_complete_in_task`), so a submitter that later blocks
//! — a cold open can hold a gRPC stream for seconds — would strand
//! every request delivery parked on its task. Only the queue thread,
//! which provably always returns to `io_uring_enter`, submits on its
//! ring. A slow worker finishes by writing the reply into the entry's
//! arena slot, pushing the entry index onto the queue's commit list,
//! and posting an `IORING_OP_MSG_RING` wake (via a shared waker ring)
//! into the queue's ring; the queue thread drains the list and submits
//! the `COMMIT_AND_FETCH`es itself. MSG_RING landed in 5.18 and
//! fuse-over-io_uring needs 6.14, so the wake op is always available
//! when this engine can run at all.
//!
//! # What stays on `/dev/fuse`
//!
//! The fuser session keeps running in parallel for the request classes
//! the kernel never routes over rings (INTERRUPT, FORGET,
//! notifications) and for the INIT handshake itself; it serves no
//! regular requests — the kernel parks those between the INIT reply
//! and ring readiness, then routes them all to the rings. Passthrough
//! is unchanged by the transport: `open()` replies carry the
//! mountd-brokered backing id and warm reads stay kernel-direct with
//! zero upcalls.
//!
//! # Failure posture
//!
//! The transport is mandatory: any setup error (io_uring unavailable,
//! kernel without `FUSE_OVER_IO_URING`, registration rejected) fails
//! the mount with an error naming the kernel requirement — Linux
//! 6.14+ with `fuse.enable_uring=1`. [`prepare`] runs before the INIT
//! handshake so the flag is only advertised when the rings already
//! exist, and [`Prepared::spawn`] returns only after every queue's
//! REGISTER batch reached the kernel without an inline rejection
//! (`fuse_uring_register` runs in the submit path, so a rejection's
//! CQE is visible when `io_uring_enter` returns). On a failed mount
//! the caller drops the session; closing the connection completes
//! whatever did register. Teardown of a live engine rides the
//! existing abort discipline: the fusectl `abort` write in
//! `CastoreSession::drop` makes the kernel complete every parked
//! entry with `-ENOTCONN`; each queue thread exits when all of its
//! entries saw a terminal CQE (entries checked out to the slow pool
//! come back through the commit path first, whose `COMMIT_AND_FETCH`
//! then completes with the teardown errno).

mod abi;
mod dispatch;
mod sys;

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use nix::libc;

use super::open::Opener;
use super::tree::InoMap;
use dispatch::{FastOutcome, RingFs};

/// Ring entries per queue. Each entry is one in-flight request slot on
/// its CPU; a depth-2 ring would serialize a CPU's metadata storm
/// behind two outstanding requests. Depth 8 keeps a queue busy through
/// readdirplus bursts while one or two entries wait in the slow pool.
/// Memory math: every entry pins `HEADER_BUF_SZ + PAYLOAD_BUF_SZ`
/// (512 B + 1 MiB) of zeroed-anonymous arena — on a 96-CPU box that is
/// 96 × 8 × ~1 MiB ≈ 768 MiB of *virtual* space, but untouched pages
/// cost no RSS and the kernel only writes the entries it actually
/// delivers into, so resident cost tracks concurrent request depth,
/// not the registration footprint.
const QUEUE_DEPTH: usize = 8;

/// Headers iovec size. Must be ≥ `sizeof(struct fuse_uring_req_header)`
/// (288); rounded up for alignment hygiene.
const HEADER_BUF_SZ: usize = 512;

/// Payload iovec size. The kernel demands
/// `max(FUSE_MIN_READ_BUFFER, max_write, max_pages·PAGE_SIZE)`:
/// `max_write` is pinned to [`URING_MAX_WRITE`] by `CastoreFs::init`,
/// and `max_pages` is kernel-clamped to
/// `fs.fuse.max_pages_limit` (default 256 → 1 MiB). 1 MiB therefore
/// covers every default configuration; an exotic sysctl bump surfaces
/// as a REGISTER `EINVAL` → failed mount at the registration gate,
/// not corruption. The allocation is zeroed-anonymous, so untouched
/// pages cost no RSS.
const PAYLOAD_BUF_SZ: usize = 1 << 20;

/// `max_write` set in the INIT reply. The castore-FUSE is read-only
/// (every write op is EROFS), so the only effect is bounding the
/// per-entry payload buffers the kernel makes us pre-register.
pub(super) const URING_MAX_WRITE: u32 = 256 * 1024;

/// Per-queue ring geometry. SQ: each of the queue's entries has at
/// most one staged SQE (REGISTER or COMMIT_AND_FETCH) at a time. CQ:
/// deliveries (≤ depth) plus MSG_RING wakes (≤ depth outstanding
/// commits, plus drained-early stragglers) — 4× depth leaves the
/// overflow path unreachable in practice.
const RING_SQ_ENTRIES: u32 = QUEUE_DEPTH as u32;
const RING_CQ_ENTRIES: u32 = (QUEUE_DEPTH * 4) as u32;

// Compile-time geometry guarantees: the headers iovec must satisfy the
// kernel's `iov_len >= sizeof(struct fuse_uring_req_header)` check and
// the payload must cover the largest request `URING_MAX_WRITE` allows.
const _: () = assert!(HEADER_BUF_SZ >= abi::URING_REQ_HEADER_SZ);
const _: () = assert!(PAYLOAD_BUF_SZ >= URING_MAX_WRITE as usize);
// fs.fuse.max_pages_limit default (256 pages → 1 MiB): the payload
// must also cover max_pages·PAGE_SIZE.
const _: () = assert!(PAYLOAD_BUF_SZ >= 256 * 4096);
// One SQ slot per entry is the invariant the no-rollback push relies
// on; a depth that outgrows the SQ would turn "SQ full" into a live
// failure mode instead of a logic-bug assertion.
const _: () = assert!(RING_SQ_ENTRIES as usize >= QUEUE_DEPTH);

/// Count the possible CPUs (`nr_queues = num_possible_cpus()` on the
/// kernel side): every queue id below this must be registered before
/// the kernel switches the connection to the rings — `is_ring_ready`
/// iterates all `nr_queues`, so registering only *online* CPUs would
/// park the mount forever. On our fleet possible == online; a box
/// where they diverge spends idle threads on the offline queues
/// (requests route by `task_cpu`, which only ever names online CPUs).
fn possible_cpus() -> io::Result<usize> {
    let raw = std::fs::read_to_string("/sys/devices/system/cpu/possible")?;
    parse_cpu_list(raw.trim())
        .ok_or_else(|| io::Error::other(format!("unparseable cpu list {raw:?}")))
}

/// Parse a kernel CPU list ("0", "0-63", "0-1,3") into a CPU count.
/// The kernel's queue ids are `task_cpu()` numbers, which assumes the
/// possible mask is dense (true everywhere we run); a sparse mask
/// would make `count != max+1` and io_uring mode is refused for it.
fn parse_cpu_list(s: &str) -> Option<usize> {
    let mut count = 0usize;
    let mut max = 0usize;
    for part in s.split(',') {
        let (lo, hi) = match part.split_once('-') {
            Some((lo, hi)) => (lo.parse().ok()?, hi.parse().ok()?),
            None => {
                let v: usize = part.parse().ok()?;
                (v, v)
            }
        };
        if hi < lo {
            return None;
        }
        count += hi - lo + 1;
        max = max.max(hi);
    }
    (count > 0 && count == max + 1).then_some(count)
}

/// Best-effort pin of the calling thread to `cpu`. Pinning is a
/// locality optimization, not a correctness requirement — the kernel
/// routes a request to the queue of the CPU the *caller* ran on, and
/// any thread may own any queue's ring — so failure (offline CPU,
/// restrictive cpuset) just logs and runs unpinned.
fn pin_to_cpu(cpu: usize) {
    if cpu >= libc::CPU_SETSIZE as usize {
        return;
    }
    // SAFETY: a zeroed cpu_set_t is a valid empty set; CPU_SET is
    // bounds-checked above; sched_setaffinity reads the set.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
            tracing::debug!(cpu, "queue thread pin failed; running unpinned");
        }
    }
}

// ─── buffer arena ─────────────────────────────────────────────────────────

/// One zeroed allocation holding every entry's headers + payload
/// region. Raw (not `Vec`) so workers can carve disjoint `&mut`
/// slices without aliasing an outer borrow.
struct BufArena {
    ptr: NonNull<u8>,
    layout: std::alloc::Layout,
    entries: usize,
}

// SAFETY: the arena is plain bytes; entry regions are disjoint and
// each is accessed by at most one thread at a time (see `slices`).
unsafe impl Send for BufArena {}
// SAFETY: see above.
unsafe impl Sync for BufArena {}

impl BufArena {
    fn new(entries: usize) -> io::Result<Self> {
        let size = entries * (HEADER_BUF_SZ + PAYLOAD_BUF_SZ);
        let layout = std::alloc::Layout::from_size_align(size, 4096)
            .map_err(|e| io::Error::other(format!("arena layout: {e}")))?;
        // SAFETY: layout has non-zero size (entries ≥ 1).
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        let ptr = NonNull::new(ptr).ok_or_else(|| io::Error::from(io::ErrorKind::OutOfMemory))?;
        Ok(Self {
            ptr,
            layout,
            entries,
        })
    }

    fn header_ptr(&self, idx: usize) -> *mut u8 {
        debug_assert!(idx < self.entries);
        // SAFETY: idx-checked offset within the allocation.
        unsafe { self.ptr.as_ptr().add(idx * HEADER_BUF_SZ) }
    }

    fn payload_ptr(&self, idx: usize) -> *mut u8 {
        debug_assert!(idx < self.entries);
        // SAFETY: idx-checked offset within the allocation.
        unsafe {
            self.ptr
                .as_ptr()
                .add(self.entries * HEADER_BUF_SZ + idx * PAYLOAD_BUF_SZ)
        }
    }

    /// The entry's (headers, payload) regions.
    ///
    /// SAFETY: the caller must hold exclusive use of entry `idx` —
    /// guaranteed by the ring protocol: the kernel hands an entry to
    /// userspace via exactly one CQE, exactly one thread serves that
    /// index at a time (queue thread inline, or the one slow worker it
    /// was punted to), and the kernel does not touch the buffers again
    /// until the entry is committed back.
    #[allow(clippy::mut_from_ref)]
    unsafe fn slices(&self, idx: usize) -> (&mut [u8], &mut [u8]) {
        // SAFETY: disjoint, in-bounds regions per the function contract.
        unsafe {
            (
                std::slice::from_raw_parts_mut(self.header_ptr(idx), HEADER_BUF_SZ),
                std::slice::from_raw_parts_mut(self.payload_ptr(idx), PAYLOAD_BUF_SZ),
            )
        }
    }
}

impl Drop for BufArena {
    fn drop(&mut self) {
        // SAFETY: allocated with exactly this layout in `new`.
        unsafe { std::alloc::dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

/// The per-entry iovec pairs the REGISTER SQEs point at. Kept alive
/// for the ring's lifetime (the kernel reads them during REGISTER).
struct IovecTable(Box<[libc::iovec]>);

// SAFETY: the table is written once at setup and only read afterwards.
unsafe impl Send for IovecTable {}
// SAFETY: see above.
unsafe impl Sync for IovecTable {}

fn build_iovecs(arena: &BufArena, entries: usize) -> IovecTable {
    let iovecs: Vec<libc::iovec> = (0..entries)
        .flat_map(|idx| {
            [
                libc::iovec {
                    iov_base: arena.header_ptr(idx).cast(),
                    iov_len: HEADER_BUF_SZ,
                },
                libc::iovec {
                    iov_base: arena.payload_ptr(idx).cast(),
                    iov_len: PAYLOAD_BUF_SZ,
                },
            ]
        })
        .collect();
    IovecTable(iovecs.into_boxed_slice())
}

// ─── engine ──────────────────────────────────────────────────────────────

/// Pre-INIT probe result: created (but unregistered) per-queue rings
/// plus the waker ring. Probing **before** INIT matters: once the INIT
/// reply advertises `FUSE_OVER_IO_URING` the kernel blocks request
/// processing until the rings register (or a registration fails), so
/// the flag must only be sent when ring creation already succeeded —
/// an io_uring blocked by seccomp/sysctl is discovered here, before
/// the handshake.
pub(super) struct Prepared {
    rings: Vec<sys::Ring>,
    waker: sys::Ring,
    nqueues: usize,
}

/// Probe io_uring + queue geometry. `Err` fails the mount — the
/// transport is mandatory and the caller reports the kernel
/// requirement.
pub(super) fn prepare() -> io::Result<Prepared> {
    let nqueues = possible_cpus()?;
    let mut rings = Vec::with_capacity(nqueues);
    for _ in 0..nqueues {
        let ring = sys::Ring::new(RING_SQ_ENTRIES, RING_CQ_ENTRIES)?;
        if ring.sq_capacity() < QUEUE_DEPTH as u32 {
            // CLAMP shrank the SQ below one slot per entry; the commit
            // path could then hit a full SQ. Refuse rather than wedge.
            return Err(io::Error::other(format!(
                "io_uring SQ clamped to {} (< {QUEUE_DEPTH} entries)",
                ring.sq_capacity()
            )));
        }
        rings.push(ring);
    }
    // The waker only ever holds one MSG_RING at a time —
    // push_submit_drain serializes callers.
    let waker = sys::Ring::new(2, 8)?;
    Ok(Prepared {
        rings,
        waker,
        nqueues,
    })
}

/// One FUSE queue's engine-side state.
struct Queue {
    ring: sys::Ring,
    /// Entries whose reply a slow worker wrote and that wait for the
    /// queue thread to submit the COMMIT_AND_FETCH (workers never
    /// submit — see the module docs).
    commits: Mutex<Vec<usize>>,
}

struct Shared {
    /// Declared before the arena: dropping the ring fds cancels any
    /// still-parked commands, after which the kernel cannot touch the
    /// arena again.
    queues: Vec<Queue>,
    /// MSG_RING source for slow-worker wakes; shared by any thread via
    /// `push_submit_drain`.
    waker: sys::Ring,
    /// Dup of the session's `/dev/fuse` fd (same `struct file`, same
    /// fuse connection) the uring cmds target.
    fuse_fd: OwnedFd,
    arena: BufArena,
    iovecs: IovecTable,
    /// First-failure latch so a post-registration entry error logs
    /// once at warn.
    failed: AtomicBool,
}

/// A running fuse-over-io_uring engine. Held by `CastoreSession`
/// purely to tie the buffer lifetime to the session; the threads are
/// detached and wind down on the teardown CQEs (same discipline as
/// fuser's own worker threads).
pub(crate) struct Engine {
    _shared: Arc<Shared>,
}

impl Prepared {
    /// Start one queue thread per queue (each registers its own
    /// entries) plus `workers` slow-pool threads, then wait for every
    /// queue to confirm its REGISTER batch reached the kernel without
    /// an inline rejection. `Err` fails the mount; the caller drops
    /// the session, and closing the fuse connection completes whatever
    /// did register (the threads hold their own `Arc<Shared>`, so the
    /// arena outlives any kernel reference either way).
    // r[impl builder.fs.io-uring-transport]
    pub(super) fn spawn(
        self,
        fuse_fd: OwnedFd,
        tree: Arc<InoMap>,
        opener: Arc<Opener>,
        workers: usize,
    ) -> io::Result<Engine> {
        let nqueues = self.nqueues;
        let entries = nqueues * QUEUE_DEPTH;
        let arena = BufArena::new(entries)?;
        let iovecs = build_iovecs(&arena, entries);

        let shared = Arc::new(Shared {
            queues: self
                .rings
                .into_iter()
                .map(|ring| Queue {
                    ring,
                    commits: Mutex::new(Vec::new()),
                })
                .collect(),
            waker: self.waker,
            fuse_fd,
            arena,
            iovecs,
            failed: AtomicBool::new(false),
        });

        let (tx, rx) = mpsc::channel::<usize>();
        let rx = Arc::new(Mutex::new(rx));
        let fs = Arc::new(RingFs { tree, opener });

        for w in 0..workers.max(1) {
            let worker_shared = Arc::clone(&shared);
            let rx = Arc::clone(&rx);
            let fs = Arc::clone(&fs);
            std::thread::Builder::new()
                .name(format!("castore-uring-w{w}"))
                .spawn(move || slow_worker_loop(&worker_shared, &fs, &rx))?;
        }

        let (ack_tx, ack_rx) = mpsc::channel::<io::Result<()>>();
        for qid in 0..nqueues {
            let queue_shared = Arc::clone(&shared);
            let fs = Arc::clone(&fs);
            let tx = tx.clone();
            let ack = ack_tx.clone();
            std::thread::Builder::new()
                .name(format!("castore-uring-q{qid}"))
                .spawn(move || queue_loop(&queue_shared, qid, &fs, &tx, &ack))?;
        }
        drop(ack_tx);

        // Registration gate: the mount is only declared up when every
        // queue's REGISTERs are parked in the kernel. A rejection
        // (e.g. a sysctl-bumped max_pages making the payload iovec too
        // small) completes inline, so the queue thread sees it before
        // acking.
        for _ in 0..nqueues {
            match ack_rx.recv() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(io::Error::other("queue thread exited before registering"));
                }
            }
        }
        tracing::info!(
            queues = nqueues,
            depth = QUEUE_DEPTH,
            payload_kib = PAYLOAD_BUF_SZ / 1024,
            "fuse-over-io_uring active: ring entries registered"
        );

        Ok(Engine { _shared: shared })
    }
}

/// The queue an entry index belongs to (`idx / QUEUE_DEPTH` — entries
/// are laid out queue-major in the arena and iovec table).
fn qid_of(idx: usize) -> usize {
    idx / QUEUE_DEPTH
}

/// Build the uring-cmd SQE for entry `idx` (REGISTER or
/// COMMIT_AND_FETCH — identical shape, different sub-command and
/// commit id).
fn entry_sqe(shared: &Shared, idx: usize, cmd_op: u32, commit_id: u64) -> sys::Sqe128 {
    sys::Sqe128::uring_cmd(
        shared.fuse_fd.as_raw_fd(),
        cmd_op,
        &shared.iovecs.0[idx * 2] as *const libc::iovec as u64,
        2,
        idx as u64,
        abi::encode_cmd_req(commit_id, qid_of(idx) as u16),
    )
}

/// `user_data` sentinel the MSG_RING wake posts into a queue ring
/// (entry indices are small, so the max value can never collide).
const WAKE_UD: u64 = u64::MAX;

/// One queue's serving thread: register the queue's entries, report
/// the outcome to [`Prepared::spawn`]'s registration gate, then run
/// the pump until every entry saw a terminal CQE.
fn queue_loop(
    shared: &Shared,
    qid: usize,
    fs: &RingFs,
    tx: &mpsc::Sender<usize>,
    ack: &mpsc::Sender<io::Result<()>>,
) {
    pin_to_cpu(qid);
    let ring = &shared.queues[qid].ring;

    // Stage this queue's REGISTERs, then flush them with one explicit
    // submit so the ack means the kernel actually saw them — the
    // pump's submit_and_wait would otherwise conflate "submitted" with
    // "first request arrived".
    for d in 0..QUEUE_DEPTH {
        let idx = qid * QUEUE_DEPTH + d;
        if let Err(e) = ring.push(&entry_sqe(shared, idx, abi::FUSE_IO_URING_CMD_REGISTER, 0)) {
            // Nothing submitted yet (push only stages); the thread can
            // exit cleanly without leaking kernel-held entries.
            let _ = ack.send(Err(e));
            return;
        }
    }
    if let Err(e) = ring.submit() {
        // Consumed-state unknown: some REGISTERs may have reached the
        // kernel. Report the failure (spawn aborts the mount), then
        // fall into the pump to drain whatever registered — the
        // caller's session drop closes the connection and completes
        // those entries.
        let _ = ack.send(Err(e));
    } else {
        // A rejected REGISTER completes inline during the submit
        // (`fuse_uring_register` runs in the issue path), so a failure
        // CQE is already visible here. Peek without consuming: the
        // pump below stays the ring's only CQ consumer and does the
        // terminal accounting for the same CQE.
        let _ = ack.send(match ring.peek_failure() {
            Some(cqe) => Err(io::Error::from_raw_os_error(-cqe.res)),
            None => Ok(()),
        });
    }

    let mut serve = |idx: usize| serve_on_queue(shared, fs, idx, tx);
    pump(shared, qid, QUEUE_DEPTH, &mut serve);
    tracing::debug!(qid, "fuse-over-io_uring queue drained; thread exiting");
}

/// The queue thread's steady-state loop, factored from [`queue_loop`]
/// so the lifecycle (terminal accounting, wake handling, commit-list
/// drain, exit condition) is unit-testable without a fuse connection.
///
/// `live` is the number of entries that still owe a terminal CQE;
/// `serve` handles one delivered entry (dispatch inline or punt) and
/// stages any resulting COMMIT_AND_FETCH on the queue's ring — the
/// next `submit_and_wait` flushes it.
fn pump(shared: &Shared, qid: usize, mut live: usize, serve: &mut dyn FnMut(usize)) {
    let queue = &shared.queues[qid];
    let lo = qid * QUEUE_DEPTH;
    let mut cqes = Vec::new();
    while live > 0 {
        if let Err(e) = queue.ring.submit_and_wait(&mut cqes) {
            tracing::warn!(qid, error = %e, "fuse-over-io_uring CQE wait failed; queue loop exiting");
            return;
        }
        for cqe in &cqes {
            if cqe.user_data == WAKE_UD {
                // The wake's only job was to interrupt the GETEVENTS
                // wait; the drain below runs on every batch.
            } else if cqe.res < 0 {
                entry_terminated(shared, cqe.res);
                live -= 1;
            } else if (cqe.user_data as usize) < lo || (cqe.user_data as usize) >= lo + QUEUE_DEPTH
            {
                // Our SQEs only ever carry this queue's entry indices
                // or the wake sentinel; anything else would index out
                // of the queue's slice of the arena. Drop it rather
                // than corrupt memory.
                tracing::error!(qid, user_data = cqe.user_data, "CQE with unknown user_data");
            } else {
                serve(cqe.user_data as usize);
            }
        }
        // Drain on every batch, not only on WAKE CQEs: the wake is
        // just a kick out of GETEVENTS, so a wake that coalesces with
        // (or loses a race to) another wakeup can never strand a
        // queued commit.
        drain_commits(shared, qid);
    }
}

/// Submit the queued COMMIT_AND_FETCHes for entries whose reply a slow
/// worker finished (the commit id is read back from the trailer the
/// worker filled in). Stages only — the pump's next `submit_and_wait`
/// flushes.
fn drain_commits(shared: &Shared, qid: usize) {
    let queue = &shared.queues[qid];
    let pending: Vec<usize> = std::mem::take(&mut *queue.commits.lock().expect("commits poisoned"));
    for idx in pending {
        // SAFETY: the worker is done with the entry once it lands on
        // the commit queue; the queue thread now holds exclusive use.
        let (header, _) = unsafe { shared.arena.slices(idx) };
        let Some(ent) = abi::EntInOut::parse(header) else {
            tracing::error!(idx, "commit with unparseable trailer; entry parked");
            continue;
        };
        stage_commit(shared, qid, idx, ent.commit_id);
    }
}

/// Stage one COMMIT_AND_FETCH. A push failure parks the entry for
/// good: the kernel keeps the request pending and the queue's live
/// count never drains, so teardown leaks this engine's threads + arena
/// — accepted over the alternative (freeing buffers the kernel may
/// still write). Unreachable by construction: the SQ has one slot per
/// entry.
fn stage_commit(shared: &Shared, qid: usize, idx: usize, commit_id: u64) {
    let sqe = entry_sqe(
        shared,
        idx,
        abi::FUSE_IO_URING_CMD_COMMIT_AND_FETCH,
        commit_id,
    );
    if let Err(e) = shared.queues[qid].ring.push(&sqe) {
        tracing::error!(idx, error = %e, "fuse-over-io_uring commit failed; entry parked");
    }
}

/// Account one entry's terminal CQE. Teardown errnos are the normal
/// wind-down path; anything else logs once at warn. A failed REGISTER
/// already failed the mount through the registration gate; a failed
/// COMMIT_AND_FETCH retires this entry's slot (the connection keeps
/// the ring with one slot less) — either way the slot is gone, never
/// reused.
fn entry_terminated(shared: &Shared, res: i32) {
    let teardown = matches!(-res, libc::ENOTCONN | libc::ECANCELED | libc::ECONNABORTED);
    if !teardown && !shared.failed.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            errno = -res,
            "fuse-over-io_uring entry failed; slot retired"
        );
    } else {
        tracing::debug!(errno = -res, "fuse-over-io_uring entry retired");
    }
}

/// Serve one delivered entry on its queue thread: fast ops are
/// answered and committed inline; blocking ops are punted to the slow
/// pool, which hands the entry back through the commit list + wake.
fn serve_on_queue(shared: &Shared, fs: &RingFs, idx: usize, tx: &mpsc::Sender<usize>) {
    // SAFETY: `idx` arrived via exactly one CQE, so this thread holds
    // exclusive use of the entry until it commits or punts.
    let (header, payload) = unsafe { shared.arena.slices(idx) };
    let (hdr, ent) = match parse_entry(header) {
        Ok(pair) => pair,
        Err(e) => {
            // Kernel contract violation; no reply can be produced and
            // the buffers must stay allocated, so the entry parks and
            // teardown leaks this engine's threads + arena.
            tracing::error!(idx, error = %e, "fuse-over-io_uring dispatch failed; entry parked");
            return;
        }
    };
    let req_len = (ent.payload_sz as usize).min(PAYLOAD_BUF_SZ);
    let reply = match dispatch::handle_fast(
        fs,
        &hdr,
        &header[abi::URING_IN_OUT_HEADER_SZ..],
        payload,
        req_len,
    ) {
        FastOutcome::Done(reply) => reply,
        FastOutcome::Punt => match tx.send(idx) {
            Ok(()) => return, // the slow pool owns the entry now
            Err(_) => {
                // Workers gone (a worker panicked). Degrade to an
                // inline error rather than wedge the request.
                tracing::error!(idx, "fuse-over-io_uring slow pool gone; replying EIO");
                dispatch::Reply::err(fuser::Errno::EIO)
            }
        },
    };
    abi::write_out_header(header, reply.error, hdr.unique, reply.len);
    abi::EntInOut::write(header, ent.commit_id, reply.len as u32);
    stage_commit(shared, qid_of(idx), idx, ent.commit_id);
}

/// Parse the entry's request framing, shared by both tiers.
fn parse_entry(header: &[u8]) -> io::Result<(abi::InHeader, abi::EntInOut)> {
    let hdr = abi::InHeader::parse(header)
        .ok_or_else(|| io::Error::other("short fuse_in_header in ring entry"))?;
    let ent = abi::EntInOut::parse(header)
        .ok_or_else(|| io::Error::other("short fuse_uring_ent_in_out in ring entry"))?;
    Ok((hdr, ent))
}

/// Slow-pool worker: serve punted (network-bound) ops, then hand the
/// entry back to its queue thread. Never submits to any ring — see
/// the module docs for why.
fn slow_worker_loop(shared: &Shared, fs: &RingFs, rx: &Mutex<mpsc::Receiver<usize>>) {
    loop {
        // Hold the receiver lock only for the recv itself.
        let idx = match rx.lock().expect("uring rx poisoned").recv() {
            Ok(idx) => idx,
            Err(_) => return, // every queue thread exited
        };
        // SAFETY: the queue thread punted `idx` to exactly one worker;
        // this worker holds exclusive use until the commit-list push.
        let (header, payload) = unsafe { shared.arena.slices(idx) };
        let (hdr, ent) = match parse_entry(header) {
            Ok(pair) => pair,
            Err(e) => {
                tracing::error!(idx, error = %e, "fuse-over-io_uring dispatch failed; entry parked");
                continue;
            }
        };
        let reply =
            dispatch::handle_slow(fs, &hdr, &header[abi::URING_IN_OUT_HEADER_SZ..], payload);
        abi::write_out_header(header, reply.error, hdr.unique, reply.len);
        abi::EntInOut::write(header, ent.commit_id, reply.len as u32);

        // Queue-then-wake: the commit must be visible before the
        // MSG_RING CQE so the queue thread's drain always finds it.
        let qid = qid_of(idx);
        shared.queues[qid]
            .commits
            .lock()
            .expect("commits poisoned")
            .push(idx);
        wake_queue(shared, qid, idx);
    }
}

/// Post a MSG_RING wake into `qid`'s ring. On failure the commit is
/// already queued, so any later wake (or any delivery CQE) still
/// drains it — only the prompt delivery is lost; on an otherwise idle
/// queue that means the request waits until teardown, hence the warn.
fn wake_queue(shared: &Shared, qid: usize, idx: usize) {
    let sqe = sys::Sqe128::msg_ring(shared.queues[qid].ring.raw_fd(), WAKE_UD, 0, idx as u64);
    let mut out = Vec::new();
    if let Err(e) = shared.waker.push_submit_drain(&sqe, &mut out) {
        tracing::warn!(idx, error = %e, "fuse-over-io_uring wake submit failed");
        return;
    }
    for cqe in &out {
        if cqe.res < 0 {
            tracing::warn!(
                idx = cqe.user_data,
                errno = -cqe.res,
                "fuse-over-io_uring wake rejected"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CPU-list parsing: queue count drives ring registration; an
    /// off-by-one here either under-registers (mount wedges on a
    /// never-ready ring… until the kernel-side register failure) or
    /// over-registers (EINVAL).
    #[test]
    fn cpu_list_parses_dense_masks() {
        assert_eq!(parse_cpu_list("0"), Some(1));
        assert_eq!(parse_cpu_list("0-3"), Some(4));
        assert_eq!(parse_cpu_list("0-63"), Some(64));
        assert_eq!(parse_cpu_list("0-1,2-3"), Some(4));
        // Sparse masks (count != max+1) are refused: kernel qids are
        // raw cpu numbers, which a sparse mask would push out of the
        // registered range.
        assert_eq!(parse_cpu_list("0,2"), None);
        assert_eq!(parse_cpu_list(""), None);
        assert_eq!(parse_cpu_list("3-1"), None);
        assert_eq!(parse_cpu_list("x"), None);
    }

    // r[verify builder.fs.io-uring-required]
    /// The pre-INIT probe must fail cleanly (an `Err`, never a panic
    /// or a partial state) when io_uring is unavailable, and succeed
    /// on capable hosts — this is the decision point that fails the
    /// mount before the INIT handshake when rings cannot exist, so an
    /// unsupported environment gets a clear error instead of a wedged
    /// mount.
    #[test]
    fn prepare_probes_without_panicking() {
        match prepare() {
            Ok(p) => {
                assert!(p.nqueues >= 1);
                assert_eq!(p.rings.len(), p.nqueues, "one ring per queue");
            }
            Err(e) => {
                // seccomp/sysctl-disabled io_uring or an unparseable
                // cpu mask — exactly the hard-mount-error class.
                tracing::debug!(error = %e, "probe failed (acceptable on restricted hosts)");
            }
        }
    }

    /// Arena regions are disjoint and zeroed — overlapping regions
    /// would let two in-flight requests corrupt each other's replies.
    /// Indices straddle the first queue boundary at depth 8 so the
    /// queue-major layout (idx 7 = q0 last, idx 8 = q1 first) is what
    /// gets checked.
    #[test]
    fn arena_regions_disjoint_across_queue_boundary() {
        let arena = BufArena::new(2 * QUEUE_DEPTH).unwrap();
        // SAFETY: test holds the only reference; indices distinct.
        let (h0, p0) = unsafe { arena.slices(QUEUE_DEPTH - 1) };
        let (h1, p1) = unsafe { arena.slices(QUEUE_DEPTH) };
        let r0 = h0.as_ptr() as usize..h0.as_ptr() as usize + h0.len();
        assert!(!r0.contains(&(h1.as_ptr() as usize)));
        assert!(!r0.contains(&(p0.as_ptr() as usize)));
        assert!(!r0.contains(&(p1.as_ptr() as usize)));
        assert!(h0.iter().all(|&b| b == 0));
        h0[0] = 7;
        assert_eq!(h1[0], 0, "writes must not bleed across entries");
    }

    /// Entry→queue routing at depth 8: the qid in the SQE command area
    /// is what the kernel uses to pick the queue, so a drifted mapping
    /// would register entries against the wrong queue and the ring
    /// would never become ready.
    #[test]
    fn entry_indices_map_queue_major() {
        assert_eq!(QUEUE_DEPTH, 8, "tests below assume depth 8");
        assert_eq!(qid_of(0), 0);
        assert_eq!(qid_of(QUEUE_DEPTH - 1), 0);
        assert_eq!(qid_of(QUEUE_DEPTH), 1);
        assert_eq!(qid_of(3 * QUEUE_DEPTH + 5), 3);
    }

    /// A `Shared` against a regular file instead of a fuse connection:
    /// regular files have no `f_op->uring_cmd`, so every uring cmd
    /// completes immediately with an `-EOPNOTSUPP` CQE — exactly what
    /// the lifecycle tests need (terminal accounting without a kernel
    /// fuse session). NOT `/dev/null`: that one *implements* uring_cmd
    /// (`uring_cmd_null`, returns 0) and would look like a successful
    /// delivery. `None` when io_uring itself is unavailable
    /// (restricted sandbox) — callers skip.
    fn test_shared(nqueues: usize) -> Option<Arc<Shared>> {
        let mut rings = Vec::new();
        for _ in 0..nqueues {
            rings.push(sys::Ring::new(RING_SQ_ENTRIES, RING_CQ_ENTRIES).ok()?);
        }
        let waker = sys::Ring::new(2, 8).ok()?;
        let fuse_fd: OwnedFd = tempfile::tempfile().unwrap().into();
        let entries = nqueues * QUEUE_DEPTH;
        let arena = BufArena::new(entries).unwrap();
        let iovecs = build_iovecs(&arena, entries);
        Some(Arc::new(Shared {
            queues: rings
                .into_iter()
                .map(|ring| Queue {
                    ring,
                    commits: Mutex::new(Vec::new()),
                })
                .collect(),
            waker,
            fuse_fd,
            arena,
            iovecs,
            failed: AtomicBool::new(false),
        }))
    }

    /// MSG_RING support probe for the lifecycle tests (always true on
    /// kernels that can run the engine; CI sandboxes may differ).
    fn msg_ring_supported(shared: &Shared) -> bool {
        let mut out = Vec::new();
        let sqe = sys::Sqe128::msg_ring(shared.queues[0].ring.raw_fd(), 0xAA, 0, 0xBB);
        if shared.waker.push_submit_drain(&sqe, &mut out).is_err() {
            return false;
        }
        // The source CQE posts inline; anything else (missing or
        // res<0) means the wake cannot be trusted on this kernel.
        let ok = out.len() == 1 && out[0].res >= 0;
        if ok {
            // Eat the probe CQE so the test's own wake is the first
            // thing the queue ring sees.
            let mut cqes = Vec::new();
            shared.queues[0].ring.submit_and_wait(&mut cqes).unwrap();
        }
        ok
    }

    /// The slow-op handoff wake: `wake_queue` must surface in the
    /// target queue's ring as a `WAKE_UD` CQE — this is the only thing
    /// standing between a finished slow op and its commit.
    #[test]
    fn slow_handoff_wake_reaches_queue_ring() {
        let Some(shared) = test_shared(1) else {
            eprintln!("io_uring unavailable; skipping wake test");
            return;
        };
        if !msg_ring_supported(&shared) {
            eprintln!("MSG_RING unavailable; skipping wake test");
            return;
        }
        wake_queue(&shared, 0, 3);
        let mut cqes = Vec::new();
        shared.queues[0].ring.submit_and_wait(&mut cqes).unwrap();
        assert_eq!(cqes.len(), 1);
        assert_eq!(cqes[0].user_data, WAKE_UD, "wake must carry the sentinel");
    }

    /// Commit-list drain: every queued index must turn into exactly
    /// one staged COMMIT_AND_FETCH carrying that index as user_data
    /// (verified through real CQEs — against the regular-file stand-in
    /// each commit completes with an error, which is also the teardown
    /// shape).
    #[test]
    fn commit_list_drain_stages_one_commit_per_entry() {
        let Some(shared) = test_shared(1) else {
            eprintln!("io_uring unavailable; skipping drain test");
            return;
        };
        for idx in [2usize, 4, 6] {
            // SAFETY: test holds the only reference to these entries.
            let (header, _) = unsafe { shared.arena.slices(idx) };
            abi::EntInOut::write(header, 0xC0 + idx as u64, 0);
            shared.queues[0].commits.lock().unwrap().push(idx);
        }
        drain_commits(&shared, 0);
        assert!(
            shared.queues[0].commits.lock().unwrap().is_empty(),
            "drain must take the whole list"
        );
        let mut got = Vec::new();
        let mut cqes = Vec::new();
        while got.len() < 3 {
            shared.queues[0].ring.submit_and_wait(&mut cqes).unwrap();
            for c in &cqes {
                assert!(
                    c.res < 0,
                    "a regular file must reject the uring cmd (user_data={}, res={})",
                    c.user_data,
                    c.res
                );
                got.push(c.user_data);
            }
        }
        got.sort_unstable();
        assert_eq!(got, vec![2, 4, 6], "one commit per queued entry");
    }

    // r[verify builder.fs.io-uring-required]
    /// The registration gate: a REGISTER the kernel rejects completes
    /// inline, so `peek_failure` must surface it right after the
    /// submit — this is what turns a rejected registration into a
    /// failed mount instead of a silently degraded one. The peek must
    /// not consume: the pump still owes every entry its terminal
    /// accounting, so the same CQEs must drain it to exit afterwards.
    #[test]
    fn rejected_register_fails_gate_and_pump_still_drains() {
        let Some(shared) = test_shared(1) else {
            eprintln!("io_uring unavailable; skipping registration-gate test");
            return;
        };
        let ring = &shared.queues[0].ring;
        assert!(ring.peek_failure().is_none(), "no CQEs before submit");
        // The regular-file stand-in rejects every REGISTER inline —
        // same completion shape as a real fuse rejection (EINVAL from
        // a bad iovec geometry).
        for idx in 0..QUEUE_DEPTH {
            ring.push(&entry_sqe(&shared, idx, abi::FUSE_IO_URING_CMD_REGISTER, 0))
                .unwrap();
        }
        ring.submit().unwrap();
        let cqe = ring
            .peek_failure()
            .expect("rejected REGISTER must be visible to the gate");
        assert!(cqe.res < 0);
        // Peek again: idempotent (nothing consumed).
        assert!(ring.peek_failure().is_some());
        let mut served = Vec::new();
        let mut serve = |idx: usize| served.push(idx);
        pump(&shared, 0, QUEUE_DEPTH, &mut serve);
        assert!(served.is_empty(), "rejections must not be served");
    }

    /// Teardown with entries parked in both tiers: kernel-parked
    /// entries (here: REGISTERs that error) and a slow-pool entry that
    /// comes home through commit-list + wake must all reach a terminal
    /// CQE, after which the pump exits — the discipline that lets the
    /// abort write wind every queue thread down. The serve callback
    /// must never run (no entry is ever *delivered*).
    #[test]
    fn pump_exits_when_both_tiers_drain() {
        let Some(shared) = test_shared(1) else {
            eprintln!("io_uring unavailable; skipping teardown test");
            return;
        };
        if !msg_ring_supported(&shared) {
            eprintln!("MSG_RING unavailable; skipping teardown test");
            return;
        }
        // Tier 1: kernel-side entries. The regular file rejects the
        // REGISTER, standing in for the abort's -ENOTCONN completions.
        for idx in 0..QUEUE_DEPTH - 1 {
            shared.queues[0]
                .ring
                .push(&entry_sqe(&shared, idx, abi::FUSE_IO_URING_CMD_REGISTER, 0))
                .unwrap();
        }
        // Tier 2: one entry "returns from the slow pool": reply
        // trailer written, index queued, queue thread woken.
        let idx = QUEUE_DEPTH - 1;
        {
            // SAFETY: test holds the only reference to this entry.
            let (header, _) = unsafe { shared.arena.slices(idx) };
            abi::EntInOut::write(header, 99, 0);
        }
        shared.queues[0].commits.lock().unwrap().push(idx);
        wake_queue(&shared, 0, idx);

        let mut served = Vec::new();
        let mut serve = |idx: usize| served.push(idx);
        // All QUEUE_DEPTH entries owe a terminal CQE; pump must return
        // (a hang here is the bug this guards against).
        pump(&shared, 0, QUEUE_DEPTH, &mut serve);
        assert!(served.is_empty(), "no entry was ever delivered");
        assert!(shared.queues[0].commits.lock().unwrap().is_empty());
    }

    /// Delivery routing: a `res >= 0` CQE whose user_data is one of
    /// this queue's entry indices must reach `serve` exactly once, and
    /// one outside the queue's range must be dropped (serving it would
    /// index another queue's slice of the arena). Deliveries are
    /// faked with MSG_RING — same (user_data, res=0) shape the kernel
    /// produces for a real request.
    #[test]
    fn pump_routes_deliveries_by_entry_index() {
        let Some(shared) = test_shared(1) else {
            eprintln!("io_uring unavailable; skipping routing test");
            return;
        };
        if !msg_ring_supported(&shared) {
            eprintln!("MSG_RING unavailable; skipping routing test");
            return;
        }
        let fake_delivery = |data: u64| {
            let sqe = sys::Sqe128::msg_ring(shared.queues[0].ring.raw_fd(), data, 0, 0);
            let mut out = Vec::new();
            shared.waker.push_submit_drain(&sqe, &mut out).unwrap();
        };
        fake_delivery(3); // in range: queue 0 owns indices 0..8
        fake_delivery(QUEUE_DEPTH as u64); // queue 1's first index: foreign
        // Terminal CQEs for every entry so the pump can exit (the
        // regular file rejects each REGISTER).
        for idx in 0..QUEUE_DEPTH {
            shared.queues[0]
                .ring
                .push(&entry_sqe(&shared, idx, abi::FUSE_IO_URING_CMD_REGISTER, 0))
                .unwrap();
        }
        let mut served = Vec::new();
        let mut serve = |idx: usize| served.push(idx);
        pump(&shared, 0, QUEUE_DEPTH, &mut serve);
        assert_eq!(served, vec![3], "exactly the in-range delivery is served");
    }
}
