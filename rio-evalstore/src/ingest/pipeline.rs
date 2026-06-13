//! Pipeline internals: shared work deque, byte-budget accounting, reader
//! threads, chunk workers, and the NAR-sha256 spine.
//!
//! # Thread roles
//!
//! - **Readers** (R, spawned per ingest) pop work items — directories for
//!   readdir, files for open+read — from one shared deque. A reader that
//!   dequeues a directory lists it (sorted, NAR order) and pushes the
//!   children; this is what makes *discovery* parallel, the load-bearing
//!   property on true-cold trees.
//! - **The spine** (the calling thread) performs a DFS in exact NAR order,
//!   emitting canonical framing via [`rio_nix::nar::frame`] interleaved
//!   with file contents into a SHA-256 tee.
//! - **Chunk workers** (W) consume completed file buffers from an mpsc
//!   channel and produce per-file blake3 + FastCDC chunk lists.
//!
//! # Budget discipline (why readers never block holding a node)
//!
//! One mutex owns the deque AND the byte-budget balance; readers wait on
//! a single condvar. A reader takes a file off the deque only together
//! with its byte charge — if the budget cannot admit the file, the file
//! stays queued and the reader scans on (directories and smaller files
//! behind it remain reachable) or parks. Every budget release therefore
//! funds whatever pending file is closest to the deque front, which
//! tracks the spine's NAR position.
//!
//! Charges returned by buffer drops are staged in an atomic counter and
//! folded back into the balance in batches (and at every point the spine
//! could block or steal): on a page-cache-warm source the spine is the
//! last holder of nearly every buffer, and a per-drop lock + wakeup put
//! the queue mutex on the NAR-sha256 critical path for every file of the
//! tree. Staged bytes are unspendable until folded back, so memory use
//! only ever undershoots the accounted budget.
//!
//! The P1 profiling campaign measured the failure mode this prevents:
//! the previous design had each budget-starved reader block on the
//! budget *holding* one specific node. Once the budget saturated with
//! buffers the spine would only reach much later (out-of-NAR-order
//! completion after a slow sibling readdir), every reader was parked on
//! an unfundable node, the spine stole and read the entire remaining
//! tree inline at QD1 (~35 s on cold ext4 for nixpkgs), and the readers
//! drained the deque as no-ops and exited.
//!
//! Two structural guards:
//! - **Head reserve:** while any directory listing is in flight, a
//!   quarter of the budget cannot be charged. A listing in flight means
//!   NAR-earlier children may still be unborn, so read-ahead into
//!   NAR-later siblings is capped at ¾ of the budget — the reserve stays
//!   liquid to fund the spine's path once those children land at the
//!   deque front. (An oversized file — larger than the whole budget —
//!   is still admitted alone when the budget is fully free.)
//! - **Stolen-entry GC:** nodes the spine claims while still queued are
//!   dropped from the deque by the next reader scan, so the deque front
//!   stays at the spine's frontier and readers exit only when all work
//!   is genuinely accounted.
//!
//! # Progress guarantee (why the spine can steal)
//!
//! Reads complete out of NAR order, so the budget can fill up entirely
//! with files the spine has not reached yet while the spine's *next*
//! file cannot be admitted — a deadlock on adversarial tree shapes
//! (e.g. a slow readdir whose children sort before an already-read
//! sibling subtree). The escape hatch: when the spine reaches a node
//! whose IO has not *started*, it claims the node and does the IO
//! inline, bypassing the budget. A stolen read runs plane 2 inline too —
//! handing it to the chunk workers would park a budget-exempt buffer in
//! their channel, and with the workers behind, steal after steal would
//! grow that queue without bound. Kept inline, the stolen buffer dies
//! with the spine's visit, so the memory bound only widens by the one
//! file the spine is processing — and every wait the spine ever blocks
//! on is an in-flight reader operation that finishes without further
//! dependencies, so the pipeline always makes progress.
//!
//! Every blocking point has a wake path: readers wait on the queue
//! condvar, which is notified by child pushes, item completion, budget
//! release flushes, listing completion, the spine's stolen-listing
//! claims, the spine finishing its walk, and failure; the spine waits on
//! per-node condvars, which `resolve` notifies.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;

use fastcdc::v2020::FastCDC;
use rio_common::limits::{FASTCDC_AVG_BYTES, FASTCDC_MAX_BYTES, FASTCDC_MIN_BYTES};
use rio_nix::nar::{MAX_DIRECTORY_ENTRIES, MAX_NAR_DEPTH, frame};
use sha2::{Digest, Sha256};

use super::{
    IngestChunk, IngestConfig, IngestDir, IngestEntry, IngestError, IngestFile, IngestNode,
    IngestResult, IngestRunStats, IngestSymlink,
};

/// What the discovery `lstat` said a node is. Fixed before the node enters
/// the deque; the NAR header is written from these values, and a file
/// whose read disagrees with `size` fails the ingest
/// ([`IngestError::SizeChanged`]).
#[derive(Clone, Copy)]
enum Kind {
    File { size: u64, executable: bool },
    Dir,
}

/// Lifecycle of a file/dir node. Symlinks never become nodes — readlink
/// is one cheap syscall, done inline at discovery.
enum State {
    /// In the deque; IO not started. The spine may steal it.
    Pending,
    /// A reader (or the spine) is doing the IO. Always resolves to
    /// `FileReady`/`DirReady`/`Failed` — the IO needs nothing from the
    /// rest of the pipeline once started.
    Reading,
    /// File read complete; buffer parked for the spine.
    FileReady(Arc<FileBuf>),
    /// The spine hashed the contents and dropped its buffer reference
    /// (the chunk worker may still hold one).
    Consumed,
    /// Directory listed; children discovered and queued.
    DirReady(Arc<Vec<ChildEntry>>),
    /// IO failed; the error is in [`Shared::error_slot`].
    Failed,
}

/// A file or directory awaiting/undergoing/holding its IO result.
struct Node {
    /// Filesystem location, for the IO syscalls and error messages.
    path: PathBuf,
    /// Depth below the ingest root (root = 0), for the MAX_NAR_DEPTH bound.
    depth: usize,
    kind: Kind,
    /// Set (under the `state` lock) the moment anyone claims the IO.
    /// Read lock-free by the reader scan — a deque entry the spine
    /// already claimed is garbage-collected, never re-processed.
    taken: AtomicBool,
    state: Mutex<State>,
    cond: Condvar,
    /// Plane-2 output, written by a chunk worker.
    chunks: Mutex<Option<ChunkOutput>>,
}

impl Node {
    fn new(path: PathBuf, depth: usize, kind: Kind) -> Self {
        Self {
            path,
            depth,
            kind,
            taken: AtomicBool::new(false),
            state: Mutex::new(State::Pending),
            cond: Condvar::new(),
            chunks: Mutex::new(None),
        }
    }

    /// Atomically claim this node's IO. False means someone else (a
    /// reader or the spine) got there first — the caller skips it.
    fn claim(&self) -> bool {
        let mut st = self.state.lock().expect("node lock");
        if matches!(*st, State::Pending) {
            *st = State::Reading;
            self.taken.store(true, Ordering::Release);
            true
        } else {
            false
        }
    }

    /// Publish a state transition and wake the spine if it is parked here.
    fn resolve(&self, state: State) {
        *self.state.lock().expect("node lock") = state;
        self.cond.notify_all();
    }
}

/// A discovered directory entry, in NAR (byte-lex) order.
struct ChildEntry {
    name: Vec<u8>,
    child: Child,
}

/// Either an async node (file/dir) or an inline-resolved symlink.
enum Child {
    Node(Arc<Node>),
    Symlink(Vec<u8>),
}

/// Plane-2 result for one file.
struct ChunkOutput {
    digest: [u8; 32],
    chunks: Vec<IngestChunk>,
}

/// A read file's bytes plus its byte-budget charge. Both planes hold an
/// `Arc`; the charge returns to the budget when the last plane drops it —
/// that is what makes the budget simultaneously the prefetch window and
/// the tee bound.
struct FileBuf {
    data: Vec<u8>,
    /// 0 for spine-stolen reads (they bypass the budget).
    charge: u64,
    shared: Arc<Shared>,
}

impl Drop for FileBuf {
    fn drop(&mut self) {
        self.shared.release(self.charge);
    }
}

/// One unit of plane-2 work.
struct ChunkJob {
    node: Arc<Node>,
    buf: Arc<FileBuf>,
}

/// Work deque + termination accounting + the byte-budget balance. One
/// lock for all of it: a reader takes a file and its charge atomically,
/// so a freed byte always funds the frontmost (spine-nearest) pending
/// file instead of whichever reader happened to be parked.
struct QueueState {
    deque: VecDeque<Arc<Node>>,
    /// Items pushed but not yet finished (still queued OR in flight).
    /// Readers exit when this hits zero with an empty deque.
    outstanding: usize,
    /// Unspent byte budget.
    avail: u64,
    /// Directory listings currently running (readers or spine steals).
    /// While nonzero, charges may not dip into the head reserve — a
    /// listing in flight means NAR-earlier children may still be unborn.
    listings_in_flight: usize,
}

/// Everything the pipeline threads share. `Arc`d so file buffers can
/// release their charge from whichever thread drops them last; all
/// threads are still scoped and joined inside [`run`].
struct Shared {
    queue: Mutex<QueueState>,
    queue_cond: Condvar,
    /// Total byte budget (`IngestConfig::byte_budget`, min 1).
    budget_total: u64,
    /// Head reserve: the slice of the budget that must stay liquid while
    /// any listing is in flight. A quarter — enough to keep tens of
    /// typical small files revolving on the spine's path even when
    /// read-ahead has parked the rest of the budget out of NAR order.
    budget_floor: u64,
    /// Byte charges returned by buffer drops but not yet folded back
    /// into [`QueueState::avail`]. On a fast (page-cache-warm) source
    /// the spine is the last holder of nearly every buffer, so a
    /// per-drop lock + reader wakeup put ~8 µs of queue-mutex traffic on
    /// the NAR-sha256 critical path for every file of the tree — the
    /// measured bulk of the fast-device regression. Drops stage their
    /// charge here (one atomic add) instead; [`Shared::flush_releases`]
    /// folds it back in batches. Staged bytes are unspendable until
    /// flushed, so real memory use only ever undershoots the accounted
    /// budget — the bound is unchanged.
    pending_release: AtomicU64,
    /// Fold threshold for `pending_release` (budget/32, min 1): big
    /// enough to amortize the queue lock, small enough that read-ahead
    /// loses at most ~3% of its window to staging lag.
    release_batch: u64,
    /// Fast failure flag; the actual error lives in `error_slot`.
    error: AtomicBool,
    /// First failure wins; later ones are dropped.
    error_slot: Mutex<Option<IngestError>>,
    /// Files read by the reader pool (structural regression surface).
    reader_file_reads: AtomicU64,
    /// Files read inline by the spine via the steal escape hatch.
    spine_file_reads: AtomicU64,
    #[cfg(test)]
    test_delays: super::TestDelays,
}

#[cfg(test)]
fn test_sleep(delay: Option<std::time::Duration>) {
    if let Some(d) = delay {
        thread::sleep(d);
    }
}

impl Shared {
    fn failed(&self) -> bool {
        self.error.load(Ordering::Acquire)
    }

    /// Record the first failure and wake every parked thread so the
    /// whole pipeline winds down (readers exit, spine aborts).
    fn fail(&self, e: IngestError) {
        {
            let mut slot = self.error_slot.lock().expect("error slot lock");
            if slot.is_none() {
                *slot = Some(e);
            }
        }
        self.error.store(true, Ordering::Release);
        // Lock-then-notify: a reader that checked the flag before the
        // store is already parked on the condvar and gets the wakeup.
        self.notify_queue();
    }

    /// Return a byte charge to the budget. The charge is staged in
    /// `pending_release` (no lock, no wakeup — this runs on the spine
    /// for nearly every consumed buffer) and folded back into the
    /// spendable balance once a batch accumulates. The spine also
    /// flushes at every point where it could block or steal (see
    /// [`spine_wait_file`] / [`spine_wait_dir`]), so readers are never
    /// left parked behind staged budget while the spine is idle.
    fn release(&self, charge: u64) {
        if charge == 0 {
            return;
        }
        let staged = self.pending_release.fetch_add(charge, Ordering::AcqRel) + charge;
        if staged >= self.release_batch {
            self.flush_releases();
        }
    }

    /// Fold staged byte charges back into the spendable balance and wake
    /// parked readers. Cheap when nothing is staged.
    fn flush_releases(&self) {
        let staged = self.pending_release.swap(0, Ordering::AcqRel);
        if staged == 0 {
            return;
        }
        let mut q = self.queue.lock().expect("queue lock");
        q.avail += staged;
        drop(q);
        self.queue_cond.notify_all();
    }

    /// Lock-then-notify the queue condvar (stolen-listing claims, end of
    /// the spine walk, failure): a reader between its scan and its wait
    /// holds the queue lock, so acquiring it here serializes the notify
    /// after the wait begins.
    fn notify_queue(&self) {
        drop(self.queue.lock().expect("queue lock"));
        self.queue_cond.notify_all();
    }

    /// A directory listing finished without pushing children (claim lost
    /// to the spine, or the listing errored): undo its in-flight count.
    fn listing_done(&self) {
        let mut q = self.queue.lock().expect("queue lock");
        q.listings_in_flight -= 1;
        drop(q);
        // Dropping to zero unlocks the head reserve for charges.
        self.queue_cond.notify_all();
    }

    /// Count a directory listing that is about to start. The spine
    /// calls this BEFORE marking a stolen dir taken: a reader that
    /// garbage-collects the stale deque entry decrements `outstanding`,
    /// and if the unborn children were not represented by
    /// `listings_in_flight` at that moment, the readers could observe
    /// "deque empty, nothing outstanding, no listings" mid-tree and
    /// exit — leaving the spine to read everything that listing was
    /// about to discover alone at QD1 (the absorbing half of the
    /// measured steal-lock degraded mode).
    fn listing_started(&self) {
        self.queue.lock().expect("queue lock").listings_in_flight += 1;
    }

    /// Snapshot: could a reader charge `size` bytes right now? Drives
    /// the spine's steal grace — only a heuristic, so a stale answer
    /// costs at most one grace period, never correctness. Flushes staged
    /// releases first so the answer (and the readers racing to claim the
    /// file during the grace) sees the budget the spine just returned.
    fn reader_could_charge(&self, size: u64) -> bool {
        self.flush_releases();
        let q = self.queue.lock().expect("queue lock");
        charge_admissible(&q, self, size)
    }
}

/// How long the spine waits for a reader to claim a Pending file the
/// budget could fund before stealing it. Pure performance heuristic
/// (the steal still always happens, so liveness is untouched): on
/// true-cold trees discovery keeps every reader busy in multi-ms
/// listings, and without the grace the spine — already parked on the
/// frontier directory's condvar — wins the claim race for every
/// just-discovered file and serializes the whole ingest at QD1.
/// Unaffordable files steal immediately: readers could not take them
/// anyway, and the degraded-mode escape hatch must not slow down.
const STEAL_GRACE: std::time::Duration = std::time::Duration::from_millis(2);

/// Would a `size`-byte charge be admitted right now? The head reserve
/// applies while any listing is in flight; an oversized file (> the
/// whole budget) is admissible only when the budget is fully free.
fn charge_admissible(q: &QueueState, shared: &Shared, size: u64) -> bool {
    if size > shared.budget_total {
        return q.avail == shared.budget_total;
    }
    let floor = if q.listings_in_flight == 0 {
        0
    } else {
        shared.budget_floor
    };
    match q.avail.checked_sub(size) {
        Some(after) => after >= floor,
        None => false,
    }
}

/// Charge `size` bytes against the budget, honoring the head reserve.
/// Called under the queue lock (the balance lives in [`QueueState`]).
/// An oversized file is charged the whole budget so nothing else is
/// admitted alongside it.
fn try_charge(q: &mut QueueState, shared: &Shared, size: u64) -> Option<u64> {
    if !charge_admissible(q, shared, size) {
        return None;
    }
    let charge = size.min(shared.budget_total);
    q.avail -= charge;
    Some(charge)
}

/// Spine abort marker: the ingest failed and the real error is already
/// recorded in [`Shared::error_slot`].
struct Abort;

/// `Write` sink for the spine: SHA-256 + byte count, never fails.
struct TeeHasher {
    sha: Sha256,
    written: u64,
}

impl Write for TeeHasher {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.sha.update(buf);
        self.written += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The frame emitters return `io::Result` for real sinks; the spine's
/// sink is an in-memory hasher whose `Write` impl cannot fail.
fn emit(r: io::Result<()>) {
    r.expect("sha256 tee sink is infallible");
}

/// Entry point — see [`super::ingest_tree`].
pub(super) fn run(
    root: &Path,
    config: &IngestConfig,
) -> Result<(IngestResult, IngestRunStats), IngestError> {
    let readers = config.reader_threads.max(1);
    let workers = config.chunk_workers.max(1);
    let total = config.byte_budget.max(1);
    let shared = Arc::new(Shared {
        queue: Mutex::new(QueueState {
            deque: VecDeque::new(),
            outstanding: 0,
            avail: total,
            listings_in_flight: 0,
        }),
        queue_cond: Condvar::new(),
        budget_total: total,
        budget_floor: total / 4,
        pending_release: AtomicU64::new(0),
        release_batch: (total / 32).max(1),
        error: AtomicBool::new(false),
        error_slot: Mutex::new(None),
        reader_file_reads: AtomicU64::new(0),
        spine_file_reads: AtomicU64::new(0),
        #[cfg(test)]
        test_delays: config.test_delays,
    });

    // Discover the root on the calling thread (one lstat; readlink if it
    // is a symlink). Errors here never start a thread.
    let root_child = discover(root, 0)?;
    if let Child::Node(n) = &root_child {
        let mut q = shared.queue.lock().expect("queue lock");
        q.deque.push_back(Arc::clone(n));
        q.outstanding = 1;
    }

    let (chunk_tx, chunk_rx) = mpsc::channel::<ChunkJob>();
    let chunk_rx = Mutex::new(chunk_rx);

    // FORK SAFETY: every thread lives inside this scope; the scope joins
    // them all before `run` returns, on success and on error alike.
    let spine_out = thread::scope(|s| {
        for _ in 0..readers {
            let tx = chunk_tx.clone();
            let shared = &shared;
            s.spawn(move || reader_loop(shared, &tx));
        }
        for _ in 0..workers {
            s.spawn(|| chunk_worker(&shared, &chunk_rx));
        }
        // Only the readers send chunk jobs (the spine chunks stolen
        // reads inline). Once they exit — outstanding==0 on success,
        // the error flag on failure — and drop their senders, the
        // chunk workers see the disconnect, drain, and exit.
        drop(chunk_tx);

        let mut tee = TeeHasher {
            sha: Sha256::new(),
            written: 0,
        };
        emit(frame::magic(&mut tee));
        let out = spine_child(&shared, &mut tee, &root_child).map(|()| (tee.sha, tee.written));
        // The spine walk is complete; deque entries it claimed without a
        // wakeup may still be awaiting garbage collection by a parked
        // reader. One wake here lets the readers GC them, observe the
        // tree as fully accounted, and exit so the scope can join.
        shared.notify_queue();
        out
    });

    // All threads are joined; results and errors are final.
    if let Some(e) = shared.error_slot.lock().expect("error slot lock").take() {
        return Err(e);
    }
    let (sha, nar_size) = spine_out
        .map_err(|Abort| ())
        .expect("spine aborts only after recording an error");
    Ok((
        IngestResult {
            nar_sha256: sha.finalize().into(),
            nar_size,
            root: assemble(&root_child),
        },
        IngestRunStats {
            reader_file_reads: shared.reader_file_reads.load(Ordering::Relaxed),
            spine_file_reads: shared.spine_file_reads.load(Ordering::Relaxed),
        },
    ))
}

/// lstat one path and turn it into a `Child`. Symlinks resolve inline;
/// files and directories become deque-able nodes.
fn discover(path: &Path, depth: usize) -> Result<Child, IngestError> {
    if depth > MAX_NAR_DEPTH {
        return Err(IngestError::TooDeep {
            path: path.to_path_buf(),
            depth,
        });
    }
    let meta = fs::symlink_metadata(path).map_err(|e| IngestError::Stat {
        path: path.to_path_buf(),
        source: e,
    })?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        let target = fs::read_link(path).map_err(|e| IngestError::ReadLink {
            path: path.to_path_buf(),
            source: e,
        })?;
        use std::os::unix::ffi::OsStringExt;
        Ok(Child::Symlink(target.into_os_string().into_vec()))
    } else if ft.is_file() {
        use std::os::unix::fs::PermissionsExt;
        Ok(Child::Node(Arc::new(Node::new(
            path.to_path_buf(),
            depth,
            Kind::File {
                size: meta.len(),
                executable: meta.permissions().mode() & 0o111 != 0,
            },
        ))))
    } else if ft.is_dir() {
        Ok(Child::Node(Arc::new(Node::new(
            path.to_path_buf(),
            depth,
            Kind::Dir,
        ))))
    } else {
        Err(IngestError::UnsupportedFileType {
            path: path.to_path_buf(),
        })
    }
}

/// List a directory node: readdir, byte-lex sort (NAR entry order),
/// lstat + discover each child.
fn list_dir(node: &Node) -> Result<Vec<ChildEntry>, IngestError> {
    let read_dir_err = |e| IngestError::ReadDir {
        path: node.path.clone(),
        source: e,
    };
    let mut names: Vec<std::ffi::OsString> = Vec::new();
    for entry in fs::read_dir(&node.path).map_err(read_dir_err)? {
        names.push(entry.map_err(read_dir_err)?.file_name());
    }
    // Same producer-side bound as rio-nix's dump: a NAR every consumer
    // rejects must not be emitted.
    if names.len() >= MAX_DIRECTORY_ENTRIES {
        return Err(IngestError::TooManyEntries {
            path: node.path.clone(),
            count: names.len(),
        });
    }
    {
        use std::os::unix::ffi::OsStrExt;
        names.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    }
    let mut children = Vec::with_capacity(names.len());
    for name in names {
        let child = discover(&node.path.join(&name), node.depth + 1)?;
        use std::os::unix::ffi::OsStringExt;
        children.push(ChildEntry {
            name: name.into_vec(),
            child,
        });
    }
    Ok(children)
}

/// Queue a listing's file/dir children for the readers and mark the
/// listing finished. Pushed to the FRONT in reverse order: the deque
/// behaves as a DFS stack, so readers process the tree in approximate
/// NAR order and the byte budget prefetches the files the spine needs
/// next instead of distant subtrees.
fn push_children(shared: &Shared, children: &[ChildEntry]) {
    let mut q = shared.queue.lock().expect("queue lock");
    for e in children.iter().rev() {
        if let Child::Node(n) = &e.child {
            q.deque.push_front(Arc::clone(n));
            q.outstanding += 1;
        }
    }
    q.listings_in_flight -= 1;
    drop(q);
    shared.queue_cond.notify_all();
}

/// Read one file fully, enforcing the discovery-time length (the NAR
/// header is already committed to it once the spine writes it).
fn read_file(path: &Path, expected: u64) -> Result<Vec<u8>, IngestError> {
    let read_err = |e| IngestError::ReadFile {
        path: path.to_path_buf(),
        source: e,
    };
    let mut f = fs::File::open(path).map_err(read_err)?;
    let mut data = Vec::with_capacity(expected as usize);
    f.read_to_end(&mut data).map_err(read_err)?;
    if data.len() as u64 != expected {
        return Err(IngestError::SizeChanged {
            path: path.to_path_buf(),
            expected,
            actual: data.len() as u64,
        });
    }
    Ok(data)
}

/// One unit of reader work: a directory to list, or a file together
/// with its already-acquired byte charge.
enum Work {
    Dir(Arc<Node>),
    File(Arc<Node>, u64),
}

/// How many deque entries one scan inspects before parking. Bounds the
/// per-wake cost on very wide trees; entries beyond the window are
/// reached as the front drains (the spine's claims are GC'd from the
/// front, so the window follows the spine's frontier).
const SCAN_WINDOW: usize = 1024;

/// Reader thread body: take admissible work until the tree is finished
/// or the ingest failed.
fn reader_loop(shared: &Arc<Shared>, chunk_tx: &mpsc::Sender<ChunkJob>) {
    while let Some(work) = next_work(shared) {
        match work {
            Work::Dir(node) => process_dir(shared, &node),
            Work::File(node, charge) => process_file(shared, chunk_tx, &node, charge),
        }
        finish_item(shared);
    }
}

/// Scan the deque front for work this reader can start now: any
/// directory, or the first file whose byte charge the budget admits.
/// Entries the spine already claimed are garbage-collected in passing.
/// Parks on the queue condvar when nothing is admissible; returns `None`
/// when the tree is done or the ingest failed.
fn next_work(shared: &Shared) -> Option<Work> {
    let mut q = shared.queue.lock().expect("queue lock");
    loop {
        if shared.failed() {
            return None;
        }
        let mut i = 0;
        let mut scanned = 0;
        while i < q.deque.len() && scanned < SCAN_WINDOW {
            if q.deque[i].taken.load(Ordering::Acquire) {
                // Claimed by the spine while still queued. The steal
                // protocol guarantees claimed IO always completes, so
                // account it finished here (this is the only finish for
                // a deque-resident steal — nodes handed out as Work are
                // finished by their reader).
                q.deque.remove(i);
                q.outstanding -= 1;
                if q.outstanding == 0 {
                    shared.queue_cond.notify_all();
                }
                continue;
            }
            match q.deque[i].kind {
                Kind::Dir => {
                    let node = q.deque.remove(i).expect("scanned index");
                    q.listings_in_flight += 1;
                    return Some(Work::Dir(node));
                }
                Kind::File { size, .. } => {
                    if let Some(charge) = try_charge(&mut q, shared, size) {
                        let node = q.deque.remove(i).expect("scanned index");
                        return Some(Work::File(node, charge));
                    }
                }
            }
            i += 1;
            scanned += 1;
        }
        // Exit only when no listing can still push work: a stolen dir's
        // entry is GC'd (outstanding--) before the spine's inline
        // listing pushes its children, and that gap must not read as
        // "all done".
        if q.deque.is_empty() && q.outstanding == 0 && q.listings_in_flight == 0 {
            return None;
        }
        q = shared.queue_cond.wait(q).expect("queue lock");
    }
}

/// Mark one taken work item finished; the last one wakes readers parked
/// on an empty deque so they can exit.
fn finish_item(shared: &Shared) {
    let mut q = shared.queue.lock().expect("queue lock");
    q.outstanding -= 1;
    if q.outstanding == 0 {
        drop(q);
        shared.queue_cond.notify_all();
    }
}

fn process_dir(shared: &Shared, node: &Arc<Node>) {
    if !node.claim() {
        // Stolen by the spine between scan and claim; the spine's inline
        // listing carries its own in-flight count.
        shared.listing_done();
        return;
    }
    match list_dir(node) {
        Ok(children) => {
            let children = Arc::new(children);
            push_children(shared, &children);
            node.resolve(State::DirReady(children));
        }
        Err(e) => {
            shared.listing_done();
            // fail() before resolve(): the spine wakes on the node
            // condvar and must find the error recorded.
            shared.fail(e);
            node.resolve(State::Failed);
        }
    }
}

fn process_file(
    shared: &Arc<Shared>,
    chunk_tx: &mpsc::Sender<ChunkJob>,
    node: &Arc<Node>,
    charge: u64,
) {
    if !node.claim() {
        // Stolen by the spine between scan and claim.
        shared.release(charge);
        return;
    }
    let Kind::File { size, .. } = node.kind else {
        unreachable!("process_file is only called for file nodes")
    };
    match read_file(&node.path, size) {
        Ok(data) => {
            #[cfg(test)]
            test_sleep(shared.test_delays.read);
            shared.reader_file_reads.fetch_add(1, Ordering::Relaxed);
            let buf = Arc::new(FileBuf {
                data,
                charge,
                shared: Arc::clone(shared),
            });
            send_chunk_job(chunk_tx, node, &buf);
            node.resolve(State::FileReady(buf));
        }
        Err(e) => {
            shared.release(charge);
            shared.fail(e);
            node.resolve(State::Failed);
        }
    }
}

fn send_chunk_job(chunk_tx: &mpsc::Sender<ChunkJob>, node: &Arc<Node>, buf: &Arc<FileBuf>) {
    chunk_tx
        .send(ChunkJob {
            node: Arc::clone(node),
            buf: Arc::clone(buf),
        })
        // The receiver outlives every sender: workers exit only on
        // channel disconnect, which needs all senders dropped first.
        .expect("chunk workers outlive all senders");
}

/// `IngestChunk::len` is `u32`: FastCDC never emits a chunk longer than
/// `FASTCDC_MAX_BYTES`.
const _: () = assert!(FASTCDC_MAX_BYTES <= u32::MAX as usize);

/// Plane 2 for one file: whole-file blake3 + FastCDC chunk list, same
/// constants and call shape as rio-builder's fused output walk so chunks
/// dedup across the builder and the eval store. Called by the chunk
/// workers and, for stolen reads, inline by the spine.
fn chunk_buf(data: &[u8]) -> ChunkOutput {
    let digest = *blake3::hash(data).as_bytes();
    let mut chunks = Vec::new();
    if !data.is_empty() {
        // FastCDC::new panics on empty input; a zero-byte file has an
        // empty chunk run by contract.
        for c in FastCDC::new(
            data,
            FASTCDC_MIN_BYTES,
            FASTCDC_AVG_BYTES,
            FASTCDC_MAX_BYTES,
        ) {
            chunks.push(IngestChunk {
                digest: *blake3::hash(&data[c.offset..c.offset + c.length]).as_bytes(),
                offset: c.offset as u64,
                len: c.length as u32,
            });
        }
    }
    ChunkOutput { digest, chunks }
}

/// Chunk worker body: run plane 2 over reader-read buffers.
fn chunk_worker(shared: &Shared, rx: &Mutex<mpsc::Receiver<ChunkJob>>) {
    loop {
        // Holding the mutex across recv is deliberate: idle workers
        // queue on the mutex instead of the channel — same dispatch.
        let job = match rx.lock().expect("chunk rx lock").recv() {
            Ok(j) => j,
            Err(_) => return, // all senders dropped: pipeline drained
        };
        if shared.failed() {
            continue; // drop the buffer (releases budget), keep draining
        }
        *job.node.chunks.lock().expect("chunks lock") = Some(chunk_buf(&job.buf.data));
    }
}

/// Emit one child (symlink inline, file/dir via [`spine_node`]).
fn spine_child(shared: &Arc<Shared>, w: &mut TeeHasher, child: &Child) -> Result<(), Abort> {
    if shared.failed() {
        return Err(Abort);
    }
    match child {
        Child::Symlink(target) => {
            emit(frame::node_open(w));
            emit(frame::symlink(w, target));
            emit(frame::node_close(w));
            Ok(())
        }
        Child::Node(node) => spine_node(shared, w, node),
    }
}

/// Emit one file/dir node in canonical NAR token order.
fn spine_node(shared: &Arc<Shared>, w: &mut TeeHasher, node: &Arc<Node>) -> Result<(), Abort> {
    emit(frame::node_open(w));
    match node.kind {
        Kind::File { size, executable } => {
            let buf = spine_wait_file(shared, node)?;
            emit(frame::regular_header(w, executable, size));
            emit(w.write_all(&buf.data));
            emit(frame::contents_padding(w, size));
            #[cfg(test)]
            test_sleep(shared.test_delays.spine);
        }
        Kind::Dir => {
            let children = spine_wait_dir(shared, node)?;
            emit(frame::directory_open(w));
            for e in children.iter() {
                emit(frame::entry_open(w, &e.name));
                spine_child(shared, w, &e.child)?;
                emit(frame::entry_close(w));
            }
        }
    }
    emit(frame::node_close(w));
    Ok(())
}

/// Block until this file's buffer is available, stealing the read if it
/// has not started (see the module-level progress guarantee).
fn spine_wait_file(shared: &Arc<Shared>, node: &Arc<Node>) -> Result<Arc<FileBuf>, Abort> {
    let mut st = node.state.lock().expect("node lock");
    let mut graced = false;
    loop {
        match &*st {
            State::Pending => {
                let Kind::File { size, .. } = node.kind else {
                    unreachable!("spine_wait_file is only called for file nodes")
                };
                if !graced && shared.reader_could_charge(size) {
                    // Give the readers one chance to fund and claim this
                    // file before stealing (see STEAL_GRACE). A reader
                    // that claims wakes us when its read resolves; on
                    // timeout with the node still Pending we steal.
                    graced = true;
                    let (g, _) = node.cond.wait_timeout(st, STEAL_GRACE).expect("node lock");
                    st = g;
                    continue;
                }
                *st = State::Reading;
                node.taken.store(true, Ordering::Release);
                drop(st);
                // The stale deque entry is left for the next reader scan
                // to garbage-collect (any scan GCs every claimed entry it
                // walks over). No wakeup here: parked readers have
                // nothing else to do with it, and on a fast device this
                // path runs thousands of times per ingest — the final
                // wake in [`run`] covers the case where the tree ends on
                // a run of stolen files with every reader parked.
                return match read_file(&node.path, size) {
                    Ok(data) => {
                        #[cfg(test)]
                        test_sleep(shared.test_delays.read);
                        shared.spine_file_reads.fetch_add(1, Ordering::Relaxed);
                        // charge 0: the spine consumes this buffer right
                        // now; budgeting it could deadlock against the
                        // very files it is meant to evict. Plane 2 runs
                        // inline for the same reason — a chunk job would
                        // park this budget-exempt buffer in the workers'
                        // queue, unbounded across repeated steals.
                        *node.chunks.lock().expect("chunks lock") = Some(chunk_buf(&data));
                        let buf = Arc::new(FileBuf {
                            data,
                            charge: 0,
                            shared: Arc::clone(shared),
                        });
                        node.resolve(State::Consumed);
                        Ok(buf)
                    }
                    Err(e) => {
                        shared.fail(e);
                        node.resolve(State::Failed);
                        Err(Abort)
                    }
                };
            }
            State::Reading => {
                // About to block on a reader's in-flight IO: hand any
                // staged budget back first so the readers can spend it
                // while the spine sleeps. Nested node→queue locking,
                // the one allowed order.
                shared.flush_releases();
                st = node.cond.wait(st).expect("node lock");
            }
            State::FileReady(buf) => {
                let buf = Arc::clone(buf);
                // Drop the node's buffer reference now — keeping it
                // until assembly would hold every file of the tree in
                // memory and defeat the tee bound.
                *st = State::Consumed;
                return Ok(buf);
            }
            State::Failed => return Err(Abort),
            State::Consumed | State::DirReady(_) => {
                unreachable!("spine visits each node exactly once")
            }
        }
    }
}

/// Block until this directory's listing is available, stealing the
/// readdir if it has not started.
fn spine_wait_dir(shared: &Arc<Shared>, node: &Arc<Node>) -> Result<Arc<Vec<ChildEntry>>, Abort> {
    let mut st = node.state.lock().expect("node lock");
    loop {
        match &*st {
            State::Pending => {
                // The listing must be counted in flight BEFORE the node
                // reads as taken (see Shared::listing_started) — a GC of
                // the stale deque entry between the two would let the
                // readers exit while this listing's children are unborn.
                // Nested node→queue locking, the one allowed order.
                shared.listing_started();
                *st = State::Reading;
                node.taken.store(true, Ordering::Release);
                drop(st);
                // Wake readers to GC the stale deque entry.
                shared.notify_queue();
                return match list_dir(node) {
                    Ok(children) => {
                        let children = Arc::new(children);
                        // Still queue the children: the readers prefetch
                        // them while the spine descends.
                        push_children(shared, &children);
                        node.resolve(State::DirReady(Arc::clone(&children)));
                        Ok(children)
                    }
                    Err(e) => {
                        shared.listing_done();
                        shared.fail(e);
                        node.resolve(State::Failed);
                        Err(Abort)
                    }
                };
            }
            State::Reading => {
                // Same as spine_wait_file: flush staged budget before
                // blocking on the in-flight listing.
                shared.flush_releases();
                st = node.cond.wait(st).expect("node lock");
            }
            State::DirReady(children) => return Ok(Arc::clone(children)),
            State::Failed => return Err(Abort),
            State::Consumed | State::FileReady(_) => {
                unreachable!("spine_wait_dir is only called for dir nodes")
            }
        }
    }
}

/// Fold the resolved skeleton into the public tree. Only called after a
/// fully successful run with all threads joined, so every file has its
/// chunk output and every directory its listing.
fn assemble(child: &Child) -> IngestNode {
    match child {
        Child::Symlink(target) => IngestNode::Symlink(IngestSymlink {
            target: target.clone(),
        }),
        Child::Node(node) => match node.kind {
            Kind::File { size, executable } => {
                let out = node
                    .chunks
                    .lock()
                    .expect("chunks lock")
                    .take()
                    .expect("chunk plane finished before threads joined");
                IngestNode::File(IngestFile {
                    digest: out.digest,
                    size,
                    executable,
                    chunks: out.chunks,
                })
            }
            Kind::Dir => {
                let children = {
                    let st = node.state.lock().expect("node lock");
                    match &*st {
                        State::DirReady(c) => Arc::clone(c),
                        _ => unreachable!("successful spine resolved every directory"),
                    }
                };
                IngestNode::Dir(IngestDir {
                    entries: children
                        .iter()
                        .map(|e| IngestEntry {
                            name: e.name.clone(),
                            node: assemble(&e.child),
                        })
                        .collect(),
                })
            }
        },
    }
}
